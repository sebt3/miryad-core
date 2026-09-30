//! Entité SeaORM de la table `miryad_groups`, `ensure_group` en get-or-create par nom exact et
//! lectures d'appartenance `is_admin`/`is_member` : la seule porte d'entrée du groupe dans le
//! moteur. `admin` est une pure convention de nom (`ADMIN_GROUP_NAME`), rien de spécial au
//! schéma ; les écritures de `miryad_groups` sont le monopole de `ensure_group`.

use chrono::Utc;
use sea_orm::entity::prelude::*;
use sea_orm::{ConnectionTrait, Set};

use crate::users::membership;

/// Nom du groupe admin, pré-câblé (seedé par migration) mais sans rien de spécial au niveau
/// schéma — juste la convention lue par l'évaluateur RBAC (`rbac::is_admin`).
pub const ADMIN_GROUP_NAME: &str = "admin";

/// Ligne `DeriveEntityModel` de la table `miryad_groups` (posée par la migration
/// `m20260822_000002`, groupe `admin` seedé par `m20260822_000003`) — trois colonnes.
#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "miryad_groups")]
pub struct Model {
    /// Clé primaire `i32` auto-incrémentée — identifiant interne référencé par
    /// `miryad_group_memberships.group_id`.
    #[sea_orm(primary_key)]
    pub id: i32,
    /// Nom du groupe, contrainte `UNIQUE` — lu par correspondance exacte (`ensure_group`,
    /// `is_member`), sans registre de noms autorisés.
    #[sea_orm(unique)]
    pub name: String,
    /// Horodatage de création : posé par l'appelant à l'`insert` (aucun défaut côté serveur).
    pub created_at: DateTimeUtc,
}

/// `DeriveRelation` déclaré avec un enum vide : aucune relation `SeaORM` malgré les `FK` du
/// schéma — les jointures se font par filtres explicites, donc `DeriveRelatedEntity` (et
/// `Seaography`) ne verra jamais de relation depuis ce modèle.
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

/// Alias lisible de l'`Entity` généré par `DeriveEntityModel` — même type ; mis à plat sous
/// `crate::users::Group`.
pub type Group = Entity;

/// Get-or-create par nom — un groupe cité dans un claim `groups` mais jamais vu est créé à la
/// volée (pas de registre préalable des noms de groupe autorisés).
///
/// # Errors
///
/// Propage par `?` le `sea_orm::DbErr` de chacun des SELECT et de l'`INSERT` : table absente
/// (base non migrée), connexion invalide, erreur serveur — jamais converti en `Ok(false)`,
/// jamais wrappé en type propre, aucun code `MRD-*` posé ici (translation par les surfaces en
/// aval). Quand l'insertion échoue et que la relance ne trouve aucune ligne, le `DbErr` d'origine
/// de l'`INSERT` remonte verbatim (arbitré 2026-09-29) ; quand la relance trouve la ligne,
/// l'appelant voit l'`id` du gagnant.
pub async fn ensure_group<C: ConnectionTrait>(db: &C, name: &str) -> Result<i32, DbErr> {
    if let Some(existing) = Entity::find().filter(Column::Name.eq(name)).one(db).await? {
        return Ok(existing.id);
    }

    let active = ActiveModel {
        name: Set(name.to_string()),
        created_at: Set(Utc::now()),
        ..Default::default()
    };

    match active.insert(db).await {
        Ok(model) => Ok(model.id),
        Err(insert_err) => {
            tracing::debug!(
                name,
                "group get-or-create: INSERT failed, re-reading by name: {insert_err}"
            );
            Entity::find()
                .filter(Column::Name.eq(name))
                .one(db)
                .await?
                .map(|group| group.id)
                .ok_or(insert_err)
        }
    }
}

/// # Errors
///
/// Propage le `sea_orm::DbErr` du SELECT délégué à `is_member` (table absente, connexion
/// invalide, erreur serveur), sans code `MRD-*` posé ici. Absence d'appartenance : `Ok(false)`,
/// pas une erreur.
pub async fn is_admin<C: ConnectionTrait>(db: &C, user_id: i32) -> Result<bool, DbErr> {
    is_member(db, user_id, ADMIN_GROUP_NAME).await
}

/// # Errors
///
/// Propage par `?` le `sea_orm::DbErr` des SELECT (table absente, connexion invalide, erreur
/// serveur), sans code `MRD-*` posé ici. Groupe inconnu ou appartenance absente : `Ok(false)`,
/// pas une erreur — aucune ligne créée.
pub async fn is_member<C: ConnectionTrait>(db: &C, user_id: i32, group_name: &str) -> Result<bool, DbErr> {
    let Some(group) = Entity::find().filter(Column::Name.eq(group_name)).one(db).await? else {
        return Ok(false);
    };

    let exists = membership::Entity::find()
        .filter(membership::Column::UserId.eq(user_id))
        .filter(membership::Column::GroupId.eq(group.id))
        .one(db)
        .await?
        .is_some();
    Ok(exists)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migration::Migrator;
    use crate::users::membership::sync_group_memberships;
    use crate::users::user::resolve_user;
    use sea_orm::{DbBackend, MockDatabase, RuntimeErr};
    use sea_orm_migration::MigratorTrait;
    use std::sync::{Arc, Mutex};

    async fn test_db() -> DatabaseConnection {
        let db = sea_orm::Database::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite connects");
        Migrator::up(&db, None).await.expect("migrations apply cleanly");
        db
    }

    /// Capture des traces `tracing` émises par cette crate (`target` préfixé `miryad_core`),
    /// pattern de `auth/oidc.rs` — `tracing-subscriber` en dev-dependency. Le `DefaultGuard`
    /// est thread-local : sans effet sur les tests voisins. Le niveau est capturé pour
    /// verrouiller la clause « trace `debug` au rebond » du `Must` (arbitré 2026-09-29).
    #[derive(Clone, Default)]
    struct CapturedTraces(Arc<Mutex<Vec<(tracing::Level, String)>>>);

    impl CapturedTraces {
        fn lines(&self) -> Vec<(tracing::Level, String)> {
            self.0.lock().expect("test mutex is not poisoned").clone()
        }
    }

    struct CaptureLayer(Arc<Mutex<Vec<(tracing::Level, String)>>>);

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for CaptureLayer {
        fn on_event(&self, event: &tracing::Event<'_>, _ctx: tracing_subscriber::layer::Context<'_, S>) {
            if !event.metadata().target().starts_with("miryad_core") {
                return;
            }
            let mut visitor = MessageVisitor::default();
            event.record(&mut visitor);
            self.0
                .lock()
                .expect("test mutex is not poisoned")
                .push((*event.metadata().level(), visitor.finish()));
        }
    }

    #[derive(Default)]
    struct MessageVisitor {
        rendered: String,
    }

    impl MessageVisitor {
        fn finish(&self) -> String {
            self.rendered.clone()
        }
    }

    impl tracing::field::Visit for MessageVisitor {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            use std::fmt::Write as _;

            if !self.rendered.is_empty() {
                self.rendered.push(' ');
            }
            let _ = write!(&mut self.rendered, "{}={value:?}", field.name());
        }
    }

    /// Garde d'intérêt globale contre le flakiness de la capture (fix tâche F) — miroir
    /// exact de celle de `./user.rs` : le `Interest` d'un callsite est un atomique GLOBAL
    /// dans `tracing-core` 0.1.36, recalculé à chaque enregistrement de `Dispatch`, avec un
    /// raccourci `has_just_one` : quand un seul dispatcher est vivant, le premier
    /// déclenchement d'un callsite évalue son intérêt avec le défaut du thread émetteur —
    /// sans notre `set_default`, le `NoSubscriber` global répond `Interest::never()`, ce
    /// `never` est mémorisé sur le callsite et le macro `debug!` court-circuite avant même
    /// de consulter notre souscripteur thread-local : la capture reste vide (`[]`).
    /// L'émetteur empoisonneur est tout test déclenchant le rebond SANS capture (le jumeau
    /// `*_insert_failure_*` de ce fichier, les courses de `./membership.rs`). Deux
    /// `Dispatch` fuités à vie (deux pour que `has_just_one` retombe définitivement à
    /// `false` après le `retain` des entrées mortes) sur un souscripteur qui répond
    /// toujours `sometimes` garantissent que tout recalcul croise au moins un dispatcher
    /// vivant non-`never` ; la création du gardien reconstruit aussi l'intérêt des
    /// callsites déjà empoisonnés. Jamais posé comme défaut, il ne reçoit aucun événement
    /// et ne change le contrat d'aucun autre test.
    #[derive(Debug)]
    struct InterestGuard;

    impl tracing::Subscriber for InterestGuard {
        fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
            false
        }

        fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }

        fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}

        fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}

        fn event(&self, _event: &tracing::Event<'_>) {}

        fn enter(&self, _span: &tracing::span::Id) {}

        fn exit(&self, _span: &tracing::span::Id) {}

        fn register_callsite(&self, _metadata: &tracing::Metadata<'_>) -> tracing::subscriber::Interest {
            tracing::subscriber::Interest::sometimes()
        }
    }

    /// Pose le gardien d'intérêt (idempotent, une seule création par processus) ;
    /// statique jamais droppée, sans effet hors le cache d'intérêt global.
    fn install_interest_guard() {
        static GUARD: std::sync::OnceLock<[tracing::dispatcher::Dispatch; 2]> = std::sync::OnceLock::new();
        GUARD.get_or_init(|| {
            [
                tracing::dispatcher::Dispatch::new(InterestGuard),
                tracing::dispatcher::Dispatch::new(InterestGuard),
            ]
        });
    }

    fn capture_traces() -> (tracing::subscriber::DefaultGuard, CapturedTraces) {
        use tracing_subscriber::layer::SubscriberExt as _;

        install_interest_guard();
        let captured = CapturedTraces::default();
        let subscriber =
            tracing_subscriber::registry::Registry::default().with(CaptureLayer(Arc::clone(&captured.0)));
        let guard = tracing::subscriber::set_default(subscriber);
        (guard, captured)
    }

    /// Erreur `E` arbitraire, miroir de celle de `./user.rs` (le contrat du rebond d'`INSERT`
    /// y est formulé en `Must`/`Raises`, `./group.sdd` ne lui consacre pas de `Scenario`),
    /// clairement distincte de tout `DbErr::RecordNotFound`.
    fn insert_outage() -> DbErr {
        DbErr::Query(RuntimeErr::Internal("simulated INSERT outage E".to_string()))
    }

    /// `MockDatabase` du contrat du rebond : la première lecture ne rend aucune ligne,
    /// l'`INSERT` échoue de l'erreur `E` (sous la feature par défaut
    /// `sqlite-use-returning-for-3_35`, l'`insert` passe par un `SELECT`/`RETURNING` qui
    /// consomme une entrée de `query_results` — vérifié dans `executor/insert.rs` de
    /// sea-orm 2.0.2) et la relance ne rend aucune ligne non plus.
    fn empty_insert_outage_db() -> DatabaseConnection {
        MockDatabase::new(DbBackend::Sqlite)
            .append_query_results::<Model, _, _>([[]])
            .append_query_errors([insert_outage()])
            .append_query_results::<Model, _, _>([[]])
            .into_connection()
    }

    #[tokio::test]
    async fn admin_group_is_seeded_by_migration() {
        let db = test_db().await;
        let admin_group = Entity::find()
            .filter(Column::Name.eq(ADMIN_GROUP_NAME))
            .one(&db)
            .await
            .expect("query succeeds");
        assert!(admin_group.is_some());
    }

    #[tokio::test]
    async fn is_member_true_for_member_false_otherwise() {
        let db = test_db().await;
        let user = resolve_user(&db, "sub-1", None).await.expect("resolve succeeds");
        sync_group_memberships(&db, user.id, &["editors".to_string()])
            .await
            .expect("sync succeeds");

        assert!(is_member(&db, user.id, "editors").await.expect("query succeeds"));
        assert!(!is_member(&db, user.id, "admin").await.expect("query succeeds"));
    }

    #[tokio::test]
    async fn is_member_false_for_unknown_group() {
        let db = test_db().await;
        let user = resolve_user(&db, "sub-1", None).await.expect("resolve succeeds");
        assert!(
            !is_member(&db, user.id, "does-not-exist")
                .await
                .expect("query succeeds")
        );
    }

    /// Contrat arbitré 2026-09-29 (`Must`/`Raises` de `./group.sdd`, miroir du `Scenario`
    /// « insertion en échec sans ligne trouvée » de `./user.sdd`) : quand l'`INSERT` échoue et
    /// que la relance ne trouve aucune ligne, le `DbErr` d'origine de l'`INSERT` remonte
    /// verbatim — le `DbErr::RecordNotFound` « vanished » est supprimé.
    #[tokio::test]
    async fn ensure_group_insert_failure_with_empty_reread_propagates_original_dberr() {
        let db = empty_insert_outage_db();

        let result = ensure_group(&db, "ghost-group").await;

        assert!(
            matches!(
                &result,
                Err(DbErr::Query(RuntimeErr::Internal(msg))) if msg == "simulated INSERT outage E"
            ),
            "le DbErr d'origine de l'`INSERT` doit être propagé verbatim, aucun \
             `DbErr::RecordNotFound` « vanished » ne doit être fabriqué : {result:?}"
        );
    }

    /// `Must` (arbitré 2026-09-29) : une trace `tracing` `debug` est émise au rebond, et une
    /// course normale ne se trace jamais `warn`.
    #[tokio::test]
    async fn ensure_group_insert_bounce_emits_debug_trace_never_warn() {
        let db = empty_insert_outage_db();
        let (_guard, traces) = capture_traces();

        let result = ensure_group(&db, "ghost-group").await;
        assert!(result.is_err(), "fixture de panne : l'`INSERT` doit échouer");

        let lines = traces.lines();
        assert!(
            lines
                .iter()
                .any(|(level, line)| *level == tracing::Level::DEBUG && line.contains("ghost-group")),
            "`Must` (arbitré 2026-09-29) : le rebond doit émettre une trace `debug` citant le \
             nom du groupe : {lines:?}"
        );
        assert!(
            !lines
                .iter()
                .any(|(level, _)| matches!(*level, tracing::Level::WARN | tracing::Level::ERROR)),
            "course normale : le rebond ne doit jamais être tracé `warn` ou `error` : {lines:?}"
        );
    }

    /// Nombre de lignes de `miryad_groups`, pour les assertions « la lecture ne crée rien ».
    async fn group_row_count(db: &DatabaseConnection) -> usize {
        Entity::find().all(db).await.expect("query succeeds").len()
    }

    /// `Scenario` : « `ensure_group` sur un nom inconnu le crée et retourne son id ».
    #[tokio::test]
    async fn ensure_group_creates_unknown_group() {
        let db = test_db().await;
        assert!(
            Entity::find()
                .filter(Column::Name.eq("auditors"))
                .one(&db)
                .await
                .expect("query succeeds")
                .is_none(),
            "GIVEN : `auditors` n'existe pas après migration"
        );
        assert_eq!(
            membership::Entity::find()
                .all(&db)
                .await
                .expect("query succeeds")
                .len(),
            0,
            "GIVEN : `miryad_group_memberships` vide"
        );

        let id = ensure_group(&db, "auditors")
            .await
            .expect("le groupe inconnu est créé à la volée");
        assert!(id > 0, "`id` posé par l'auto-incrément");

        let row = Entity::find()
            .filter(Column::Name.eq("auditors"))
            .one(&db)
            .await
            .expect("query succeeds")
            .expect("une ligne `auditors` existe");
        assert_eq!(row.id, id);
        assert!(
            row.created_at.timestamp() > 0,
            "`created_at` posé par le fichier (colonne NOT NULL au schéma)"
        );
        assert_eq!(
            Entity::find()
                .filter(Column::Name.eq("auditors"))
                .all(&db)
                .await
                .expect("query succeeds")
                .len(),
            1,
            "une seule ligne `auditors`"
        );
        assert_eq!(
            membership::Entity::find()
                .all(&db)
                .await
                .expect("query succeeds")
                .len(),
            0,
            "`ensure_group` ne rattache personne : `miryad_group_memberships` reste vide"
        );
    }

    /// `Scenario` : « `ensure_group` sur un nom existant retourne le même id sans second insert ».
    #[tokio::test]
    async fn ensure_group_reuses_existing_group() {
        let db = test_db().await;
        let first = ensure_group(&db, "auditors")
            .await
            .expect("premier appel crée le groupe");

        let second = ensure_group(&db, "auditors")
            .await
            .expect("second appel réutilise la ligne");

        assert_eq!(first, second, "le premier SELECT court-circuite tout insert");
        assert_eq!(
            Entity::find()
                .filter(Column::Name.eq("auditors"))
                .all(&db)
                .await
                .expect("query succeeds")
                .len(),
            1,
            "la table ne contient toujours qu'une ligne `auditors`"
        );
    }

    /// `Scenario` : « Deux `ensure_group` concurrents convergent vers une ligne unique » —
    /// pattern `tokio::join!` du test de course de `./membership.rs` : le perdant heurte la
    /// contrainte `UNIQUE`, son rebond de `SELECT` lui rend l'`id` du gagnant.
    #[tokio::test]
    async fn concurrent_ensure_group_converges_to_single_row() {
        let db = test_db().await;
        let (first, second) = tokio::join!(ensure_group(&db, "ops"), ensure_group(&db, "ops"));
        let first = first.expect("premier appel concurrent rend Ok");
        let second = second.expect("second appel concurrent rend Ok (rebond du perdant)");

        assert_eq!(first, second, "les deux retournent le même `id`");
        assert_eq!(
            Entity::find()
                .filter(Column::Name.eq("ops"))
                .all(&db)
                .await
                .expect("query succeeds")
                .len(),
            1,
            "`miryad_groups` ne contient qu'une seule ligne `ops`"
        );
    }

    /// `Scenario` : « `is_admin` vrai pour un membre du groupe `admin` ».
    #[tokio::test]
    async fn is_admin_true_for_admin_member() {
        let db = test_db().await;
        let user = resolve_user(&db, "admin-user", None)
            .await
            .expect("resolve succeeds");
        sync_group_memberships(&db, user.id, &["admin".to_string()])
            .await
            .expect("sync succeeds");

        assert!(
            is_admin(&db, user.id).await.expect("query succeeds"),
            "membership (user, groupe `admin` seedé) trouvée par les deux SELECT de `is_member`"
        );
    }

    /// `Scenario` : « `is_admin` faux sans membership `admin`, même membre d'autres groupes ».
    #[tokio::test]
    async fn is_admin_false_without_admin_membership() {
        let db = test_db().await;
        let editor = resolve_user(&db, "editor-user", None)
            .await
            .expect("resolve succeeds");
        sync_group_memberships(&db, editor.id, &["editors".to_string()])
            .await
            .expect("le groupe `editors` est créé au passage");
        let outsider = resolve_user(&db, "unsynced-user", None)
            .await
            .expect("resolve succeeds");

        assert!(
            !is_admin(&db, editor.id).await.expect("query succeeds"),
            "être membre de n'importe quel autre groupe ne confère rien"
        );
        assert!(
            !is_admin(&db, outsider.id).await.expect("query succeeds"),
            "aucune sync, aucune appartenance, jamais admin"
        );
    }

    /// `Scenario` : « Groupe inconnu : faux sans créer le groupe » — volet absence de création,
    /// que le test gelé `is_member_false_for_unknown_group` n'assertionne pas (lecture `Handles`
    /// de `../rbac.sdd` confirmée en source).
    #[tokio::test]
    async fn is_member_unknown_group_creates_no_row() {
        let db = test_db().await;
        let user = resolve_user(&db, "reader-user", None)
            .await
            .expect("resolve succeeds");
        let rows_before = group_row_count(&db).await;

        assert!(
            !is_member(&db, user.id, "does-not-exist")
                .await
                .expect("le groupe absent est un `Ok(false)`, pas une erreur"),
            "premier SELECT vide, raccourci immédiat"
        );

        assert_eq!(
            group_row_count(&db).await,
            rows_before,
            "la lecture d'autorisation ne crée jamais le groupe cité"
        );
        assert!(
            Entity::find()
                .filter(Column::Name.eq("does-not-exist"))
                .one(&db)
                .await
                .expect("query succeeds")
                .is_none(),
            "aucune ligne `does-not-exist` n'est apparue dans `miryad_groups`"
        );
    }

    /// `Scenario` : « Utilisateur jamais vu : faux pour tout groupe, sans erreur » — la garantie
    /// anti-orphelin est la FK du schéma, pas le code.
    #[tokio::test]
    async fn is_member_false_for_unknown_user_id() {
        let db = test_db().await;
        let unknown_user_id = 9999;

        assert!(
            !is_member(&db, unknown_user_id, "admin")
                .await
                .expect("aucun DbErr pour un `id` orphelin"),
            "`is_member` ne vérifie pas l'existence de l'utilisateur"
        );
        assert!(
            !is_admin(&db, unknown_user_id).await.expect("aucun DbErr"),
            "`is_admin` délègue à `is_member`, même réponse"
        );
    }

    /// `Scenario` : « Nom exact et casse : `Admin` n'est pas `admin` » — égalité SQL binaire,
    /// vérifiée sur `SQLite` (backend des tests), dérive Postgres tracée en `Tasks`.
    #[tokio::test]
    async fn group_names_are_case_sensitive_on_test_backend() {
        let db = test_db().await;
        let admin_id = Entity::find()
            .filter(Column::Name.eq(ADMIN_GROUP_NAME))
            .one(&db)
            .await
            .expect("query succeeds")
            .expect("le groupe seedé `admin` existe")
            .id;

        let capital_id = ensure_group(&db, "Admin")
            .await
            .expect("`Admin` est un nom inconnu, pas une variante du seed");
        assert_ne!(capital_id, admin_id, "aucune normalisation de casse");

        let user = resolve_user(&db, "capital-user", None)
            .await
            .expect("resolve succeeds");
        sync_group_memberships(&db, user.id, &["Admin".to_string()])
            .await
            .expect("le user est rattaché à `Admin` seulement");

        assert_eq!(group_row_count(&db).await, 2, "`admin` et `Admin` coexistent");
        assert!(
            !is_member(&db, user.id, "admin").await.expect("query succeeds"),
            "membre de `Admin`, pas de `admin`"
        );
        assert!(
            !is_admin(&db, user.id).await.expect("query succeeds"),
            "la convention admin se lit par le nom exact, `Admin` ne confère rien"
        );
    }

    /// `Scenario` : « Base non migrée : `Err` propagé sur les trois helpers, jamais `Ok(false)` »
    /// — fail-fast, aucune panne d'infrastructure dégradée en refus silencieux.
    #[tokio::test]
    async fn group_helpers_propagate_dberr_when_tables_missing() {
        let db = sea_orm::Database::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite connects (base non migrée)");

        assert!(
            ensure_group(&db, "x").await.is_err(),
            "`ensure_group` : le premier SELECT échoue et remonte par `?`"
        );
        let member = is_member(&db, 1, "admin").await;
        assert!(
            member.is_err(),
            "`is_member` ne dégrade pas la panne en `Ok(false)` : {member:?}"
        );
        let admin = is_admin(&db, 1).await;
        assert!(
            admin.is_err(),
            "`is_admin` propage le `DbErr` délégué, jamais `Ok(false)` : {admin:?}"
        );
    }
}
