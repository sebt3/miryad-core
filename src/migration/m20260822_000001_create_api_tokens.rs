use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(ApiToken::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(ApiToken::Id)
                            .integer()
                            .not_null()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(ColumnDef::new(ApiToken::Subject).string().not_null())
                    .col(ColumnDef::new(ApiToken::Name).string().not_null())
                    .col(
                        ColumnDef::new(ApiToken::TokenHash)
                            .string()
                            .not_null()
                            .unique_key(),
                    )
                    .col(
                        ColumnDef::new(ApiToken::CreatedAt)
                            .timestamp_with_time_zone()
                            .not_null(),
                    )
                    .col(ColumnDef::new(ApiToken::ExpiresAt).timestamp_with_time_zone())
                    .col(ColumnDef::new(ApiToken::LastUsedAt).timestamp_with_time_zone())
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(ApiToken::Table).to_owned())
            .await
    }
}

#[derive(DeriveIden)]
enum ApiToken {
    #[sea_orm(iden = "miryad_api_tokens")]
    Table,
    Id,
    Subject,
    Name,
    TokenHash,
    CreatedAt,
    ExpiresAt,
    LastUsedAt,
}

// Conversion test-first des douze `Scenario` de
// `m20260822_000001_create_api_tokens.sdd` — verrous du réel sur migration committée immuable.

#[cfg(test)]
mod tests {
    use super::Migration;
    use crate::migration::Migrator;
    use sea_orm::{ConnectionTrait, Database, DatabaseConnection, DbBackend, QueryResult, Statement};
    use sea_orm_migration::MigratorTrait;
    use sea_orm_migration::prelude::*;

    const TABLE: &str = "miryad_api_tokens";
    const COLUMNS: [&str; 7] = [
        "id",
        "subject",
        "name",
        "token_hash",
        "created_at",
        "expires_at",
        "last_used_at",
    ];

    async fn fresh_db() -> DatabaseConnection {
        Database::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite connects")
    }

    /// Base avec la table posée par un premier `up` réussi — pré-condition de six `Scenario`.
    async fn db_with_table_up() -> DatabaseConnection {
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

    /// Ligne complète sans `expires_at`/`last_used_at` (omises, donc NULL) — forme valide d'un
    /// token, pattern de `m20260923_000001_create_workflow_definitions.rs`.
    async fn insert_full_row(db: &DatabaseConnection, token_hash: &str) -> Result<(), sea_orm::DbErr> {
        db.execute_unprepared(&format!(
            "INSERT INTO miryad_api_tokens (subject, name, token_hash, created_at) \
             VALUES ('sub-1', 'ci-token', '{token_hash}', '2026-08-22T00:00:00Z')"
        ))
        .await
        .map(|_| ())
    }

    /// Ligne complète de toutes les colonnes NOT NULL SAUF `created_at` (omis) — branche
    /// « pas de défaut serveur » du Scenario dédié.
    async fn insert_without_created_at(db: &DatabaseConnection) -> Result<(), sea_orm::DbErr> {
        db.execute_unprepared(
            "INSERT INTO miryad_api_tokens (subject, name, token_hash) \
             VALUES ('sub-1', 'ci-token', 'no-default-probe')",
        )
        .await
        .map(|_| ())
    }

    /// Scenario : « @up pose `miryad_api_tokens` »
    #[tokio::test]
    async fn up_lays_miryad_api_tokens() {
        let db = fresh_db().await;
        let manager = SchemaManager::new(&db);

        Migration
            .up(&manager)
            .await
            .expect("up returns Ok(()) over an empty database");

        assert!(
            manager.has_table(TABLE).await.expect("has_table probe succeeds"),
            "up must pose {TABLE}"
        );
    }

    /// Scenario : « les sept colonnes existent après up »
    #[tokio::test]
    async fn up_lays_the_seven_columns() {
        let db = db_with_table_up().await;
        let manager = SchemaManager::new(&db);

        for column in COLUMNS {
            assert!(
                manager
                    .has_column(TABLE, column)
                    .await
                    .expect("has_column probe succeeds"),
                "column {column} must exist after up"
            );
        }
    }

    /// Scenario : « id auto-incrémente la clé primaire »
    #[tokio::test]
    async fn id_auto_increment_primary_key() {
        let db = db_with_table_up().await;
        insert_full_row(&db, "hash-a")
            .await
            .expect("first complete row (no id) inserts");
        insert_full_row(&db, "hash-b")
            .await
            .expect("second complete row (no id) inserts");

        let rows = select(&db, &format!("SELECT id FROM {TABLE} ORDER BY id ASC")).await;
        let ids: Vec<i64> = rows
            .iter()
            .map(|row| row.try_get::<i64>("", "id").expect("id present and integer"))
            .collect();

        assert_eq!(ids.len(), 2, "both inserted rows are read back");
        assert!(
            ids.first()
                .is_some_and(|first| ids.get(1).is_some_and(|second| second > first)),
            "ids present, distinct and strictly increasing in insertion order: {ids:?}"
        );
    }

    /// Scenario : « `token_hash` dupliqué est rejeté »
    #[tokio::test]
    async fn token_hash_rejects_duplicate() {
        let db = db_with_table_up().await;

        insert_full_row(&db, "abc123")
            .await
            .expect("first insertion with token_hash abc123 is Ok(())");
        let second = insert_full_row(&db, "abc123").await;
        assert!(
            second.is_err(),
            "second insertion sharing the token_hash must fail with a DbErr (message backend nu)"
        );

        assert_eq!(
            count(
                &db,
                "SELECT COUNT(*) AS n FROM miryad_api_tokens WHERE token_hash = 'abc123'"
            )
            .await,
            1,
            "exactly one row carries the duplicated token_hash after both attempts"
        );
    }

    /// Scenario : « `expires_at` et `last_used_at` acceptent NULL »
    #[tokio::test]
    async fn optional_timestamps_accept_null() {
        let db = db_with_table_up().await;

        // insert_full_row fournit toutes les colonnes NOT NULL mais omet expires_at/last_used_at.
        insert_full_row(&db, "null-timestamps")
            .await
            .expect("row without expires_at/last_used_at inserts as Ok(())");

        let rows = select(&db, "SELECT expires_at, last_used_at FROM miryad_api_tokens").await;
        let row = rows.first().expect("the inserted row is read back");
        assert_eq!(
            row.try_get::<Option<String>>("", "expires_at")
                .expect("expires_at decodes as a nullable column"),
            None,
            "expires_at reads back as None"
        );
        assert_eq!(
            row.try_get::<Option<String>>("", "last_used_at")
                .expect("last_used_at decodes as a nullable column"),
            None,
            "last_used_at reads back as None"
        );
    }

    /// Scenario : « `created_at` n'a pas de défaut serveur »
    #[tokio::test]
    async fn created_at_has_no_server_default() {
        let db = db_with_table_up().await;

        assert!(
            insert_without_created_at(&db).await.is_err(),
            "insertion without created_at must fail: the column is NOT NULL with no server default"
        );
        insert_full_row(&db, "with-created-at")
            .await
            .expect("the same row providing created_at inserts as Ok(()) with no server default");
    }

    /// Scenario : « up est rejouable sur une base déjà conforme »
    #[tokio::test]
    async fn up_rerunnable() {
        let db = fresh_db().await;
        let manager = SchemaManager::new(&db);

        Migration
            .up(&manager)
            .await
            .expect("first up succeeds over an empty database");
        Migration
            .up(&manager)
            .await
            .expect("second up over the already-conforming table returns Ok(()) with no prior check");

        for column in COLUMNS {
            assert!(
                manager
                    .has_column(TABLE, column)
                    .await
                    .expect("has_column probe succeeds"),
                "column {column} must remain present after the replayed up"
            );
        }
    }

    /// Scenario : « une table préexistante en dérive est acquittée »
    #[tokio::test]
    async fn up_tolerates_drifted_table() {
        let db = fresh_db().await;
        db.execute_unprepared(&format!("CREATE TABLE {TABLE} (foo TEXT)"))
            .await
            .expect("a drifted relation miryad_api_tokens with a single foo column is created manually");

        let manager = SchemaManager::new(&db);
        Migration
            .up(&manager)
            .await
            .expect("up returns Ok(()) — IF NOT EXISTS skips without reading the existing shape");

        assert!(
            manager
                .has_column(TABLE, "foo")
                .await
                .expect("has_column probe succeeds"),
            "has_column confirms foo: the drifted table was skipped, not replaced"
        );
        assert!(
            !manager
                .has_column(TABLE, "subject")
                .await
                .expect("has_column probe succeeds"),
            "has_column confirms subject nowhere: drift stays unchecked, unrepaired, unreported"
        );
    }

    /// Scenario : « @down défait la table `miryad_api_tokens` »
    #[tokio::test]
    async fn down_drops_the_table() {
        let db = db_with_table_up().await;
        insert_full_row(&db, "doomed-token")
            .await
            .expect("one valid row inserted before down");
        let manager = SchemaManager::new(&db);

        Migration.down(&manager).await.expect("down returns Ok(())");

        assert!(
            !manager.has_table(TABLE).await.expect("has_table probe succeeds"),
            "down must drop {TABLE}"
        );
        assert!(
            db.execute_unprepared("SELECT * FROM miryad_api_tokens")
                .await
                .is_err(),
            "the inserted row and its unique token_hash disappeared with the dropped table"
        );
    }

    /// Scenario : « @down sur une table absente échoue » — remontée strict gelé, sans IF EXISTS.
    #[tokio::test]
    async fn down_errors_without_table() {
        let db = fresh_db().await;
        let manager = SchemaManager::new(&db);

        assert!(
            Migration.down(&manager).await.is_err(),
            "down over a base where up was never applied must return a DbErr — DROP TABLE without IF EXISTS"
        );
        assert!(
            !manager.has_table(TABLE).await.expect("has_table probe succeeds"),
            "no miryad_api_tokens table exists after the failed down"
        );
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
            stem, "m20260822_000001_create_api_tokens",
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
    async fn down_then_up_restores_schema() {
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
        assert!(
            manager.has_table(TABLE).await.expect("has_table probe succeeds"),
            "miryad_api_tokens is laid down again after the rollback/reapply cycle"
        );
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
                .any(|name| name == "m20260822_000001_create_api_tokens"),
            "get_applied_migrations recomptes m20260822_000001_create_api_tokens: {applied:?}"
        );
    }
}
