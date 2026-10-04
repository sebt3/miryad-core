use sea_orm_migration::prelude::*;

use crate::users::group::{ADMIN_GROUP_NAME, ensure_group};

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    /// Seed idempotent — passe par le même `ensure_group` que la synchronisation OIDC (feature 3)
    /// plutôt que du SQL brut, pour rester portable SQLite/Postgres sans dupliquer la logique de
    /// conversion `DateTimeUtc`.
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        ensure_group(manager.get_connection(), ADMIN_GROUP_NAME).await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared("DELETE FROM miryad_groups WHERE name = 'admin'")
            .await?;
        Ok(())
    }
}

// Conversion test-first des douze `Scenario` de
// `m20260822_000003_seed_admin_group.sdd` — verrous du réel sur migration committée immuable.

#[cfg(test)]
mod tests {
    use super::Migration;
    use crate::migration::Migrator;
    use crate::migration::m20260822_000002_create_users_groups::Migration as CreateUsersGroups;
    use crate::users::group::{
        ADMIN_GROUP_NAME, ActiveModel as GroupActiveModel, Column as GroupColumn, Entity as GroupEntity,
        is_admin,
    };
    use crate::users::user::resolve_user;
    use sea_orm::{
        ActiveModelTrait, ColumnTrait, ConnectionTrait, Database, DatabaseConnection, DbBackend, EntityTrait,
        QueryFilter, QueryResult, Set, Statement,
    };
    use sea_orm_migration::prelude::*;

    const TABLE: &str = "miryad_groups";

    async fn fresh_db() -> DatabaseConnection {
        Database::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite connects")
    }

    /// Base dont les tables de 000002 seules sont posées — la soeur est atteignable par la
    /// portée du module parent (`mod.rs` garde les `mod` privés, accessibles aux descendants).
    async fn db_with_000002() -> DatabaseConnection {
        let db = fresh_db().await;
        CreateUsersGroups
            .up(&SchemaManager::new(&db))
            .await
            .expect("the sibling 000002 migration poses the base tables");
        db
    }

    /// Base migrée en entier par le `Migrator` (seed `admin` posé) — pré-condition de cinq
    /// `Scenario`, pattern de `mod.rs`.
    async fn migrated_db() -> DatabaseConnection {
        let db = fresh_db().await;
        Migrator::up(&db, None)
            .await
            .expect("the full Migrator up applies the three internals");
        db
    }

    async fn select(db: &DatabaseConnection, sql: &str) -> Vec<QueryResult> {
        db.query_all_raw(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            sql,
            Vec::<sea_orm::Value>::new(),
        ))
        .await
        .expect("raw SELECT succeeds")
    }

    async fn count(db: &DatabaseConnection, sql: &str) -> i64 {
        select(db, sql)
            .await
            .first()
            .expect("COUNT always returns one row")
            .try_get::<i64>("", "n")
            .expect("count decodes as integer")
    }

    async fn membership_count(db: &DatabaseConnection) -> i64 {
        count(db, "SELECT COUNT(*) AS n FROM miryad_group_memberships").await
    }

    /// Ligne `admin` relue par l'entité `users::group` — emprunt sans toucher à son contrat.
    async fn admin_row(db: &DatabaseConnection) -> Option<crate::users::group::Model> {
        GroupEntity::find()
            .filter(GroupColumn::Name.eq(ADMIN_GROUP_NAME))
            .one(db)
            .await
            .expect("query succeeds")
    }

    /// Scenario : « @up seede une unique ligne `admin` »
    #[tokio::test]
    async fn up_seeds_single_admin_group() {
        let db = fresh_db().await;
        let before = chrono::Utc::now();
        Migrator::up(&db, None)
            .await
            .expect("the full Migrator up (the three internal migrations) succeeds");
        let after = chrono::Utc::now();

        assert_eq!(
            count(
                &db,
                "SELECT COUNT(*) AS n FROM miryad_groups WHERE name = 'admin'"
            )
            .await,
            1,
            "miryad_groups contains exactly one row named admin"
        );

        let row = admin_row(&db).await.expect("the seeded admin row exists");
        assert!(row.id > 0, "id abandoned to the auto-increment: {:?}", row.id);
        assert!(
            row.created_at >= before && row.created_at <= after,
            "created_at non null posed at execution by ensure_group's Utc::now: {}",
            row.created_at
        );
        assert_eq!(row.name, ADMIN_GROUP_NAME);

        let by_constant = GroupEntity::find()
            .filter(GroupColumn::Name.eq(ADMIN_GROUP_NAME))
            .one(&db)
            .await
            .expect("query succeeds");
        let by_literal = GroupEntity::find()
            .filter(GroupColumn::Name.eq("admin"))
            .one(&db)
            .await
            .expect("query succeeds");
        assert_eq!(
            by_constant.map(|group| group.id),
            by_literal.map(|group| group.id),
            "the lookup by ADMIN_GROUP_NAME and the one by the literal admin find the same row"
        );
    }

    /// Scenario : « second @up direct n'ajoute rien »
    #[tokio::test]
    async fn up_rerun_direct_preserves_row() {
        let db = db_with_000002().await;
        let manager = SchemaManager::new(&db);

        Migration
            .up(&manager)
            .await
            .expect("first direct up seeds the admin row");
        let first = admin_row(&db).await.expect("the admin row exists");

        Migration
            .up(&manager)
            .await
            .expect("the second direct up returns Ok(())");

        let rows = GroupEntity::find()
            .filter(GroupColumn::Name.eq(ADMIN_GROUP_NAME))
            .all(&db)
            .await
            .expect("query succeeds");
        assert_eq!(rows.len(), 1, "Entity::find still counts one single admin row");
        let row = rows.first().expect("the row exists");
        assert_eq!(row.id, first.id, "same id — no second INSERT happened");
        assert_eq!(
            row.created_at, first.created_at,
            "same created_at intact: the first SELECT of ensure_group short-circuited any INSERT"
        );
    }

    /// Scenario : « rejeu du migrateur ne re-seed pas »
    #[tokio::test]
    async fn migrator_second_up_does_not_reseed() {
        let db = migrated_db().await;
        let first = admin_row(&db).await.expect("the seeded admin row exists");

        Migrator::up(&db, None)
            .await
            .expect("the second Migrator::up returns Ok(()) — exec_up_with holds this migration Applied from the dedicated tracking table");

        let rows = GroupEntity::find()
            .filter(GroupColumn::Name.eq(ADMIN_GROUP_NAME))
            .all(&db)
            .await
            .expect("query succeeds");
        assert_eq!(rows.len(), 1, "the admin row stays unique");
        let row = rows.first().expect("the row exists");
        assert_eq!(
            row.created_at, first.created_at,
            "the admin row keeps the created_at of the first application"
        );
        assert_eq!(row.id, first.id, "and its original id");
    }

    /// Scenario : « le seed ne confère l'admin à personne »
    #[tokio::test]
    async fn seed_grants_no_membership() {
        let db = migrated_db().await;

        let user = resolve_user(&db, "sub-seed-check", None)
            .await
            .expect("resolve_user creates the user");
        assert!(
            !is_admin(&db, user.id).await.expect("is_admin query succeeds"),
            "is_admin answers Ok(false): the seeded row makes nobody an administrator"
        );
        assert_eq!(
            membership_count(&db).await,
            0,
            "miryad_group_memberships contains no row"
        );
    }

    /// Scenario : « ligne `admin` préexistante est acceptée telle quelle »
    #[tokio::test]
    async fn up_accepts_existing_admin_unchanged() {
        let db = db_with_000002().await;
        let witness = chrono::Utc::now() - chrono::Duration::days(365);
        let hand_made = GroupActiveModel {
            name: Set(ADMIN_GROUP_NAME.to_string()),
            created_at: Set(witness),
            ..Default::default()
        }
        .insert(&db)
        .await
        .expect("an admin row created by hand with a witness created_at exists before up");

        Migration
            .up(&SchemaManager::new(&db))
            .await
            .expect("up returns Ok(()) over the pre-existing admin row");

        let rows = GroupEntity::find()
            .filter(GroupColumn::Name.eq(ADMIN_GROUP_NAME))
            .all(&db)
            .await
            .expect("query succeeds");
        assert_eq!(rows.len(), 1, "no second admin row exists");
        let row = rows.first().expect("the row exists");
        assert_eq!(row.id, hand_made.id, "the row keeps its witness id");
        assert_eq!(
            row.created_at, witness,
            "the row keeps its witness created_at — up has presence semantics, not conformity"
        );
    }

    /// Scenario : « @down ne retire que `admin` parmi les groupes »
    #[tokio::test]
    async fn down_deletes_only_admin_row() {
        let db = migrated_db().await;
        crate::users::group::ensure_group(&db, "editors")
            .await
            .expect("a second group editors is added via ensure_group");
        let manager = SchemaManager::new(&db);

        Migration.down(&manager).await.expect("down returns Ok(())");
        assert!(
            admin_row(&db).await.is_none(),
            "the admin row disappeared from miryad_groups"
        );
        assert_eq!(
            count(
                &db,
                "SELECT COUNT(*) AS n FROM miryad_groups WHERE name = 'editors'"
            )
            .await,
            1,
            "the editors row is still present"
        );

        Migration.down(&manager).await.expect(
            "a second immediate down also returns Ok(()) with zero rows touched — idempotent rollback",
        );
        assert!(
            SchemaManager::new(&db)
                .has_table(TABLE)
                .await
                .expect("has_table probe succeeds"),
            "miryad_groups survives: down carries no DDL"
        );
    }

    /// Scenario : « @down sans table `miryad_groups` échoue » — chemin impossible dans le cadre
    /// `Migrator` (l'inversion d'ordre fait passer ce `down` avant les `DROP` de 000002).
    #[tokio::test]
    async fn down_without_table_is_dberr() {
        let db = fresh_db().await;

        assert!(
            Migration.down(&SchemaManager::new(&db)).await.is_err(),
            "down over a base where no up ever ran must return a sea_orm::DbErr — a raw DELETE on an absent table has no tolerance"
        );
        assert!(
            !SchemaManager::new(&db)
                .has_table(TABLE)
                .await
                .expect("has_table probe succeeds"),
            "no miryad_groups table appears from the failed down"
        );
    }

    /// Scenario : « @up sans tables échoue sans rien écrire »
    #[tokio::test]
    async fn up_without_table_is_dberr() {
        let db = fresh_db().await;
        let manager = SchemaManager::new(&db);

        assert!(
            Migration.up(&manager).await.is_err(),
            "up without the 000002 tables must return a sea_orm::DbErr: the first SELECT of ensure_group fails and returns through the ? of up's body"
        );
        for table in ["miryad_users", "miryad_groups", "miryad_group_memberships"] {
            assert!(
                !manager.has_table(table).await.expect("has_table probe succeeds"),
                "no table appears — the file contains no DDL: {table}"
            );
        }
    }

    /// Scenario : « @down cascade les memberships de l'admin seedé » — cascade exécutée sur
    /// `SQLite` par le `PRAGMA foreign_keys = ON` de sqlx (vérifié au source, `Must` de la spec).
    #[tokio::test]
    async fn down_cascades_admin_memberships() {
        let db = migrated_db().await;
        let user = resolve_user(&db, "admin-member", None)
            .await
            .expect("resolve_user poses the user");
        crate::users::membership::sync_group_memberships(&db, user.id, &["admin".to_string()])
            .await
            .expect("sync_group_memberships attaches the user to the seeded admin group only");
        assert!(
            is_admin(&db, user.id).await.expect("is_admin query succeeds"),
            "GIVEN: is_admin answers Ok(true) before the rollback"
        );

        Migration
            .down(&SchemaManager::new(&db))
            .await
            .expect("down returns Ok(())");

        assert!(admin_row(&db).await.is_none(), "the admin row is deleted");
        assert_eq!(
            membership_count(&db).await,
            0,
            "the membership row pointing at it was cascade-deleted by the 000002 FK"
        );
        assert!(
            !is_admin(&db, user.id).await.expect("is_admin query succeeds"),
            "is_admin answers Ok(false) — no group left to find"
        );

        Migration
            .up(&SchemaManager::new(&db))
            .await
            .expect("replayed direct up returns Ok(())");
        assert!(
            admin_row(&db).await.is_some(),
            "the admin group exists again after the re-up"
        );
        assert!(
            !is_admin(&db, user.id).await.expect("is_admin query succeeds"),
            "the right does not survive the rollback: only sync_group_memberships reattaches"
        );
    }

    /// Scenario : « `Admin` de casse distincte survit au @down » — égalité binaire du backend
    /// `SQLite` des tests ; le sens Postgres relève de l'arbitrage ouvert de `users/group.sdd`.
    #[tokio::test]
    async fn down_preserves_case_distinct_group() {
        let db = db_with_000002().await;
        let manager = SchemaManager::new(&db);
        Migration.up(&manager).await.expect("up seeds the admin row");
        let capital_id = crate::users::group::ensure_group(&db, "Admin")
            .await
            .expect("a capital-A Admin group is created additionally via ensure_group");

        assert_eq!(
            count(&db, "SELECT COUNT(*) AS n FROM miryad_groups").await,
            2,
            "two distinct rows coexist for lack of COLLATE posed by 000002"
        );

        Migration
            .down(&manager)
            .await
            .expect("direct down returns Ok(())");

        assert!(
            admin_row(&db).await.is_none(),
            "the admin row is deleted by the binary equality of the DELETE literal"
        );
        let capital = GroupEntity::find()
            .filter(GroupColumn::Name.eq("Admin"))
            .one(&db)
            .await
            .expect("query succeeds");
        assert_eq!(
            capital.map(|group| group.id),
            Some(capital_id),
            "the Admin row survives — SQLite binary comparison spares the case-distinct group"
        );
    }

    /// Scenario : « remontée totale puis re-application reconstruit le seed »
    #[tokio::test]
    async fn full_rollback_then_reapply_restores_seed() {
        let db = migrated_db().await;
        let user = resolve_user(&db, "sub-rollback", None)
            .await
            .expect("resolve_user poses the user");
        crate::users::membership::sync_group_memberships(&db, user.id, &["admin".to_string()])
            .await
            .expect("the user is synchronised toward [\"admin\"]");
        assert!(
            is_admin(&db, user.id).await.expect("is_admin query succeeds"),
            "GIVEN: is_admin Ok(true) before the rollback"
        );
        let first = admin_row(&db).await.expect("the first admin row exists");

        Migrator::down(&db, None)
            .await
            .expect("the full Migrator down returns Ok(()) — this down passed before the 000002 DROPs, order guaranteed by the framework's reversal");
        Migrator::up(&db, None)
            .await
            .expect("the full Migrator up returns Ok(())");

        let rows = GroupEntity::find()
            .filter(GroupColumn::Name.eq(ADMIN_GROUP_NAME))
            .all(&db)
            .await
            .expect("query succeeds");
        assert_eq!(rows.len(), 1, "a unique admin row exists again");
        let row = rows.first().expect("the row exists");
        assert!(
            row.created_at > first.created_at,
            "the rebuilt seed carries a created_at more recent than the first application: {} > {}",
            row.created_at,
            first.created_at
        );
        assert!(
            !is_admin(&db, user.id).await.expect("is_admin query succeeds"),
            "is_admin answers Ok(false) again: the cascade then the DROP destroyed the membership, the reconstruction starts from zero"
        );
    }

    /// Scenario : « le tracking nomme cette migration par son stem de fichier »
    #[tokio::test]
    async fn applied_name_is_the_seed_stem() {
        let db = fresh_db().await;
        Migrator::up(&db, None)
            .await
            .expect("the full Migrator up applies every registered migration");

        let applied: Vec<String> = Migrator::get_applied_migrations(&db)
            .await
            .expect("applied list is readable")
            .iter()
            .map(sea_orm_migration::Migration::name)
            .map(str::to_string)
            .collect();

        let stem = std::path::Path::new(file!())
            .file_stem()
            .and_then(std::ffi::OsStr::to_str)
            .expect("this source file has a readable stem");
        assert_eq!(
            stem, "m20260822_000003_seed_admin_group",
            "the DeriveMigrationName expansion is the get_file_stem of this path"
        );
        assert_eq!(
            applied.iter().filter(|name| name.as_str() == stem).count(),
            1,
            "get_applied_migrations counts one applied migration named exactly {stem}: {applied:?}"
        );
        assert_eq!(
            count(
                &db,
                &format!("SELECT COUNT(*) AS n FROM seaql_migrations_miryad_core WHERE version = '{stem}'")
            )
            .await,
            1,
            "the stem is inscribed in seaql_migrations_miryad_core, the dedicated table owned by ./mod.rs"
        );
    }
}
