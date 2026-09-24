use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(WorkflowDefinition::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(WorkflowDefinition::Id)
                            .integer()
                            .not_null()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(
                        ColumnDef::new(WorkflowDefinition::Name)
                            .string()
                            .not_null()
                            .unique_key(),
                    )
                    .col(ColumnDef::new(WorkflowDefinition::Steps).json().not_null())
                    .col(ColumnDef::new(WorkflowDefinition::OwnerId).integer())
                    .col(
                        ColumnDef::new(WorkflowDefinition::CreatedAt)
                            .timestamp_with_time_zone()
                            .not_null(),
                    )
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(WorkflowDefinition::Table).to_owned())
            .await
    }
}

#[derive(DeriveIden)]
enum WorkflowDefinition {
    #[sea_orm(iden = "miryad_workflow_definitions")]
    Table,
    Id,
    Name,
    Steps,
    OwnerId,
    CreatedAt,
}

// Phase test-first : les tests des huit `Scenario` de
// `m20260923_000001_create_workflow_definitions.sdd` sont écrits avant le comportement de
// production (le `Migration` et son `DeriveIden` suivent dans ce fichier).

#[cfg(test)]
mod tests {
    use super::Migration;
    use sea_orm::{ConnectionTrait, Database, DatabaseConnection, DbBackend, QueryResult, Statement};
    use sea_orm_migration::prelude::*;

    const TABLE: &str = "miryad_workflow_definitions";
    const COLUMNS: [&str; 5] = ["id", "name", "steps", "owner_id", "created_at"];

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

    /// Ligne complète hors `owner_id` (omise, donc NULL) — forme valide d'une définition.
    async fn insert_full_row(db: &DatabaseConnection, name: &str) -> Result<(), sea_orm::DbErr> {
        db.execute_unprepared(&format!(
            "INSERT INTO {TABLE} (name, steps, created_at) \
             VALUES ('{name}', '[{{\"step\": \"notify\"}}]', '2026-09-23T00:00:00Z')"
        ))
        .await
        .map(|_| ())
    }

    async fn select(db: &DatabaseConnection, sql: &str) -> Vec<QueryResult> {
        db.query_all_raw(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            sql,
            Vec::<sea_orm::Value>::new(),
        ))
        .await
        .expect("raw SELECT over the table succeeds")
    }

    /// Scenario: up pose `miryad_workflow_definitions`
    #[tokio::test]
    async fn up_poses_miryad_workflow_definitions() {
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

    /// Scenario: les cinq colonnes existent après up
    #[tokio::test]
    async fn the_five_contract_columns_exist_after_up() {
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

    /// Scenario: id auto-incrémente la clé primaire
    #[tokio::test]
    async fn id_auto_increments_the_primary_key() {
        let db = db_with_table_up().await;
        insert_full_row(&db, "deploy-app")
            .await
            .expect("first complete row (owner_id omitted) inserts");
        insert_full_row(&db, "rollback-app")
            .await
            .expect("second complete row (owner_id omitted) inserts");

        let rows = select(&db, "SELECT id FROM miryad_workflow_definitions ORDER BY id ASC").await;
        let mut ids = Vec::new();
        for row in &rows {
            ids.push(row.try_get::<i64>("", "id").expect("id present and integer"));
        }

        assert_eq!(ids.len(), 2, "both inserted rows are read back");
        assert!(
            ids.first()
                .is_some_and(|first| ids.get(1).is_some_and(|second| second > first)),
            "ids present, distinct and strictly increasing: {ids:?}"
        );
    }

    /// Scenario: name dupliqué est rejeté
    #[tokio::test]
    async fn duplicate_name_is_rejected() {
        let db = db_with_table_up().await;

        insert_full_row(&db, "deploy-app")
            .await
            .expect("first insertion with name deploy-app is Ok(())");
        let second = insert_full_row(&db, "deploy-app").await;
        assert!(
            second.is_err(),
            "second insertion sharing the name must fail with a DbErr"
        );

        let rows = select(
            &db,
            "SELECT COUNT(*) AS n FROM miryad_workflow_definitions WHERE name = 'deploy-app'",
        )
        .await;
        let count = rows
            .first()
            .expect("COUNT always returns one row")
            .try_get::<i64>("", "n")
            .expect("count decodes as integer");
        assert_eq!(count, 1, "exactly one row carries the duplicated name");
    }

    /// Scenario: `owner_id` accepte NULL
    #[tokio::test]
    async fn owner_id_accepts_null() {
        let db = db_with_table_up().await;

        // insert_full_row fournit toutes les colonnes NOT NULL mais omet owner_id.
        insert_full_row(&db, "deploy-app")
            .await
            .expect("row without owner_id inserts as Ok(())");

        let rows = select(&db, "SELECT owner_id FROM miryad_workflow_definitions").await;
        let owner_id = rows
            .first()
            .expect("the inserted row is read back")
            .try_get::<Option<i64>>("", "owner_id")
            .expect("owner_id decodes as a nullable integer");
        assert_eq!(owner_id, None, "owner_id reads back as None");
    }

    /// Scenario: steps accepte un JSON arbitraire, sans validation de forme au niveau colonne
    #[tokio::test]
    async fn steps_accepts_arbitrary_json() {
        let db = db_with_table_up().await;

        // Forme qui échouerait à workflow::definition::validate_dag : la colonne ne valide pas la
        // structure, seule la présence d'une valeur JSON est garantie par le schéma.
        db.execute_unprepared(
            "INSERT INTO miryad_workflow_definitions (name, steps, created_at) \
             VALUES ('not-a-dag', '{\"not\":\"a step list\"}', '2026-09-23T00:00:00Z')",
        )
        .await
        .expect("steps stores arbitrary JSON — DAG validation is before_create's job, not the schema's");
    }

    /// Scenario: up est rejouable sur une base déjà conforme
    #[tokio::test]
    async fn up_is_replayable_on_a_conforming_database() {
        let db = fresh_db().await;
        let manager = SchemaManager::new(&db);

        Migration
            .up(&manager)
            .await
            .expect("first up succeeds over an empty database");
        Migration
            .up(&manager)
            .await
            .expect("second up over the already-conforming table returns Ok(())");

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

    /// Scenario: down défait la table
    #[tokio::test]
    async fn down_undoes_the_table() {
        let db = db_with_table_up().await;
        insert_full_row(&db, "deploy-app")
            .await
            .expect("one valid row inserted before down");
        let manager = SchemaManager::new(&db);

        Migration.down(&manager).await.expect("down returns Ok(())");

        assert!(
            !manager.has_table(TABLE).await.expect("has_table probe succeeds"),
            "down must drop {TABLE}"
        );
    }
}
