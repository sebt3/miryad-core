//! Stores `SeaORM` des traits hôte de `lymaille`, construits par principal
//! (`src/lymaille/store/mod.sdd`).
//!
//! Les traits de `lymaille` ne prennent aucun propriétaire en paramètre (DSD `DEC-7`) : chaque
//! store est lié à un `subject` et n'est jamais partagé entre deux sujets. `LymailleStores` est
//! la fabrique — un exemplaire par montage de l'état, `Clone` mais jamais recyclé d'un sujet à
//! l'autre.
//!
//! Règles communes à tous les stores (./mod.sdd, clé de voûte du module) :
//!
//! 1. **Cloisonnement** : le `subject` du store fixe la frontière. Une lecture ou une écriture
//!    ne touche jamais la ligne privée d'un autre `subject` (DSD `AC-7`). Le catalogue
//!    (`miryad_lymaille_catalog_agents`, `miryad_lymaille_catalog_toolsets`) est lisible de
//!    tous ; le privé (`agents`, `toolsets`, lignes à `owner_id`) n'est lisible que de son
//!    propriétaire.
//! 2. **Principal à chaque appel** : les stores de définitions (agents, toolsets) résolvent le
//!    principal par [`Identity::resolve`] à chaque opération (DSD `DEC-3`) — `UnknownSubject`
//!    pour un `subject` sans compte, jamais de principal mis en cache ; l'`owner_id` d'une ligne
//!    privée est `CallerPrincipal::user_id`. Sessions et mémoires sont clés par `subject` et ne
//!    résolvent pas le principal.
//! 3. **Résolution d'un identifiant de définition** : [`identity::classify`] tranche —
//!    `Catalog` : la table `catalog_*` ; `Private` : la table privée avec
//!    `owner_id = user_id` ; `ReservedPrefix` ou identifiant invalide à la lecture : `Ok(None)`,
//!    jamais une erreur. Une résolution est **une** requête sur **une** table, sans union
//!    (DSD `DEC-19`).
//! 4. **Écriture d'une définition** : [`identity::check_write`] d'abord (`Catalog` exige un
//!    admin, `ReservedPrefix` refusé), l'objet passe par sa validation `lymaille` avant tout
//!    `INSERT`/`UPDATE`, l'écriture est un upsert sur la clé de la table visée et réécrit les
//!    colonnes dérivées comme les hooks de `../definitions/`.
//! 5. **Traduction des refus** : un refus de l'hôte (`Forbidden`, `ReservedPrefix`,
//!    `UnknownSubject`, `InvalidDefinition`) est rendu à `lymaille` par la variante
//!    `Store(<Display de la LymailleError, code MRD-LYMAILLE-NNN compris>)` de l'erreur du
//!    trait ; un `DbErr` devient `Store([`db_text`])` — jamais un secret ni le contenu d'un
//!    objet dans ce texte.
//! 6. **Horodatages** : `created_at` et `updated_at` sont posés par le store à l'heure UTC
//!    courante.

pub mod catalog;
pub mod memory;
pub mod session;
pub mod toolset;

pub use catalog::{SeaAgentStore, SeaProfileStore, SeaProviderStore};
pub use memory::SeaMemoryStore;
pub use session::SeaSessionStore;
pub use toolset::{SeaContextStore, SeaContextTypeStore, SeaToolsetStore};

use sea_orm::DatabaseConnection;
use sea_orm::DbErr;

use super::identity::Identity;

/// Fabrique des stores du module, liée à un `subject` (DSD `DEC-7`) : jamais partagé entre deux
/// sujets, chaque store qu'elle rend est `Clone` et porte le même `subject`.
#[derive(Clone)]
pub struct LymailleStores {
    db: DatabaseConnection,
    identity: Identity,
    subject: String,
}

impl LymailleStores {
    /// Attache la fabrique à une connexion et à un `subject` de session (verbatim, claim `sub`).
    #[must_use]
    pub fn new(db: DatabaseConnection, subject: impl Into<String>) -> Self {
        let identity = Identity::new(db.clone());
        Self {
            db,
            identity,
            subject: subject.into(),
        }
    }

    /// Le `subject` auquel cette fabrique (et chaque store qu'elle rend) est liée.
    #[must_use]
    pub fn subject(&self) -> &str {
        &self.subject
    }

    /// Store des sessions, clé par `(subject, session_id)` (`./session.sdd`).
    #[must_use]
    pub fn sessions(&self) -> SeaSessionStore {
        SeaSessionStore::new(self.db.clone(), self.subject.clone())
    }

    /// Store des définitions d'agent, catalogue et privé (`./catalog.sdd`).
    #[must_use]
    pub fn agents(&self) -> SeaAgentStore {
        SeaAgentStore::new(self.db.clone(), self.identity.clone(), self.subject.clone())
    }

    /// Store des profils de modèle, table globale (`./catalog.sdd`).
    #[must_use]
    pub fn profiles(&self) -> SeaProfileStore {
        SeaProfileStore::new(self.db.clone(), self.identity.clone(), self.subject.clone())
    }

    /// Store des providers, table globale en lecture libre (`./catalog.sdd`).
    #[must_use]
    pub fn providers(&self) -> SeaProviderStore {
        SeaProviderStore::new(self.db.clone(), self.identity.clone(), self.subject.clone())
    }

    /// Store des toolsets, version active et brouillon (`./toolset.sdd`).
    #[must_use]
    pub fn toolsets(&self) -> SeaToolsetStore {
        SeaToolsetStore::new(self.db.clone(), self.identity.clone(), self.subject.clone())
    }

    /// Store des types de contexte, table globale interne (`./toolset.sdd`).
    #[must_use]
    pub fn context_types(&self) -> SeaContextTypeStore {
        SeaContextTypeStore::new(self.db.clone(), self.identity.clone(), self.subject.clone())
    }

    /// Store des contextes de toolset, table globale interne (`./toolset.sdd`).
    #[must_use]
    pub fn contexts(&self) -> SeaContextStore {
        SeaContextStore::new(self.db.clone(), self.identity.clone(), self.subject.clone())
    }

    /// Store des mémoires, clé par `(subject, portée, identifiant)` (`./memory.sdd`).
    #[must_use]
    pub fn memories(&self) -> SeaMemoryStore {
        SeaMemoryStore::new(self.db.clone(), self.subject.clone())
    }
}

/// Texte expurgé d'un `DbErr` pour la règle 5 : ni un secret, ni le contenu d'un objet. Le
/// `Display` de `SeaORM` 2.x/sqlx 0.9 ne rend que le message de la base — l'instruction et ses
/// paramètres liés n'apparaissent que dans le `Debug`. Seule réserve : une erreur de
/// décodage d'une colonne peut avoir sa cause (le texte serde) qui cite la valeur décodée —
/// elle est réduite à une phrase fixe.
///
/// La détection du décodage se fait par sonde sur le `Debug` de l'erreur : les variantes
/// de décodage du pilote sqlx (`ColumnDecode`, `Decode`) y laissent leur nom de variante
/// (`Debug` dérivé, `RuntimeErr`/`DbErr` le propagent). Aucun chemin d'import sqlx n'est écrit
/// ici à dessein : `sea_orm::sqlx` et la variante sqlx de `RuntimeErr` n'existent que
/// derrière la feature `sqlx-dep` de `sea-orm`, que notre `Cargo.toml` n'active pas — une
/// correspondance structurelle sur ces variantes rendrait ce fichier incompilable selon le
/// graphe de features. Angle mort assumé : un futur erreur de décodage dont le `Debug` ne
/// porterait aucun de ces marqueurs (autre moteur, renommage amont) passerait en pass-through
/// de son `Display` ; les erreurs de syntaxe `serde_json` ne citent pas la valeur brute,
/// l'exposition y est limitée. Une fausse correspondance (le texte d'une erreur quelconque
/// contenant ces marqueurs) réduit à la phrase fixe : c'est perdant en précision, jamais en
/// confidentialité.
pub(crate) fn db_text(error: &DbErr) -> String {
    let debug_probe = format!("{error:?}");
    if debug_probe.contains("ColumnDecode") || debug_probe.contains("Decode(") {
        "database failed to decode a stored column".to_owned()
    } else {
        error.to_string()
    }
}

/// Base de test des stores : SQLite mémoire ou PostgreSQL réel derrière `MIRYAD_TEST_PG_URL`,
/// migrations de l'hôte appliquées (`src/matrix/store/test_support.sdd`, même contrat pour le
/// module `lymaille`, dont le store n'exerce les règles que sur une base migrée).
#[cfg(test)]
pub(crate) mod test_support {
    use sea_orm::{ConnectOptions, ConnectionTrait, Database, DatabaseConnection};
    use sea_orm_migration::MigratorTrait as _;

    /// Variable d'environnement qui porte l'URL PostgreSQL de test (jamais affichée).
    const PG_URL_VAR: &str = "MIRYAD_TEST_PG_URL";

    /// URL PostgreSQL de test, si la variable est posée et non vide.
    fn pg_url() -> Option<String> {
        std::env::var(PG_URL_VAR).ok().filter(|value| !value.is_empty())
    }

    /// Base neuve et migrée (migrations de l'hôte, `lymaille` comprises sous la feature).
    ///
    /// Sans `MIRYAD_TEST_PG_URL` : SQLite mémoire, une seule connexion (sinon chaque connexion
    /// du pool verrait sa propre base). Avec : un schéma neuf `t_<hex>` sur la base PostgreSQL,
    /// et un pool dont le `search_path` est ce schéma. Les messages d'échec ne citent jamais
    /// l'URL (elle porte un mot de passe).
    pub(crate) async fn test_db() -> DatabaseConnection {
        let db = match pg_url() {
            None => {
                let mut opts = ConnectOptions::new("sqlite::memory:");
                opts.max_connections(1).min_connections(1);
                Database::connect(opts)
                    .await
                    .expect("base SQLite mémoire de test : connexion impossible")
            }
            Some(url) => pg_schema_db(url).await,
        };
        crate::migration::Migrator::up(&db, None)
            .await
            .expect("migrations de l'hôte appliquées sur la base de test");
        db
    }

    /// Crée un schéma `t_<16 hex>` via une connexion d'amorçage, puis rend un pool dont le
    /// `search_path` est ce schéma. La base de test est jetable : les schémas créés restent.
    async fn pg_schema_db(url: String) -> DatabaseConnection {
        let scrub = |what: &str, err: sea_orm::DbErr| -> String {
            format!(
                "PostgreSQL de test : {what} : {}",
                err.to_string().replace(url.as_str(), "<url>")
            )
        };
        let schema = format!("t_{:016x}", rand::random::<u64>());
        let boot = Database::connect(url.as_str())
            .await
            .unwrap_or_else(|err| panic!("{}", scrub("connexion d'amorçage impossible", err)));
        boot.execute_unprepared(&format!("CREATE SCHEMA \"{schema}\""))
            .await
            .unwrap_or_else(|err| panic!("{}", scrub("création du schéma de test impossible", err)));
        boot.close()
            .await
            .unwrap_or_else(|err| panic!("{}", scrub("fermeture de l'amorçage impossible", err)));
        let mut opts = ConnectOptions::new(url.clone());
        opts.set_schema_search_path(schema);
        Database::connect(opts)
            .await
            .unwrap_or_else(|err| panic!("{}", scrub("pool sur le schéma de test impossible", err)))
    }
}

#[cfg(test)]
mod tests {
    use super::LymailleStores;
    use super::test_support::test_db;
    use crate::users::membership::{self, sync_group_memberships};
    use crate::users::resolve_user;
    use ::lymaille::AgentDefinition;
    use ::lymaille::AgentKind;
    use ::lymaille::AgentStore as _;
    use sea_orm::ConnectionTrait as _;
    use sea_orm::DatabaseConnection;
    use sea_orm::entity::prelude::*;

    /// Compte local de test, membre ou non du groupe `admin`.
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

    /// Retrait de toutes les appartenances d'un compte (fixture : le chemin de production des
    /// appartenances est `sync_group_memberships`).
    async fn strip_memberships(db: &DatabaseConnection, subject: &str) {
        let account = crate::users::user::Entity::find()
            .filter(crate::users::user::Column::Subject.eq(subject))
            .one(db)
            .await
            .expect("compte local retrouvé")
            .expect("le compte local existe");
        membership::Entity::delete_many()
            .filter(membership::Column::UserId.eq(account.id))
            .exec(db)
            .await
            .expect("appartenances retirées");
    }

    fn agent(id: &str) -> AgentDefinition {
        AgentDefinition::new(
            id.to_string(),
            AgentKind::Work,
            "prompt de test".to_string(),
            "qwen".to_string(),
        )
    }

    /// Les huit accesseurs rendent bien les huit types de store (`Exposes`).
    #[tokio::test]
    async fn the_factory_renders_the_eight_stores() {
        let stores = LymailleStores::new(test_db().await, "alice");
        let _: super::SeaSessionStore = stores.sessions();
        let _: super::SeaAgentStore = stores.agents();
        let _: super::SeaProfileStore = stores.profiles();
        let _: super::SeaProviderStore = stores.providers();
        let _: super::SeaToolsetStore = stores.toolsets();
        let _: super::SeaContextTypeStore = stores.context_types();
        let _: super::SeaContextStore = stores.contexts();
        let _: super::SeaMemoryStore = stores.memories();
        assert_eq!(stores.subject(), "alice", "`subject()` rend le sujet verbatim");
        let clone = stores.clone();
        assert_eq!(clone.subject(), "alice", "la fabrique est `Clone`");
    }

    /// `Scenario` « deux sujets ne se voient pas, le catalogue est commun » : la ligne privée
    /// d'`alice` est invisible à `bob`, la ligne `G_y` du catalogue écrite par un admin est
    /// lisible des deux.
    #[tokio::test]
    async fn two_subjects_are_separated_and_the_catalog_is_shared() {
        let db = test_db().await;
        user(&db, "alice", false).await;
        user(&db, "bob", false).await;
        user(&db, "root", true).await;

        LymailleStores::new(db.clone(), "alice")
            .agents()
            .put_agent(&agent("x"))
            .await
            .expect("alice écrit son agent privé `x`");
        LymailleStores::new(db.clone(), "root")
            .agents()
            .put_agent(&agent("G_y"))
            .await
            .expect("l'admin écrit `G_y` au catalogue");

        let bob = LymailleStores::new(db.clone(), "bob").agents();
        assert!(
            matches!(bob.get_agent("x").await, Ok(None)),
            "`bob` ne lit pas le privé d'une autre : {:?}",
            bob.get_agent("x").await
        );
        assert_eq!(
            bob.get_agent("G_y").await.expect("le catalogue se lit"),
            Some(agent("G_y")),
            "`G_y` est la définition du catalogue"
        );
        assert_eq!(
            LymailleStores::new(db.clone(), "alice")
                .agents()
                .get_agent("G_y")
                .await
                .expect("le catalogue se lit"),
            Some(agent("G_y")),
            "le catalogue est commun"
        );
    }

    /// `Scenario` « un identifiant de casse réservée n'est ni lu ni écrit » : la lecture rend
    /// `Ok(None)` (règle 3, jamais une erreur), l'écriture est un refus dont le texte cite
    /// `MRD-LYMAILLE-003` (règle 5).
    #[tokio::test]
    async fn the_reserved_case_is_neither_read_nor_written() {
        let db = test_db().await;
        user(&db, "bob", false).await;
        let agents = LymailleStores::new(db.clone(), "bob").agents();

        assert!(
            matches!(agents.get_agent("g_x").await, Ok(None)),
            "la lecture de `g_x` rend `Ok(None)` : {:?}",
            agents.get_agent("g_x").await
        );
        let refused = agents
            .put_agent(&agent("g_x"))
            .await
            .expect_err("l'écriture de `g_x` est refusée");
        let text = refused.to_string();
        assert!(
            text.contains("MRD-LYMAILLE-003"),
            "le refus de préfixe réservé cite le code MRD (règle 5) : {text}"
        );
    }

    /// `Scenario` « l'écriture au catalogue exige un admin au moment de l'appel » (DSD `AC-5`) :
    /// retirée du groupe admin entre les deux appels, `alice` est refusée au second — le
    /// principal n'est jamais mis en cache (règle 2).
    #[tokio::test]
    async fn the_catalog_write_requires_admin_at_call_time() {
        let db = test_db().await;
        user(&db, "alice", true).await;
        let agents = LymailleStores::new(db.clone(), "alice").agents();

        agents
            .put_agent(&agent("G_a"))
            .await
            .expect("admin, alice écrit `G_a` au catalogue");

        strip_memberships(&db, "alice").await;

        let refused = agents
            .put_agent(&agent("G_b"))
            .await
            .expect_err("non-admin au second appel, l'écriture est refusée");
        let text = refused.to_string();
        assert!(
            text.contains("MRD-LYMAILLE-002"),
            "le refus cite le code MRD (règle 5) : {text}"
        );
    }

    /// `Scenario` « un subject sans compte est refusé » (DSD `DEC-3`) : lecture d'agent par
    /// `ghost` → variante `Store` citant `MRD-LYMAILLE-001`, aucune ligne créée.
    #[tokio::test]
    async fn a_subject_without_account_is_refused() {
        let db = test_db().await;
        let agents = LymailleStores::new(db.clone(), "ghost").agents();

        let refused = agents
            .get_agent("x")
            .await
            .expect_err("aucun compte derrière `ghost`");
        let text = refused.to_string();
        assert!(
            text.contains("MRD-LYMAILLE-001"),
            "`UnknownSubject` traverse en `Store` avec son code : {text}"
        );
        let rows = crate::users::user::Entity::find()
            .all(&db)
            .await
            .expect("relecture de miryad_users");
        assert!(rows.is_empty(), "la résolution ne crée aucun compte : {rows:?}");
    }

    /// Règle 5, branche « erreur de décodage » de `db_text` : une colonne JSON corrompue en base
    /// est rendue à `lymaille` par la phrase fixe — ni le texte serde, ni la valeur de la colonne
    /// (le secret ci-dessous) ne doivent apparaître dans l'erreur.
    ///
    /// Exercé sur SQLite seulement : la colonne `definition` y est un `TEXT` (affinité SQLite,
    /// pas de contrôle JSON) que l'on peut corrompre par un `UPDATE` brut ; PostgreSQL stocke un
    /// vrai `json` et refuserait la corruption au `UPDATE`, il n'y a donc là-bas aucun
    /// `ColumnDecode` à exercer — le tester sur PG serait un faux test.
    #[tokio::test]
    async fn a_corrupt_json_column_is_reduced_to_a_fixed_sentence() {
        let db = test_db().await;
        if !matches!(db.get_database_backend(), sea_orm::DatabaseBackend::Sqlite) {
            return;
        }
        user(&db, "root", true).await;
        let agents = LymailleStores::new(db.clone(), "root").agents();
        agents
            .put_agent(&agent("G_x"))
            .await
            .expect("la fixture écrit `G_x` au catalogue");
        db.execute_unprepared(
            "UPDATE miryad_lymaille_catalog_agents SET definition = '{\"prompt\":\"SECRET-dans-la-colonne\"'",
        )
        .await
        .expect("SQLite accepte la colonne corrompue (JSON tronqué, pas de contrôle)");

        let refused = agents
            .get_agent("G_x")
            .await
            .expect_err("la colonne corrompue ne se décode pas");
        let text = refused.to_string();
        assert!(
            text.ends_with("database failed to decode a stored column"),
            "le décodage raté est réduit à la phrase fixe, préfixe de `Store` compris (règle 5) : {text}"
        );
        assert!(
            !text.contains("SECRET"),
            "la valeur de la colonne ne fuit pas dans l'erreur : {text}"
        );
    }
}
