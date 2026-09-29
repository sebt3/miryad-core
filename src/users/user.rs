use chrono::Utc;
use sea_orm::entity::prelude::*;
use sea_orm::{ConnectionTrait, Set};

/// Ligne `DeriveEntityModel` de la table `miryad_users` (posée par la migration
/// `m20260822_000002`) — cinq colonnes, un utilisateur `OIDC` ou de service vu par la crate.
#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "miryad_users")]
pub struct Model {
    /// Clé primaire `i32` auto-incrémentée — identifiant interne stable des appartenances et
    /// du `RBAC`, dérivé du `subject`.
    #[sea_orm(primary_key)]
    pub id: i32,
    /// Claim `sub` OIDC — lien avec `AuthPrincipal.subject` (feature 2b).
    #[sea_orm(unique)]
    pub subject: String,
    /// Nullable, sans contrainte de format — snapshot de la première vue, jamais rafraîchi
    /// depuis (get-or-create, pas `upsert`).
    pub email: Option<String>,
    /// Nullable, colonne réservée en attente de feature : créée à `NULL`, jamais écrite par la
    /// crate.
    pub display_name: Option<String>,
    /// Horodatage de création : `Utc::now` de l'application à l'`insert` (aucun défaut côté
    /// serveur), figé ensuite.
    pub created_at: DateTimeUtc,
}

/// `DeriveRelation` déclaré avec un enum vide : l'appartenance aux groupes ne se lit que par
/// requête explicite (`membership`), le graphe d'entités `SeaORM` (et donc `Seaography`) n'en
/// voit aucune.
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

/// Alias lisible de l'`Entity` généré par `DeriveEntityModel` — même type, pas une entité
/// distincte ; mis à plat sous `crate::users::User`.
pub type User = Entity;

/// Get-or-create par `subject`. Pas de vraie contrainte `ON CONFLICT` portable entre `SQLite`/
/// `Postgres` exploitée ici — en cas de course (deux premiers logins concurrents du même
/// `subject`), l'`insert` échoue sur la contrainte unique et on retombe sur un `find` pour
/// récupérer la ligne posée par l'autre requête, plutôt que de propager l'erreur.
///
/// # Errors
///
/// Aucun code `MRD-*` porté ici — les deux lectures propagent le `sea_orm::DbErr` brut par `?`,
/// la décoration en code `MRD-*` est un contrat des appelants (`MRD-AUTH-016` côté callback,
/// `MRD-REST-003` côté REST). Quand l'insertion échoue et que la relecture de rebond ne trouve
/// toujours aucune ligne, le `DbErr` de l'`INSERT` d'origine est propagé verbatim, sans code
/// `MRD-*` (arbitré 2026-09-29 : plus aucun `DbErr::RecordNotFound` synthétisé « vanished »).
pub async fn resolve_user<C: ConnectionTrait>(
    db: &C,
    subject: &str,
    email: Option<&str>,
) -> Result<Model, DbErr> {
    if let Some(existing) = Entity::find().filter(Column::Subject.eq(subject)).one(db).await? {
        return Ok(existing);
    }

    let active = ActiveModel {
        subject: Set(subject.to_string()),
        email: Set(email.map(str::to_string)),
        display_name: Set(None),
        created_at: Set(Utc::now()),
        ..Default::default()
    };

    match active.insert(db).await {
        Ok(model) => Ok(model),
        Err(insert_err) => {
            tracing::debug!(
                subject,
                "user get-or-create: INSERT failed, re-reading by subject: {insert_err}"
            );
            Entity::find()
                .filter(Column::Subject.eq(subject))
                .one(db)
                .await?
                .ok_or(insert_err)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migration::Migrator;
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
    /// verrouiller la clause « jamais `warn` » du `Must` (arbitré 2026-09-29).
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

    /// Garde d'intérêt globale contre le flakiness de la capture (fix tâche F) — le
    /// `Interest` d'un callsite est un atomique GLOBAL dans `tracing-core`, recalculé à
    /// chaque enregistrement de `Dispatch`, avec un raccourci `has_just_one` : quand un
    /// seul dispatcher est vivant, le premier déclenchement d'un callsite évalue son
    /// intérêt avec le défaut du thread émetteur — sans notre `set_default`, le
    /// `NoSubscriber` global répond `Interest::never()`, cet `never` est mémorisé sur le
    /// callsite et le macro `debug!` court-circuite avant même de consulter notre
    /// souscripteur thread-local : la capture reste vide (`[]`). L'émetteur empoisonneur
    /// est n'importe quel test déclenchant le rebond SANS capture (le jumeau
    /// `*_insert_failure_*` de ce fichier, les courses de `membership`). Deux `Dispatch`
    /// fuités à vie (deux pour que `has_just_one` retombe définitivement à `false` après
    /// le `retain` des entrées mortes) sur un souscripteur qui répond toujours
    /// `sometimes` garantissent que tout recalcul croise au moins un dispatcher vivant
    /// non-`never` ; la création du gardien reconstruit aussi l'intérêt des callsites
    /// déjà empoisonnés. Jamais posé comme défaut, il ne reçoit aucun événement et ne
    /// change le contrat d'aucun autre test.
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

    /// Erreur `E` arbitraire du Scenario « insertion en échec sans ligne trouvée »,
    /// clairement distincte de tout `DbErr::RecordNotFound`.
    fn insert_outage() -> DbErr {
        DbErr::Query(RuntimeErr::Internal("simulated INSERT outage E".to_string()))
    }

    /// `MockDatabase` du Scenario : la première lecture ne rend aucune ligne, l'`INSERT` échoue
    /// de l'erreur `E` (sous la feature par défaut `sqlite-use-returning-for-3_35`, l'`insert`
    /// passe par un `SELECT`/`RETURNING` qui consomme une entrée de `query_results` — vérifié
    /// dans `executor/insert.rs` de sea-orm 2.0.2) et la relecture de rebond ne rend aucune
    /// ligne non plus : le rebond ne trouve rien.
    fn empty_insert_outage_db() -> DatabaseConnection {
        MockDatabase::new(DbBackend::Sqlite)
            .append_query_results::<Model, _, _>([[]])
            .append_query_errors([insert_outage()])
            .append_query_results::<Model, _, _>([[]])
            .into_connection()
    }

    #[tokio::test]
    async fn resolve_user_creates_then_reuses_same_row() {
        let db = test_db().await;
        let first = resolve_user(&db, "sub-1", Some("a@example.com"))
            .await
            .expect("first resolve succeeds");
        let second = resolve_user(&db, "sub-1", Some("a@example.com"))
            .await
            .expect("second resolve succeeds");
        assert_eq!(first.id, second.id);
    }

    /// `Scenario` : « insertion en échec sans ligne trouvée propage l'erreur d'insertion
    /// d'origine » (arbitré 2026-09-29) — le `DbErr` `E` de l'`INSERT` est propagé verbatim,
    /// aucun `DbErr::RecordNotFound` « vanished » n'est fabriqué.
    #[tokio::test]
    async fn resolve_user_insert_failure_with_empty_reread_propagates_original_dberr() {
        let db = empty_insert_outage_db();

        let result = resolve_user(&db, "sub-ghost", None).await;

        assert!(
            matches!(
                &result,
                Err(DbErr::Query(RuntimeErr::Internal(msg))) if msg == "simulated INSERT outage E"
            ),
            "le DbErr d'origine de l'`INSERT` doit être propagé verbatim, aucun \
             `DbErr::RecordNotFound` « vanished » ne doit être fabriqué : {result:?}"
        );
    }

    /// `Must` (arbitré 2026-09-29) : le rebond émet une trace `tracing` `debug` (course
    /// normale, jamais `warn`).
    #[tokio::test]
    async fn resolve_user_insert_bounce_emits_debug_trace_never_warn() {
        let db = empty_insert_outage_db();
        let (_guard, traces) = capture_traces();

        let result = resolve_user(&db, "sub-ghost", None).await;
        assert!(result.is_err(), "fixture de panne : l'`INSERT` doit échouer");

        let lines = traces.lines();
        assert!(
            lines
                .iter()
                .any(|(level, line)| *level == tracing::Level::DEBUG && line.contains("sub-ghost")),
            "`Must` (arbitré 2026-09-29) : le rebond doit émettre une trace `debug` citant le \
             subject : {lines:?}"
        );
        assert!(
            !lines
                .iter()
                .any(|(level, _)| matches!(*level, tracing::Level::WARN | tracing::Level::ERROR)),
            "course normale : le rebond ne doit jamais être tracé `warn` ou `error` : {lines:?}"
        );
    }
}
