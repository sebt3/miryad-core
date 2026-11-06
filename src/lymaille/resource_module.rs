//! Module Rhai `resources` : accès des agents `lymaille` aux `MiryadResource` (`src/lymaille/
//! resource_module.sdd`). Une seule troisième surface (DSD `FR-2`, `DEC-15`, écritures au lot 2
//! `DEC-21`) : chaque lecture délègue à `rest::core::list` / `rest::core::get` et chaque écriture
//! à `rest::core::create` / `update` / `delete` — mêmes politiques, mêmes hooks que REST et MCP,
//! **aucune décision d'accès propre** (précédent : `src/mcp/registry.sdd`). Le principal est
//! reconstruit à **chaque appel** depuis le `subject` de la session (DSD `FR-4`, `DEC-3`) par
//! `Identity::resolve`, jamais mis en cache. Fonctions Rhai synchrones, pont `async` par
//! `vynil_core::rt::block_on` (précédent : les modules de `lymaille`). En step batch
//! (`scope = Some(WriteScope)`), les écritures sont idempotentes au rejeu par le journal
//! d'écritures (`store/write_journal.sdd`) : `replay`, sinon écrire, puis `record`.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use rhai::{Array, Dynamic, EvalAltResult, ImmutableString, Map, Module, Position};
use sea_orm::sea_query::Iden;
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};
use serde_json::{Value as JsonValue, json};
use vynil_core::rt::block_on;

use crate::auth::{AuthPrincipal, PrincipalSource};
use crate::ir::{EntityIr, resource_ir};
use crate::lymaille::store::write_journal::{Op, WriteScope, op_key, record, replay};
use crate::query::Pagination;
use crate::rbac::{ListAccess, can_create, list_access};
use crate::rest::RestEntity;
use crate::rest::core;
use crate::rest::error::RestError;
use crate::users::user;

use super::error::LymailleError;
use crate::lymaille::identity::Identity;

/// Plafond de `per_page` côté agent (DSD `OI-6`) : la taille d'une réponse est bornée ici et par
/// le plafond de rendu du bac à sable de `lymaille` (`SandboxLimits`), pas par un second plafond.
pub const LLM_MAX_PER_PAGE: u64 = 50;

/// `per_page` par défaut quand le script n'en donne pas — les 100 de la REST ne sont pas la
/// granularité d'un contexte de LLM.
pub const LLM_DEFAULT_PER_PAGE: u64 = 20;

/// Échecs du module, rendus en messages Rhai **fixes et courts** (spec `Must` « Erreurs »,
/// amendement lot 2) : jamais le texte brut d'une `DbErr` ni d'une `RestError` — le détail part
/// en `tracing::warn!`, sans contenu de ligne. Seule exception assumée : le message d'un
/// `HookError`, texte applicatif sans code ni contenu de ligne, rendu tel quel après `hook refused: `.
enum Failure {
    /// Ressource absente du registre — ou absente de sa partie écriture (`register_read_only`) :
    /// message `unknown resource: <nom>`.
    UnknownResource(String),
    /// `subject` de la session sans compte local (`Identity::resolve` → `UnknownSubject`).
    UnknownSubject,
    /// `RestError::NotFound` délégué.
    NotFound,
    /// `RestError::Forbidden` délégué — l'oracle d'existence de `rest::core` est préservé.
    Forbidden,
    /// Corps de script refusé par la conversion en `E::Model` : champ inconnu, type invalide ou
    /// forme non-objet (amendement lot 2).
    InvalidBody,
    /// Violation de contrainte d'unicité en création, sans correspondance trouvée en relecture
    /// (amendement lot 2, fenêtre de crash).
    Conflict,
    /// Marqueur de contrôle : violation d'unicité en création — relue par `label_column` par
    /// [`Handle::create`] avant le `conflict` ; jamais rendue telle quelle.
    UniqueViolation,
    /// `HookError` applicatif délégué : `hook refused: <message>` (amendement lot 2).
    HookRefused(String),
    /// Plafond [`MAX_WRITES_PER_RUN`] atteint pour la session ou le step (amendement lot 2).
    WriteLimit,
    /// Toute autre cause (détail déjà expédié en `warn!` par [`Failure::log`]).
    Internal,
}

/// Plafond d'écritures par session interactive ou par step (`max_writes_per_run` de la spec,
/// amendement lot 2) : borne un agent qui s'emballe. Compté à chaque appel `create`, `update`
/// ou `delete` — y compris au rejeu, pour que le compteur d'une reprise reproduise exactement
/// les appels du run d'origine.
const MAX_WRITES_PER_RUN: usize = 100;

impl Failure {
    /// Trace le détail (jamais un contenu de ligne) et rend la variante `internal error`.
    fn log(detail: impl std::fmt::Display) -> Self {
        tracing::warn!("lymaille resources module: {detail}");
        Self::Internal
    }

    /// Le pont `block_on` : refus du pont lui-même (runtime `current_thread`, exécution annulée)
    /// est une panne d'infrastructure, trace et rend `internal error`.
    fn bridge(detail: impl std::fmt::Display) -> Self {
        Self::log(detail)
    }

    /// Trace le détail (jamais un contenu de ligne) et rend la variante demandée.
    fn logged(detail: impl std::fmt::Display, failure: Self) -> Self {
        tracing::warn!("lymaille resources module: {detail}");
        failure
    }

    /// Message fixe rendu au script.
    fn message(&self) -> String {
        match self {
            Self::UnknownResource(name) => format!("unknown resource: {name}"),
            Self::UnknownSubject => "no account for this session".to_string(),
            Self::NotFound => "not found".to_string(),
            Self::Forbidden => "forbidden".to_string(),
            Self::InvalidBody => "invalid body".to_string(),
            // `UniqueViolation` est un marqueur de contrôle rattrapé par la relecture ; si elle
            // atteignait le script, le `conflict` est le seul rendu honnête.
            Self::Conflict | Self::UniqueViolation => "conflict".to_string(),
            Self::HookRefused(message) => format!("hook refused: {message}"),
            Self::WriteLimit => "write limit reached".to_string(),
            Self::Internal => "internal error".to_string(),
        }
    }

    /// Erreur d'exécution Rhai portant le message fixe (forme des modules de `lymaille`).
    fn into_eval_error(self) -> EvalAltResult {
        EvalAltResult::ErrorRuntime(Dynamic::from(self.message()), Position::NONE)
    }
}

impl From<Failure> for Box<EvalAltResult> {
    fn from(failure: Failure) -> Self {
        Box::new(failure.into_eval_error())
    }
}

impl From<RestError> for Failure {
    fn from(err: RestError) -> Self {
        match err {
            RestError::NotFound => Self::NotFound,
            RestError::Forbidden => Self::Forbidden,
            // Le `HookError` est un texte applicatif, sans code `MRD-*` ni contenu de ligne :
            // sa `message` est rendue telle quel après le préfixe (amendement lot 2).
            RestError::Application(hook) => {
                Self::logged(hook.message.clone(), Self::HookRefused(hook.message))
            }
            other => Self::log(other),
        }
    }
}

impl From<LymailleError> for Failure {
    fn from(err: LymailleError) -> Self {
        match err {
            LymailleError::UnknownSubject => Self::UnknownSubject,
            other => Self::log(other),
        }
    }
}

impl From<sea_orm::DbErr> for Failure {
    fn from(err: sea_orm::DbErr) -> Self {
        Self::log(err)
    }
}

/// Trait privé d'accès en lecture à une entité enregistrée — enveloppe type-érasée de
/// `E: RestEntity`, déléguant intégralement à `rest::core` (aucune décision d'accès ici).
#[async_trait::async_trait]
trait Reader: Send + Sync {
    /// IR de l'entité (`resource_ir::<E>()`), pour `describe()`.
    fn ir(&self) -> EntityIr;

    /// La ressource est-elle lisible par cet appelant — `rbac::list_access` ≠ `Forbidden`, sans
    /// lire de ligne.
    async fn list_readable(
        &self,
        db: &DatabaseConnection,
        user: &user::Model,
    ) -> Result<bool, sea_orm::DbErr>;

    /// Liste déléguée : `rest::core::list` avec les valeurs de pagination déjà normalisées.
    async fn list(
        &self,
        db: &DatabaseConnection,
        principal: &AuthPrincipal,
        page: u64,
        per_page: u64,
        filter: Option<&str>,
    ) -> Result<JsonValue, Failure>;

    /// Ligne déléguée : `rest::core::get`.
    async fn get(
        &self,
        db: &DatabaseConnection,
        principal: &AuthPrincipal,
        id: i32,
    ) -> Result<JsonValue, Failure>;
}

struct ReaderImpl<E>(std::marker::PhantomData<E>);

#[async_trait::async_trait]
impl<E: RestEntity> Reader for ReaderImpl<E> {
    fn ir(&self) -> EntityIr {
        resource_ir::<E>()
    }

    async fn list_readable(
        &self,
        db: &DatabaseConnection,
        user: &user::Model,
    ) -> Result<bool, sea_orm::DbErr> {
        Ok(!matches!(
            list_access::<E>(db, user).await?,
            ListAccess::Forbidden
        ))
    }

    async fn list(
        &self,
        db: &DatabaseConnection,
        principal: &AuthPrincipal,
        page: u64,
        per_page: u64,
        filter: Option<&str>,
    ) -> Result<JsonValue, Failure> {
        let page_result = core::list::<E>(db, principal, Some(page), Some(per_page), filter).await?;
        serde_json::to_value(page_result).map_err(Failure::log)
    }

    async fn get(
        &self,
        db: &DatabaseConnection,
        principal: &AuthPrincipal,
        id: i32,
    ) -> Result<JsonValue, Failure> {
        let record = core::get::<E>(db, principal, id).await?;
        serde_json::to_value(record).map_err(Failure::log)
    }
}

/// Trait privé d'accès en écriture à une entité enregistrée — enveloppe type-érasée de
/// `E: RestEntity`, déléguant intégralement à `rest::core::{create, update, delete}` (mêmes
/// `write_policy`, `owner_column` et hooks que REST et MCP, aucune décision d'accès ici —
/// amendement lot 2, DSD `DEC-21`).
#[async_trait::async_trait]
trait Writer: Send + Sync {
    /// Le principal peut-il écrire d'après `rbac` (`can_create` sur la `write_policy`, sans lire
    /// de ligne) — porte `writable` de `describe()`.
    async fn writable(&self, db: &DatabaseConnection, user: &user::Model) -> Result<bool, sea_orm::DbErr>;

    /// Création déléguée : `rest::core::create`. Une violation d'unicité rend
    /// [`Failure::UniqueViolation`] pour que le rejeu batch la rattape par relecture.
    async fn create(
        &self,
        db: &DatabaseConnection,
        principal: &AuthPrincipal,
        body: &JsonValue,
    ) -> Result<JsonValue, Failure>;

    /// Mise à jour déléguée : `rest::core::update` (naturellement idempotente).
    async fn update(
        &self,
        db: &DatabaseConnection,
        principal: &AuthPrincipal,
        id: i32,
        body: &JsonValue,
    ) -> Result<JsonValue, Failure>;

    /// Suppression déléguée : `rest::core::delete`, rendue `#{ deleted: true }`.
    async fn delete(
        &self,
        db: &DatabaseConnection,
        principal: &AuthPrincipal,
        id: i32,
    ) -> Result<JsonValue, Failure>;

    /// Relecture après violation d'unicité en création (fenêtre de crash du batch) : la ligne
    /// dont `label_column` porte la valeur du corps et dont tous les champs non-PK du corps sont
    /// égaux, sinon `None`.
    async fn reread_matching(
        &self,
        db: &DatabaseConnection,
        body: &JsonValue,
    ) -> Result<Option<JsonValue>, Failure>;
}

struct WriterImpl<E>(std::marker::PhantomData<E>);

/// Corps Rhai (déjà en JSON) converti en `E::Model` : un champ inconnu des champs de
/// `resource_ir::<E>()` ou un type invalide au sens serde est `invalid body` — le texte serde
/// part en trace, jamais au script.
fn model_from_json<E: RestEntity>(body: &JsonValue) -> Result<E::Model, Failure> {
    let Some(object) = body.as_object() else {
        return Err(Failure::InvalidBody);
    };
    let ir = resource_ir::<E>();
    if !object
        .keys()
        .all(|key| ir.fields.iter().any(|field| &field.name == key))
    {
        return Err(Failure::InvalidBody);
    }
    serde_json::from_value(body.clone()).map_err(|err| Failure::logged(err, Failure::InvalidBody))
}

/// Nom de champ de la clé primaire (la comparaison de relecture l'ignore : la PK est retirée du
/// corps par `rest::core::create`, une PK auto-attribuée n'est jamais « égale » au `0` du corps).
fn primary_key_field<E: RestEntity>() -> Option<String> {
    resource_ir::<E>()
        .fields
        .into_iter()
        .find(|field| field.is_primary_key)
        .map(|field| field.name)
}

#[async_trait::async_trait]
impl<E: RestEntity> Writer for WriterImpl<E> {
    async fn writable(&self, db: &DatabaseConnection, user: &user::Model) -> Result<bool, sea_orm::DbErr> {
        can_create::<E>(db, user).await
    }

    async fn create(
        &self,
        db: &DatabaseConnection,
        principal: &AuthPrincipal,
        body: &JsonValue,
    ) -> Result<JsonValue, Failure> {
        let model = model_from_json::<E>(body)?;
        match core::create::<E>(db, principal, model).await {
            Ok(created) => serde_json::to_value(created).map_err(Failure::log),
            Err(RestError::Database(err))
                if matches!(err.sql_err(), Some(sea_orm::SqlErr::UniqueConstraintViolation(_))) =>
            {
                Err(Failure::UniqueViolation)
            }
            Err(other) => Err(other.into()),
        }
    }

    async fn update(
        &self,
        db: &DatabaseConnection,
        principal: &AuthPrincipal,
        id: i32,
        body: &JsonValue,
    ) -> Result<JsonValue, Failure> {
        let model = model_from_json::<E>(body)?;
        let updated = core::update::<E>(db, principal, id, model).await?;
        serde_json::to_value(updated).map_err(Failure::log)
    }

    async fn delete(
        &self,
        db: &DatabaseConnection,
        principal: &AuthPrincipal,
        id: i32,
    ) -> Result<JsonValue, Failure> {
        core::delete::<E>(db, principal, id).await?;
        Ok(json!({ "deleted": true }))
    }

    async fn reread_matching(
        &self,
        db: &DatabaseConnection,
        body: &JsonValue,
    ) -> Result<Option<JsonValue>, Failure> {
        let Some(label_column) = E::label_column() else {
            return Ok(None);
        };
        let Some(object) = body.as_object() else {
            return Ok(None);
        };
        let label_name = label_column.to_string();
        let Some(label_value) = (match object.get(&label_name) {
            Some(JsonValue::String(text)) => Some(sea_orm::Value::from(text.as_str())),
            Some(JsonValue::Number(number)) => number.as_i64().map(sea_orm::Value::from),
            _ => None,
        }) else {
            return Ok(None);
        };
        let primary_key = primary_key_field::<E>();
        for row in E::find().filter(label_column.eq(label_value)).all(db).await? {
            let row_json = serde_json::to_value(&row).map_err(Failure::log)?;
            let matches = object.iter().all(|(key, value)| {
                if primary_key.as_deref() == Some(key.as_str()) {
                    return true;
                }
                row_json.get(key) == Some(value)
            });
            if matches {
                return Ok(Some(row_json));
            }
        }
        Ok(None)
    }
}

/// Ce que le module expose d'une entité enregistrée : la lecture toujours, l'écriture seulement
/// si `register` (et non `register_read_only`) l'a ouverte.
struct Entry {
    reader: Box<dyn Reader>,
    writer: Option<Box<dyn Writer>>,
}

/// Registre type-érasé des entités lues et écrites par les agents (précédent :
/// `McpToolRegistry`, `src/mcp/registry.sdd`) — `BTreeMap` trié par `resource_name`, aucune
/// entité par défaut : l'application choisit celles qu'elle ouvre aux agents.
#[derive(Default)]
pub struct ResourceRegistry {
    entries: BTreeMap<&'static str, Entry>,
}

/// Nom du lot 1 du registre, conservé comme alias déprécié de [`ResourceRegistry`] (amendement
/// lot 2, DSD `DEC-21`) : le registre porte désormais les écritures, son nom dit registre, pas
/// lecteurs.
#[deprecated(note = "renamed to `ResourceRegistry` (amendement lot 2, DSD `DEC-21`)")]
pub type ResourceReaders = ResourceRegistry;

impl ResourceRegistry {
    /// Registre vide.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Enregistre lecture **et** écriture de `E` sous `E::resource_name()`, rend `&mut Self` pour
    /// l'enchaînement.
    ///
    /// # Panics
    ///
    /// Une clé déjà présente — y compris le même type enregistré deux fois — est une erreur de
    /// programmation détectée au démarrage (`assert!` explicite, même règle que
    /// `src/mcp/registry.rs`).
    pub fn register<E: RestEntity>(&mut self) -> &mut Self {
        let name = E::resource_name();
        assert!(
            !self.entries.contains_key(name),
            "`{name}` is already registered in the agent resource registry — duplicate \
             `resource_name` (the same entity type registered twice counts) is a programming \
             error, refusing to register it a second time"
        );
        self.entries.insert(
            name,
            Entry {
                reader: Box::new(ReaderImpl::<E>(std::marker::PhantomData)),
                writer: Some(Box::new(WriterImpl::<E>(std::marker::PhantomData))),
            },
        );
        self
    }

    /// N'enregistre que la lecture de `E` — les verbes d'écriture traitent la ressource comme
    /// absente du registre (`unknown resource: <nom>` : le verbe n'est pas exposé pour elle,
    /// aucune décision d'accès propre). Même `assert!` de collision que [`Self::register`].
    ///
    /// # Panics
    ///
    /// Une clé déjà présente — y compris le même type enregistré deux fois — est une erreur de
    /// programmation détectée au démarrage (`assert!` explicite, même règle que
    /// `src/mcp/registry.rs`).
    pub fn register_read_only<E: RestEntity>(&mut self) -> &mut Self {
        let name = E::resource_name();
        assert!(
            !self.entries.contains_key(name),
            "`{name}` is already registered in the agent resource registry — duplicate \
             `resource_name` (the same entity type registered twice counts) is a programming \
             error, refusing to register it a second time"
        );
        self.entries.insert(
            name,
            Entry {
                reader: Box::new(ReaderImpl::<E>(std::marker::PhantomData)),
                writer: None,
            },
        );
        self
    }
}

/// Convertit une valeur Rhai (corps de script) en `serde_json::Value` — réciproque de
/// [`json_to_dynamic`] ; une valeur de forme non convertible (fonction, tableau interne) rend
/// `None`, traduite en `invalid body` par l'appelant.
fn dynamic_to_json(value: &Dynamic) -> Option<JsonValue> {
    if value.is::<()>() {
        Some(JsonValue::Null)
    } else if value.is::<bool>() {
        value.as_bool().ok().map(JsonValue::Bool)
    } else if value.is::<i64>() {
        value.as_int().ok().map(|number| JsonValue::Number(number.into()))
    } else if value.is::<f64>() {
        value
            .as_float()
            .ok()
            .and_then(|number| serde_json::Number::from_f64(number).map(JsonValue::Number))
    } else if let Ok(text) = value.clone().into_string() {
        Some(JsonValue::String(text))
    } else if let Some(items) = value.clone().try_cast::<Array>() {
        let converted = items
            .into_iter()
            .map(|item| dynamic_to_json(&item))
            .collect::<Option<Vec<JsonValue>>>()?;
        Some(JsonValue::Array(converted))
    } else if let Some(map) = value.clone().try_cast::<Map>() {
        let mut object = serde_json::Map::new();
        for (key, item) in map {
            object.insert(key.to_string(), dynamic_to_json(&item)?);
        }
        Some(JsonValue::Object(object))
    } else {
        None
    }
}

/// Convertit un `serde_json::Value` (sortie de la délégation `rest::core`) en valeur Rhai.
/// `rhai` est compilé sans sa feature `serde` : la conversion est faite ici.
fn json_to_dynamic(value: JsonValue) -> Dynamic {
    match value {
        JsonValue::Null => Dynamic::UNIT,
        JsonValue::Bool(b) => Dynamic::from(b),
        JsonValue::Number(number) => number
            .as_i64()
            .map(Dynamic::from)
            .or_else(|| number.as_f64().map(Dynamic::from))
            .unwrap_or(Dynamic::UNIT),
        JsonValue::String(s) => Dynamic::from(s),
        JsonValue::Array(items) => Dynamic::from(items.into_iter().map(json_to_dynamic).collect::<Array>()),
        JsonValue::Object(object) => Dynamic::from(
            object
                .into_iter()
                .map(|(key, value)| (key.into(), json_to_dynamic(value)))
                .collect::<Map>(),
        ),
    }
}

/// Construit une valeur Rhai `#{}` à partir de ses champs nommés (forme des modules de
/// `lymaille`).
fn rhai_map<const N: usize>(fields: [(&str, Dynamic); N]) -> Dynamic {
    Dynamic::from(
        fields
            .into_iter()
            .map(|(key, value)| (key.into(), value))
            .collect::<Map>(),
    )
}

/// Entrée `#{ name, filter_column, fields, writable }` de `describe()` — `filter_column` vaut
/// `()` quand l'entité n'en déclare pas ; `fields` est le tableau des `#{ name, type, nullable,
/// primary_key }` de `resource_ir::<E>()` ; `writable` est le droit d'écrire du principal
/// (amendement lot 2).
fn describe_entry(name: &str, ir: EntityIr, writable: bool) -> Dynamic {
    let fields: Array = ir
        .fields
        .into_iter()
        .map(|field| {
            rhai_map([
                ("name", Dynamic::from(field.name)),
                ("type", Dynamic::from(field.r#type)),
                ("nullable", Dynamic::from(field.nullable)),
                ("primary_key", Dynamic::from(field.is_primary_key)),
            ])
        })
        .collect();
    rhai_map([
        ("name", Dynamic::from(name.to_owned())),
        (
            "filter_column",
            ir.filter_column.map_or(Dynamic::UNIT, Dynamic::from),
        ),
        ("fields", Dynamic::from(fields)),
        ("writable", Dynamic::from(writable)),
    ])
}

/// État partagé des fonctions du module : registre, connexion, `subject` de la session, scope
/// d'idempotence du step batch et compteur d'écritures de la session ou du step.
#[derive(Clone)]
struct Handle {
    registry: Arc<ResourceRegistry>,
    db: DatabaseConnection,
    subject: String,
    scope: Option<WriteScope>,
    writes: Arc<AtomicUsize>,
}

impl Handle {
    /// Plafond d'écritures de la session ou du step : chaque appel `create`, `update` ou
    /// `delete` en consomme un, avant toute autre décision — un run rejoué rejoue le même
    /// nombre d'appels et se voit refuser les mêmes.
    fn charge_write(&self) -> Result<(), Failure> {
        if self.writes.fetch_add(1, Ordering::Relaxed) >= MAX_WRITES_PER_RUN {
            return Err(Failure::WriteLimit);
        }
        Ok(())
    }

    /// Partie écriture du registre pour `resource` — une ressource enregistrée en lecture seule
    /// (`register_read_only`) n'expose pas les verbes : `unknown resource: <nom>`.
    fn writer_of(&self, resource: &str) -> Result<&dyn Writer, Failure> {
        self.registry
            .entries
            .get(resource)
            .and_then(|entry| entry.writer.as_deref())
            .ok_or_else(|| Failure::UnknownResource(resource.to_owned()))
    }

    /// `Identity::resolve` du `subject` à CET instant, puis le `AuthPrincipal` passé à
    /// `rest::core` (DSD `FR-4`, `DEC-3` : jamis mis en cache entre deux appels). Une écriture
    /// batch sans compte de déclencheur échoue là (`no account for this session`, `FR-10`).
    async fn resolve_principal(&self) -> Result<AuthPrincipal, Failure> {
        let caller = Identity::new(self.db.clone()).resolve(&self.subject).await?;
        Ok(AuthPrincipal {
            subject: caller.subject,
            email: None,
            preferred_username: None,
            // Seule variante non portée par un transport HTTP (`auth/principal.sdd`,
            // amendement 2026-11-02) : accès d'agent reconstruit hors requête ; aucun
            // consommateur ne lit `source`.
            source: PrincipalSource::Agent,
        })
    }

    /// `describe()` : une entrée par ressource enregistrée dont `list_access` ≠ `Forbidden`,
    /// triées par nom (`entries` est un `BTreeMap`) ; `writable` est le droit d'écrire du
    /// principal d'après `rbac`, sans lire de ligne — une ressource non ouverte à l'écriture
    /// (`register_read_only`) n'y est jamais inscriptible.
    async fn describe(&self) -> Result<Array, Failure> {
        let caller = Identity::new(self.db.clone()).resolve(&self.subject).await?;
        let account = user::Entity::find_by_id(caller.user_id)
            .one(&self.db)
            .await?
            .ok_or(Failure::UnknownSubject)?;
        let mut entries = Array::new();
        for (name, entry) in &self.registry.entries {
            if entry.reader.list_readable(&self.db, &account).await? {
                let writable = match &entry.writer {
                    Some(writer) => writer.writable(&self.db, &account).await?,
                    None => false,
                };
                entries.push(describe_entry(name, entry.reader.ir(), writable));
            }
        }
        Ok(entries)
    }

    /// `list()` : principal résolu, ressource cherchée dans le registre, pagination normalisée
    /// par `Pagination::from_raw` puis écrêtée à `LLM_MAX_PER_PAGE`, délégation à `rest::core`.
    async fn list(
        &self,
        resource: &str,
        filter: Option<&str>,
        page: Option<u64>,
        per_page: Option<u64>,
    ) -> Result<Dynamic, Failure> {
        let principal = self.resolve_principal().await?;
        let entry = self
            .registry
            .entries
            .get(resource)
            .ok_or_else(|| Failure::UnknownResource(resource.to_owned()))?;
        let pagination = Pagination::from_raw(page, Some(per_page.unwrap_or(LLM_DEFAULT_PER_PAGE)));
        let per_page = pagination.per_page.min(LLM_MAX_PER_PAGE);
        let value = entry
            .reader
            .list(&self.db, &principal, pagination.page, per_page, filter)
            .await?;
        Ok(json_to_dynamic(value))
    }

    /// `get()` : principal résolu, ressource cherchée, ligne déléguée à `rest::core::get`.
    async fn get(&self, resource: &str, id: i64) -> Result<Dynamic, Failure> {
        let principal = self.resolve_principal().await?;
        let entry = self
            .registry
            .entries
            .get(resource)
            .ok_or_else(|| Failure::UnknownResource(resource.to_owned()))?;
        let id = i32::try_from(id).map_err(|_| Failure::log("get id out of the i32 range"))?;
        let value = entry.reader.get(&self.db, &principal, id).await?;
        Ok(json_to_dynamic(value))
    }

    /// `create()` (amendement lot 2) — ordre d'idempotence imposé par `store/write_journal.sdd` :
    /// `replay` ; sinon écrire ; puis `record`. La violation d'unicité au rejeu batch est
    /// rattrapée par relecture sur `label_column` : contenu égal, succès journalisé ; contenu
    /// différent, `conflict`.
    async fn create(&self, resource: String, body: JsonValue) -> Result<Dynamic, Failure> {
        self.charge_write()?;
        let principal = self.resolve_principal().await?;
        let writer = self.writer_of(&resource)?;
        let key = op_key(Op::Create, &resource, None, &body);
        if let Some(scope) = &self.scope
            && let Some(journaled) = replay(&self.db, scope, &key).await?
        {
            return Ok(json_to_dynamic(journaled));
        }
        let outcome = writer.create(&self.db, &principal, &body).await;
        let value = match outcome {
            Ok(value) => value,
            Err(Failure::UniqueViolation) if self.scope.is_some() => {
                match writer.reread_matching(&self.db, &body).await? {
                    Some(row) => row,
                    None => return Err(Failure::Conflict),
                }
            }
            Err(Failure::UniqueViolation) => return Err(Failure::Conflict),
            Err(other) => return Err(other),
        };
        self.record_write(&key, &value).await?;
        Ok(json_to_dynamic(value))
    }

    /// `update()` — naturellement idempotent ; le journal rend le résultat déjà obtenu au rejeu.
    async fn update(&self, resource: String, id: i64, body: JsonValue) -> Result<Dynamic, Failure> {
        self.charge_write()?;
        let principal = self.resolve_principal().await?;
        let id = i32::try_from(id).map_err(|_| Failure::log("update id out of the i32 range"))?;
        let writer = self.writer_of(&resource)?;
        let key = op_key(Op::Update, &resource, Some(id), &body);
        if let Some(scope) = &self.scope
            && let Some(journaled) = replay(&self.db, scope, &key).await?
        {
            return Ok(json_to_dynamic(journaled));
        }
        let value = writer.update(&self.db, &principal, id, &body).await?;
        self.record_write(&key, &value).await?;
        Ok(json_to_dynamic(value))
    }

    /// `delete()` — naturellement idempotent ; une ligne déjà absente sans que le journal
    /// l'atteste reste `not found` (le succès du rejeu vient du `replay`, pas d'un pardon).
    async fn delete(&self, resource: String, id: i64) -> Result<Dynamic, Failure> {
        self.charge_write()?;
        let principal = self.resolve_principal().await?;
        let id = i32::try_from(id).map_err(|_| Failure::log("delete id out of the i32 range"))?;
        let writer = self.writer_of(&resource)?;
        let key = op_key(Op::Delete, &resource, Some(id), &JsonValue::Null);
        if let Some(scope) = &self.scope
            && let Some(journaled) = replay(&self.db, scope, &key).await?
        {
            return Ok(json_to_dynamic(journaled));
        }
        let value = writer.delete(&self.db, &principal, id).await?;
        self.record_write(&key, &value).await?;
        Ok(json_to_dynamic(value))
    }

    /// `record` du résultat de l'écriture dans le journal du step (aucun effet en session
    /// interactive, `scope = None`).
    async fn record_write(&self, key: &str, result: &JsonValue) -> Result<(), Failure> {
        if let Some(scope) = &self.scope {
            record(&self.db, scope, key, result).await?;
        }
        Ok(())
    }
}

/// Pont sync → async d'une future du module (forme du `bridge` des modules de `lymaille`) : le
/// refus du pont lui-même est tracé et rendu `internal error`.
fn drive<T>(future: impl Future<Output = Result<T, Failure>>) -> Result<T, Box<EvalAltResult>> {
    match block_on(future) {
        Ok(outcome) => outcome.map_err(Into::into),
        Err(bridge_err) => Err(Failure::bridge(bridge_err).into()),
    }
}

/// Construit le module Rhai `resources` : `describe()`, `list(ressource, filtre)`,
/// `list(ressource, filtre, page, par_page)`, `get(ressource, id)`, et les verbes d'écriture
/// `create(ressource, corps)`, `update(ressource, id, corps)`, `delete(ressource, id)`
/// (amendement lot 2). `scope = None` : session interactive, pas de journal ; `scope = Some` :
/// step batch idempotent par `store/write_journal.sdd`.
#[must_use]
pub fn resources_module(
    registry: Arc<ResourceRegistry>,
    db: DatabaseConnection,
    subject: String,
    scope: Option<WriteScope>,
) -> Module {
    let mut module = Module::new();
    let handle = Handle {
        registry,
        db,
        subject,
        scope,
        writes: Arc::new(AtomicUsize::new(0)),
    };

    let describe_handle = handle.clone();
    let _describe = module.set_native_fn("describe", move || -> Result<Array, Box<EvalAltResult>> {
        drive(describe_handle.describe())
    });
    let list_handle = handle.clone();
    let _list_2 = module.set_native_fn(
        "list",
        move |resource: ImmutableString, filter: ImmutableString| -> Result<Dynamic, Box<EvalAltResult>> {
            let filter = if filter.is_empty() {
                None
            } else {
                Some(filter.as_str())
            };
            drive(list_handle.list(resource.as_str(), filter, None, None))
        },
    );
    let page_list_handle = handle.clone();
    let _list_4 = module.set_native_fn(
        "list",
        move |resource: ImmutableString,
              filter: ImmutableString,
              page: i64,
              per_page: i64|
              -> Result<Dynamic, Box<EvalAltResult>> {
            let filter = if filter.is_empty() {
                None
            } else {
                Some(filter.as_str())
            };
            drive(page_list_handle.list(
                resource.as_str(),
                filter,
                u64::try_from(page).ok(),
                u64::try_from(per_page).ok(),
            ))
        },
    );
    let get_handle = handle.clone();
    let _get = module.set_native_fn(
        "get",
        move |resource: ImmutableString, id: i64| -> Result<Dynamic, Box<EvalAltResult>> {
            drive(get_handle.get(resource.as_str(), id))
        },
    );
    let create_handle = handle.clone();
    let _create = module.set_native_fn(
        "create",
        move |resource: ImmutableString, body: Map| -> Result<Dynamic, Box<EvalAltResult>> {
            match dynamic_to_json(&Dynamic::from(body)) {
                Some(body) => drive(create_handle.create(resource.to_string(), body)),
                None => Err(Failure::InvalidBody.into()),
            }
        },
    );
    let update_handle = handle.clone();
    let _update = module.set_native_fn(
        "update",
        move |resource: ImmutableString, id: i64, body: Map| -> Result<Dynamic, Box<EvalAltResult>> {
            match dynamic_to_json(&Dynamic::from(body)) {
                Some(body) => drive(update_handle.update(resource.to_string(), id, body)),
                None => Err(Failure::InvalidBody.into()),
            }
        },
    );
    let delete_handle = handle;
    let _delete = module.set_native_fn(
        "delete",
        move |resource: ImmutableString, id: i64| -> Result<Dynamic, Box<EvalAltResult>> {
            drive(delete_handle.delete(resource.to_string(), id))
        },
    );
    module
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migration::Migrator;
    use crate::resource::{AccessPolicy, MiryadResource};
    use sea_orm::entity::prelude::*;
    use sea_orm::{ActiveValue::Set, ConnectionTrait, Database, EntityTrait, Schema};
    use sea_orm_migration::MigratorTrait;
    use serde::{Deserialize, Serialize};

    /// Ressource `Public` avec colonne de filtre déclarée — fixture du `Scenario` « describe ne
    /// liste que les ressources lisibles » et des tests de pagination.
    mod public_res {
        use super::*;

        #[derive(Clone, Debug, PartialEq, Serialize, Deserialize, DeriveEntityModel)]
        #[sea_orm(table_name = "llm_publics")]
        pub struct Model {
            #[sea_orm(primary_key)]
            pub id: i32,
            pub title: String,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {}

        impl ActiveModelBehavior for ActiveModel {}

        impl MiryadResource for Entity {
            fn resource_name() -> &'static str {
                "public_res"
            }
            fn read_policy() -> AccessPolicy {
                AccessPolicy::Public
            }
            fn write_policy() -> AccessPolicy {
                AccessPolicy::Public
            }
            fn owner_column() -> Option<Column> {
                None
            }
            fn filter_column() -> Option<Column> {
                Some(Column::Title)
            }
        }
    }

    /// Ressource `AdminOnly` — fixtures des Scenario « describe », « une ressource inconnue ou
    /// interdite ne fuit pas » et « le droit est relu à chaque appel ».
    mod secret_res {
        use super::*;

        #[derive(Clone, Debug, PartialEq, Serialize, Deserialize, DeriveEntityModel)]
        #[sea_orm(table_name = "llm_secrets")]
        pub struct Model {
            #[sea_orm(primary_key)]
            pub id: i32,
            pub title: String,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {}

        impl ActiveModelBehavior for ActiveModel {}

        impl MiryadResource for Entity {
            fn resource_name() -> &'static str {
                "secret_res"
            }
            fn read_policy() -> AccessPolicy {
                AccessPolicy::AdminOnly
            }
            fn write_policy() -> AccessPolicy {
                AccessPolicy::AdminOnly
            }
            fn owner_column() -> Option<Column> {
                None
            }
        }
    }

    /// Ressource `OwnerOnly` avec colonne propriétaire — fixture du `Scenario` « list et get
    /// suivent les mêmes politiques que REST ».
    mod owner_res {
        use super::*;

        #[derive(Clone, Debug, PartialEq, Serialize, Deserialize, DeriveEntityModel)]
        #[sea_orm(table_name = "llm_owners")]
        pub struct Model {
            #[sea_orm(primary_key)]
            pub id: i32,
            pub title: String,
            pub owner_id: i32,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {}

        impl ActiveModelBehavior for ActiveModel {}

        impl MiryadResource for Entity {
            fn resource_name() -> &'static str {
                "owner_res"
            }
            fn read_policy() -> AccessPolicy {
                AccessPolicy::OwnerOnly
            }
            fn write_policy() -> AccessPolicy {
                AccessPolicy::OwnerOnly
            }
            fn owner_column() -> Option<Column> {
                Some(Column::OwnerId)
            }
        }
    }

    /// Ressource `Public` en lecture, `AdminOnly` en écriture, sans colonne propriétaire —
    /// fixture du `Scenario` lot 2 « describe indique writable » : lisible, non inscriptible
    /// pour un non-admin.
    mod gated_res {
        use super::*;

        #[derive(Clone, Debug, PartialEq, Serialize, Deserialize, DeriveEntityModel)]
        #[sea_orm(table_name = "llm_gateds")]
        pub struct Model {
            #[sea_orm(primary_key)]
            pub id: i32,
            pub title: String,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {}

        impl ActiveModelBehavior for ActiveModel {}

        impl MiryadResource for Entity {
            fn resource_name() -> &'static str {
                "gated_res"
            }
            fn read_policy() -> AccessPolicy {
                AccessPolicy::Public
            }
            fn write_policy() -> AccessPolicy {
                AccessPolicy::AdminOnly
            }
            fn owner_column() -> Option<Column> {
                None
            }
        }
    }

    /// Ressource `Public` dont `before_create` refuse un libellé vide — fixture du `Scenario`
    /// lot 2 « un hook refuse avec son message ».
    mod hooked {
        use super::*;
        use crate::resource::HookError;

        #[derive(Clone, Debug, PartialEq, Serialize, Deserialize, DeriveEntityModel)]
        #[sea_orm(table_name = "llm_hooked")]
        pub struct Model {
            #[sea_orm(primary_key)]
            pub id: i32,
            pub label: String,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {}

        impl ActiveModelBehavior for ActiveModel {}

        impl MiryadResource for Entity {
            fn resource_name() -> &'static str {
                "hooked"
            }
            fn read_policy() -> AccessPolicy {
                AccessPolicy::Public
            }
            fn write_policy() -> AccessPolicy {
                AccessPolicy::Public
            }
            fn owner_column() -> Option<Column> {
                None
            }

            fn before_create(
                active: ActiveModel,
                _principal: &crate::auth::AuthPrincipal,
            ) -> Result<ActiveModel, HookError> {
                let label = match &active.label {
                    sea_orm::ActiveValue::Set(value) | sea_orm::ActiveValue::Unchanged(value) => {
                        value.clone()
                    }
                    sea_orm::ActiveValue::NotSet => String::new(),
                };
                if label.is_empty() {
                    return Err(HookError::new("label must not be empty"));
                }
                Ok(active)
            }
        }
    }

    /// Ressource `Public` à colonne `name` sous contrainte `UNIQUE` (index posé à la main, la
    /// fixture n'a pas de migration) et `label_column` déclarée — fixture du `Scenario` lot 2
    /// « une fenêtre de crash est rattrapée par relecture ».
    mod unique_res {
        use super::*;

        #[derive(Clone, Debug, PartialEq, Serialize, Deserialize, DeriveEntityModel)]
        #[sea_orm(table_name = "llm_uniques")]
        pub struct Model {
            #[sea_orm(primary_key)]
            pub id: i32,
            pub name: String,
            pub note: String,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {}

        impl ActiveModelBehavior for ActiveModel {}

        impl MiryadResource for Entity {
            fn resource_name() -> &'static str {
                "unique_res"
            }
            fn read_policy() -> AccessPolicy {
                AccessPolicy::Public
            }
            fn write_policy() -> AccessPolicy {
                AccessPolicy::Public
            }
            fn owner_column() -> Option<Column> {
                None
            }
            fn label_column() -> Option<Column> {
                Some(Column::Name)
            }
        }
    }

    /// Base sqlite en mémoire migrée (`miryad_users` comprise, exigée par la chaîne déléguée ;
    /// sous `lymaille`, la migration `m20261104_000001` pose aussi la table du journal).
    async fn migrated_db() -> DatabaseConnection {
        let db = Database::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite connects");
        Migrator::up(&db, None).await.expect("migrations apply cleanly");
        db
    }

    /// Crée la table physique d'une entité fixture sur une base déjà migrée.
    async fn create_entity_table<E: sea_orm::EntityTrait>(db: &DatabaseConnection, entity: E) {
        let schema = Schema::new(db.get_database_backend());
        db.execute(&schema.create_table_from_entity(entity))
            .await
            .expect("entity table creates");
    }

    /// Crée l'index `UNIQUE` de la fixture `unique_res` (la fixture n'a pas de migration).
    async fn create_unique_index(db: &DatabaseConnection) {
        db.execute_unprepared("CREATE UNIQUE INDEX llm_uniques_name_unique ON llm_uniques (name)")
            .await
            .expect("l'index unique de la fixture crée");
    }

    /// Nombre de lignes du journal d'écritures (`m20261104_000001` l'a posée, base migrée).
    async fn journal_row_count(db: &DatabaseConnection) -> usize {
        let mut select = sea_orm::sea_query::Query::select();
        select
            .column(sea_orm::sea_query::Alias::new("op_key"))
            .from(sea_orm::sea_query::Alias::new("miryad_lymaille_write_journal"));
        db.query_all(&select)
            .await
            .expect("comptage des lignes du journal")
            .len()
    }

    /// `WriteScope` de fixture — run et step d'un step batch simulé.
    fn write_scope() -> WriteScope {
        WriteScope {
            run_key: "run-fixture".to_string(),
            step_key: "step-fixture".to_string(),
        }
    }

    /// Registre des deux seules ressources du `Given` du Scenario `describe` : une `Public`,
    /// une `AdminOnly`.
    fn public_and_secret_readers() -> ResourceRegistry {
        let mut readers = ResourceRegistry::new();
        readers
            .register::<public_res::Entity>()
            .register::<secret_res::Entity>();
        readers
    }

    /// Les trois entités du registre, clés en ordre croissant.
    fn all_readers() -> ResourceRegistry {
        let mut readers = ResourceRegistry::new();
        readers
            .register::<public_res::Entity>()
            .register::<owner_res::Entity>()
            .register::<secret_res::Entity>();
        readers
    }

    /// Moteur Rhai avec le module `resources` du `subject` — exécution de scripts réels, jamais
    /// d'appel Rust contournant le module. Session interactive (`scope = None`).
    fn engine(registry: ResourceRegistry, db: DatabaseConnection, subject: &str) -> rhai::Engine {
        engine_scope(registry, db, subject, None)
    }

    /// Moteur Rhai avec `scope` — `Some` pour simuler un step batch rejouable.
    fn engine_scope(
        registry: ResourceRegistry,
        db: DatabaseConnection,
        subject: &str,
        scope: Option<WriteScope>,
    ) -> rhai::Engine {
        let module = resources_module(Arc::new(registry), db, subject.to_string(), scope);
        let mut engine = rhai::Engine::new();
        let _registered = engine.register_static_module("resources", Arc::new(module));
        engine
    }

    /// Principal de fixture à l'appel direct de `rest::core` (forme du registre MCP).
    fn rest_principal(subject: &str) -> AuthPrincipal {
        AuthPrincipal {
            subject: subject.to_string(),
            email: None,
            preferred_username: None,
            source: PrincipalSource::ApiToken { token_id: 0 },
        }
    }

    /// Texte de l'erreur d'un script qui doit échouer : le message utile porté par
    /// `ErrorRuntime` (sans son préfixe d'affichage `Runtime error: `), ou le `Display` de
    /// l'erreur de Rhai elle-même (fonction inconnue).
    fn error_text<T: std::fmt::Debug>(result: Result<T, Box<EvalAltResult>>) -> String {
        match result {
            Ok(value) => panic!("le script doit échouer, il a rendu {value:?}"),
            Err(error) => match *error {
                EvalAltResult::ErrorRuntime(value, _) => {
                    value.into_string().unwrap_or_else(ToString::to_string)
                }
                other => other.to_string(),
            },
        }
    }

    fn as_map(value: Dynamic) -> Map {
        value.try_cast::<Map>().expect("une valeur de tableau de champs")
    }

    fn as_array(value: &Dynamic) -> Array {
        value.clone().try_cast::<Array>().expect("un tableau")
    }

    fn text(map: &Map, key: &str) -> String {
        map.get(key)
            .unwrap_or_else(|| panic!("la clé `{key}` est rendue"))
            .clone()
            .into_string()
            .unwrap_or_else(|v| panic!("la clé `{key}` est une chaîne, rendue : {v:?}"))
    }

    fn boolean(map: &Map, key: &str) -> bool {
        map.get(key)
            .unwrap_or_else(|| panic!("la clé `{key}` est rendue"))
            .as_bool()
            .unwrap_or_else(|v| panic!("la clé `{key}` est un booléen, rendu : {v:?}"))
    }

    fn integer(map: &Map, key: &str) -> i64 {
        map.get(key)
            .unwrap_or_else(|| panic!("la clé `{key}` est rendue"))
            .as_int()
            .unwrap_or_else(|v| panic!("la clé `{key}` est un entier, rendu : {v:?}"))
    }

    /// Le `#{ name, type, nullable, primary_key }` du champ nommé dans le tableau `fields`.
    fn field(fields: &Array, name: &str) -> Map {
        fields
            .iter()
            .map(|f| as_map(f.clone()))
            .find(|f| text(f, "name") == name)
            .unwrap_or_else(|| panic!("le champ `{name}` est rendu : {fields:?}"))
    }

    // ——— Scenario : « describe ne liste que les ressources lisibles » ———
    #[tokio::test(flavor = "multi_thread")]
    async fn describe_lists_only_readable_resources() {
        let db = migrated_db().await;
        create_entity_table(&db, public_res::Entity).await;
        create_entity_table(&db, secret_res::Entity).await;
        crate::users::resolve_user(&db, "alice", None)
            .await
            .expect("le compte local d'alice");
        let engine = engine(public_and_secret_readers(), db, "alice");

        let described = engine
            .eval::<Array>(r"resources::describe()")
            .expect("describe réussit");
        assert_eq!(
            described.len(),
            1,
            "seule la ressource `Public` apparaît pour un non-admin"
        );
        let entry = as_map(described.into_iter().next().expect("une entrée"));
        assert_eq!(text(&entry, "name"), "public_res");
        assert_eq!(
            text(&entry, "filter_column"),
            "title",
            "la colonne de filtre déclarée est rendue"
        );
        let fields = as_array(entry.get("fields").expect("la clé fields"));
        assert_eq!(fields.len(), 2, "les deux colonnes de l'entité");
        let names: Vec<String> = fields.iter().map(|f| text(&as_map(f.clone()), "name")).collect();
        assert_eq!(names, ["id", "title"]);
        let title = field(&fields, "title");
        assert_eq!(text(&title, "type"), "string");
        assert!(!boolean(&title, "nullable"));
        assert!(!boolean(&title, "primary_key"));
        let id = field(&fields, "id");
        assert!(boolean(&id, "primary_key"));
    }

    // ——— Scenario : « list et get suivent les mêmes politiques que REST » ———
    // Écart mesuré à l'exécution, remonté en question : le `Then` de la spec dit `not found`
    // comme `rest::core::get`, or `rest::core::get` refuse la ligne étrangère `OwnerOnly` par
    // `RestError::Forbidden` (`static_verdict` → `None`, `can_read` → `false`). Le test verrouille
    // la PARITÉ avec le verdict direct de `rest::core::get`, quelle que soit sa variante.
    #[tokio::test(flavor = "multi_thread")]
    async fn list_and_get_follow_the_same_policies_as_rest() {
        let db = migrated_db().await;
        create_entity_table(&db, owner_res::Entity).await;
        let alice = crate::users::resolve_user(&db, "alice", None)
            .await
            .expect("le compte local d'alice");
        let bob = crate::users::resolve_user(&db, "bob", None)
            .await
            .expect("le compte local de bob");
        let alice_row = owner_res::Entity::insert(owner_res::ActiveModel {
            title: Set("la tarte d'alice".to_string()),
            owner_id: Set(alice.id),
            ..Default::default()
        })
        .exec(&db)
        .await
        .expect("la ligne d'alice est insérée");
        let bob_row = owner_res::Entity::insert(owner_res::ActiveModel {
            title: Set("la tarte de bob".to_string()),
            owner_id: Set(bob.id),
            ..Default::default()
        })
        .exec(&db)
        .await
        .expect("la ligne de bob est insérée");

        let engine = engine(all_readers(), db.clone(), "alice");

        let listed = engine
            .eval::<Dynamic>(r#"resources::list("owner_res", "")"#)
            .expect("list réussit");
        let listed = as_map(listed);
        let items = as_array(listed.get("items").expect("la clé items"));
        assert_eq!(items.len(), 1, "list ne rend que les lignes de l'appelant");
        let item = as_map(items[0].clone());
        assert_eq!(
            integer(&item, "id"),
            i64::from(alice_row.last_insert_id),
            "la ligne rendue est bien celle insérée pour alice"
        );
        assert_eq!(
            integer(&item, "owner_id"),
            i64::from(alice.id),
            "la ligne rendue est celle d'alice"
        );

        // Parité exacte avec l'appel REST direct, mêmes entrées normalisées.
        let direct = crate::rest::core::list::<owner_res::Entity>(
            &db,
            &rest_principal("alice"),
            Some(1),
            Some(crate::lymaille::resource_module::LLM_DEFAULT_PER_PAGE),
            None,
        )
        .await
        .expect("rest::core::list direct réussit");
        assert_eq!(
            integer(&listed, "page"),
            i64::try_from(direct.page).expect("page tient en i64")
        );
        assert_eq!(
            integer(&listed, "total_items"),
            i64::try_from(direct.total_items).expect("total_items tient en i64")
        );

        // `get` sur la ligne de l'autre : le module rend le message fixe correspondant au verdict
        // DIRECT de `rest::core::get` — aucune décision propre ici.
        let direct_get = crate::rest::core::get::<owner_res::Entity>(
            &db,
            &rest_principal("alice"),
            bob_row.last_insert_id,
        )
        .await;
        let expected = match direct_get {
            Err(RestError::NotFound) => "not found",
            Err(RestError::Forbidden) => "forbidden",
            other => panic!("le get direct doit échouer, il a rendu {other:?}"),
        };
        let script = format!(r#"resources::get("owner_res", {})"#, bob_row.last_insert_id);
        let message = error_text(engine.eval::<Dynamic>(&script));
        assert_eq!(message, expected, "la parité avec rest::core::get est le contrat");
    }

    // ——— Scenario : « une ressource inconnue ou interdite ne fuit pas » ———
    #[tokio::test(flavor = "multi_thread")]
    async fn unknown_or_forbidden_resource_does_not_leak() {
        let db = migrated_db().await;
        create_entity_table(&db, secret_res::Entity).await;
        let inserted = secret_res::Entity::insert(secret_res::ActiveModel {
            title: Set("contenu classifié".to_string()),
            ..Default::default()
        })
        .exec(&db)
        .await
        .expect("la ligne secret est insérée");
        crate::users::resolve_user(&db, "alice", None)
            .await
            .expect("le compte local d'alice");
        let engine = engine(all_readers(), db, "alice");

        let message = error_text(engine.eval::<Dynamic>(&format!(
            r#"resources::get("secret_res", {})"#,
            inserted.last_insert_id
        )));
        assert_eq!(message, "forbidden", "ressource AdminOnly par un non-admin");
        assert!(
            !message.contains("contenu classifié"),
            "aucun contenu de ligne : {message}"
        );
        assert!(
            !message.contains("MRD-"),
            "jamais de code MRD-* au script : {message}"
        );

        let message = error_text(engine.eval::<Dynamic>(r#"resources::get("n_existe_pas", 1)"#));
        assert_eq!(message, "unknown resource: n_existe_pas");
    }

    // ——— Scenario : « la pagination est écrêtée » ———
    #[tokio::test(flavor = "multi_thread")]
    async fn pagination_is_capped() {
        let db = migrated_db().await;
        create_entity_table(&db, public_res::Entity).await;
        for n in 0..80 {
            public_res::Entity::insert(public_res::ActiveModel {
                title: Set(format!("ligne {n}")),
                ..Default::default()
            })
            .exec(&db)
            .await
            .expect("les lignes de fixture sont insérées");
        }
        crate::users::resolve_user(&db, "alice", None)
            .await
            .expect("le compte local d'alice");
        let engine = engine(all_readers(), db, "alice");

        let listed = as_map(
            engine
                .eval::<Dynamic>(r#"resources::list("public_res", "", 1, 1000)"#)
                .expect("list réussit"),
        );
        assert_eq!(
            integer(&listed, "per_page"),
            50,
            "per_page est écrêté à LLM_MAX_PER_PAGE"
        );
        assert_eq!(
            as_array(listed.get("items").expect("la clé items")).len(),
            50,
            "items compte 50 lignes"
        );
        assert_eq!(integer(&listed, "total_items"), 80);
    }

    // ——— Scenario : « le droit est relu à chaque appel » ———
    #[tokio::test(flavor = "multi_thread")]
    async fn rights_are_reread_on_every_call() {
        let db = migrated_db().await;
        create_entity_table(&db, secret_res::Entity).await;
        secret_res::Entity::insert(secret_res::ActiveModel {
            title: Set("classifié".to_string()),
            ..Default::default()
        })
        .exec(&db)
        .await
        .expect("la ligne secret est insérée");
        let alice = crate::users::resolve_user(&db, "alice", None)
            .await
            .expect("le compte local d'alice");
        crate::users::membership::sync_group_memberships(&db, alice.id, &["admin".to_string()])
            .await
            .expect("alice membre du groupe admin seedé");
        let engine = engine(all_readers(), db.clone(), "alice");

        let listed = as_map(
            engine
                .eval::<Dynamic>(r#"resources::list("secret_res", "")"#)
                .expect("l'admin lit la ressource AdminOnly"),
        );
        assert_eq!(as_array(listed.get("items").expect("la clé items")).len(), 1);

        // Retrait du groupe admin entre les deux appels (même fixture que `identity.rs`).
        crate::users::membership::Entity::delete_many()
            .filter(crate::users::membership::Column::UserId.eq(alice.id))
            .exec(&db)
            .await
            .expect("retrait des appartenances d'alice");

        let message = error_text(engine.eval::<Dynamic>(r#"resources::list("secret_res", "")"#));
        assert_eq!(message, "forbidden", "le droit est relu à chaque appel");
    }

    // ——— Scenario lot 2 : « une écriture suit les mêmes politiques que REST » ———
    #[tokio::test(flavor = "multi_thread")]
    async fn a_write_follows_the_same_policies_as_rest() {
        let db = migrated_db().await;
        create_entity_table(&db, secret_res::Entity).await;
        crate::users::resolve_user(&db, "alice", None)
            .await
            .expect("le compte local d'alice");
        let engine = engine(all_readers(), db.clone(), "alice");

        let message = error_text(
            engine.eval::<Dynamic>(r#"resources::create("secret_res", #{ id: 0, title: "classifié" })"#),
        );
        assert_eq!(
            message, "forbidden",
            "la `write_policy` AdminOnly refuse le non-admin"
        );
        assert_eq!(
            secret_res::Entity::find()
                .all(&db)
                .await
                .expect("la table secret est lisible")
                .len(),
            0,
            "aucune ligne n'est créée par le refus"
        );

        // Parité : le verdict direct de `rest::core::create` est bien `Forbidden` — le module ne
        // décide rien par lui-même.
        assert!(
            matches!(
                crate::rest::core::create::<secret_res::Entity>(
                    &db,
                    &rest_principal("alice"),
                    secret_res::Model {
                        id: 0,
                        title: "classifié".to_string(),
                    },
                )
                .await,
                Err(RestError::Forbidden)
            ),
            "le refus vient de rest::core, pas d'une règle du module"
        );
    }

    // ——— Scenario lot 2 : « un hook refuse avec son message » ———
    #[tokio::test(flavor = "multi_thread")]
    async fn a_hook_refusal_keeps_its_message() {
        let db = migrated_db().await;
        create_entity_table(&db, hooked::Entity).await;
        crate::users::resolve_user(&db, "alice", None)
            .await
            .expect("le compte local d'alice");
        let mut registry = ResourceRegistry::new();
        registry.register::<hooked::Entity>();
        let engine = engine(registry, db.clone(), "alice");

        let message =
            error_text(engine.eval::<Dynamic>(r#"resources::create("hooked", #{ id: 0, label: "" })"#));
        assert_eq!(
            message, "hook refused: label must not be empty",
            "le message du HookError est rendu tel quel, sans code ni détail"
        );
        assert!(
            !message.contains("MRD-"),
            "jamais de code MRD-* sur le chemin d'un hook : {message}"
        );
        assert_eq!(
            hooked::Entity::find()
                .all(&db)
                .await
                .expect("la table hooked est lisible")
                .len(),
            0,
            "le refus du hook interrompt avant l'écriture"
        );

        // Contrôle positif : le rejet vient du hook, pas du module — un libellé licite passe.
        let created = as_map(
            engine
                .eval::<Dynamic>(r#"resources::create("hooked", #{ id: 0, label: "tarte" })"#)
                .expect("le hook laisse passer un libellé licite"),
        );
        assert_eq!(text(&created, "label"), "tarte");
    }

    // ——— Scenario lot 2 : « un step rejoué ne duplique pas un create » ———
    #[tokio::test(flavor = "multi_thread")]
    async fn a_replayed_step_does_not_duplicate_a_create() {
        let db = migrated_db().await;
        create_entity_table(&db, public_res::Entity).await;
        crate::users::resolve_user(&db, "alice", None)
            .await
            .expect("le compte local d'alice");
        let engine = engine_scope(
            public_and_secret_readers(),
            db.clone(),
            "alice",
            Some(write_scope()),
        );

        let script = r#"resources::create("public_res", #{ id: 0, title: "tarte" })"#;
        let first = as_map(engine.eval::<Dynamic>(script).expect("le premier create réussit"));
        let id = integer(&first, "id");
        let second = as_map(engine.eval::<Dynamic>(script).expect("le rejeu réussit"));
        assert_eq!(
            integer(&second, "id"),
            id,
            "le résultat journalisé est rendu tel quel"
        );
        assert_eq!(text(&second, "title"), "tarte");

        assert_eq!(
            public_res::Entity::find()
                .all(&db)
                .await
                .expect("la table public est lisible")
                .len(),
            1,
            "une seule ligne existe après rejeu"
        );
        assert_eq!(
            journal_row_count(&db).await,
            1,
            "une seule écriture est journalisée, le rejeu n'a rien réécrit"
        );
    }

    // ——— Scenario lot 2 : « une fenêtre de crash est rattrapée par relecture » ———
    #[tokio::test(flavor = "multi_thread")]
    async fn a_crash_window_is_caught_by_rereading() {
        let db = migrated_db().await;
        create_entity_table(&db, unique_res::Entity).await;
        create_unique_index(&db).await;
        crate::users::resolve_user(&db, "alice", None)
            .await
            .expect("le compte local d'alice");
        // Crash simulé : la ligne est créée, le `record` du journal ne l'a jamais été.
        unique_res::Entity::insert(unique_res::ActiveModel {
            name: Set("miam".to_string()),
            note: Set("x".to_string()),
            ..Default::default()
        })
        .exec(&db)
        .await
        .expect("la ligne du run crashé est insérée");
        let mut registry = ResourceRegistry::new();
        registry.register::<unique_res::Entity>();
        let engine = engine_scope(registry, db.clone(), "alice", Some(write_scope()));

        let replayed = as_map(
            engine
                .eval::<Dynamic>(r#"resources::create("unique_res", #{ id: 0, name: "miam", note: "x" })"#)
                .expect("le rejeu du même create réussit sans doublon"),
        );
        assert_eq!(text(&replayed, "name"), "miam", "la ligne relue est rendue");
        assert_eq!(
            unique_res::Entity::find()
                .all(&db)
                .await
                .expect("la table unique est lisible")
                .len(),
            1,
            "toujours une seule ligne"
        );
        assert_eq!(
            journal_row_count(&db).await,
            1,
            "le succès rattrapé est journalisé"
        );

        let message = error_text(engine.eval::<Dynamic>(
            r#"resources::create("unique_res", #{ id: 0, name: "miam", note: "différent" })"#,
        ));
        assert_eq!(
            message, "conflict",
            "corps différent de la ligne existante sous contrainte d'unicité"
        );
        assert_eq!(
            unique_res::Entity::find()
                .all(&db)
                .await
                .expect("la table unique est lisible")
                .len(),
            1,
            "le conflit n'a rien écrit"
        );
        assert_eq!(journal_row_count(&db).await, 1, "le conflit n'est pas journalisé");
    }

    // ——— Scenario lot 2 : « la session interactive n'écrit pas dans le journal » ———
    #[tokio::test(flavor = "multi_thread")]
    async fn an_interactive_session_does_not_write_the_journal() {
        let db = migrated_db().await;
        create_entity_table(&db, public_res::Entity).await;
        crate::users::resolve_user(&db, "alice", None)
            .await
            .expect("le compte local d'alice");
        let engine = engine(public_and_secret_readers(), db.clone(), "alice");

        let _ = engine
            .eval::<Dynamic>(r#"resources::create("public_res", #{ id: 0, title: "tarte" })"#)
            .expect("l'écriture interactive réussit");
        assert_eq!(
            public_res::Entity::find()
                .all(&db)
                .await
                .expect("la table public est lisible")
                .len(),
            1,
            "la ligne est écrite"
        );
        assert_eq!(
            journal_row_count(&db).await,
            0,
            "`scope = None` : la table du journal reste vide"
        );
    }

    // ——— Scenario lot 2 : « plafond d'écritures » ———
    #[tokio::test(flavor = "multi_thread")]
    async fn writes_are_capped_per_run() {
        let db = migrated_db().await;
        create_entity_table(&db, public_res::Entity).await;
        crate::users::resolve_user(&db, "alice", None)
            .await
            .expect("le compte local d'alice");
        let engine = engine(public_and_secret_readers(), db.clone(), "alice");

        engine
            .eval::<()>(
                r#"for n in 1..=100 { resources::create("public_res", #{ id: 0, title: "tarte" + n }) }"#,
            )
            .expect("les 100 premières écritures passent");
        let message = error_text(
            engine
                .eval::<Dynamic>(r#"resources::create("public_res", #{ id: 0, title: "la ligne de trop" })"#),
        );
        assert_eq!(message, "write limit reached", "la 101e écriture échoue");
        assert_eq!(
            public_res::Entity::find()
                .all(&db)
                .await
                .expect("la table public est lisible")
                .len(),
            100,
            "cent lignes, pas une de plus"
        );
    }

    // ——— Scenario lot 2 : « describe indique writable » ———
    #[tokio::test(flavor = "multi_thread")]
    async fn describe_shows_writable() {
        let db = migrated_db().await;
        create_entity_table(&db, public_res::Entity).await;
        create_entity_table(&db, gated_res::Entity).await;
        crate::users::resolve_user(&db, "alice", None)
            .await
            .expect("le compte local d'alice");
        let mut registry = ResourceRegistry::new();
        registry
            .register::<public_res::Entity>()
            .register::<gated_res::Entity>();
        let engine = engine(registry, db, "alice");

        let described = engine
            .eval::<Array>(r"resources::describe()")
            .expect("describe réussit");
        assert_eq!(described.len(), 2, "les deux ressources sont lisibles");
        let entries: Vec<(String, Map)> = described
            .into_iter()
            .map(|entry| {
                let entry = as_map(entry);
                (text(&entry, "name"), entry)
            })
            .collect();
        let writable_of = |name: &str| -> Option<bool> {
            entries
                .iter()
                .find(|(entry_name, _)| entry_name == name)
                .map(|(_, entry)| boolean(entry, "writable"))
        };
        assert_eq!(
            writable_of("public_res"),
            Some(true),
            "écriture `Public` : writable = true"
        );
        assert_eq!(
            writable_of("gated_res"),
            Some(false),
            "écriture `AdminOnly` par un non-admin : writable = false"
        );
    }

    // ——— Must amendement lot 2 « idempotence en batch » : `update` et `delete` rejoués rendent
    // le résultat journalisé ; une `delete` sans attestation du journal sur ligne absente reste
    // `not found`. ———
    #[tokio::test(flavor = "multi_thread")]
    async fn journaled_updates_and_deletes_replay_their_result() {
        let db = migrated_db().await;
        create_entity_table(&db, public_res::Entity).await;
        crate::users::resolve_user(&db, "alice", None)
            .await
            .expect("le compte local d'alice");
        let engine = engine_scope(
            public_and_secret_readers(),
            db.clone(),
            "alice",
            Some(write_scope()),
        );

        let created = as_map(
            engine
                .eval::<Dynamic>(r#"resources::create("public_res", #{ id: 0, title: "tarte" })"#)
                .expect("le create réussit"),
        );
        let id = integer(&created, "id");

        let update_script =
            format!(r#"resources::update("public_res", {id}, #{{ id: {id}, title: "nouveau" }})"#);
        let updated = as_map(engine.eval::<Dynamic>(&update_script).expect("l'update réussit"));
        assert_eq!(text(&updated, "title"), "nouveau");
        let replayed = as_map(
            engine
                .eval::<Dynamic>(&update_script)
                .expect("le rejeu de l'update réussit"),
        );
        assert_eq!(
            integer(&replayed, "id"),
            id,
            "le résultat journalisé est rendu tel quel"
        );
        assert_eq!(text(&replayed, "title"), "nouveau");
        assert_eq!(
            public_res::Entity::find()
                .all(&db)
                .await
                .expect("la table public est lisible")
                .len(),
            1,
            "l'update rejoué n'a rien dupliqué"
        );

        let delete_script = format!(r#"resources::delete("public_res", {id})"#);
        let deleted = as_map(engine.eval::<Dynamic>(&delete_script).expect("le delete réussit"));
        assert!(boolean(&deleted, "deleted"));
        let replayed_delete = as_map(
            engine
                .eval::<Dynamic>(&delete_script)
                .expect("le rejeu du delete réussit sans retoucher la base"),
        );
        assert!(
            boolean(&replayed_delete, "deleted"),
            "le journal atteste le succès"
        );
        assert_eq!(
            public_res::Entity::find()
                .all(&db)
                .await
                .expect("la table public est lisible")
                .len(),
            0,
            "la ligne est supprimée une seule fois"
        );
        assert_eq!(
            journal_row_count(&db).await,
            3,
            "create + update + delete : trois écritures journalisées ; les rejeux rendent le \
             journal sans rien y ajouter"
        );

        let message = error_text(engine.eval::<Dynamic>(r#"resources::delete("public_res", 999)"#));
        assert_eq!(
            message, "not found",
            "delete d'une ligne absente sans attestation du journal"
        );
    }

    // ——— Lot 1, contrat amendé par `DEC-21` : le test « aucune écriture n'existe » est retiré —
    // les verbes d'écriture existent et sont verrouillés par les Scenario lot 2 ci-dessus.
}
