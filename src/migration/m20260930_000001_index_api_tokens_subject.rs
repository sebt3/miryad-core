use sea_orm_migration::prelude::*;

/// Nom unique de l'index posé par ce fichier (`m20260930_000001_index_api_tokens_subject.sdd`
/// `Must` : littéral unique du fichier, jamais recalculé par un `Iden` sur l'entité — `SeaORM` 2
/// ne porte pas d'`Index` sur `DeriveEntityModel`).
const INDEX_NAME: &str = "idx_miryad_api_tokens_subject";

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_index(
                Index::create()
                    .name(INDEX_NAME)
                    .table(Alias::new("miryad_api_tokens"))
                    .col(Alias::new("subject"))
                    .if_not_exists()
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_index(
                Index::drop()
                    .name(INDEX_NAME)
                    .table(Alias::new("miryad_api_tokens"))
                    .to_owned(),
            )
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::{INDEX_NAME, Migration};
    use crate::migration::Migrator;
    use sea_orm::{ConnectionTrait, Database, DatabaseConnection, DbBackend, Statement};
    use sea_orm_migration::prelude::*;

    const TABLE: &str = "miryad_api_tokens";
    const STEM: &str = "m20260930_000001_index_api_tokens_subject";

    /// Base vide passée par le `Migrator` complet (`Migrator::up` sur `sqlite::memory:` — même
    /// harnais que les sœurs `m20260923_000001_*` ; sous feature `workflow` la 004 s'ajoute,
    /// sans effet sur les assertions de ce fichier).
    async fn migrated_db() -> DatabaseConnection {
        let db = Database::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite connects");
        Migrator::up(&db, None).await.expect("migrations apply cleanly");
        db
    }

    /// `PRAGMA index_list` de `miryad_api_tokens` : paires (nom, unique) — sonde du `Scenario`
    /// « up pose un index nommé » (la spec autorise « `PRAGMA index_list` côté `SQLite` »).
    async fn index_list(db: &DatabaseConnection) -> Vec<(String, i64)> {
        let rows = db
            .query_all_raw(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                format!("PRAGMA index_list('{TABLE}')"),
                Vec::<sea_orm::Value>::new(),
            ))
            .await
            .expect("PRAGMA index_list succeeds");
        rows.iter()
            .map(|row| {
                (
                    row.try_get::<String>("", "name").expect("index name is a string"),
                    row.try_get::<i64>("", "unique")
                        .expect("unique flag is an integer"),
                )
            })
            .collect()
    }

    /// Colonnes de l'index sous son nom (`PRAGMA index_info`) — une seule ligne attendue.
    async fn index_columns(db: &DatabaseConnection) -> Vec<String> {
        let rows = db
            .query_all_raw(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                format!("PRAGMA index_info('{INDEX_NAME}')"),
                Vec::<sea_orm::Value>::new(),
            ))
            .await
            .expect("PRAGMA index_info succeeds");
        rows.iter()
            .map(|row| {
                row.try_get::<String>("", "name")
                    .expect("column name is a string")
            })
            .collect()
    }

    /// `Scenario` : « up pose un index nommé sur la colonne subject » — index non unique nommé
    /// `idx_miryad_api_tokens_subject` sur la seule colonne `subject`, et nom du stem enregistré
    /// dans la table de tracking.
    #[tokio::test]
    async fn up_poses_named_index_on_subject_column() {
        let db = migrated_db().await;

        let indexes = index_list(&db).await;
        let entry = indexes
            .iter()
            .find(|(name, _)| name == INDEX_NAME)
            .expect("named index idx_miryad_api_tokens_subject present after Migrator::up");
        assert_eq!(
            entry.1, 0,
            "the index is non-unique (several tokens per subject is the normal case)"
        );

        assert_eq!(
            index_columns(&db).await,
            vec!["subject".to_string()],
            "single-column index on subject"
        );

        let versions: Vec<String> = Migrator::get_migration_models(&db)
            .await
            .expect("tracking table is readable")
            .into_iter()
            .map(|model| model.version)
            .collect();
        assert!(
            versions.iter().any(|version| version == STEM),
            "le nom de l'index est tracking sous le stem {STEM} : {versions:?}"
        );
    }

    /// `Scenario` : « rejeu de up est idempotent, down est strict » — second `up` `Ok(())` sans
    /// effet (`if_not_exists`), premier `down` `Ok(())`, second `down` `Err` (strict, sans
    /// `if_exists`, règle `mod.sdd`).
    #[tokio::test]
    async fn up_replay_is_idempotent_and_down_is_strict() {
        let db = migrated_db().await;
        let manager = SchemaManager::new(&db);

        let before = index_list(&db).await;
        assert!(
            before.iter().any(|(name, _)| name == INDEX_NAME),
            "pré-condition : l'index est posé par le Migrator::up complet"
        );

        Migration
            .up(&manager)
            .await
            .expect("replayed up returns Ok(()) thanks to if_not_exists");
        assert_eq!(
            index_list(&db).await.len(),
            before.len(),
            "the replay leaves the index list unchanged"
        );

        Migration
            .down(&manager)
            .await
            .expect("first down returns Ok(()) and drops the index");
        assert!(
            !index_list(&db).await.iter().any(|(name, _)| name == INDEX_NAME),
            "down dropped the index"
        );

        let second = Migration.down(&manager).await;
        assert!(
            second.is_err(),
            "strict down (no if_exists) on a base without the index returns Err<DbErr> — règle mod.sdd"
        );
    }

    /// `Scenario` : « la migration suit l'ordre d'enregistrement attendu » — quatre entrées
    /// sous features par défaut dont le stem, après les trois internes ; cinq sous `workflow`.
    #[tokio::test]
    async fn get_migration_files_registers_the_index_migration_after_the_three_internals() {
        let files = Migrator::get_migration_files();
        let names: Vec<&str> = files.iter().map(sea_orm_migration::Migration::name).collect();

        #[cfg(not(feature = "workflow"))]
        assert_eq!(
            names,
            vec![
                "m20260822_000001_create_api_tokens",
                "m20260822_000002_create_users_groups",
                "m20260822_000003_seed_admin_group",
                STEM,
            ],
            "quatre entrées sous features par défaut, l'index en dernière position"
        );

        #[cfg(feature = "workflow")]
        assert_eq!(
            names,
            vec![
                "m20260822_000001_create_api_tokens",
                "m20260822_000002_create_users_groups",
                "m20260822_000003_seed_admin_group",
                "m20260923_000001_create_workflow_definitions",
                STEM,
            ],
            "cinq entrées sous workflow — la 004 gateée garde sa position (dernière enregistrée \
             gateée), l'index s'inscrit hors du branchement de feature, en fin de liste"
        );
    }
}
