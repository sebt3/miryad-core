//! `SeaToolsetStore`, `SeaContextTypeStore`, `SeaContextStore` — les traits de toolsets de
//! `lymaille` sur `SeaORM` (`src/lymaille/store/toolset.sdd`).
//!
//! Une ligne de `miryad_lymaille_catalog_toolsets` (catalogue `G_…`) ou de
//! `miryad_lymaille_toolsets` (privé, `owner_id`) porte la version **active** et le
//! **brouillon** d'un toolset (DSD `DEC-18`) : `get_active` ne lit jamais le brouillon et
//! `put_draft` n'écrit que lui — l'activation est l'écriture de `active` par un admin via
//! l'entité, jamais par le store. `put_draft` valide par le contrôle **partagé**
//! `check_toolset` de `../definitions/toolset.rs`, qu'il appelle sans logique propre. Types de
//! contexte et contextes sont globaux et internes au
//! lot 1 (DSD `OI-9`), écriture réservée aux admins.

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
use crate::lymaille::identity::CallerPrincipal;
use crate::lymaille::identity::IdKind;
use crate::lymaille::identity::Identity;
use crate::lymaille::identity::check_write;
use crate::lymaille::identity::classify;

/// Tables, noms complets (../migration.sdd).
const CATALOG_TOOLSETS_TABLE: &str = "miryad_lymaille_catalog_toolsets";
const PRIVATE_TOOLSETS_TABLE: &str = "miryad_lymaille_toolsets";
const CONTEXT_TYPES_TABLE: &str = "miryad_lymaille_context_types";
const CONTEXTS_TABLE: &str = "miryad_lymaille_contexts";

/// Refus de l'hôte rendu à `lymaille` par la variante `Store` de son erreur, code `MRD-*`
/// compris (./mod.sdd règle 5).
fn deny(error: &LymailleError) -> ToolsetsError {
    ToolsetsError::Store(error.to_string())
}

/// Nom de la forme sérialisée d'un objet de table à clé par nom : `ContextType` et
/// `ToolsetContext` ne rendent pas leur nom par un accesseur public — la colonne `name` est la
/// copie de la clé de l'objet JSON, source de vérité (DSD `DEC-17`).
fn stored_name(value: &serde_json::Value, what: &str) -> ToolsetsResult<String> {
    value
        .get("name")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| ToolsetsError::Store(format!("stored {what} carries no name")))
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
        // DEC-27 (amendement à venir dans `./toolset.sdd`) : un identifiant `H_…` est résolu
        // par l'hôte, jamais stocké en table — lecture `Ok(None)` sans requête, avant même la
        // reconstruction du principal.
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
            // DEC-27 (amendement à venir dans `./toolset.sdd`) : `check_write` refuse déjà
            // `H_…` en amont — aucun accès table ; branche de couverture au même refus.
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
    pub(crate) fn new(db: DatabaseConnection, identity: Identity, subject: String) -> Self {
        Self {
            db,
            identity,
            subject,
        }
    }
}

impl ContextTypeStore for SeaContextTypeStore {
    async fn get_type(&self, name: &str) -> ToolsetsResult<Option<ContextType>> {
        let select = Query::select()
            .columns([Alias::new("context_type")])
            .from(Alias::new(CONTEXT_TYPES_TABLE))
            .and_where(Expr::col(Alias::new("name")).eq(name))
            .to_owned();
        let stored = first_json(&self.db, &select, "context_type")
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
        let name = stored_name(&value, "context type")?;
        serde_json::from_value::<ContextType>(value.clone()).map_err(|_cause| {
            ToolsetsError::InvalidContextType {
                name: name.clone(),
                reason: "the stored form does not read back",
            }
        })?;
        upsert(
            &self.db,
            CONTEXT_TYPES_TABLE,
            &["name", "context_type"],
            vec![Expr::val(name), Expr::val(value)],
            &["name"],
            &["context_type"],
        )
        .await
        .map_err(|err| ToolsetsError::Store(db_text(&err)))
    }
}

impl ContextStore for SeaContextStore {
    async fn get_context(&self, name: &str) -> ToolsetsResult<Option<ToolsetContext>> {
        let select = Query::select()
            .columns([Alias::new("context")])
            .from(Alias::new(CONTEXTS_TABLE))
            .and_where(Expr::col(Alias::new("name")).eq(name))
            .to_owned();
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
        require_admin(&self.identity, &self.subject, "toolset context write").await?;
        let value = serde_json::to_value(context)
            .map_err(|_cause| ToolsetsError::Store("toolset context could not be serialized".to_owned()))?;
        upsert(
            &self.db,
            CONTEXTS_TABLE,
            &["name", "context"],
            vec![
                Expr::val(stored_name(&value, "toolset context")?),
                Expr::val(value),
            ],
            &["name"],
            &["context"],
        )
        .await
        .map_err(|err| ToolsetsError::Store(db_text(&err)))
    }
}

#[cfg(test)]
mod tests {
    use super::{SeaContextStore, SeaContextTypeStore, SeaToolsetStore};
    use crate::lymaille::identity::Identity;
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
    use chrono::Utc;
    use sea_orm::ActiveModelTrait as _;
    use sea_orm::ActiveValue;
    use sea_orm::ConnectionTrait as _;
    use sea_orm::DatabaseConnection;
    use sea_orm::sea_query::Alias;
    use sea_orm::sea_query::Query;
    use serde_json::json;
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

    async fn row_count(db: &DatabaseConnection, table: &str) -> usize {
        let select = Query::select()
            .columns([Alias::new("*")])
            .from(Alias::new(table))
            .to_owned();
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
            row_count(&db, "miryad_lymaille_catalog_toolsets").await,
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
        SeaContextStore::new(db.clone(), identity.clone(), "root".to_string())
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
            SeaContextStore::new(db.clone(), identity, "root".to_string())
                .get_context("prod")
                .await
                .expect("lecture du contexte"),
            Some(context),
            "le contexte relu égale l'original"
        );
    }
}
