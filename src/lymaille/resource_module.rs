//! Module Rhai `resources` : accès des agents `lymaille` aux `MiryadResource` en lecture seule
//! (`src/lymaille/resource_module.sdd`). Une seule troisième surface (DSD `FR-2`, `DEC-15`) :
//! chaque lecture délègue à `rest::core::list` / `rest::core::get` — mêmes politiques que REST et
//! MCP, **aucune décision d'accès propre** (précédent : `src/mcp/registry.sdd`). Le principal est
//! reconstruit à **chaque appel** depuis le `subject` de la session (DSD `FR-4`, `DEC-3`) par
//! `Identity::resolve`, jamais mis en cache. Fonctions Rhai synchrones, pont `async` par
//! `vynil_core::rt::block_on` (précédent : les modules de `lymaille`). Lecture seule au lot 1 :
//! `create`, `update`, `delete` n'existent pas (DSD `DEC-15`, `DEC-11`).

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;

use rhai::{Array, Dynamic, EvalAltResult, ImmutableString, Map, Module, Position};
use sea_orm::{DatabaseConnection, EntityTrait};
use serde_json::Value as JsonValue;
use vynil_core::rt::block_on;

use crate::auth::{AuthPrincipal, PrincipalSource};
use crate::ir::{EntityIr, resource_ir};
use crate::query::Pagination;
use crate::rbac::{ListAccess, list_access};
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

/// Échecs du module, rendus en messages Rhai **fixes et courts** (spec `Must` « Erreurs ») :
/// jamais le texte brut d'une `DbErr` ni d'une `RestError` — le détail part en `tracing::warn!`,
/// sans contenu de ligne.
enum Failure {
    /// Ressource absente du registre : message `unknown resource: <nom>`.
    UnknownResource(String),
    /// `subject` de la session sans compte local (`Identity::resolve` → `UnknownSubject`).
    UnknownSubject,
    /// `RestError::NotFound` délégué.
    NotFound,
    /// `RestError::Forbidden` délégué — l'oracle d'existence de `rest::core` est préservé.
    Forbidden,
    /// Toute autre cause (détail déjà expédié en `warn!` par [`Failure::log`]).
    Internal,
}

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

    /// Message fixe rendu au script.
    fn message(&self) -> String {
        match self {
            Self::UnknownResource(name) => format!("unknown resource: {name}"),
            Self::UnknownSubject => "no account for this session".to_string(),
            Self::NotFound => "not found".to_string(),
            Self::Forbidden => "forbidden".to_string(),
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

/// Registre type-érasé des entités lisibles par les agents (précédent : `McpToolRegistry`,
/// `src/mcp/registry.sdd`) — `BTreeMap` trié par `resource_name`, aucune entité par défaut :
/// l'application choisit celles qu'elle ouvre aux agents.
#[derive(Default)]
pub struct ResourceReaders {
    entries: BTreeMap<&'static str, Box<dyn Reader>>,
}

impl ResourceReaders {
    /// Registre vide.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Enregistre `E` sous `E::resource_name()`, rend `&mut Self` pour l'enchaînement.
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
            "`{name}` is already registered in the agent resource readers — duplicate \
             `resource_name` (the same entity type registered twice counts) is a programming \
             error, refusing to register it a second time"
        );
        self.entries
            .insert(name, Box::new(ReaderImpl::<E>(std::marker::PhantomData)));
        self
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

/// Entrée `#{ name, filter_column, fields }` de `describe()` — `filter_column` vaut `()` quand
/// l'entité n'en déclare pas ; `fields` est le tableau des `#{ name, type, nullable,
/// primary_key }` de `resource_ir::<E>()`.
fn describe_entry(name: &str, ir: EntityIr) -> Dynamic {
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
    ])
}

/// État partagé des fonctions du module : registre, connexion, `subject` de la session.
#[derive(Clone)]
struct Handle {
    readers: Arc<ResourceReaders>,
    db: DatabaseConnection,
    subject: String,
}

impl Handle {
    /// `Identity::resolve` du `subject` à CET instant, puis le `AuthPrincipal` passé à
    /// `rest::core` (DSD `FR-4`, `DEC-3` : jamis mis en cache entre deux appels).
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
    /// triées par nom (`entries` est un `BTreeMap`).
    async fn describe(&self) -> Result<Array, Failure> {
        let caller = Identity::new(self.db.clone()).resolve(&self.subject).await?;
        let account = user::Entity::find_by_id(caller.user_id)
            .one(&self.db)
            .await?
            .ok_or(Failure::UnknownSubject)?;
        let mut entries = Array::new();
        for (name, reader) in &self.readers.entries {
            if reader.list_readable(&self.db, &account).await? {
                entries.push(describe_entry(name, reader.ir()));
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
        let reader = self
            .readers
            .entries
            .get(resource)
            .ok_or_else(|| Failure::UnknownResource(resource.to_owned()))?;
        let pagination = Pagination::from_raw(page, Some(per_page.unwrap_or(LLM_DEFAULT_PER_PAGE)));
        let per_page = pagination.per_page.min(LLM_MAX_PER_PAGE);
        let value = reader
            .list(&self.db, &principal, pagination.page, per_page, filter)
            .await?;
        Ok(json_to_dynamic(value))
    }

    /// `get()` : principal résolu, ressource cherchée, ligne déléguée à `rest::core::get`.
    async fn get(&self, resource: &str, id: i64) -> Result<Dynamic, Failure> {
        let principal = self.resolve_principal().await?;
        let reader = self
            .readers
            .entries
            .get(resource)
            .ok_or_else(|| Failure::UnknownResource(resource.to_owned()))?;
        let id = i32::try_from(id).map_err(|_| Failure::log("get id out of the i32 range"))?;
        let value = reader.get(&self.db, &principal, id).await?;
        Ok(json_to_dynamic(value))
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
/// `list(ressource, filtre, page, par_page)`, `get(ressource, id)` — écriture aucune.
#[must_use]
pub fn resources_module(readers: Arc<ResourceReaders>, db: DatabaseConnection, subject: String) -> Module {
    let mut module = Module::new();
    let handle = Handle { readers, db, subject };

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
    let get_handle = handle;
    let _get = module.set_native_fn(
        "get",
        move |resource: ImmutableString, id: i64| -> Result<Dynamic, Box<EvalAltResult>> {
            drive(get_handle.get(resource.as_str(), id))
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

    /// Base sqlite en mémoire migrée (`miryad_users` comprise, exigée par la chaîne déléguée).
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

    /// Registre des deux seules ressources du `Given` du Scenario `describe` : une `Public`,
    /// une `AdminOnly`.
    fn public_and_secret_readers() -> ResourceReaders {
        let mut readers = ResourceReaders::new();
        readers
            .register::<public_res::Entity>()
            .register::<secret_res::Entity>();
        readers
    }

    /// Les trois entités du registre, clés en ordre croissant.
    fn all_readers() -> ResourceReaders {
        let mut readers = ResourceReaders::new();
        readers
            .register::<public_res::Entity>()
            .register::<owner_res::Entity>()
            .register::<secret_res::Entity>();
        readers
    }

    /// Moteur Rhai avec le module `resources` du `subject` — exécution de scripts réels, jamais
    /// d'appel Rust contournant le module.
    fn engine(readers: ResourceReaders, db: DatabaseConnection, subject: &str) -> rhai::Engine {
        let module = resources_module(Arc::new(readers), db, subject.to_string());
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

    // ——— Scenario : « aucune écriture n'existe » ———
    #[tokio::test(flavor = "multi_thread")]
    async fn no_write_exists() {
        let db = migrated_db().await;
        let engine = engine(ResourceReaders::new(), db, "alice");
        for (verb, script) in [
            ("create", r#"resources::create("public_res", 1)"#),
            ("update", r#"resources::update("public_res", 1)"#),
            ("delete", r#"resources::delete("public_res", 1)"#),
        ] {
            let message = error_text(engine.eval::<()>(script)).to_lowercase();
            assert!(
                message.contains("not found") || message.contains("does not exist"),
                "`{verb}` doit être une fonction inconnue de Rhai : {message}"
            );
            assert!(
                message.contains(verb),
                "l'erreur nomme le verbe appelé : {message}"
            );
        }
    }
}
