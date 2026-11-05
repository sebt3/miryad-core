//! `SeaDraftStore` — brouillons de la session de configuration sur les colonnes `draft` des
//! toolsets (catalogue et privé) et des types de contexte (`src/lymaille/store/draft.sdd`).
//!
//! Le trait `DraftStore` de `lymaille` complète `ToolsetStore` (qui sait déjà lire et écrire un
//! brouillon de toolset) : lister et supprimer des brouillons, et tenir les brouillons de
//! **types de contexte**, que `ContextTypeStore` ignore (DSD `DEC-18`). Le store est lié à un
//! `subject` : il ne liste et ne supprime que les brouillons de cet utilisateur (toolsets privés)
//! et les brouillons du catalogue seulement pour un admin. **Aucune opération du trait n'écrit
//! `active`** — l'activation est une action de l'hôte (`../interactive/activation.sdd`).

use sea_orm::ConnectionTrait;
use sea_orm::DatabaseConnection;
use sea_orm::DbErr;
use sea_orm::sea_query::Alias;
use sea_orm::sea_query::Expr;
use sea_orm::sea_query::ExprTrait;
use sea_orm::sea_query::OnConflict;
use sea_orm::sea_query::Query;

use ::lymaille::ContextType;
use ::lymaille::DraftEntry;
use ::lymaille::DraftKind;
use ::lymaille::DraftStore;
use ::lymaille::ToolsetsError;
use ::lymaille::ToolsetsResult;

use super::db_text;
use crate::lymaille::error::LymailleError;
use crate::lymaille::identity::CallerPrincipal;
use crate::lymaille::identity::IdKind;
use crate::lymaille::identity::Identity;
use crate::lymaille::identity::check_write;

/// Tables, noms complets (../migration.sdd) : les mêmes que ../toolset.rs (`DEC-18`), plus
/// `catalog_context_types` du lot 2 (`DEC-20`).
const CATALOG_TOOLSETS_TABLE: &str = "miryad_lymaille_catalog_toolsets";
const PRIVATE_TOOLSETS_TABLE: &str = "miryad_lymaille_toolsets";
const CATALOG_CONTEXT_TYPES_TABLE: &str = "miryad_lymaille_catalog_context_types";

/// Refus de l'hôte rendu à `lymaille` par la variante `Store` de son erreur, code `MRD-*`
/// compris (./mod.sdd règle 5).
fn deny(error: &LymailleError) -> ToolsetsError {
    ToolsetsError::Store(error.to_string())
}

/// Principal de l'appel, reconstruit à chaque opération (./mod.sdd règle 2, DSD `DEC-3`).
async fn principal(identity: &Identity, subject: &str) -> Result<CallerPrincipal, ToolsetsError> {
    identity.resolve(subject).await.map_err(|err| deny(&err))
}

/// Nom de la forme sérialisée d'un `ContextType` : `name` n'est rendu par aucun accesseur public
/// (`pub(in crate::toolsets)` chez `lymaille`) — la colonne `name` est la copie de la clé de
/// l'objet JSON, source de vérité (DSD `DEC-17`, même motif que ../toolset.rs).
fn stored_name(value: &serde_json::Value, what: &str) -> ToolsetsResult<String> {
    value
        .get("name")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| ToolsetsError::Store(format!("stored {what} carries no name")))
}

/// Version de la forme sérialisée d'un `ContextType` (colonne dérivée `version`, motif des
/// hooks de `../definitions/context_type.rs`).
fn stored_version(value: &serde_json::Value, what: &str) -> ToolsetsResult<String> {
    value
        .get("version")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| ToolsetsError::Store(format!("stored {what} carries no version")))
}

/// Store des brouillons, lié à un `subject` (`./draft.sdd`).
#[derive(Clone)]
pub struct SeaDraftStore {
    db: DatabaseConnection,
    identity: Identity,
    subject: String,
}

impl SeaDraftStore {
    // Constructeur appelé par `LymailleStores::drafts()` (amendement lot 2 de `./mod.sdd`).
    pub(crate) fn new(db: DatabaseConnection, identity: Identity, subject: String) -> Self {
        Self {
            db,
            identity,
            subject,
        }
    }

    /// Noms des lignes d'une table portant un `draft` non nul, filtres equality additionnels
    /// (./draft.sdd : `list_drafts`, une requête par table — DSD `DEC-19`).
    async fn names_with_draft(
        &self,
        table: &str,
        name_column: &str,
        conditions: &[(&str, Expr)],
    ) -> Result<Vec<String>, ToolsetsError> {
        let mut select = Query::select();
        select
            .columns([Alias::new(name_column)])
            .from(Alias::new(table))
            .and_where(Expr::col(Alias::new("draft")).is_not_null());
        for (column, condition) in conditions {
            select.and_where(Expr::col(Alias::new(*column)).eq(condition.clone()));
        }
        let rows = self
            .db
            .query_all(&select)
            .await
            .map_err(|err| ToolsetsError::Store(db_text(&err)))?;
        rows.iter()
            .map(|row| {
                row.try_get::<String>("", name_column)
                    .map_err(|err| ToolsetsError::Store(db_text(&err)))
            })
            .collect()
    }

    /// Biffe le `draft` d'une ligne désignée par ses conditions : `false` si la ligne est absente
    /// ou n'a pas de brouillon ; sinon mise à `NULL`, et **suppression de la ligne** si elle ne
    /// porte plus ni `active` ni `draft` (invariant de ../migration.sdd). `active` n'est jamais
    /// modifié (`Must not`).
    async fn discard_draft(&self, table: &str, conditions: &[(&str, Expr)]) -> ToolsetsResult<bool> {
        let mut probe = Query::select();
        probe
            .columns([Alias::new("active"), Alias::new("draft")])
            .from(Alias::new(table));
        for (column, condition) in conditions {
            probe.and_where(Expr::col(Alias::new(*column)).eq(condition.clone()));
        }
        let row = self
            .db
            .query_one(&probe)
            .await
            .map_err(|err| ToolsetsError::Store(db_text(&err)))?;
        let Some(row) = row else { return Ok(false) };
        let draft: Option<serde_json::Value> = row
            .try_get("", "draft")
            .map_err(|err| ToolsetsError::Store(db_text(&err)))?;
        if draft.is_none() {
            return Ok(false);
        }
        let active: Option<serde_json::Value> = row
            .try_get("", "active")
            .map_err(|err| ToolsetsError::Store(db_text(&err)))?;
        if active.is_some() {
            let mut update = Query::update();
            update
                .table(Alias::new(table))
                .value(Alias::new("draft"), Expr::null());
            for (column, condition) in conditions {
                update.and_where(Expr::col(Alias::new(*column)).eq(condition.clone()));
            }
            self.db
                .execute(&update)
                .await
                .map(|_| ())
                .map_err(|err| ToolsetsError::Store(db_text(&err)))?;
        } else {
            let mut delete = Query::delete();
            delete.from_table(Alias::new(table));
            for (column, condition) in conditions {
                delete.and_where(Expr::col(Alias::new(*column)).eq(condition.clone()));
            }
            self.db
                .execute(&delete)
                .await
                .map(|_| ())
                .map_err(|err| ToolsetsError::Store(db_text(&err)))?;
        }
        Ok(true)
    }

    /// Droit d'écriture d'une ligne du catalogue des types (table `AdminOnly`, arbitrage (a) du
    /// 2026-11-04 de `../definitions/context_type.sdd` : la garantie de catalogue est la table,
    /// pas le préfixe du nom).
    async fn require_type_admin(&self, what: &str) -> Result<CallerPrincipal, ToolsetsError> {
        let caller = principal(&self.identity, &self.subject).await?;
        if caller.is_admin {
            Ok(caller)
        } else {
            Err(deny(&LymailleError::Forbidden(format!("{what} requires admin"))))
        }
    }
}

impl DraftStore for SeaDraftStore {
    async fn list_drafts(&self) -> ToolsetsResult<Vec<DraftEntry>> {
        // Règle 2 de ./mod.sdd : le principal est reconstruit à chaque appel (DSD `DEC-3`).
        let caller = principal(&self.identity, &self.subject).await?;
        let mut entries: Vec<DraftEntry> = Vec::new();
        // Toolsets privés de l'appelant : `owner_id = user_id` (règle 1, jamais ceux d'un autre).
        for name in self
            .names_with_draft(
                PRIVATE_TOOLSETS_TABLE,
                "toolset_id",
                &[("owner_id", Expr::val(caller.user_id))],
            )
            .await?
        {
            entries.push(DraftEntry::new(DraftKind::Toolset, name));
        }
        // Toolsets du catalogue : seulement pour un admin (DSD `FR-9`).
        if caller.is_admin {
            for name in self
                .names_with_draft(CATALOG_TOOLSETS_TABLE, "toolset_id", &[])
                .await?
            {
                entries.push(DraftEntry::new(DraftKind::Toolset, name));
            }
        }
        // Types de contexte du catalogue : leur liste est publique (lecture `Public` de
        // `../definitions/context_type.sdd`) ; l'écriture seule est réservée.
        for name in self
            .names_with_draft(CATALOG_CONTEXT_TYPES_TABLE, "name", &[])
            .await?
        {
            entries.push(DraftEntry::new(DraftKind::ContextType, name));
        }
        // Trié par (`DraftKind`, nom) (`./draft.sdd`).
        entries.sort_unstable_by(|first, second| {
            (first.kind(), first.name()).cmp(&(second.kind(), second.name()))
        });
        Ok(entries)
    }

    async fn delete_draft(&self, kind: DraftKind, name: &str) -> ToolsetsResult<bool> {
        match kind {
            DraftKind::Toolset => {
                // Règle 4 de ./mod.sdd : `check_write` d'abord — catalogue = admin, préfixe
                // réservé et identifiant `H_…` refusés, identifiant invalide refusé.
                let caller = principal(&self.identity, &self.subject).await?;
                let id_kind = check_write(&caller, name).map_err(|err| deny(&err))?;
                match id_kind {
                    IdKind::Catalog => {
                        self.discard_draft(CATALOG_TOOLSETS_TABLE, &[("toolset_id", Expr::val(name))])
                            .await
                    }
                    IdKind::Private => {
                        self.discard_draft(
                            PRIVATE_TOOLSETS_TABLE,
                            &[
                                ("owner_id", Expr::val(caller.user_id)),
                                ("toolset_id", Expr::val(name)),
                            ],
                        )
                        .await
                    }
                    // Garde de couverture : `check_write` refuse `H_…` en amont.
                    IdKind::Host => Err(deny(&LymailleError::Forbidden(
                        "host identifiers are not writable".to_string(),
                    ))),
                }
            }
            DraftKind::ContextType => {
                // La table des types est la table du catalogue : admin à chaque appel.
                self.require_type_admin("context type draft write").await?;
                self.discard_draft(CATALOG_CONTEXT_TYPES_TABLE, &[("name", Expr::val(name))])
                    .await
            }
            kind => Err(ToolsetsError::Store(format!("unsupported draft kind {kind:?}"))),
        }
    }

    async fn put_type_draft(&self, context_type: &ContextType) -> ToolsetsResult<()> {
        // Validation `lymaille` : déjà valide par construction (le constructeur de
        // `ContextType` compile le schéma et vérifie la grammaire du nom). Droit relu à
        // l'appel : la table des types est celle du catalogue (arbitrage (a) du 2026-11-04,
        // `../definitions/context_type.sdd` — les noms `lymaille` ne portent pas de préfixe
        // `G_`, la garantie est `AdminOnly` + la table, donc `check_write` est ici un contrôle
        // de table, pas de préfixe).
        self.require_type_admin("context type draft write").await?;
        let value = serde_json::to_value(context_type)
            .map_err(|_cause| ToolsetsError::Store("context type could not be serialized".to_owned()))?;
        let name = stored_name(&value, "context type")?;
        let version = stored_version(&value, "context type")?;
        // Upsert écrivant **uniquement** la colonne `draft` à la mise à jour ; ligne nouvelle
        // créée avec `active = NULL` — le store n'active jamais. Une ligne de type créée par le
        // store ne porte **pas** `allowed_agents` (vide) — seule colonne DEC-28 restante depuis
        // `lymaille` 0.2.0, `kind` et `default_agent` étant dans le JSON du type : le store ne les
        // invente pas ; les hooks de l'entité refuseront l'activation tant que l'admin n'aura pas
        // complété la ligne par le CRUD (`../store/toolset.sdd`, acté 2026-11-04, révisé
        // 2026-11-05).
        let statement = Query::insert()
            .into_table(Alias::new(CATALOG_CONTEXT_TYPES_TABLE))
            .columns([
                "name",
                "version",
                "allowed_agents",
                "active",
                "draft",
                "created_at",
            ])
            .values(vec![
                Expr::val(name),
                Expr::val(version),
                Expr::val(serde_json::json!([])),
                Expr::null(),
                Expr::val(value),
                Expr::val(chrono::Utc::now()),
            ])
            .map_err(|err| ToolsetsError::Store(db_text(&DbErr::Custom(err.to_string()))))?
            .on_conflict(
                OnConflict::columns([Alias::new("name")])
                    .update_columns([Alias::new("draft")])
                    .to_owned(),
            )
            .to_owned();
        self.db
            .execute(&statement)
            .await
            .map(|_| ())
            .map_err(|err| ToolsetsError::Store(db_text(&err)))
    }

    async fn get_type_draft(&self, name: &str) -> ToolsetsResult<Option<ContextType>> {
        let mut select = Query::select();
        select
            .columns([Alias::new("draft")])
            .from(Alias::new(CATALOG_CONTEXT_TYPES_TABLE))
            .and_where(Expr::col(Alias::new("name")).eq(name));
        let row = self
            .db
            .query_one(&select)
            .await
            .map_err(|err| ToolsetsError::Store(db_text(&err)))?;
        let Some(row) = row else { return Ok(None) };
        let draft: Option<serde_json::Value> = row
            .try_get("", "draft")
            .map_err(|err| ToolsetsError::Store(db_text(&err)))?;
        draft
            .map(|value| {
                serde_json::from_value::<ContextType>(value).map_err(|_cause| {
                    // La cause serde est abandonnée : le texte d'un type porte son schéma.
                    ToolsetsError::Store(format!("stored context type {name} is unreadable"))
                })
            })
            .transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::SeaDraftStore;
    use crate::lymaille::store::test_support::test_db;
    use crate::lymaille::store::toolset::SeaToolsetStore;
    use crate::users::membership::sync_group_memberships;
    use crate::users::resolve_user;
    use ::lymaille::ContextType;
    use ::lymaille::DraftEntry;
    use ::lymaille::DraftKind;
    use ::lymaille::DraftStore as _;
    use ::lymaille::Toolset;
    use ::lymaille::ToolsetStore as _;
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

    fn drafts(db: &DatabaseConnection, subject: &str) -> SeaDraftStore {
        SeaDraftStore::new(
            db.clone(),
            crate::lymaille::identity::Identity::new(db.clone()),
            subject.to_string(),
        )
    }

    fn toolsets(db: &DatabaseConnection, subject: &str) -> SeaToolsetStore {
        SeaToolsetStore::new(
            db.clone(),
            crate::lymaille::identity::Identity::new(db.clone()),
            subject.to_string(),
        )
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

    /// Type de contexte de fixture — nom de la grammaire `lymaille` (`[a-z0-9-]+`, sans préfixe
    /// `G_`, arbitrage (a) du 2026-11-04 de `../definitions/context_type.sdd`).
    fn context_type(name: &str) -> ContextType {
        ContextType::new(
            name.to_string(),
            semver("1.0.0"),
            json!({"type": "object", "properties": {"cluster": {"type": "string"}}, "required": ["cluster"]}),
        )
        .expect("un type de contexte valide")
    }

    /// Ligne du catalogue écrite « via l'entité » : l'`ActiveModel` de
    /// `../definitions/toolset.rs` inséré avec ses colonnes dérivées (version active posée).
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

    /// `Scenario` « on ne liste que ses brouillons » : `alice` et `bob` ont chacun un brouillon
    /// de toolset privé ; la liste d'`alice` ne rend que le sien — et un `delete_draft` sur le
    /// brouillon d'un autre rend `false` sans y toucher (règle 1 de ./mod.sdd).
    #[tokio::test]
    async fn only_ones_own_drafts_are_listed() {
        let db = test_db().await;
        user(&db, "alice", false).await;
        user(&db, "bob", false).await;
        toolsets(&db, "alice")
            .put_draft(&toolset("x", "1.0.0"))
            .await
            .expect("alice écrit son brouillon privé `x`");
        toolsets(&db, "bob")
            .put_draft(&toolset("y", "1.0.0"))
            .await
            .expect("bob écrit son brouillon privé `y`");

        let listed = drafts(&db, "alice")
            .list_drafts()
            .await
            .expect("la liste des brouillons d'alice réussit");
        assert_eq!(
            listed,
            vec![DraftEntry::new(DraftKind::Toolset, "x".to_string())],
            "une seule entrée, la sienne"
        );

        let removed = drafts(&db, "bob")
            .delete_draft(DraftKind::Toolset, "x")
            .await
            .expect("la suppression par bob ne lève pas");
        assert!(!removed, "jamais le brouillon d'un autre utilisateur");
        assert!(
            toolsets(&db, "alice")
                .get_draft("x")
                .await
                .expect("lecture du brouillon d'alice")
                .is_some(),
            "le brouillon d'alice est intact"
        );
    }

    /// `Scenario` « un non-admin ne voit pas les brouillons du catalogue » : le brouillon
    /// `G_git` du catalogue n'apparaît que pour un admin.
    #[tokio::test]
    async fn a_non_admin_does_not_see_the_catalog_drafts() {
        let db = test_db().await;
        user(&db, "bob", false).await;
        user(&db, "root", true).await;
        toolsets(&db, "root")
            .put_draft(&toolset("G_git", "1.0.0"))
            .await
            .expect("l'admin écrit le brouillon `G_git` du catalogue");

        let bob = drafts(&db, "bob")
            .list_drafts()
            .await
            .expect("la liste de bob réussit");
        assert!(
            bob.is_empty(),
            "le catalogue est invisible à un non-admin : {bob:?}"
        );

        let admin = drafts(&db, "root")
            .list_drafts()
            .await
            .expect("la liste de l'admin réussit");
        assert_eq!(
            admin,
            vec![DraftEntry::new(DraftKind::Toolset, "G_git".to_string())],
            "l'entrée n'apparaît que pour l'admin"
        );
    }

    /// `Scenario` « supprimer un brouillon épargne l'actif » : sur une ligne `active` v1 +
    /// `draft` v2, `delete_draft` rend `true`, l'active v1 subsiste, le brouillon est parti.
    #[tokio::test]
    async fn deleting_a_draft_spares_the_active_version() {
        let db = test_db().await;
        user(&db, "root", true).await;
        write_active_row(&db, "G_git", "1.0.0").await;
        toolsets(&db, "root")
            .put_draft(&toolset("G_git", "2.0.0"))
            .await
            .expect("le brouillon v2 est écrit");

        assert!(
            drafts(&db, "root")
                .delete_draft(DraftKind::Toolset, "G_git")
                .await
                .expect("la suppression du brouillon réussit"),
            "un brouillon existait"
        );
        assert_eq!(
            toolsets(&db, "root")
                .get_active("G_git")
                .await
                .expect("lecture de l'active"),
            Some(toolset("G_git", "1.0.0")),
            "`get_active` rend v1"
        );
        assert!(
            matches!(toolsets(&db, "root").get_draft("G_git").await, Ok(None)),
            "`get_draft` rend `None` : {:?}",
            toolsets(&db, "root").get_draft("G_git").await
        );
    }

    /// `Scenario` « la dernière version supprimée retire la ligne » : sur une ligne `draft` seul,
    /// `delete_draft` rend `true` et la ligne n'existe plus (invariant de ../migration.sdd).
    #[tokio::test]
    async fn deleting_the_last_version_removes_the_row() {
        let db = test_db().await;
        user(&db, "root", true).await;
        toolsets(&db, "root")
            .put_draft(&toolset("G_git", "1.0.0"))
            .await
            .expect("la ligne est créée par le brouillon seul");

        assert!(
            drafts(&db, "root")
                .delete_draft(DraftKind::Toolset, "G_git")
                .await
                .expect("la suppression du brouillon réussit"),
            "un brouillon existait"
        );
        assert_eq!(
            row_count(
                &db,
                "miryad_lymaille_catalog_toolsets",
                &[("toolset_id", "G_git")]
            )
            .await,
            0,
            "la ligne sans active ni draft est retirée"
        );
    }

    /// `Scenario` « un non-admin n'écrit pas un type du catalogue » : la table des types est la
    /// table du catalogue (`AdminOnly`, arbitrage (a) du 2026-11-04 de
    /// `../definitions/context_type.sdd`) — `put_type_draft` et `delete_draft` d'un type sont
    /// refusés avec le code `MRD-LYMAILLE-002`.
    ///
    /// Le `Given` de la spec nommait le type `G_x` ; `lymaille` 0.1.0 n'accepte comme nom de
    /// `ContextType` que `[a-z0-9-]+` (arbitrage (a), même date) — un tel nom ne se construit
    /// pas. Le fixture porte donc le nom `cluster` ; le `Then` est verbatim.
    #[tokio::test]
    async fn a_non_admin_does_not_write_a_catalog_type() {
        let db = test_db().await;
        user(&db, "bob", false).await;

        let refused = drafts(&db, "bob")
            .put_type_draft(&context_type("cluster"))
            .await
            .expect_err("un non-admin n'écrit pas un type du catalogue");
        assert!(
            refused.to_string().contains("MRD-LYMAILLE-002"),
            "le refus cite le code : {refused}"
        );

        let refused = drafts(&db, "bob")
            .delete_draft(DraftKind::ContextType, "cluster")
            .await
            .expect_err("un non-admin ne supprime pas un brouillon de type du catalogue");
        assert!(
            refused.to_string().contains("MRD-LYMAILLE-002"),
            "le refus de suppression cite le code : {refused}"
        );
        assert_eq!(
            row_count(&db, "miryad_lymaille_catalog_context_types", &[]).await,
            0,
            "aucune ligne de type écrite par un non-admin"
        );
    }

    /// `Scenario` « un brouillon de type fait l'aller-retour » : l'admin écrit `put_type_draft`,
    /// `get_type_draft` rend le type original et la colonne `active` reste nulle. La liste de
    /// l'admin trie par (`DraftKind`, nom) : le brouillon de toolset du catalogue précède le
    /// brouillon de type (`DraftKind::Toolset < DraftKind::ContextType`).
    #[tokio::test]
    async fn a_type_draft_round_trips_and_the_active_stays_null() {
        let db = test_db().await;
        user(&db, "root", true).await;
        let original = context_type("cluster");

        drafts(&db, "root")
            .put_type_draft(&original)
            .await
            .expect("l'admin écrit le brouillon du type");
        assert_eq!(
            drafts(&db, "root")
                .get_type_draft("cluster")
                .await
                .expect("lecture du brouillon du type"),
            Some(original),
            "le type relu égale l'original"
        );

        let mut select = Query::select();
        select
            .columns([Alias::new("active")])
            .from(Alias::new("miryad_lymaille_catalog_context_types"))
            .and_where(Expr::col(Alias::new("name")).eq("cluster"));
        let row = db
            .query_one(&select)
            .await
            .expect("relecture de la ligne du type")
            .expect("la ligne du type existe");
        let active: Option<serde_json::Value> = row
            .try_get("", "active")
            .expect("la colonne active est JSON ou nulle");
        assert!(active.is_none(), "`active` reste nul — le store n'active jamais");

        toolsets(&db, "root")
            .put_draft(&toolset("G_git", "1.0.0"))
            .await
            .expect("l'admin écrit aussi un brouillon de toolset du catalogue");
        let listed = drafts(&db, "root")
            .list_drafts()
            .await
            .expect("la liste de l'admin réussit");
        assert_eq!(
            listed,
            vec![
                DraftEntry::new(DraftKind::Toolset, "G_git".to_string()),
                DraftEntry::new(DraftKind::ContextType, "cluster".to_string()),
            ],
            "triée par (`DraftKind`, nom)"
        );
    }
}
