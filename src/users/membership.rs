//! Table d'association `miryad_group_memberships` et sa réconciliation exclusive depuis le
//! claim groupes de l'`OIDC` par `sync_group_memberships` : aucun chemin d'assignation manuel,
//! `Authentik` reste la source de vérité des appartenances.

use sea_orm::entity::prelude::*;
use sea_orm::{ConnectionTrait, Set, TransactionSession, TransactionTrait};

use crate::users::group::ensure_group;

/// Ligne `DeriveEntityModel` de la table d'association `miryad_group_memberships` (posée par la
/// migration `m20260822_000002`) — trois colonnes, une ligne par appartenance.
#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "miryad_group_memberships")]
pub struct Model {
    /// Clé primaire `i32` auto-incrémentée — seule identité de la ligne, ciblée par la
    /// suppression de réconciliation.
    #[sea_orm(primary_key)]
    pub id: i32,
    /// `miryad_users.id` du membre — `FK` `ON DELETE CASCADE`, la paire (`user_id`, `group_id`)
    /// est `UNIQUE`.
    pub user_id: i32,
    /// `miryad_groups.id` du groupe — `FK` `ON DELETE CASCADE`, la paire (`user_id`, `group_id`)
    /// est `UNIQUE`.
    pub group_id: i32,
}

/// `DeriveRelation` déclaré avec un enum vide : entité sans relation `SeaORM`, invisible au
/// graphe d'entités malgré ses `FK` — les liens se lisent par filtres explicites.
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

/// Alias lisible de l'`Entity` généré par `DeriveEntityModel` — même type ; mis à plat sous
/// `crate::users::GroupMembership`.
pub type GroupMembership = Entity;

/// Réconciliation complète des appartenances de `user_id` depuis un claim `groups` OIDC : les
/// groupes absents sont retirés, les nouveaux sont ajoutés (créés à la volée si inconnus). Seul
/// chemin d'écriture de cette table — pas d'API d'assignation manuelle (Authentik est la source
/// de vérité, cf. `docs/architecture.md`).
///
/// Les noms vides ou blancs du claim sont filtrés à l'entrée (trace `debug`) : le claim OIDC est
/// la frontière de confiance externe, `ensure_group` n'est pas gardé (arbitré 2026-09-29). Les
/// noms dupliqués restent absorbés par le `ON CONFLICT DO NOTHING`, sans déduplication en entrée.
///
/// Toute la réconciliation (`ensure_group`, suppressions et insertions) s'exécute dans une
/// transaction interne ouverte sur `db` : tout ou rien, jamais d'utilisateur à moitié réconcilié
/// (arbitré 2026-09-29). Un échec en cours de route déclenche un `ROLLBACK` explicite avant la
/// remontée de l'erreur — aucune suppression partielle ne reste visible.
///
/// # Errors
///
/// Aucune erreur à code `MRD-*` ici — toutes les pannes remontent en `DbErr` brut propagé par
/// `?` : via `ensure_group` une erreur de requête sur `miryad_groups`, ou le `DbErr` d'origine
/// de l'`INSERT` propagé verbatim quand la relance est vide (arbitré 2026-09-29 — plus de
/// `DbErr::RecordNotFound` « vanished » fabriqué, cf. `group.sdd`) ; et pour les find/insert/
/// delete une erreur de connexion, de contrainte non ciblée ou d'auto-incrément inaccessible —
/// les conflits ciblés sur (`user_id`, `group_id`) ne remontent pas (`ON CONFLICT DO NOTHING`).
/// Violation de FK à l'insertion quand `user_id` n'existe pas dans `miryad_users`.
/// Les pannes de `begin`/`commit`/`rollback` de la transaction interne remontent aussi en
/// `DbErr` brut.
pub async fn sync_group_memberships<C: ConnectionTrait + TransactionTrait>(
    db: &C,
    user_id: i32,
    groups: &[String],
) -> Result<(), DbErr> {
    // Filtre d'entrée (arbitré 2026-09-29) : le claim OIDC est la frontière de confiance
    // externe, les noms vides ou blancs sont écartés avant `ensure_group` qui n'est pas gardé.
    let ignored_blank = groups.iter().filter(|name| name.trim().is_empty()).count();
    if ignored_blank > 0 {
        tracing::debug!(
            user_id,
            ignored = ignored_blank,
            "group claim: blank group names ignored before reconciliation"
        );
    }
    let wanted_names: Vec<&str> = groups
        .iter()
        .map(String::as_str)
        .filter(|name| !name.trim().is_empty())
        .collect();

    // Transaction interne (arbitré 2026-09-29) : `ensure_group`, suppressions et insertions
    // s'exécutent sous un même `BEGIN`/`COMMIT` — tout ou rien, jamais d'utilisateur à moitié
    // réconcilié. Un échec en cours de route déclenche le `ROLLBACK` avant la remontée du
    // `DbErr` nu : aucune suppression partielle ne reste visible.
    let txn = db.begin().await?;
    match reconcile_group_memberships(&txn, user_id, &wanted_names).await {
        Ok((added, removed)) => {
            txn.commit().await?;
            // Garde d'émission : la trace `debug` « par réconciliation » (arbitré 2026-09-29)
            // n'est posée que quand la réconciliation a effectivement modifié l'état. La lecture
            // strictement littérale (trace même à vide) casse les deux tests gelés de
            // `src/auth/mod.rs` (« le succès ne trace qu'une ligne », capture globale à niveau
            // indifférent) : le callback login, lui, ne trace qu'à effectif. Écart signalé à
            // trancher (rapport B2a), aucun `#[allow]`, aucun contournement par niveau.
            if added > 0 || removed > 0 {
                tracing::debug!(user_id, added, removed, "group memberships reconciled");
            }
            if removed > 0 {
                tracing::info!(user_id, removed, "group memberships removed by reconciliation");
            }
            Ok(())
        }
        Err(err) => {
            txn.rollback().await?;
            Err(err)
        }
    }
}

/// Réconciliation proprement dite, exécutée dans la transaction interne de
/// `sync_group_memberships` ; rend les comptes `(ajoutés, retirés)` pour la trace `debug`.
async fn reconcile_group_memberships<C: ConnectionTrait>(
    db: &C,
    user_id: i32,
    wanted_names: &[&str],
) -> Result<(usize, usize), DbErr> {
    let mut wanted_group_ids = Vec::with_capacity(wanted_names.len());
    for name in wanted_names {
        wanted_group_ids.push(ensure_group(db, name).await?);
    }

    let current = Entity::find().filter(Column::UserId.eq(user_id)).all(db).await?;

    let stale: Vec<&Model> = current
        .iter()
        .filter(|membership| !wanted_group_ids.contains(&membership.group_id))
        .collect();
    for membership in &stale {
        Entity::delete_by_id(membership.id).exec(db).await?;
    }

    let current_group_ids: Vec<i32> = current.iter().map(|m| m.group_id).collect();
    let missing: Vec<i32> = wanted_group_ids
        .iter()
        .copied()
        .filter(|group_id| !current_group_ids.contains(group_id))
        .collect();
    for group_id in &missing {
        let active = ActiveModel {
            user_id: Set(user_id),
            group_id: Set(*group_id),
            ..Default::default()
        };
        // `current_group_ids` est un instantané pris en début de fonction, jamais rafraîchi
        // pendant cette boucle : deux appels concurrents pour le même user_id (double
        // callback OIDC) peuvent tous les deux tenter d'insérer la même ligne. ON CONFLICT
        // DO NOTHING rend l'insertion idempotente sous concurrence sans retirer la
        // contrainte unique — le perdant de la course n'échoue plus, il n'a juste rien à
        // faire (la ligne existe déjà, posée par le gagnant).
        Entity::insert(active)
            .on_conflict_do_nothing_on([Column::UserId, Column::GroupId])
            .exec(db)
            .await?;
    }

    Ok((missing.len(), stale.len()))
}

/// Capture des traces `tracing` émises par cette crate (`target` préfixé `miryad_core`), pattern
/// de `auth/oidc.rs` dupliqué en ligne dans `./user.rs` et `./group.rs` — le garde d'intérêt
/// global anti-flakiness posé en 70d5b3f y est inclus. La tâche B2a étant bornée en `Owns` à
/// `membership.rs` + `service_account.rs` (un auxiliaire partagé ne peut pas vivre dans
/// `./mod.rs`), le pattern est centralisé ici et réutilisé par les tests de
/// `./service_account.rs` plutôt que dupliqué une quatrième fois.
#[cfg(test)]
pub(crate) mod trace_capture {
    use std::sync::{Arc, Mutex};

    /// Le `DefaultGuard` de `capture_traces` est thread-local : sans effet sur les tests voisins.
    /// Le niveau est capturé pour verrouiller les clauses « trace `debug` par réconciliation »,
    /// « trace `info` sur retrait » et « trace `debug` du filtre » (arbitrés 2026-09-29).
    #[derive(Clone, Default)]
    pub(crate) struct CapturedTraces(Arc<Mutex<Vec<(tracing::Level, String)>>>);

    impl CapturedTraces {
        pub(crate) fn lines(&self) -> Vec<(tracing::Level, String)> {
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

    /// Garde d'intérêt globale contre le flakiness de la capture (fix tâche F, 70d5b3f) — miroir
    /// exact de celles de `./user.rs` et `./group.rs` : le `Interest` d'un callsite est un
    /// atomique GLOBAL dans `tracing-core` 0.1.36, recalculé à chaque enregistrement de
    /// `Dispatch`, avec un raccourci `has_just_one` : quand un seul dispatcher est vivant, le
    /// premier déclenchement d'un callsite évalue son intérêt avec le défaut du thread émetteur —
    /// sans notre `set_default`, le `NoSubscriber` global répond `Interest::never()`, ce `never`
    /// est mémorisé sur le callsite et le macro `debug!` court-circuite avant même de consulter
    /// notre souscripteur thread-local : la capture reste vide (`[]`). L'émetteur empoisonneur
    /// est tout test déclenchant ces traces SANS capture (les syncs des tests voisins de ce
    /// fichier, les provisionnements de `./service_account.rs`). Deux `Dispatch` fuités à vie
    /// (deux pour que `has_just_one` retombe définitivement à `false` après le `retain` des
    /// entrées mortes) sur un souscripteur qui répond toujours `sometimes` garantissent que tout
    /// recalcul croise au moins un dispatcher vivant non-`never` ; la création du gardien
    /// reconstruit aussi l'intérêt des callsites déjà empoisonnés. Jamais posé comme défaut, il
    /// ne reçoit aucun événement et ne change le contrat d'aucun autre test.
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

    pub(crate) fn capture_traces() -> (tracing::subscriber::DefaultGuard, CapturedTraces) {
        use tracing_subscriber::layer::SubscriberExt as _;

        install_interest_guard();
        let captured = CapturedTraces::default();
        let subscriber =
            tracing_subscriber::registry::Registry::default().with(CaptureLayer(Arc::clone(&captured.0)));
        let guard = tracing::subscriber::set_default(subscriber);
        (guard, captured)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migration::Migrator;
    use crate::users::group;
    use crate::users::membership::trace_capture::capture_traces;
    use crate::users::user::resolve_user;
    use sea_orm::{DbBackend, MockDatabase, MockExecResult, RuntimeErr, Statement};
    use sea_orm_migration::MigratorTrait;

    async fn test_db() -> DatabaseConnection {
        let db = sea_orm::Database::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite connects");
        Migrator::up(&db, None).await.expect("migrations apply cleanly");
        db
    }

    #[tokio::test]
    async fn sync_adds_missing_groups_including_unknown_ones() {
        let db = test_db().await;
        let user = resolve_user(&db, "sub-1", None).await.expect("resolve succeeds");

        sync_group_memberships(&db, user.id, &["admin".to_string(), "editors".to_string()])
            .await
            .expect("sync succeeds");

        let memberships = Entity::find()
            .filter(Column::UserId.eq(user.id))
            .all(&db)
            .await
            .expect("query succeeds");
        assert_eq!(memberships.len(), 2);
    }

    #[tokio::test]
    async fn second_sync_with_fewer_groups_removes_stale_memberships() {
        let db = test_db().await;
        let user = resolve_user(&db, "sub-1", None).await.expect("resolve succeeds");

        sync_group_memberships(&db, user.id, &["admin".to_string(), "editors".to_string()])
            .await
            .expect("first sync succeeds");
        sync_group_memberships(&db, user.id, &["editors".to_string()])
            .await
            .expect("second sync succeeds");

        let memberships = Entity::find()
            .filter(Column::UserId.eq(user.id))
            .all(&db)
            .await
            .expect("query succeeds");
        assert_eq!(memberships.len(), 1);
    }

    #[tokio::test]
    async fn sync_is_idempotent() {
        let db = test_db().await;
        let user = resolve_user(&db, "sub-1", None).await.expect("resolve succeeds");

        sync_group_memberships(&db, user.id, &["editors".to_string()])
            .await
            .expect("first sync succeeds");
        sync_group_memberships(&db, user.id, &["editors".to_string()])
            .await
            .expect("second sync succeeds");

        let memberships = Entity::find()
            .filter(Column::UserId.eq(user.id))
            .all(&db)
            .await
            .expect("query succeeds");
        assert_eq!(memberships.len(), 1);
    }

    /// Reproduit le scénario de l'issue #3 : un double callback OIDC (prefetch navigateur, double
    /// requête) déclenche deux invocations concurrentes pour le même `user_id`. Les deux lisent
    /// le même instantané `current` avant que l'une ou l'autre n'ait committé ses insertions —
    /// sans ON CONFLICT DO NOTHING, la perdante de la course viole la contrainte unique
    /// `(user_id, group_id)` et l'appelante (le callback OIDC) échoue en entier alors que les
    /// données finissent par être correctes.
    #[tokio::test]
    async fn concurrent_syncs_for_the_same_user_do_not_violate_the_unique_constraint() {
        let db = test_db().await;
        let user = resolve_user(&db, "sub-1", None).await.expect("resolve succeeds");
        let groups = vec!["admin".to_string(), "editors".to_string()];

        let (first, second) = tokio::join!(
            sync_group_memberships(&db, user.id, &groups),
            sync_group_memberships(&db, user.id, &groups),
        );
        first.expect("first concurrent sync succeeds");
        second.expect("second concurrent sync succeeds");

        let memberships = Entity::find()
            .filter(Column::UserId.eq(user.id))
            .all(&db)
            .await
            .expect("query succeeds");
        assert_eq!(memberships.len(), 2);
    }

    /// `Scenario` « Mapping de l'entité sur sa migration ».
    #[tokio::test]
    async fn entity_maps_exactly_the_three_columns_of_its_migration() {
        let db = test_db().await;
        let user = resolve_user(&db, "sub-1", None).await.expect("resolve succeeds");
        let group_id = ensure_group(&db, "editors").await.expect("group created");

        db.execute_raw(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "INSERT INTO miryad_group_memberships (user_id, group_id) VALUES (?, ?)".to_string(),
            vec![user.id.into(), group_id.into()],
        ))
        .await
        .expect("raw insert succeeds");

        let row = Entity::find()
            .one(&db)
            .await
            .expect("query succeeds")
            .expect("raw line exists");
        assert_eq!(row.user_id, user.id);
        assert_eq!(row.group_id, group_id);
        assert_eq!(row.id, 1, "l'auto-incrément est posé par la base");

        let columns = db
            .query_all_raw(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT name FROM pragma_table_info('miryad_group_memberships')".to_string(),
            ))
            .await
            .expect("pragma table_info reads");
        let names: Vec<String> = columns
            .iter()
            .map(|row| row.try_get::<String>("", "name").expect("column name reads"))
            .collect();
        assert_eq!(
            names,
            ["id", "user_id", "group_id"],
            "la table ne porte ni horodatage ni champ absent de m20260822_000002"
        );
    }

    /// `Scenario` « Une sync à claim vide purge toutes les appartenances » — fail-closed
    /// (arbitré 2026-09-29).
    #[tokio::test]
    async fn empty_claim_purges_all_memberships_and_spares_group_rows() {
        let db = test_db().await;
        let user = resolve_user(&db, "sub-1", None).await.expect("resolve succeeds");
        sync_group_memberships(&db, user.id, &["admin".to_string(), "editors".to_string()])
            .await
            .expect("setup sync succeeds");

        sync_group_memberships(&db, user.id, &[])
            .await
            .expect("sync with an empty claim succeeds");

        let memberships = Entity::find()
            .filter(Column::UserId.eq(user.id))
            .all(&db)
            .await
            .expect("query succeeds");
        assert_eq!(memberships.len(), 0, "le claim vide réconcilie vers zéro");

        let groups = group::Entity::find().all(&db).await.expect("query succeeds");
        let names: Vec<&str> = groups.iter().map(|g| g.name.as_str()).collect();
        assert!(
            names.contains(&"admin") && names.contains(&"editors"),
            "les lignes de groupes survivent à la purge des appartenances : {names:?}"
        );
    }

    /// `Scenario` « Un nom dupliqué dans le claim produit une seule appartenance » — absorbé par
    /// le `ON CONFLICT DO NOTHING`, sans déduplication en entrée (arbitré 2026-09-29).
    #[tokio::test]
    async fn duplicate_name_in_claim_yields_a_single_membership() {
        let db = test_db().await;
        let user = resolve_user(&db, "sub-1", None).await.expect("resolve succeeds");

        sync_group_memberships(&db, user.id, &["dup".to_string(), "dup".to_string()])
            .await
            .expect("sync with a duplicated name succeeds");

        let memberships = Entity::find()
            .filter(Column::UserId.eq(user.id))
            .all(&db)
            .await
            .expect("query succeeds");
        assert_eq!(
            memberships.len(),
            1,
            "la seconde tentative est absorbée par le ON CONFLICT"
        );
    }

    /// `Scenario` « Une sync sur un utilisateur inconnu échoue en violation de FK ».
    #[tokio::test]
    async fn sync_for_unknown_user_id_fails_on_foreign_key_violation() {
        let db = test_db().await;
        let unknown_user_id = 9999;

        let result = sync_group_memberships(&db, unknown_user_id, &["editors".to_string()]).await;
        let err = result.expect_err("sync for an unknown user_id must fail");
        assert!(
            err.to_string().to_uppercase().contains("FOREIGN KEY"),
            "le seul garde-fou est la FK de m20260822_000002 (PRAGMA foreign_keys=ON sur SQLite \
             de test) : {err}"
        );

        let memberships = Entity::find()
            .filter(Column::UserId.eq(unknown_user_id))
            .all(&db)
            .await
            .expect("query succeeds");
        assert!(memberships.is_empty(), "aucune appartenance pour cet identifiant");
    }

    /// `Scenario` « La suppression d'un utilisateur cascade sur ses appartenances » — nettoyage
    /// du schéma, écrit par aucune ligne de ce fichier.
    #[tokio::test]
    async fn user_deletion_cascades_to_its_memberships() {
        let db = test_db().await;
        let user = resolve_user(&db, "sub-1", None).await.expect("resolve succeeds");
        sync_group_memberships(&db, user.id, &["editors".to_string()])
            .await
            .expect("sync succeeds");

        crate::users::user::Entity::delete_by_id(user.id)
            .exec(&db)
            .await
            .expect("user deletion succeeds");

        let memberships = Entity::find()
            .filter(Column::UserId.eq(user.id))
            .all(&db)
            .await
            .expect("query succeeds");
        assert!(
            memberships.is_empty(),
            "`on delete cascade` a emporté les appartenances"
        );
    }

    /// `Scenario` « La suppression d'un groupe cascade sur les appartenances qui le référencent »
    /// — par SQL brut, ce que le moteur ne fait jamais.
    #[tokio::test]
    async fn group_deletion_cascades_to_memberships_referencing_it() {
        let db = test_db().await;
        let user = resolve_user(&db, "sub-1", None).await.expect("resolve succeeds");
        sync_group_memberships(&db, user.id, &["editors".to_string()])
            .await
            .expect("sync succeeds");

        db.execute_unprepared("DELETE FROM miryad_groups WHERE name = 'editors'")
            .await
            .expect("raw group deletion succeeds");

        let memberships = Entity::find()
            .filter(Column::UserId.eq(user.id))
            .all(&db)
            .await
            .expect("query succeeds");
        assert!(
            memberships.is_empty(),
            "le cascade de la FK `group_id` a emporté l'appartenance"
        );
    }

    /// `Scenario` « échec en cours de réconciliation, rollback complet » (arbitré 2026-09-29) —
    /// la contrainte est forcée en test par un `SQLite` qui fait échouer l'`INSERT` de
    /// l'appartenance vers `c`, après que la suppression de `b` a déjà été exécutée. Sans
    /// transaction interne, `b` disparaît : le test est le rouge attendu de la tâche « Transaction
    /// interne ». Le `Must` dit `ensure_group` compris dans la transaction : la ligne de groupe
    /// `c`, créée juste avant l'`INSERT` fautif, ne doit pas survivre non plus.
    #[tokio::test]
    async fn sync_failure_mid_reconciliation_rolls_back_everything() {
        let db = test_db().await;
        let user = resolve_user(&db, "sub-1", None).await.expect("resolve succeeds");
        sync_group_memberships(&db, user.id, &["a".to_string(), "b".to_string()])
            .await
            .expect("setup sync succeeds");

        db.execute_unprepared(
            "CREATE TRIGGER force_membership_insert_fail
                 BEFORE INSERT ON miryad_group_memberships
                 WHEN NEW.group_id = (SELECT id FROM miryad_groups WHERE name = 'c')
                 BEGIN SELECT RAISE(ABORT, 'forced membership insert failure'); END",
        )
        .await
        .expect("forced-constraint trigger creates");

        let result = sync_group_memberships(&db, user.id, &["a".to_string(), "c".to_string()]).await;
        let err = result.expect_err("the forced INSERT failure must be returned");
        assert!(
            err.to_string().contains("forced membership insert failure"),
            "expected the forced trigger error, got: {err}"
        );

        let memberships = Entity::find()
            .filter(Column::UserId.eq(user.id))
            .all(&db)
            .await
            .expect("query succeeds");
        let mut remaining: Vec<i32> = memberships.iter().map(|m| m.group_id).collect();
        remaining.sort_unstable();
        let mut wanted = vec![
            ensure_group(&db, "a").await.expect("group a reads"),
            ensure_group(&db, "b").await.expect("group b reads"),
        ];
        wanted.sort_unstable();
        assert_eq!(
            remaining, wanted,
            "tout ou rien : l'utilisateur doit rester membre de `a` ET `b`, la suppression de `b` \
             exécutée avant l'`INSERT` fautif doit avoir été annulée"
        );

        assert!(
            group::Entity::find()
                .filter(group::Column::Name.eq("c"))
                .one(&db)
                .await
                .expect("query succeeds")
                .is_none(),
            "`ensure_group` s'exécute dans la même transaction : la ligne de groupe `c` créée \
             avant l'échec ne doit pas survivre au rollback"
        );
    }

    /// `Scenario` « Une panne d'insertion en cours de réconciliation roule tout et remonte le
    /// `DbErr` » (réécrit 2026-09-30 — l'ancienne panne par `DROP TABLE miryad_groups` en
    /// cascade FK était invérifiable) — GIVEN distinct du rollback ci-dessus : claim
    /// `["a", "b", "c"]` sans aucune suppression (l'utilisateur est déjà membre de `a` et `b`),
    /// et la création du groupe `c` avortée par un trigger `RAISE(ABORT)` sur l'`INSERT` dans
    /// `miryad_groups`. La panne frappe `ensure_group` avant toute suppression : le `DbErr` est
    /// propagé nu et aucune écriture partielle ne survit.
    #[tokio::test]
    async fn group_creation_failure_mid_reconciliation_rolls_back_and_propagates_dberr() {
        let db = test_db().await;
        let user = resolve_user(&db, "sub-1", None).await.expect("resolve succeeds");
        sync_group_memberships(&db, user.id, &["a".to_string(), "b".to_string()])
            .await
            .expect("setup sync succeeds");

        db.execute_unprepared(
            "CREATE TRIGGER force_group_insert_fail
                 BEFORE INSERT ON miryad_groups
                 WHEN NEW.name = 'c'
                 BEGIN SELECT RAISE(ABORT, 'forced group insert failure'); END",
        )
        .await
        .expect("forced-constraint trigger creates");

        let result =
            sync_group_memberships(&db, user.id, &["a".to_string(), "b".to_string(), "c".to_string()]).await;
        let err = result.expect_err("the forced group INSERT failure must be returned");
        assert!(
            err.to_string().contains("forced group insert failure"),
            "le `DbErr` propagé de `ensure_group` doit traverser nu : {err}"
        );

        let memberships = Entity::find()
            .filter(Column::UserId.eq(user.id))
            .all(&db)
            .await
            .expect("query succeeds");
        let mut remaining: Vec<i32> = memberships.iter().map(|m| m.group_id).collect();
        remaining.sort_unstable();
        let mut intact = vec![
            ensure_group(&db, "a").await.expect("group a reads"),
            ensure_group(&db, "b").await.expect("group b reads"),
        ];
        intact.sort_unstable();
        assert_eq!(
            remaining, intact,
            "rien n'est appliqué : les appartenances d'origine `a` et `b` sont intactes, aucune \
             membership partielle vers `c`"
        );

        assert!(
            group::Entity::find()
                .filter(group::Column::Name.eq("c"))
                .one(&db)
                .await
                .expect("query succeeds")
                .is_none(),
            "le groupe `c` ne survit pas : la boucle des noms est dans la même transaction que \
             les suppressions, aucune fenêtre « suppression commitée avant la panne »"
        );
    }

    /// `Scenario` « noms vides ou blancs du claim ignorés » (arbitré 2026-09-29) — filtre en
    /// entrée, aucune ligne de groupe au nom vide/blanc, trace `debug` des entrées ignorées.
    #[tokio::test]
    async fn blank_names_in_claim_are_ignored_without_creating_groups() {
        let db = test_db().await;
        let user = resolve_user(&db, "sub-1", None).await.expect("resolve succeeds");
        let (_guard, traces) = capture_traces();

        sync_group_memberships(
            &db,
            user.id,
            &[String::new(), "   ".to_string(), "editors".to_string()],
        )
        .await
        .expect("sync succeeds");

        let memberships = Entity::find()
            .filter(Column::UserId.eq(user.id))
            .all(&db)
            .await
            .expect("query succeeds");
        assert_eq!(memberships.len(), 1, "seul `editors` est réconcilié");

        let groups = group::Entity::find().all(&db).await.expect("query succeeds");
        assert!(
            groups.iter().all(|g| !g.name.trim().is_empty()),
            "aucun groupe de nom vide ou blanc n'est créé : {:?}",
            groups.iter().map(|g| &g.name).collect::<Vec<_>>()
        );

        let lines = traces.lines();
        assert!(
            lines
                .iter()
                .any(|(level, line)| *level == tracing::Level::DEBUG && line.contains("ignored=2")),
            "une trace `debug` signale les entrées ignorées : {lines:?}"
        );
    }

    /// Tâche « Traces » (arbitré 2026-09-29) : une trace `debug` par réconciliation (comptes
    /// ajoutés et retirés) ; l'`info` est réservée aux retraits — une pure addition ne trace
    /// jamais `info`.
    #[tokio::test]
    async fn sync_traces_debug_per_reconciliation_never_info_on_pure_add() {
        let db = test_db().await;
        let user = resolve_user(&db, "sub-1", None).await.expect("resolve succeeds");
        let (_guard, traces) = capture_traces();

        sync_group_memberships(&db, user.id, &["editors".to_string()])
            .await
            .expect("sync succeeds");

        let lines = traces.lines();
        assert!(
            lines.iter().any(|(level, line)| *level == tracing::Level::DEBUG
                && line.contains("added=1")
                && line.contains("removed=0")),
            "`Must` (arbitré 2026-09-29) : une trace `debug` par réconciliation portant les \
             comptes ajoutés et retirés : {lines:?}"
        );
        assert!(
            !lines.iter().any(|(level, _)| *level == tracing::Level::INFO),
            "l'`info` n'est due que quand des appartenances sont retirées : {lines:?}"
        );
    }

    /// Tâche « Traces » (arbitré 2026-09-29) : trace `info` quand des appartenances sont
    /// retirées (fail-closed compris), portant l'`user_id` et le compte des retraits.
    #[tokio::test]
    async fn sync_traces_info_when_memberships_are_removed() {
        let db = test_db().await;
        let user = resolve_user(&db, "sub-1", None).await.expect("resolve succeeds");
        sync_group_memberships(&db, user.id, &["admin".to_string(), "editors".to_string()])
            .await
            .expect("setup sync succeeds");

        let (_guard, traces) = capture_traces();
        sync_group_memberships(&db, user.id, &["editors".to_string()])
            .await
            .expect("second sync succeeds");

        let lines = traces.lines();
        assert!(
            lines.iter().any(|(level, line)| *level == tracing::Level::INFO
                && line.contains(&format!("user_id={}", user.id))
                && line.contains("removed=1")),
            "`Must` (arbitré 2026-09-29) : trace `info` quand des appartenances sont retirées : \
             {lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|(level, line)| *level == tracing::Level::DEBUG && line.contains("removed=1")),
            "la trace `debug` de réconciliation porte le compte des retraits : {lines:?}"
        );
    }

    /// Tâche « Transaction interne » (arbitré 2026-09-29), verrou de forme par `MockDatabase` :
    /// le journal des transactions de `sea-orm` 2.0.2 (`into_transaction_log`, vérifié au source
    /// `database/mock.rs`) enregistre `BEGIN`/`COMMIT`/`ROLLBACK` et les requêtes exécutées dans
    /// la transaction. Toute la réconciliation doit tenir dans UNE transaction commitée.
    #[tokio::test]
    async fn reconciliation_runs_inside_a_single_committed_transaction() {
        let db = MockDatabase::new(DbBackend::Sqlite)
            .append_query_results([[group::Model {
                id: 2,
                name: "editors".to_string(),
                created_at: chrono::Utc::now(),
            }]])
            .append_query_results([[Model {
                id: 1,
                user_id: 1,
                group_id: 1,
            }]])
            .append_exec_results([MockExecResult {
                last_insert_id: 0,
                rows_affected: 1,
            }])
            .append_query_results([[Model {
                id: 2,
                user_id: 1,
                group_id: 2,
            }]])
            .into_connection();

        sync_group_memberships(&db, 1, &["editors".to_string()])
            .await
            .expect("sync succeeds on the mock");

        let log = db.into_transaction_log();
        assert_eq!(
            log.len(),
            1,
            "toute la réconciliation doit tenir dans une seule transaction"
        );
        let sqls: Vec<String> = log[0].statements().iter().map(ToString::to_string).collect();
        assert_eq!(
            sqls.first().map(String::as_str),
            Some("BEGIN"),
            "la réconciliation s'ouvre sur BEGIN : {sqls:?}"
        );
        assert_eq!(
            sqls.last().map(String::as_str),
            Some("COMMIT"),
            "la réconciliation se referme sur COMMIT : {sqls:?}"
        );
        assert!(
            sqls.iter().any(|sql| sql.starts_with("DELETE FROM")),
            "la suppression de la ligne surannée est dans la transaction : {sqls:?}"
        );
        assert!(
            sqls.iter().any(|sql| sql.starts_with("INSERT INTO")),
            "l'insertion de la nouvelle appartenance est dans la transaction : {sqls:?}"
        );
    }

    /// Tâche « Transaction interne » (arbitré 2026-09-29), verrou de forme par `MockDatabase` :
    /// panne d'`INSERT` en cours de boucle — la `DELETE` déjà exécutée doit être enfermée dans
    /// une transaction soldée par `ROLLBACK`, et le `DbErr` remonter nu.
    #[tokio::test]
    async fn mid_loop_insert_failure_rolls_back_the_transaction() {
        let db = MockDatabase::new(DbBackend::Sqlite)
            .append_query_results([[group::Model {
                id: 2,
                name: "editors".to_string(),
                created_at: chrono::Utc::now(),
            }]])
            .append_query_results([[Model {
                id: 1,
                user_id: 1,
                group_id: 1,
            }]])
            .append_exec_results([MockExecResult {
                last_insert_id: 0,
                rows_affected: 1,
            }])
            .append_query_errors([DbErr::Query(RuntimeErr::Internal(
                "simulated membership INSERT outage".to_string(),
            ))])
            .into_connection();

        let result = sync_group_memberships(&db, 1, &["editors".to_string()]).await;
        assert!(
            matches!(
                &result,
                Err(DbErr::Query(RuntimeErr::Internal(msg)))
                    if msg == "simulated membership INSERT outage"
            ),
            "le DbErr de l'`INSERT` doit remonter nu : {result:?}"
        );

        let log = db.into_transaction_log();
        assert_eq!(
            log.len(),
            1,
            "toute la réconciliation doit tenir dans une seule transaction"
        );
        let sqls: Vec<String> = log[0].statements().iter().map(ToString::to_string).collect();
        assert_eq!(
            sqls.first().map(String::as_str),
            Some("BEGIN"),
            "la réconciliation s'ouvre sur BEGIN : {sqls:?}"
        );
        assert_eq!(
            sqls.last().map(String::as_str),
            Some("ROLLBACK"),
            "l'échec en cours de boucle solde la transaction par ROLLBACK : {sqls:?}"
        );
        assert!(
            sqls.iter().any(|sql| sql.starts_with("DELETE FROM")),
            "la suppression exécutée avant la panne est bien enfermée dans le rollback : {sqls:?}"
        );
    }
}
