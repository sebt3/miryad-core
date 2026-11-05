//! `SeaToolsetStore`, `SeaContextTypeStore`, `SeaContextStore` — les traits de toolsets de
//! `lymaille` sur `SeaORM` (`src/lymaille/store/toolset.sdd`).
//!
//! Une ligne de `miryad_lymaille_catalog_toolsets` (catalogue `G_…`) ou de
//! `miryad_lymaille_toolsets` (privé, `owner_id`) porte la version **active** et le
//! **brouillon** d'un toolset (DSD `DEC-18`) : `get_active` ne lit jamais le brouillon et
//! `put_draft` n'écrit que lui — l'activation est l'écriture de `active` par un admin via
//! l'entité, jamais par le store. `put_draft` valide par le contrôle **partagé**
//! `check_toolset` de `../definitions/toolset.rs`, qu'il appelle sans logique propre. Types de
//! contexte et contextes sont routés par le lot 2 (DSD `DEC-20`) : les types dans
//! `miryad_lymaille_catalog_context_types` (colonne `active` ; `draft` réservée à ./draft.sdd),
//! les contextes `G_…` dans `miryad_lymaille_catalog_contexts` (écriture admin) et les contextes
//! privés dans `miryad_lymaille_user_contexts` (`owner_id = user_id`, jamais lus chez un autre).

use sea_orm::ConnectionTrait;
use sea_orm::DatabaseConnection;
use sea_orm::DbErr;
use sea_orm::sea_query::Alias;
use sea_orm::sea_query::Expr;
use sea_orm::sea_query::ExprTrait;
use sea_orm::sea_query::OnConflict;
use sea_orm::sea_query::Query;
use sea_orm::sea_query::SelectStatement;

use ::lymaille::ContextStore;
use ::lymaille::ContextType;
use ::lymaille::ContextTypeStore;
use ::lymaille::Toolset;
use ::lymaille::ToolsetContext;
use ::lymaille::ToolsetStore;
use ::lymaille::ToolsetsError;
use ::lymaille::ToolsetsResult;

use super::db_text;
use crate::lymaille::definitions::toolset::check_toolset;
use crate::lymaille::error::LymailleError;
use crate::lymaille::host::SharedHost;
use crate::lymaille::identity::CallerPrincipal;
use crate::lymaille::identity::IdKind;
use crate::lymaille::identity::Identity;
use crate::lymaille::identity::check_write;
use crate::lymaille::identity::classify;

/// Tables, noms complets (../migration.sdd). Contextes et types de contexte : les tables du
/// lot 2 (amendement `DEC-20`) — `catalog_context_types` (dont la colonne `draft` est réservée à
/// ./draft.sdd), `catalog_contexts` (noms `G_…`) et `user_contexts` (noms privés, `owner_id`).
const CATALOG_TOOLSETS_TABLE: &str = "miryad_lymaille_catalog_toolsets";
const PRIVATE_TOOLSETS_TABLE: &str = "miryad_lymaille_toolsets";
const CATALOG_CONTEXT_TYPES_TABLE: &str = "miryad_lymaille_catalog_context_types";
const CATALOG_CONTEXTS_TABLE: &str = "miryad_lymaille_catalog_contexts";
const USER_CONTEXTS_TABLE: &str = "miryad_lymaille_user_contexts";

/// Refus de l'hôte rendu à `lymaille` par la variante `Store` de son erreur, code `MRD-*`
/// compris (./mod.sdd règle 5).
fn deny(error: &LymailleError) -> ToolsetsError {
    ToolsetsError::Store(error.to_string())
}

/// Champ texte de la forme sérialisée d'un objet de table à clé par nom : `ContextType` et
/// `ToolsetContext` ne rendent ni leur nom ni leur version par un accesseur public — les
/// colonnes `name` et `version` sont des copies de la clé et de la version de l'objet JSON,
/// source de vérité (DSD `DEC-17`).
fn stored_field(value: &serde_json::Value, field: &str, what: &str) -> ToolsetsResult<String> {
    value
        .get(field)
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| ToolsetsError::Store(format!("stored {what} carries no {field}")))
}

/// Principal de l'appel, reconstruit à chaque opération (./mod.sdd règle 2, DSD `DEC-3`).
async fn principal(identity: &Identity, subject: &str) -> Result<CallerPrincipal, ToolsetsError> {
    identity.resolve(subject).await.map_err(|err| deny(&err))
}

/// Vérifie que l'appelant est admin à cet instant (tables globales internes, `./toolset.sdd`).
async fn require_admin(identity: &Identity, subject: &str, what: &str) -> ToolsetsResult<()> {
    let caller = principal(identity, subject).await?;
    if caller.is_admin {
        Ok(())
    } else {
        Err(deny(&LymailleError::Forbidden(format!("{what} requires admin"))))
    }
}

/// Première valeur JSON d'une requête, `None` si aucune ligne.
async fn first_json(
    db: &DatabaseConnection,
    select: &SelectStatement,
    column: &str,
) -> Result<Option<serde_json::Value>, DbErr> {
    let row = db.query_one(select).await?;
    row.map(|row| row.try_get::<Option<serde_json::Value>>("", column))
        .transpose()
        .map(Option::flatten)
}

/// Upsert : `INSERT` sur `columns`/`values`, conflit sur `keys` → mise à jour de `updates`.
async fn upsert(
    db: &DatabaseConnection,
    table: &str,
    columns: &[&str],
    values: Vec<Expr>,
    keys: &[&str],
    updates: &[&str],
) -> Result<(), DbErr> {
    let statement = Query::insert()
        .into_table(Alias::new(table))
        .columns(columns.iter().copied().map(Alias::new))
        .values(values)
        .map_err(|err| DbErr::Custom(err.to_string()))?
        .on_conflict(
            OnConflict::columns(keys.iter().copied().map(Alias::new))
                .update_columns(updates.iter().copied().map(Alias::new))
                .to_owned(),
        )
        .to_owned();
    db.execute(&statement).await.map(|_| ())
}

/// Store des toolsets, version active et brouillon sur la même ligne (`./toolset.sdd`).
#[derive(Clone)]
pub struct SeaToolsetStore {
    db: DatabaseConnection,
    identity: Identity,
    subject: String,
}

/// Store des types de contexte, table globale interne (`./toolset.sdd`).
#[derive(Clone)]
pub struct SeaContextTypeStore {
    db: DatabaseConnection,
    identity: Identity,
    subject: String,
}

/// Store des contextes de toolset, table globale interne (`./toolset.sdd`).
#[derive(Clone)]
pub struct SeaContextStore {
    db: DatabaseConnection,
    identity: Identity,
    subject: String,
    /// Hôte partagé posé par `LymailleStores::with_host` (amendement DEC-27 de ./toolset.sdd).
    host: Option<SharedHost>,
}

impl SeaToolsetStore {
    pub(crate) fn new(db: DatabaseConnection, identity: Identity, subject: String) -> Self {
        Self {
            db,
            identity,
            subject,
        }
    }

    /// Relecture d'une colonne de version (`active` ou `draft`) d'une ligne du catalogue ou du
    /// privé, règle 3 de ./mod.sdd : la ligne vaut `None` si absente, si la colonne est `NULL`
    /// (un brouillon seul n'est **jamais** rendu par `get_active`) ou illisible.
    async fn read_version(&self, name: &str, column: &str) -> ToolsetsResult<Option<Toolset>> {
        let Ok(kind) = classify(name) else {
            return Ok(None);
        };
        // DEC-27 (./toolset.sdd, amendement approuvé 2026-11-04) : un identifiant `H_…` est
        // résolu par l'hôte, jamais stocké en table — lecture `Ok(None)` sans requête, avant
        // même la reconstruction du principal.
        if kind == IdKind::Host {
            return Ok(None);
        }
        let caller = principal(&self.identity, &self.subject).await?;
        let mut select = Query::select();
        select.column(Alias::new(column));
        match kind {
            IdKind::Catalog => {
                select
                    .from(Alias::new(CATALOG_TOOLSETS_TABLE))
                    .and_where(Expr::col(Alias::new("toolset_id")).eq(name));
            }
            IdKind::Private => {
                select
                    .from(Alias::new(PRIVATE_TOOLSETS_TABLE))
                    .and_where(Expr::col(Alias::new("owner_id")).eq(caller.user_id))
                    .and_where(Expr::col(Alias::new("toolset_id")).eq(name));
            }
            // Garde de couverture : `Host` rend déjà `Ok(None)` sans requête plus haut.
            IdKind::Host => return Ok(None),
        }
        let stored = first_json(&self.db, &select, column)
            .await
            .map_err(|err| ToolsetsError::Store(db_text(&err)))?;
        stored
            .map(|value| {
                serde_json::from_value::<Toolset>(value).map_err(|_cause| {
                    // La cause serde est abandonnée : le texte d'un toolset porte son source.
                    ToolsetsError::Store(format!("stored toolset {name} is unreadable"))
                })
            })
            .transpose()
    }

    /// Écriture du brouillon seul (./toolset.sdd) : la colonne `active` n'est jamais touchée
    /// par le store ; `version` ne suit le brouillon que lorsque aucune version active n'est
    /// présente, comme la dérivation de `../definitions/toolset.rs`.
    async fn write_draft(
        &self,
        toolset: &Toolset,
        kind: IdKind,
        caller: &CallerPrincipal,
    ) -> ToolsetsResult<()> {
        let value = serde_json::to_value(toolset)
            .map_err(|_cause| ToolsetsError::Store("toolset could not be serialized".to_owned()))?;
        let version = toolset.version().to_string();
        let (table, keys, conditions): (&str, Vec<&str>, Vec<(&str, Expr)>) = match kind {
            IdKind::Catalog => (
                CATALOG_TOOLSETS_TABLE,
                vec!["toolset_id"],
                vec![("toolset_id", Expr::val(toolset.name()))],
            ),
            IdKind::Private => (
                PRIVATE_TOOLSETS_TABLE,
                vec!["owner_id", "toolset_id"],
                vec![
                    ("owner_id", Expr::val(caller.user_id)),
                    ("toolset_id", Expr::val(toolset.name())),
                ],
            ),
            // DEC-27 (./toolset.sdd, amendement approuvé 2026-11-04) : `check_write` refuse
            // déjà `H_…` en amont — aucun accès table ; branche de couverture au même refus.
            IdKind::Host => {
                return Err(deny(&LymailleError::Forbidden(
                    "host identifiers are not writable".to_string(),
                )));
            }
        };
        // La ligne existe-t-elle, et porte-t-elle une version active ?
        let mut probe = Query::select();
        probe.column(Alias::new("active")).from(Alias::new(table));
        for (column, condition) in &conditions {
            probe.and_where(Expr::col(Alias::new(*column)).eq(condition.clone()));
        }
        let active_present = first_json(&self.db, &probe, "active")
            .await
            .map_err(|err| ToolsetsError::Store(db_text(&err)))?;
        if active_present.is_some() {
            // Version active présente : seule la colonne `draft` change, `version` reste celle
            // de l'active.
            let mut update = Query::update();
            update
                .table(Alias::new(table))
                .value(Alias::new("draft"), Expr::val(value.clone()));
            for (column, condition) in &conditions {
                update.and_where(Expr::col(Alias::new(*column)).eq(condition.clone()));
            }
            return self
                .db
                .execute(&update)
                .await
                .map(|_| ())
                .map_err(|err| ToolsetsError::Store(db_text(&err)));
        }
        // Ligne nouvelle, ou existante sans version active : `version` dérive du brouillon.
        let mut columns: Vec<&str> = keys.clone();
        columns.extend(["version", "draft", "created_at"]);
        let mut values: Vec<Expr> = conditions.into_iter().map(|(_column, value)| value).collect();
        values.extend([
            Expr::val(version),
            Expr::val(value.clone()),
            Expr::val(chrono::Utc::now()),
        ]);
        let mut statement = Query::insert();
        statement
            .into_table(Alias::new(table))
            .columns(columns.iter().copied().map(Alias::new));
        statement
            .values(values)
            .map_err(|err| ToolsetsError::Store(db_text(&DbErr::Custom(err.to_string()))))?;
        statement.on_conflict(
            OnConflict::columns(keys.iter().copied().map(Alias::new))
                .update_columns([Alias::new("version"), Alias::new("draft")])
                .to_owned(),
        );
        self.db
            .execute(&statement)
            .await
            .map(|_| ())
            .map_err(|err| ToolsetsError::Store(db_text(&err)))
    }
}

impl ToolsetStore for SeaToolsetStore {
    async fn get_active(&self, name: &str) -> ToolsetsResult<Option<Toolset>> {
        self.read_version(name, "active").await
    }

    async fn get_draft(&self, name: &str) -> ToolsetsResult<Option<Toolset>> {
        self.read_version(name, "draft").await
    }

    async fn put_draft(&self, toolset: &Toolset) -> ToolsetsResult<()> {
        // Validation par le contrôle partagé `check_toolset` de `../definitions/toolset.rs` avant
        // toute écriture — le store n'a pas de logique de validation propre — puis droit relu à
        // l'appel.
        check_toolset(toolset)?;
        let caller = principal(&self.identity, &self.subject).await?;
        let kind = check_write(&caller, toolset.name()).map_err(|err| deny(&err))?;
        self.write_draft(toolset, kind, &caller).await
    }
}

impl SeaContextTypeStore {
    pub(crate) fn new(db: DatabaseConnection, identity: Identity, subject: String) -> Self {
        Self {
            db,
            identity,
            subject,
        }
    }
}

impl SeaContextStore {
    pub(crate) fn new(
        db: DatabaseConnection,
        identity: Identity,
        subject: String,
        host: Option<SharedHost>,
    ) -> Self {
        Self {
            db,
            identity,
            subject,
            host,
        }
    }

    /// Contexte d'un identifiant hôte `H_…` (amendement DEC-27 de ./toolset.sdd) : délégué à
    /// [`crate::lymaille::host::LymailleHost::resolve_context`] avec le principal relu, sans
    /// table. Sans hôte, ou si l'hôte répond `None` **ou** `Err`, `Ok(None)` — échec fermé,
    /// indiscernable d'un contexte absent ; aucun texte de `HostError` ne remonte (`host.sdd`).
    async fn host_context(&self, name: &str) -> ToolsetsResult<Option<ToolsetContext>> {
        let Some(host) = self.host.clone() else {
            return Ok(None);
        };
        let caller = principal(&self.identity, &self.subject).await?;
        match host.resolve_context(name, &caller).await {
            Ok(found) => Ok(found),
            Err(_host_error) => Ok(None),
        }
    }
}

impl ContextTypeStore for SeaContextTypeStore {
    async fn get_type(&self, name: &str) -> ToolsetsResult<Option<ContextType>> {
        // Amendement lot 2 (./toolset.sdd) : la table `catalog_context_types`, colonne `active`
        // (la colonne `draft` est réservée à ./draft.sdd).
        let select = Query::select()
            .columns([Alias::new("active")])
            .from(Alias::new(CATALOG_CONTEXT_TYPES_TABLE))
            .and_where(Expr::col(Alias::new("name")).eq(name))
            .to_owned();
        let stored = first_json(&self.db, &select, "active")
            .await
            .map_err(|err| ToolsetsError::Store(db_text(&err)))?;
        stored
            .map(|value| {
                serde_json::from_value::<ContextType>(value).map_err(|_cause| {
                    ToolsetsError::Store(format!("stored context type {name} is unreadable"))
                })
            })
            .transpose()
    }

    async fn put_type(&self, context_type: &ContextType) -> ToolsetsResult<()> {
        require_admin(&self.identity, &self.subject, "context type write").await?;
        // Les mêmes contrôles que `InMemoryToolsets::put_type` : le type doit s'écrire et se
        // relire depuis sa forme stockée (les constructeurs de `lymaille` le garantissent ; le
        // contrôle garde le store d'en porter un qui ne se relirait pas).
        let value = serde_json::to_value(context_type)
            .map_err(|_cause| ToolsetsError::Store("context type could not be serialized".to_owned()))?;
        let name = stored_field(&value, "name", "context type")?;
        let version = stored_field(&value, "version", "context type")?;
        serde_json::from_value::<ContextType>(value.clone()).map_err(|_cause| {
            ToolsetsError::InvalidContextType {
                name: name.clone(),
                reason: "the stored form does not read back",
            }
        })?;
        // Upsert par `name` sur la colonne `active` (avec `version` dérivée, motif des hooks de
        // `../definitions/context_type.sdd`). Les trois colonnes pilotes DEC-28 ne vivent pas
        // dans l'objet `ContextType` de `lymaille` : une ligne nouvelle les reçoit **non posées**
        // (nature vide, agents vides) et les hooks de l'entité les exigent posées au premier
        // CRUD — le store ne les invente pas.
        let statement = Query::insert()
            .into_table(Alias::new(CATALOG_CONTEXT_TYPES_TABLE))
            .columns([
                "name",
                "version",
                "kind",
                "default_agent",
                "allowed_agents",
                "active",
                "draft",
                "created_at",
            ])
            .values(vec![
                Expr::val(name),
                Expr::val(version),
                Expr::val(String::new()),
                Expr::null(),
                Expr::val(serde_json::json!([])),
                Expr::val(value),
                Expr::null(),
                Expr::val(chrono::Utc::now()),
            ])
            .map_err(|err| ToolsetsError::Store(db_text(&DbErr::Custom(err.to_string()))))?
            .on_conflict(
                OnConflict::columns([Alias::new("name")])
                    .update_columns([Alias::new("active"), Alias::new("version")])
                    .to_owned(),
            )
            .to_owned();
        self.db
            .execute(&statement)
            .await
            .map(|_| ())
            .map_err(|err| ToolsetsError::Store(db_text(&err)))
    }
}

impl ContextStore for SeaContextStore {
    async fn get_context(&self, name: &str) -> ToolsetsResult<Option<ToolsetContext>> {
        // DEC-27 (./toolset.sdd) : un identifiant `H_…` est résolu par l'hôte, aucune table
        // n'est touchée — la branche précède toute requête. Amendement lot 2 (`DEC-20`) : le
        // préfixe tranche la table (règle 3 de ./mod.sdd, une requête sur une table) — `G_…` →
        // `catalog_contexts`, nom privé → `user_contexts` avec `owner_id = user_id`, jamais la
        // ligne privée d'un autre ; identifiant invalide ou de casse réservée → `Ok(None)`.
        let kind = match classify(name) {
            Ok(IdKind::Host) => return self.host_context(name).await,
            Ok(kind) => kind,
            Err(_error) => return Ok(None),
        };
        let mut select = Query::select();
        select.column(Alias::new("context"));
        match kind {
            IdKind::Catalog => {
                select
                    .from(Alias::new(CATALOG_CONTEXTS_TABLE))
                    .and_where(Expr::col(Alias::new("name")).eq(name));
            }
            IdKind::Private => {
                let caller = principal(&self.identity, &self.subject).await?;
                select
                    .from(Alias::new(USER_CONTEXTS_TABLE))
                    .and_where(Expr::col(Alias::new("owner_id")).eq(caller.user_id))
                    .and_where(Expr::col(Alias::new("name")).eq(name));
            }
            // Garde de couverture : `Host` délègue à l'hôte plus haut, sans requête.
            IdKind::Host => return Ok(None),
        }
        let stored = first_json(&self.db, &select, "context")
            .await
            .map_err(|err| ToolsetsError::Store(db_text(&err)))?;
        stored
            .map(|value| {
                serde_json::from_value::<ToolsetContext>(value).map_err(|_cause| {
                    ToolsetsError::Store(format!("stored toolset context {name} is unreadable"))
                })
            })
            .transpose()
    }

    async fn put_context(&self, context: &ToolsetContext) -> ToolsetsResult<()> {
        // Validation de `lymaille` par construction (le constructeur du contexte refuse déjà un
        // secret) ; le store relit la forme stockée pour la clé, comme ../toolset.rs pour les
        // types.
        let value = serde_json::to_value(context)
            .map_err(|_cause| ToolsetsError::Store("toolset context could not be serialized".to_owned()))?;
        let name = stored_field(&value, "name", "toolset context")?;
        // DEC-27 (./toolset.sdd) : `check_write` refuse un nom `H_…` admin compris (règle 4),
        // un nom `G_…` chez un non-admin, un préfixe réservé ou un identifiant invalide — avant
        // tout accès table ; le refus traverse en `Store(<Display>)` (règle 5). Amendement lot 2
        // (`DEC-20`) : un nom privé est accepté pour tout appelant, la table est la sienne.
        let caller = principal(&self.identity, &self.subject).await?;
        let kind = check_write(&caller, &name).map_err(|err| deny(&err))?;
        match kind {
            IdKind::Catalog => upsert(
                &self.db,
                CATALOG_CONTEXTS_TABLE,
                &["name", "context", "created_at"],
                vec![Expr::val(name), Expr::val(value), Expr::val(chrono::Utc::now())],
                &["name"],
                &["context"],
            )
            .await
            .map_err(|err| ToolsetsError::Store(db_text(&err))),
            IdKind::Private => upsert(
                &self.db,
                USER_CONTEXTS_TABLE,
                &["owner_id", "name", "context", "created_at"],
                vec![
                    Expr::val(caller.user_id),
                    Expr::val(name),
                    Expr::val(value),
                    Expr::val(chrono::Utc::now()),
                ],
                &["owner_id", "name"],
                &["context"],
            )
            .await
            .map_err(|err| ToolsetsError::Store(db_text(&err))),
            // Garde de couverture : `check_write` refuse `H_…` en amont, aucun accès table.
            IdKind::Host => Err(deny(&LymailleError::Forbidden(
                "host identifiers are not writable".to_string(),
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{SeaContextStore, SeaContextTypeStore, SeaToolsetStore};
    use crate::lymaille::host::{HostError, LymailleHost, SharedHost};
    use crate::lymaille::identity::{CallerPrincipal, Identity};
    use crate::lymaille::store::test_support::test_db;
    use crate::users::membership::sync_group_memberships;
    use crate::users::resolve_user;
    use ::lymaille::ContextLevel;
    use ::lymaille::ContextStore as _;
    use ::lymaille::ContextType;
    use ::lymaille::ContextTypeStore as _;
    use ::lymaille::Toolset;
    use ::lymaille::ToolsetContext;
    use ::lymaille::ToolsetStore as _;
    use ::lymaille::ToolsetsError;
    use async_trait::async_trait;
    use chrono::Utc;
    use sea_orm::ActiveModelTrait as _;
    use sea_orm::ActiveValue;
    use sea_orm::ConnectionTrait as _;
    use sea_orm::DatabaseConnection;
    use sea_orm::sea_query::Alias;
    use sea_orm::sea_query::Expr;
    use sea_orm::sea_query::ExprTrait;
    use sea_orm::sea_query::Query;
    use serde_json::json;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use vynil_core::Semver;

    /// Compte local de test, admin ou non.
    async fn user(db: &DatabaseConnection, subject: &str, admin: bool) {
        let account = resolve_user(db, subject, None)
            .await
            .expect("compte local de test créé");
        if admin {
            sync_group_memberships(db, account.id, &["admin".to_string()])
                .await
                .expect("appartenance au groupe admin posée");
        }
    }

    fn semver(text: &str) -> Semver {
        Semver::parse(text).expect("la version de fixture se parse")
    }

    fn toolset(name: &str, version: &str) -> Toolset {
        Toolset::new(
            name.to_string(),
            "toolset de test".to_string(),
            semver(version),
            "fn ping() { 1 }".to_string(),
        )
        .expect("la fixture de toolset se construit")
    }

    /// Toolset dont le source ne donne pas de contrat : refusé par `extract_contract`.
    fn broken_toolset(name: &str) -> Toolset {
        Toolset::new(
            name.to_string(),
            "toolset cassé".to_string(),
            semver("1.0.0"),
            "fn broken(((1".to_string(),
        )
        .expect("la fixture se construit, le source n'est jugé qu'à la validation")
    }

    /// Contexte de toolset de fixture, nommé `name` au niveau `level`.
    fn context(name: &str, level: ContextLevel) -> ToolsetContext {
        ToolsetContext::new(name.to_string(), level).expect("le contexte de fixture se construit")
    }

    /// Ligne du catalogue écrite « via l'entité » : l'`ActiveModel` de
    /// `../definitions/toolset.rs` inséré avec ses colonnes dérivées, comme le ferait le CRUD
    /// de l'admin après ses hooks.
    async fn write_active_row(db: &DatabaseConnection, name: &str, version: &str) {
        use crate::lymaille::definitions::toolset::ActiveModel;

        let version_json = serde_json::to_value(toolset(name, version)).expect("fixture sérialisée");
        let active = ActiveModel {
            id: ActiveValue::NotSet,
            toolset_id: ActiveValue::Set(name.to_string()),
            version: ActiveValue::Set(version.to_string()),
            active: ActiveValue::Set(Some(version_json)),
            draft: ActiveValue::Set(None),
            created_at: ActiveValue::Set(Utc::now()),
        };
        active
            .insert(db)
            .await
            .expect("la ligne active est écrite par l'entité");
    }

    async fn row_count(db: &DatabaseConnection, table: &str, conditions: &[(&str, &str)]) -> usize {
        let mut select = Query::select();
        select.columns([Alias::new("*")]).from(Alias::new(table));
        for (column, value) in conditions {
            select.and_where(Expr::col(Alias::new(*column)).eq(*value));
        }
        db.query_all(&select).await.expect("comptage des lignes").len()
    }

    /// `Scenario` « un brouillon n'est jamais actif » : `put_draft` crée la ligne
    /// `active = NULL` ; `get_active` rend `None`, `get_draft` le brouillon.
    #[tokio::test]
    async fn a_draft_is_never_active() {
        let db = test_db().await;
        user(&db, "root", true).await;
        let store = SeaToolsetStore::new(db.clone(), Identity::new(db.clone()), "root".to_string());

        store
            .put_draft(&toolset("G_git", "1.0.0"))
            .await
            .expect("l'admin écrit le brouillon `G_git`");
        assert!(
            matches!(store.get_active("G_git").await, Ok(None)),
            "un brouillon seul n'est jamais actif : {:?}",
            store.get_active("G_git").await
        );
        assert_eq!(
            store.get_draft("G_git").await.expect("lecture du brouillon"),
            Some(toolset("G_git", "1.0.0")),
            "le brouillon se relit"
        );
    }

    /// `Scenario` « la version active reste visible quand un brouillon la suit » : `put_draft`
    /// n'écrit que `draft`, la colonne `active` et sa version dérivée restent.
    #[tokio::test]
    async fn the_active_version_stays_visible_behind_a_draft() {
        let db = test_db().await;
        user(&db, "root", true).await;
        write_active_row(&db, "G_git", "1.0.0").await;
        let store = SeaToolsetStore::new(db.clone(), Identity::new(db.clone()), "root".to_string());

        store
            .put_draft(&toolset("G_git", "2.0.0"))
            .await
            .expect("le brouillon v2 est écrit");
        assert_eq!(
            store.get_active("G_git").await.expect("lecture de l'active"),
            Some(toolset("G_git", "1.0.0")),
            "l'active v1 subsiste"
        );
        assert_eq!(
            store.get_draft("G_git").await.expect("lecture du brouillon"),
            Some(toolset("G_git", "2.0.0")),
            "le brouillon v2 est lu"
        );
    }

    /// `Scenario` « un toolset invalide est refusé sans écriture ».
    #[tokio::test]
    async fn an_invalid_toolset_is_refused_without_a_write() {
        let db = test_db().await;
        user(&db, "root", true).await;
        let store = SeaToolsetStore::new(db.clone(), Identity::new(db.clone()), "root".to_string());

        let refused = store
            .put_draft(&broken_toolset("G_casse"))
            .await
            .expect_err("un source sans contrat est refusé");
        assert!(
            matches!(
                refused,
                ToolsetsError::InvalidToolset { .. }
                    | ToolsetsError::Template { .. }
                    | ToolsetsError::SkillTooLarge { .. }
            ),
            "la variante d'invalidité de lymaille est rendue : {refused:?}"
        );
        assert_eq!(
            row_count(
                &db,
                "miryad_lymaille_catalog_toolsets",
                &[("toolset_id", "G_casse")]
            )
            .await,
            0,
            "aucune ligne écrite"
        );
    }

    /// `Scenario` « écrire un toolset du catalogue exige un admin » : `ToolsetsError::Store`
    /// citant `MRD-LYMAILLE-002`.
    #[tokio::test]
    async fn writing_a_catalog_toolset_requires_admin() {
        let db = test_db().await;
        user(&db, "bob", false).await;
        let store = SeaToolsetStore::new(db.clone(), Identity::new(db.clone()), "bob".to_string());

        let refused = store
            .put_draft(&toolset("G_x", "1.0.0"))
            .await
            .expect_err("un non-admin n'écrit pas au catalogue");
        assert!(
            refused.to_string().contains("MRD-LYMAILLE-002"),
            "le refus cite le code : {refused}"
        );
    }

    /// `Scenario` « types de contexte et contextes font l'aller-retour ».
    #[tokio::test]
    async fn context_types_and_contexts_round_trip() {
        let db = test_db().await;
        user(&db, "root", true).await;
        let identity = Identity::new(db.clone());

        let context_type = ContextType::new(
            "cluster".to_string(),
            semver("1.0.0"),
            json!({"type": "object", "properties": {"cluster": {"type": "string"}}, "required": ["cluster"]}),
        )
        .expect("un type de contexte valide");
        let context = ToolsetContext::new("prod".to_string(), ContextLevel::Project)
            .expect("un contexte de toolset valide");

        SeaContextTypeStore::new(db.clone(), identity.clone(), "root".to_string())
            .put_type(&context_type)
            .await
            .expect("l'admin écrit le type de contexte");
        SeaContextStore::new(db.clone(), identity.clone(), "root".to_string(), None)
            .put_context(&context)
            .await
            .expect("l'admin écrit le contexte");

        assert_eq!(
            SeaContextTypeStore::new(db.clone(), identity.clone(), "root".to_string())
                .get_type("cluster")
                .await
                .expect("lecture du type"),
            Some(context_type),
            "le type relu égale l'original"
        );
        assert_eq!(
            SeaContextStore::new(db.clone(), identity, "root".to_string(), None)
                .get_context("prod")
                .await
                .expect("lecture du contexte"),
            Some(context),
            "le contexte relu égale l'original"
        );
    }

    /// Routage lot 2 (`DEC-20`, amendement du `Must`) : un nom privé va dans `user_contexts`
    /// (`owner_id = user_id`, invisible à un autre utilisateur comme à un admin — règle 1 de
    /// ./mod.sdd), et l'upsert par `(owner_id, name)` remplace sans dupliquer.
    #[tokio::test]
    async fn private_contexts_are_routed_to_the_user_table_and_closed() {
        let db = test_db().await;
        user(&db, "alice", false).await;
        user(&db, "bob", false).await;
        user(&db, "root", true).await;
        let identity = Identity::new(db.clone());

        SeaContextStore::new(db.clone(), identity.clone(), "alice".to_string(), None)
            .put_context(&context("dev", ContextLevel::User))
            .await
            .expect("alice écrit son contexte privé `dev`");
        SeaContextStore::new(db.clone(), identity.clone(), "alice".to_string(), None)
            .put_context(
                &context("dev", ContextLevel::User)
                    .with_parameter("region".to_string(), json!("eu"))
                    .expect("un paramètre ordinaire"),
            )
            .await
            .expect("l'upsert remplace le contexte");

        assert_eq!(
            SeaContextStore::new(db.clone(), identity.clone(), "alice".to_string(), None)
                .get_context("dev")
                .await
                .expect("lecture d'alice"),
            Some(
                context("dev", ContextLevel::User)
                    .with_parameter("region".to_string(), json!("eu"))
                    .expect("un paramètre ordinaire")
            ),
            "la ligne relue est la sienne, une seule ligne"
        );
        assert_eq!(
            row_count(&db, "miryad_lymaille_user_contexts", &[("name", "dev")]).await,
            1,
            "l'upsert par (`owner_id`, `name`) n'a pas dupliqué"
        );
        assert!(
            matches!(
                SeaContextStore::new(db.clone(), identity.clone(), "bob".to_string(), None)
                    .get_context("dev")
                    .await,
                Ok(None)
            ),
            "bob ne lit pas le privé d'une autre : {:?}",
            SeaContextStore::new(db.clone(), identity.clone(), "bob".to_string(), None)
                .get_context("dev")
                .await
        );
        assert!(
            matches!(
                SeaContextStore::new(db.clone(), identity, "root".to_string(), None)
                    .get_context("dev")
                    .await,
                Ok(None)
            ),
            "`get_context` ne rend jamais le privé d'un autre, admin compris"
        );
    }

    /// Routage lot 2 : un nom `G_…` va dans `catalog_contexts`, lisible de tous ; son écriture
    /// exige un admin (`check_write`, refus rendu par `Store` citant `MRD-LYMAILLE-002`).
    #[tokio::test]
    async fn catalog_contexts_are_routed_to_the_catalog_table() {
        let db = test_db().await;
        user(&db, "alice", false).await;
        user(&db, "root", true).await;
        let identity = Identity::new(db.clone());

        SeaContextStore::new(db.clone(), identity.clone(), "root".to_string(), None)
            .put_context(&context("G_prod", ContextLevel::Project))
            .await
            .expect("l'admin écrit `G_prod` au catalogue");
        assert_eq!(
            SeaContextStore::new(db.clone(), identity.clone(), "alice".to_string(), None)
                .get_context("G_prod")
                .await
                .expect("lecture de alice"),
            Some(context("G_prod", ContextLevel::Project)),
            "le catalogue est lisible de tous"
        );
        assert_eq!(
            row_count(&db, "miryad_lymaille_catalog_contexts", &[("name", "G_prod")]).await,
            1,
            "la ligne est dans la table du catalogue"
        );
        assert_eq!(
            row_count(&db, "miryad_lymaille_user_contexts", &[]).await,
            0,
            "rien n'a été écrit dans la table privée"
        );

        let refused = SeaContextStore::new(db.clone(), identity, "alice".to_string(), None)
            .put_context(&context("G_x", ContextLevel::Project))
            .await
            .expect_err("un non-admin n'écrit pas un contexte du catalogue");
        assert!(
            refused.to_string().contains("MRD-LYMAILLE-002"),
            "le refus cite le code (règle 5) : {refused}"
        );
    }

    /// `Scenario` (réalignement R10, `../store/toolset.sdd`, DEC-28) : `put_context` ne touche
    /// **que** la colonne `context` d'une ligne existante — `context_type` (la référence au type,
    /// posée par le CRUD de l'entité) et `created_at` subsistent.
    #[tokio::test]
    async fn put_context_only_rewrites_the_context_column() {
        let db = test_db().await;
        user(&db, "root", true).await;
        db.execute_unprepared(
            "INSERT INTO miryad_lymaille_catalog_contexts (name, context, context_type, created_at) \
             VALUES ('G_prod', '{\"name\": \"G_prod\", \"level\": \"project\", \"parameters\": {}}', \
             'cluster', '2026-11-01T00:00:00Z')",
        )
        .await
        .expect("ligne de catalogue avec context_type pose");

        SeaContextStore::new(db.clone(), Identity::new(db.clone()), "root".to_string(), None)
            .put_context(
                &context("G_prod", ContextLevel::Project)
                    .with_parameter("region".to_string(), json!("eu"))
                    .expect("un parametre ordinaire"),
            )
            .await
            .expect("lecrasement du contexte par put_context reussit");

        let select = Query::select()
            .columns([Alias::new("context_type"), Alias::new("created_at")])
            .from(Alias::new("miryad_lymaille_catalog_contexts"))
            .and_where(Expr::col(Alias::new("name")).eq("G_prod"))
            .to_owned();
        let row = db
            .query_one(&select)
            .await
            .expect("relecture de la ligne")
            .expect("la ligne existe toujours");
        assert_eq!(
            row.try_get::<Option<String>>("", "context_type")
                .expect("colonne context_type relue"),
            Some("cluster".to_owned()),
            "`put_context` ne touche pas la reference au type"
        );
        assert_eq!(
            SeaContextStore::new(db.clone(), Identity::new(db.clone()), "root".to_string(), None)
                .get_context("G_prod")
                .await
                .expect("lecture du contexte"),
            Some(
                context("G_prod", ContextLevel::Project)
                    .with_parameter("region".to_string(), json!("eu"))
                    .expect("un parametre ordinaire")
            ),
            "la colonne context est bien remplacee"
        );
    }

    /// Mème verrou R10 côté privé : l'upsert de `put_context` sur une **ligne neuve** pose
    /// `context_type` non renseigné (`NULL`) — le store n'invente pas de type — et sur la ligne
    /// d'une autre table, aucune.
    #[tokio::test]
    async fn put_context_leaves_a_new_row_without_a_context_type() {
        let db = test_db().await;
        user(&db, "alice", false).await;
        SeaContextStore::new(db.clone(), Identity::new(db.clone()), "alice".to_string(), None)
            .put_context(&context("dev", ContextLevel::User))
            .await
            .expect("alice cree son contexte prive");
        let select = Query::select()
            .columns([Alias::new("context_type")])
            .from(Alias::new("miryad_lymaille_user_contexts"))
            .and_where(Expr::col(Alias::new("name")).eq("dev"))
            .to_owned();
        let row = db
            .query_one(&select)
            .await
            .expect("relecture de la ligne")
            .expect("la ligne existe");
        assert_eq!(
            row.try_get::<Option<String>>("", "context_type")
                .expect("colonne context_type relue"),
            None,
            "le store ne pose pas de type de lui-meme"
        );
    }

    /// Hôte factice de l'amendement DEC-27 : `resolve_context` rend le contexte de fixture au
    /// `subject` autorisé, `None` aux autres (ou `Err` si `broken`) ; il compte ses appels et
    /// note les `subject` reçus — preuve que la délégation passe le principal relu.
    struct SpyHost {
        granted_to: Option<String>,
        broken: bool,
        calls: AtomicUsize,
        seen: Mutex<Vec<String>>,
    }

    impl SpyHost {
        fn granting_to(subject: &str) -> Arc<Self> {
            Arc::new(Self {
                granted_to: Some(subject.to_string()),
                broken: false,
                calls: AtomicUsize::new(0),
                seen: Mutex::new(Vec::new()),
            })
        }

        fn broken() -> Arc<Self> {
            Arc::new(Self {
                granted_to: None,
                broken: true,
                calls: AtomicUsize::new(0),
                seen: Mutex::new(Vec::new()),
            })
        }

        fn seen_subjects(&self) -> Vec<String> {
            self.seen.lock().expect("verrou du spy non empoisonné").clone()
        }
    }

    #[async_trait]
    impl LymailleHost for SpyHost {
        async fn resolve_context(
            &self,
            id: &str,
            caller: &CallerPrincipal,
        ) -> Result<Option<ToolsetContext>, HostError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.seen
                .lock()
                .expect("verrou du spy non empoisonné")
                .push(caller.subject.clone());
            if self.broken {
                return Err(HostError);
            }
            if id == "H_sbx" && self.granted_to.as_deref() == Some(caller.subject.as_str()) {
                Ok(Some(spy_context()))
            } else {
                Ok(None)
            }
        }
    }

    fn spy_context() -> ToolsetContext {
        ToolsetContext::new("H_sbx".to_string(), ContextLevel::Project)
            .expect("le contexte de fixture se construit")
    }

    /// Retire une table de la base de test (tables qui y existent — pour les toolsets et le
    /// catalogue) : tout chemin qui la lirait ou l'écrirait ensuite tombe sur une erreur
    /// « table introuvable » — la preuve d'absence de requête, pas seulement le `None` rendu.
    /// Pour les contextes, aucune retraite n'est nécessaire : sous lot 2 (`DEC-20`) la table
    /// `miryad_lymaille_contexts` du lot 1 n'existe plus dans la base migrée — la moindre requête
    /// sur cette table échoue d'emblée, y compris via `SeaContextStore` (routage `G_`/privé en
    /// place sur les tables du lot 2, amendement du `Must` ; le chemin `H_…` ne doit jamais en
    /// atteindre la moindre).
    async fn drop_table(db: &DatabaseConnection, table: &str) {
        db.execute_unprepared(&format!("DROP TABLE {table}"))
            .await
            .unwrap_or_else(|err| panic!("retrait de {table} pour la preuve : {err}"));
    }

    /// `Scenario` « un contexte hôte est résolu par l'hôte pour le principal courant » : la
    /// table `miryad_lymaille_contexts` n'existe pas dans la base migrée (`DEC-20`, routage
    /// lot 2 en place sur les tables du lot 2) — toute requête sur le chemin `H_…` rendrait une
    /// erreur « table introuvable », pas le contexte de l'hôte : c'est la preuve d'absence de
    /// requête. Le spy rend le contexte à `alice`, `None` à `bob`.
    #[tokio::test]
    async fn a_host_context_is_resolved_by_the_host_for_the_current_principal() {
        let db = test_db().await;
        user(&db, "alice", false).await;
        user(&db, "bob", false).await;
        let host = SpyHost::granting_to("alice");
        let identity = Identity::new(db.clone());

        let alice = SeaContextStore::new(
            db.clone(),
            identity.clone(),
            "alice".to_string(),
            Some(Arc::clone(&host) as SharedHost),
        );
        assert_eq!(
            alice
                .get_context("H_sbx")
                .await
                .expect("l'hôte répond pour alice"),
            Some(spy_context()),
            "alice obtient le contexte rendu par l'hôte"
        );

        let bob = SeaContextStore::new(
            db.clone(),
            identity,
            "bob".to_string(),
            Some(Arc::clone(&host) as SharedHost),
        );
        assert!(
            matches!(bob.get_context("H_sbx").await, Ok(None)),
            "`None` de l'hôte pour bob, sans distinction d'absence : {:?}",
            bob.get_context("H_sbx").await
        );

        assert_eq!(
            host.seen_subjects(),
            vec!["alice".to_string(), "bob".to_string()],
            "l'hôte reçoit le principal relu à chaque appel"
        );
    }

    /// `Scenario` « sans hôte, un contexte hôte est absent » : sans `with_host`, et avec un
    /// hôte en panne, `get_context("H_sbx")` rend `Ok(None)` — échec fermé, indiscernable d'un
    /// contexte absent. La table `miryad_lymaille_contexts` n'existe pas dans la base migrée
    /// (`DEC-20`) : la moindre requête sur le chemin `H_…` rendrait une erreur, pas `Ok(None)`
    /// — preuve d'absence de requête.
    #[tokio::test]
    async fn without_a_host_a_host_context_is_absent() {
        let db = test_db().await;
        user(&db, "alice", false).await;
        let identity = Identity::new(db.clone());

        let store = SeaContextStore::new(db.clone(), identity.clone(), "alice".to_string(), None);
        assert!(
            matches!(store.get_context("H_sbx").await, Ok(None)),
            "sans hôte → `Ok(None)` fermé : {:?}",
            store.get_context("H_sbx").await
        );

        let broken = SpyHost::broken();
        let store = SeaContextStore::new(
            db.clone(),
            identity,
            "alice".to_string(),
            Some(Arc::clone(&broken) as SharedHost),
        );
        assert!(
            matches!(store.get_context("H_sbx").await, Ok(None)),
            "hôte en panne → `Ok(None)` fermé, aucun texte d'hôte propagé : {:?}",
            store.get_context("H_sbx").await
        );
    }

    /// `Scenario` « un contexte hôte ne s'écrit pas » : même un administrateur est refusé par
    /// `check_write`, avec le code du refus `Forbidden` dans le texte (règle 5). La table
    /// `miryad_lymaille_contexts` n'existe pas dans la base migrée (`DEC-20`) : si le chemin
    /// tentait l'`INSERT`, l'erreur serait « table introuvable », pas le refus d'autorité — le
    /// refus précède donc tout accès table.
    #[tokio::test]
    async fn a_host_context_is_not_written() {
        let db = test_db().await;
        user(&db, "root", true).await;
        let store = SeaContextStore::new(db.clone(), Identity::new(db.clone()), "root".to_string(), None);

        let refused = store
            .put_context(&spy_context())
            .await
            .expect_err("un contexte hôte ne s'écrit pas, admin compris");
        assert!(
            refused.to_string().contains("MRD-LYMAILLE-002"),
            "le refus cite le code `Forbidden` (règle 5) : {refused}"
        );
    }

    /// Must amendé DEC-27 pour les toolsets : `get_active`, `get_draft` et `put_draft` d'un nom
    /// `H_…` — lecture `Ok(None)`, écriture refusée, sans accès table. Les tables des toolsets
    /// sont retirées : la moindre requête rendrait une erreur, pas le verdict attendu.
    #[tokio::test]
    async fn a_host_identifier_reaches_no_toolset_table() {
        let db = test_db().await;
        user(&db, "root", true).await;
        drop_table(&db, "miryad_lymaille_catalog_toolsets").await;
        drop_table(&db, "miryad_lymaille_toolsets").await;
        let store = SeaToolsetStore::new(db.clone(), Identity::new(db.clone()), "root".to_string());

        assert!(
            matches!(store.get_active("H_sbx").await, Ok(None)),
            "lecture `H_sbx` → `Ok(None)` sans requête : {:?}",
            store.get_active("H_sbx").await
        );
        assert!(
            matches!(store.get_draft("H_sbx").await, Ok(None)),
            "lecture du brouillon `H_sbx` → `Ok(None)` sans requête : {:?}",
            store.get_draft("H_sbx").await
        );
        let refused = store
            .put_draft(&toolset("H_sbx", "1.0.0"))
            .await
            .expect_err("un toolset `H_…` ne s'écrit pas, admin compris");
        assert!(
            refused.to_string().contains("MRD-LYMAILLE-002"),
            "l'écriture cite le code `Forbidden` (règle 5) : {refused}"
        );
    }
}
