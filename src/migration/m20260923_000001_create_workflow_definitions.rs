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
                    .col(ColumnDef::new(WorkflowDefinition::Name).string().not_null())
                    .col(ColumnDef::new(WorkflowDefinition::Steps).json().not_null())
                    .col(ColumnDef::new(WorkflowDefinition::OwnerId).integer())
                    .col(
                        ColumnDef::new(WorkflowDefinition::CreatedAt)
                            .timestamp_with_time_zone()
                            .not_null(),
                    )
                    .to_owned(),
            )
            .await?;
        // Unicité du nom par propriétaire (#28) : l'index composite borne les définitions
        // possédées ; en SQL `NULL` n'égale pas `NULL` dans un index composite, donc les
        // définitions sans propriétaire lui échappent — c'est l'index partiel ci-dessous qui les
        // borne (`m20260923_000001_create_workflow_definitions.sdd` `Must`).
        manager
            .create_index(
                Index::create()
                    .unique()
                    .if_not_exists()
                    .name("uq_miryad_workflow_definitions_owner_name")
                    .table(WorkflowDefinition::Table)
                    .col(WorkflowDefinition::OwnerId)
                    .col(WorkflowDefinition::Name)
                    .to_owned(),
            )
            .await?;
        // `Index::create().unique().and_where(...)` — partiel, rendu `WHERE ...` vérifié au
        // source de sea-query `1.0.2` (`prepare_filter` des constructeurs Postgres et SQLite),
        // donc émis sur les deux backends sans repli `execute_unprepared`.
        manager
            .create_index(
                Index::create()
                    .unique()
                    .if_not_exists()
                    .name("uq_miryad_workflow_definitions_name_unowned")
                    .table(WorkflowDefinition::Table)
                    .col(WorkflowDefinition::Name)
                    .and_where(Expr::col(WorkflowDefinition::OwnerId).is_null())
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

// Phase test-first : les tests des onze `Scenario` de
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

    /// Ligne complète **possédée** (`owner_id` fourni) — forme des `Scenario` #28 de partage du
    /// `name` entre propriétaires.
    async fn insert_owned_row(
        db: &DatabaseConnection,
        name: &str,
        owner_id: i64,
    ) -> Result<(), sea_orm::DbErr> {
        db.execute_unprepared(&format!(
            "INSERT INTO {TABLE} (name, steps, owner_id, created_at) \
             VALUES ('{name}', '[{{\"step\": \"notify\"}}]', {owner_id}, '2026-09-23T00:00:00Z')"
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

    /// Scenario: même name pour un même propriétaire est rejeté — l'index unique
    /// `uq_miryad_workflow_definitions_owner_name` sur (`owner_id`, `name`) (#28). Remplace le
    /// test « name dupliqué est rejeté » d'avant #28 (unicité globale).
    #[tokio::test]
    async fn same_owner_duplicate_name_is_rejected() {
        let db = db_with_table_up().await;

        insert_owned_row(&db, "deploy-app", 1)
            .await
            .expect("first owned row (deploy-app, owner 1) is Ok(())");
        let second = insert_owned_row(&db, "deploy-app", 1).await;
        assert!(
            second.is_err(),
            "second row sharing both owner_id and name must fail with a DbErr"
        );

        let rows = select(
            &db,
            "SELECT COUNT(*) AS n FROM miryad_workflow_definitions \
             WHERE name = 'deploy-app' AND owner_id = 1",
        )
        .await;
        let count = rows
            .first()
            .expect("COUNT always returns one row")
            .try_get::<i64>("", "n")
            .expect("count decodes as integer");
        assert_eq!(count, 1, "exactly one row carries the (owner_id, name) pair");
    }

    /// Scenario: même name pour deux propriétaires différents est accepté (#28) — l'index est
    /// composite sur (`owner_id`, `name`) et non plus monocolonne sur `name`.
    #[tokio::test]
    async fn same_name_for_two_different_owners_is_accepted() {
        let db = db_with_table_up().await;

        insert_owned_row(&db, "ci", 1)
            .await
            .expect("row (ci, owner 1) is Ok(())");
        insert_owned_row(&db, "ci", 2)
            .await
            .expect("row (ci, owner 2) must coexist — uniqueness is per owner (#28)");
    }

    /// Scenario: même name sans propriétaire est rejeté (#28) — l'index composite laisse passer
    /// deux `NULL` (`NULL` n'égale pas `NULL` en SQL) ; l'index partiel `WHERE owner_id IS NULL`
    /// est ce qui ferme ce trou.
    #[tokio::test]
    async fn same_name_without_owner_is_rejected() {
        let db = db_with_table_up().await;

        insert_full_row(&db, "ci")
            .await
            .expect("first unowned row (ci, owner_id omitted) is Ok(())");
        let second = insert_full_row(&db, "ci").await;
        assert!(
            second.is_err(),
            "second unowned row with the same name must fail with a DbErr — the partial index \
             WHERE owner_id IS NULL does the work the composite index cannot (#28)"
        );
    }

    /// Scenario: un name sans propriétaire et le même name possédé coexistent (#28) — l'index
    /// partiel ne contraint que les lignes `owner_id IS NULL`, l'index composite ne distingue pas
    /// NULL de 1 : les deux formes coexistent.
    #[tokio::test]
    async fn unowned_and_owned_name_coexist() {
        let db = db_with_table_up().await;

        insert_full_row(&db, "ci")
            .await
            .expect("unowned row (ci, owner_id omitted) is Ok(())");
        insert_owned_row(&db, "ci", 1)
            .await
            .expect("owned row (ci, owner 1) must coexist with the unowned one (#28)");
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
