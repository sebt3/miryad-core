use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(User::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(User::Id)
                            .integer()
                            .not_null()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(ColumnDef::new(User::Subject).string().not_null().unique_key())
                    .col(ColumnDef::new(User::Email).string())
                    .col(ColumnDef::new(User::DisplayName).string())
                    .col(
                        ColumnDef::new(User::CreatedAt)
                            .timestamp_with_time_zone()
                            .not_null(),
                    )
                    .to_owned(),
            )
            .await?;

        manager
            .create_table(
                Table::create()
                    .table(Group::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(Group::Id)
                            .integer()
                            .not_null()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(ColumnDef::new(Group::Name).string().not_null().unique_key())
                    .col(
                        ColumnDef::new(Group::CreatedAt)
                            .timestamp_with_time_zone()
                            .not_null(),
                    )
                    .to_owned(),
            )
            .await?;

        manager
            .create_table(
                Table::create()
                    .table(GroupMembership::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(GroupMembership::Id)
                            .integer()
                            .not_null()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(ColumnDef::new(GroupMembership::UserId).integer().not_null())
                    .col(ColumnDef::new(GroupMembership::GroupId).integer().not_null())
                    .foreign_key(
                        ForeignKey::create()
                            .from(GroupMembership::Table, GroupMembership::UserId)
                            .to(User::Table, User::Id)
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .from(GroupMembership::Table, GroupMembership::GroupId)
                            .to(Group::Table, Group::Id)
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .index(
                        Index::create()
                            .unique()
                            .col(GroupMembership::UserId)
                            .col(GroupMembership::GroupId),
                    )
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(GroupMembership::Table).to_owned())
            .await?;
        manager
            .drop_table(Table::drop().table(Group::Table).to_owned())
            .await?;
        manager
            .drop_table(Table::drop().table(User::Table).to_owned())
            .await
    }
}

#[derive(DeriveIden)]
enum User {
    #[sea_orm(iden = "miryad_users")]
    Table,
    Id,
    Subject,
    Email,
    DisplayName,
    CreatedAt,
}

#[derive(DeriveIden)]
enum Group {
    #[sea_orm(iden = "miryad_groups")]
    Table,
    Id,
    Name,
    CreatedAt,
}

#[derive(DeriveIden)]
enum GroupMembership {
    #[sea_orm(iden = "miryad_group_memberships")]
    Table,
    Id,
    UserId,
    GroupId,
}

// Conversion test-first des vingt `Scenario` de
// `m20260822_000002_create_users_groups.sdd` — verrous du réel sur migration committée immuable.

#[cfg(test)]
mod tests {
    use super::Migration;
    use crate::migration::Migrator;
    use sea_orm::{ConnectionTrait, Database, DatabaseConnection, DbBackend, QueryResult, Statement};
    use sea_orm_migration::MigratorTrait;
    use sea_orm_migration::prelude::*;

    const USERS: &str = "miryad_users";
    const GROUPS: &str = "miryad_groups";
    const MEMBERSHIPS: &str = "miryad_group_memberships";
    const USER_COLUMNS: [&str; 5] = ["id", "subject", "email", "display_name", "created_at"];
    const GROUP_COLUMNS: [&str; 3] = ["id", "name", "created_at"];
    const MEMBERSHIP_COLUMNS: [&str; 3] = ["id", "user_id", "group_id"];

    async fn fresh_db() -> DatabaseConnection {
        Database::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite connects")
    }

    /// Base avec le trio posé par un premier `up` réussi — pré-condition de la majorité des
    /// `Scenario`.
    async fn db_with_trio_up() -> DatabaseConnection {
        let db = fresh_db().await;
        Migration
            .up(&SchemaManager::new(&db))
            .await
            .expect("up succeeds over an empty in-memory database");
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

    /// `id` unique d'une table rendu par une requête WHERE (pré-condition des `Scenario` de FK,
    /// de cascade et de paire).
    async fn single_id(db: &DatabaseConnection, sql: &str) -> i64 {
        select(db, sql)
            .await
            .first()
            .expect("the row exists")
            .try_get::<i64>("", "id")
            .expect("id decodes as integer")
    }

    async fn insert_user(db: &DatabaseConnection, subject: &str) -> Result<(), sea_orm::DbErr> {
        db.execute_unprepared(&format!(
            "INSERT INTO miryad_users (subject, created_at) VALUES ('{subject}', '2026-08-22T00:00:00Z')"
        ))
        .await
        .map(|_| ())
    }

    async fn insert_user_without_created_at(
        db: &DatabaseConnection,
        subject: &str,
    ) -> Result<(), sea_orm::DbErr> {
        db.execute_unprepared(&format!(
            "INSERT INTO miryad_users (subject) VALUES ('{subject}')"
        ))
        .await
        .map(|_| ())
    }

    async fn insert_group(db: &DatabaseConnection, name: &str) -> Result<(), sea_orm::DbErr> {
        db.execute_unprepared(&format!(
            "INSERT INTO miryad_groups (name, created_at) VALUES ('{name}', '2026-08-22T00:00:00Z')"
        ))
        .await
        .map(|_| ())
    }

    async fn insert_group_without_created_at(
        db: &DatabaseConnection,
        name: &str,
    ) -> Result<(), sea_orm::DbErr> {
        db.execute_unprepared(&format!("INSERT INTO miryad_groups (name) VALUES ('{name}')"))
            .await
            .map(|_| ())
    }

    async fn insert_membership(
        db: &DatabaseConnection,
        user_id: i64,
        group_id: i64,
    ) -> Result<(), sea_orm::DbErr> {
        db.execute_unprepared(&format!(
            "INSERT INTO miryad_group_memberships (user_id, group_id) VALUES ({user_id}, {group_id})"
        ))
        .await
        .map(|_| ())
    }

    async fn user_id_by_subject(db: &DatabaseConnection, subject: &str) -> i64 {
        single_id(
            db,
            &format!("SELECT id FROM miryad_users WHERE subject = '{subject}'"),
        )
        .await
    }

    async fn group_id_by_name(db: &DatabaseConnection, name: &str) -> i64 {
        single_id(db, &format!("SELECT id FROM miryad_groups WHERE name = '{name}'")).await
    }

    /// Scenario : « @up pose le trio RBAC »
    #[tokio::test]
    async fn up_lays_the_three_tables() {
        let db = fresh_db().await;
        let manager = SchemaManager::new(&db);

        Migration
            .up(&manager)
            .await
            .expect("up returns Ok(()) over an empty database");

        for table in [USERS, GROUPS, MEMBERSHIPS] {
            assert!(
                manager.has_table(table).await.expect("has_table probe succeeds"),
                "up must pose {table}"
            );
        }
    }

    /// Scenario : « les cinq colonnes de `miryad_users` existent après up »
    #[tokio::test]
    async fn users_has_the_five_columns() {
        let db = db_with_trio_up().await;
        let manager = SchemaManager::new(&db);

        for column in USER_COLUMNS {
            assert!(
                manager
                    .has_column(USERS, column)
                    .await
                    .expect("has_column probe succeeds"),
                "column {column} must exist on {USERS}"
            );
        }
    }

    /// Scenario : « les colonnes de `miryad_groups` et `miryad_group_memberships` existent après up »
    #[tokio::test]
    async fn groups_and_memberships_have_their_columns() {
        let db = db_with_trio_up().await;
        let manager = SchemaManager::new(&db);

        for column in GROUP_COLUMNS {
            assert!(
                manager
                    .has_column(GROUPS, column)
                    .await
                    .expect("has_column probe succeeds"),
                "column {column} must exist on {GROUPS}"
            );
        }
        for column in MEMBERSHIP_COLUMNS {
            assert!(
                manager
                    .has_column(MEMBERSHIPS, column)
                    .await
                    .expect("has_column probe succeeds"),
                "column {column} must exist on {MEMBERSHIPS}"
            );
        }
    }

    /// Scenario : « subject dupliqué est rejeté »
    #[tokio::test]
    async fn subject_unique_rejects_duplicate() {
        let db = db_with_trio_up().await;

        insert_user(&db, "sub-1")
            .await
            .expect("first complete user row with subject sub-1 is Ok");
        assert!(
            insert_user(&db, "sub-1").await.is_err(),
            "second insertion sharing the subject must fail with a DbErr (message backend nu)"
        );
        assert_eq!(
            count(
                &db,
                "SELECT COUNT(*) AS n FROM miryad_users WHERE subject = 'sub-1'"
            )
            .await,
            1,
            "exactly one row carries subject sub-1 after both attempts"
        );
    }

    /// Scenario : « name dupliqué est rejeté dans `miryad_groups` »
    #[tokio::test]
    async fn group_name_unique_rejects_duplicate() {
        let db = db_with_trio_up().await;

        insert_group(&db, "editors")
            .await
            .expect("first group row with name editors is Ok");
        assert!(
            insert_group(&db, "editors").await.is_err(),
            "second insertion sharing the name must fail with a DbErr"
        );
        assert_eq!(
            count(
                &db,
                "SELECT COUNT(*) AS n FROM miryad_groups WHERE name = 'editors'"
            )
            .await,
            1,
            "exactly one row editors exists after both attempts"
        );
    }

    /// Scenario : « la paire (`user_id`, `group_id`) ne peut être posée deux fois »
    #[tokio::test]
    async fn membership_pair_unique_rejects_duplicate() {
        let db = db_with_trio_up().await;
        insert_user(&db, "pair-user").await.expect("user row inserts");
        insert_group(&db, "pair-group").await.expect("group row inserts");
        let user_id = user_id_by_subject(&db, "pair-user").await;
        let group_id = group_id_by_name(&db, "pair-group").await;

        insert_membership(&db, user_id, group_id)
            .await
            .expect("first membership of the pair is Ok");
        assert!(
            insert_membership(&db, user_id, group_id).await.is_err(),
            "second membership sharing the exact same pair must fail with a DbErr"
        );
        assert_eq!(
            count(
                &db,
                &format!("SELECT COUNT(*) AS n FROM miryad_group_memberships WHERE user_id = {user_id} AND group_id = {group_id}")
            )
            .await,
            1,
            "exactly one row carries this pair"
        );
    }

    /// Scenario : « un même utilisateur peut appartenir à plusieurs groupes »
    #[tokio::test]
    async fn one_user_may_join_several_groups() {
        let db = db_with_trio_up().await;
        insert_user(&db, "multi-user").await.expect("user row inserts");
        insert_group(&db, "groupe-a")
            .await
            .expect("first group row inserts");
        insert_group(&db, "groupe-b")
            .await
            .expect("second group row inserts");
        let user_id = user_id_by_subject(&db, "multi-user").await;
        let group_a = group_id_by_name(&db, "groupe-a").await;
        let group_b = group_id_by_name(&db, "groupe-b").await;

        insert_membership(&db, user_id, group_a)
            .await
            .expect("membership (user, groupe-a) is Ok");
        insert_membership(&db, user_id, group_b)
            .await
            .expect("membership (user, groupe-b) is Ok — the unique covers only the full pair");

        assert_eq!(
            count(
                &db,
                &format!("SELECT COUNT(*) AS n FROM miryad_group_memberships WHERE user_id = {user_id}")
            )
            .await,
            2,
            "the user has exactly two membership rows"
        );
    }

    /// Scenario : « `email` et `display_name` acceptent NULL »
    #[tokio::test]
    async fn user_optional_columns_accept_null() {
        let db = db_with_trio_up().await;

        insert_user(&db, "null-columns-user")
            .await
            .expect("user with subject and created_at only, without email/display_name, is Ok");

        let rows = select(&db, "SELECT email, display_name FROM miryad_users").await;
        let row = rows.first().expect("the inserted row is read back");
        assert_eq!(
            row.try_get::<Option<String>>("", "email")
                .expect("email decodes as a nullable column"),
            None,
            "email reads back as None"
        );
        assert_eq!(
            row.try_get::<Option<String>>("", "display_name")
                .expect("display_name decodes as a nullable column"),
            None,
            "display_name reads back as None"
        );
    }

    /// Scenario : « `created_at` est NOT NULL sans défaut serveur sur les deux tables nommées »
    #[tokio::test]
    async fn created_at_is_not_null_without_server_default() {
        let db = db_with_trio_up().await;

        assert!(
            insert_user_without_created_at(&db, "no-created-at-user")
                .await
                .is_err(),
            "user row complete of subject but without created_at must fail with a DbErr"
        );
        assert!(
            insert_group_without_created_at(&db, "no-created-at-group")
                .await
                .is_err(),
            "group row complete of name but without created_at must fail with a DbErr too"
        );

        insert_user(&db, "no-created-at-user")
            .await
            .expect("the same user row providing created_at inserts as Ok with no server default");
        insert_group(&db, "no-created-at-group")
            .await
            .expect("the same group row providing created_at inserts as Ok with no server default");
    }

    /// Scenario : « un `user_id` inconnu est refusé par la FK » — enforcement par le
    /// `PRAGMA foreign_keys = ON` du connecteur `SQLite`, aucune vérification applicative.
    #[tokio::test]
    async fn membership_rejects_unknown_user_id() {
        let db = db_with_trio_up().await;
        insert_group(&db, "fk-group")
            .await
            .expect("existing group row inserts");
        let group_id = group_id_by_name(&db, "fk-group").await;

        assert!(
            insert_membership(&db, 9999, group_id).await.is_err(),
            "membership with unknown user_id 9999 must fail with a FK-violation DbErr"
        );
        assert_eq!(
            count(&db, "SELECT COUNT(*) AS n FROM miryad_group_memberships").await,
            0,
            "no orphan membership row exists afterwards"
        );
    }

    /// Scenario : « un `group_id` inconnu est refusé par la FK »
    #[tokio::test]
    async fn membership_rejects_unknown_group_id() {
        let db = db_with_trio_up().await;
        insert_user(&db, "fk-user")
            .await
            .expect("existing user row inserts");
        let user_id = user_id_by_subject(&db, "fk-user").await;

        assert!(
            insert_membership(&db, user_id, 9999).await.is_err(),
            "membership with unknown group_id 9999 must fail with a FK-violation DbErr"
        );
        assert_eq!(
            count(&db, "SELECT COUNT(*) AS n FROM miryad_group_memberships").await,
            0,
            "no orphan membership row exists afterwards"
        );
    }

    /// Scenario : « supprimer un utilisateur cascade sur ses appartenances »
    #[tokio::test]
    async fn deleting_a_user_cascades_memberships() {
        let db = db_with_trio_up().await;
        insert_user(&db, "cascade-user").await.expect("user row inserts");
        insert_group(&db, "cascade-group")
            .await
            .expect("group row inserts");
        let user_id = user_id_by_subject(&db, "cascade-user").await;
        let group_id = group_id_by_name(&db, "cascade-group").await;
        insert_membership(&db, user_id, group_id)
            .await
            .expect("the membership row inserts before the delete");

        db.execute_unprepared("DELETE FROM miryad_users WHERE subject = 'cascade-user'")
            .await
            .expect("raw delete of the user row succeeds");

        assert_eq!(
            count(
                &db,
                &format!("SELECT COUNT(*) AS n FROM miryad_group_memberships WHERE user_id = {user_id}")
            )
            .await,
            0,
            "no membership row references this user_id anymore — the user_id FK cascade swept them"
        );
        assert_eq!(
            count(&db, "SELECT COUNT(*) AS n FROM miryad_groups").await,
            1,
            "the group row survives: only the user_id FK cascade carried on the memberships"
        );
    }

    /// Scenario : « supprimer un groupe cascade sur les appartenances qui le référencent »
    #[tokio::test]
    async fn deleting_a_group_cascades_memberships() {
        let db = db_with_trio_up().await;
        insert_user(&db, "cascade-user").await.expect("user row inserts");
        insert_group(&db, "cascade-group")
            .await
            .expect("group row inserts");
        let user_id = user_id_by_subject(&db, "cascade-user").await;
        let group_id = group_id_by_name(&db, "cascade-group").await;
        insert_membership(&db, user_id, group_id)
            .await
            .expect("the membership row inserts before the delete");

        db.execute_unprepared("DELETE FROM miryad_groups WHERE name = 'cascade-group'")
            .await
            .expect("raw delete of the group row succeeds");

        assert_eq!(
            count(&db, "SELECT COUNT(*) AS n FROM miryad_group_memberships").await,
            0,
            "the matching membership row disappeared by the group_id FK cascade"
        );
        assert_eq!(
            count(&db, "SELECT COUNT(*) AS n FROM miryad_users").await,
            1,
            "the user row survives"
        );
    }

    /// Scenario : « up est rejouable sur une base déjà conforme »
    #[tokio::test]
    async fn up_rerunnable_on_conform_base() {
        let db = fresh_db().await;
        let manager = SchemaManager::new(&db);

        Migration
            .up(&manager)
            .await
            .expect("first up succeeds over an empty database");
        Migration
            .up(&manager)
            .await
            .expect("second up over the already-conforming trio returns Ok(()) with no prior check");

        for (table, columns) in [
            (USERS, USER_COLUMNS.as_slice()),
            (GROUPS, GROUP_COLUMNS.as_slice()),
            (MEMBERSHIPS, MEMBERSHIP_COLUMNS.as_slice()),
        ] {
            for column in columns {
                assert!(
                    manager
                        .has_column(table, column)
                        .await
                        .expect("has_column probe succeeds"),
                    "the eleven contractual columns must all remain present, {table}.{column} missing"
                );
            }
        }
    }

    /// Scenario : « une `miryad_group_memberships` préexistante en dérive est avalée sans réparation ».
    ///
    /// Écart consigné (B5g, 2026-10-03) : le `And` « une insertion SQL complète partageant deux
    /// fois la même paire (`user_id`, `group_id`) sur des `id` inventés réussit en `Ok` »
    /// (l'And du Scenario « une `miryad_group_memberships` préexistante en dérive est avalée sans
    /// réparation » de `./m20260822_000002_create_users_groups.sdd`) est inconstructible tel que
    /// littéral — le `Given` du même `Scenario` pose la table dérivée « sans `group_id` », donc
    /// toute référence SQL à `group_id` échoue en « no such column » avant même d'éprouver la
    /// FK ou l'unique. Rouge constaté à la conversion sur la formulation littérale. Le verrou
    /// ci-dessous verrouille la conséquence observable de la tolérance réellement constatée :
    /// deux insertions partageant le même `user_id` sur des `id` inventés réussissent en `Ok`
    /// (ni FK ni unique ne protègent la table dérivée), et la colonne `group_id` n'existe
    /// nulle part.
    #[tokio::test]
    async fn up_swallows_a_drifted_memberships_table() {
        let db = fresh_db().await;
        db.execute_unprepared(
            "CREATE TABLE miryad_group_memberships (id INTEGER PRIMARY KEY, user_id INTEGER NOT NULL)",
        )
        .await
        .expect("a drifted miryad_group_memberships with only id and user_id — no FK, no group_id, no pair unique — is created manually");

        let manager = SchemaManager::new(&db);
        Migration.up(&manager).await.expect(
            "up returns Ok(()) — CREATE TABLE IF NOT EXISTS skips without reading the existing shape",
        );

        assert!(
            manager
                .has_column(MEMBERSHIPS, "user_id")
                .await
                .expect("has_column probe succeeds"),
            "has_column confirms user_id"
        );
        assert!(
            !manager
                .has_column(MEMBERSHIPS, "group_id")
                .await
                .expect("has_column probe succeeds"),
            "has_column confirms group_id nowhere: drift unchecked, unrepaired"
        );

        db.execute_unprepared("INSERT INTO miryad_group_memberships (id, user_id) VALUES (7701, 777)")
            .await
            .expect(
                "first invented-id insertion sharing user_id 777 succeeds — no FK protects the drifted table",
            );
        db.execute_unprepared("INSERT INTO miryad_group_memberships (id, user_id) VALUES (7702, 777)")
            .await
            .expect("second insertion sharing the same user_id on another invented id succeeds too — no pair unique protects the drifted table");
        assert!(
            db.execute_unprepared("INSERT INTO miryad_group_memberships (user_id, group_id) VALUES (1, 1)")
                .await
                .is_err(),
            "the pair (user_id, group_id) as literally worded by the Scenario cannot even be attempted: the drifted table has no group_id column"
        );
    }

    /// Scenario : « @down défait les trois tables dans l'ordre FK »
    #[tokio::test]
    async fn down_drops_the_three_tables() {
        let db = db_with_trio_up().await;
        insert_user(&db, "doomed-user").await.expect("user row inserts");
        insert_group(&db, "doomed-group")
            .await
            .expect("group row inserts");
        let user_id = user_id_by_subject(&db, "doomed-user").await;
        let group_id = group_id_by_name(&db, "doomed-group").await;
        insert_membership(&db, user_id, group_id)
            .await
            .expect("membership row inserts");
        let manager = SchemaManager::new(&db);

        Migration.down(&manager).await.expect("down returns Ok(())");

        for table in [USERS, GROUPS, MEMBERSHIPS] {
            assert!(
                !manager.has_table(table).await.expect("has_table probe succeeds"),
                "down must drop {table}"
            );
        }
        assert!(
            db.execute_unprepared("SELECT * FROM miryad_users").await.is_err()
                && db
                    .execute_unprepared("SELECT * FROM miryad_groups")
                    .await
                    .is_err()
                && db
                    .execute_unprepared("SELECT * FROM miryad_group_memberships")
                    .await
                    .is_err(),
            "every inserted row disappeared with its table"
        );
    }

    /// Scenario : « @down sur une base jamais posée échoue » — remontée stricte gelée.
    #[tokio::test]
    async fn down_errors_without_tables() {
        let db = fresh_db().await;
        let manager = SchemaManager::new(&db);

        for table in [USERS, GROUPS, MEMBERSHIPS] {
            assert!(
                !manager.has_table(table).await.expect("has_table probe succeeds"),
                "GIVEN: {table} does not exist before the call"
            );
        }
        assert!(
            Migration.down(&manager).await.is_err(),
            "down over a never-posed base must return a DbErr — DROP TABLE without IF EXISTS"
        );
        for table in [USERS, GROUPS, MEMBERSHIPS] {
            assert!(
                !manager.has_table(table).await.expect("has_table probe succeeds"),
                "none of the three tables exists after the failed down: {table}"
            );
        }
    }

    /// Scenario : « le tracking nomme cette migration par son stem de fichier »
    #[tokio::test]
    async fn applied_name_is_the_file_stem() {
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
            stem, "m20260822_000002_create_users_groups",
            "the file stem is the contractual registration name"
        );
        assert_eq!(
            applied.iter().filter(|name| name.as_str() == stem).count(),
            1,
            "get_applied_migrations counts exactly one applied migration named {stem}: {applied:?}"
        );
    }

    /// Scenario : « remontée complète puis nouvelle application reconstruit le schéma »
    #[tokio::test]
    async fn down_then_up_restores_the_schema() {
        let db = fresh_db().await;
        Migrator::up(&db, None)
            .await
            .expect("first full up applies every registered migration");
        Migrator::down(&db, None)
            .await
            .expect("full down undoes every applied migration in reverse order");
        Migrator::up(&db, None)
            .await
            .expect("the second full up returns Ok(()) over the fully rolled-back base");

        let manager = SchemaManager::new(&db);
        for table in [USERS, GROUPS, MEMBERSHIPS] {
            assert!(
                manager.has_table(table).await.expect("has_table probe succeeds"),
                "{table} is laid down again after the rollback/reapply cycle"
            );
        }
        let applied: Vec<String> = Migrator::get_applied_migrations(&db)
            .await
            .expect("applied list is readable")
            .iter()
            .map(sea_orm_migration::Migration::name)
            .map(str::to_string)
            .collect();
        assert!(
            applied
                .iter()
                .any(|name| name == "m20260822_000002_create_users_groups"),
            "get_applied_migrations recomptes m20260822_000002_create_users_groups: {applied:?}"
        );
    }

    /// Scenario : « sans COLLATE déclaré, `Admin` n'est pas `admin` sur le backend des tests »
    #[tokio::test]
    async fn names_are_case_binary_on_sqlite() {
        let db = fresh_db().await;
        Migrator::up(&db, None)
            .await
            .expect("the full Migrator migrates the base — the 000003 seed posed the group admin");

        insert_group(&db, "Admin")
            .await
            .expect("insertion of Admin is Ok: two distinct rows admin and Admin coexist under the UNIQUE, binary comparison on SQLite");
        assert_eq!(
            count(&db, "SELECT COUNT(*) AS n FROM miryad_groups").await,
            2,
            "admin and Admin coexist — no case normalisation exists in this DDL"
        );
        assert!(
            insert_group(&db, "Admin").await.is_err(),
            "a second insertion of Admin fails with a DbErr: the uniqueness does compare, bit for bit"
        );
    }
}
