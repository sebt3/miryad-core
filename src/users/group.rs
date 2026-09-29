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

    fn capture_traces() -> (tracing::subscriber::DefaultGuard, CapturedTraces) {
        use tracing_subscriber::layer::SubscriberExt as _;

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
}
