//! Migrations SeaORM internes du module `lymaille` (`src/lymaille/migration.sdd`).
//!
//! Phase test-first : les tests des `Scenario` sont écrits avant le comportement de production.

use sea_orm_migration::prelude::*;

/// Nom de migration `SeaORM` (`src/lymaille/migration.sdd` `Must`), explicite : le
/// `DeriveMigrationName` donnerait le stem du fichier (`migration`).
const NAME: &str = "m20261103_000001_create_lymaille_tables";

/// Préfixe complet des tables du module (`src/lymaille/migration.sdd` `Must`).
const TABLE_PREFIX: &str = "miryad_lymaille_";

/// Migrations du module à assembler par l'hôte (`src/migration/mod.sdd`, amendement `lymaille`).
#[must_use]
pub fn migrations() -> Vec<Box<dyn sea_orm_migration::MigrationTrait>> {
    vec![Box::new(Migration)]
}

/// Migration unique du module : pose les dix tables puis les index.
struct Migration;

/// Nom de table complet : `TABLE_PREFIX` + suffixe.
fn table_name(suffix: &str) -> String {
    format!("{TABLE_PREFIX}{suffix}")
}

/// Colonne `id` INTEGER PK auto-incrémentée (`Must`).
fn id() -> ColumnDef {
    ColumnDef::new(Alias::new("id"))
        .integer()
        .not_null()
        .auto_increment()
        .primary_key()
        .to_owned()
}

/// Colonne texte `NOT NULL` (identifiants, kinds, références).
fn text(name: &str) -> ColumnDef {
    ColumnDef::new(Alias::new(name)).text().not_null().to_owned()
}

/// Colonne texte `NOT NULL UNIQUE` — unicité monocolonne du catalogue et des noms
/// (`Must` : `agent_id`/`toolset_id`/`name` **UNIQUE**).
fn unique_text(name: &str) -> ColumnDef {
    ColumnDef::new(Alias::new(name))
        .text()
        .not_null()
        .unique_key()
        .to_owned()
}

/// Colonne `INTEGER NOT NULL` (`owner_id`, `users.id` sans `FOREIGN KEY`, `Must`).
fn owner(name: &str) -> ColumnDef {
    ColumnDef::new(Alias::new(name)).integer().not_null().to_owned()
}

/// Colonne JSON `NOT NULL` — la valeur sérialisée est la source de vérité (DSD `DEC-17`).
fn json(name: &str) -> ColumnDef {
    ColumnDef::new(Alias::new(name)).json().not_null().to_owned()
}

/// Colonne JSON nullable (`active` / `draft` des toolsets, DSD `DEC-18`).
fn opt_json(name: &str) -> ColumnDef {
    ColumnDef::new(Alias::new(name)).json().to_owned()
}

/// Colonne `TIMESTAMP WITH TIME ZONE NOT NULL`, sans défaut serveur (`Must not` : le
/// peuplement incombe au consommateur).
fn timestamp(name: &str) -> ColumnDef {
    ColumnDef::new(Alias::new(name))
        .timestamp_with_time_zone()
        .not_null()
        .to_owned()
}

/// Spécification d'une table : suffixe, colonnes, colonnes de la clé primaire. Une `id`
/// PK auto-incrémentée porte sa clé primaire en ligne ; les autres (dont les PK composées
/// de `sessions` et `memories`) sont posées par la liste `primary_key`.
struct TableSpec {
    suffix: &'static str,
    columns: Vec<ColumnDef>,
    primary_key: &'static [&'static str],
}

/// Les dix tables du `Must`, dans l'ordre de `up` (`down` le parcourt à l'envers). Aucune
/// `FOREIGN KEY` (couplage lâche, comme `owner_id` de `miryad_workflow_definitions`).
fn table_specs() -> Vec<TableSpec> {
    let mut specs = definition_table_specs();
    specs.extend(store_table_specs());
    specs
}

/// Tables des définitions : catalogue (sans propriétaire) et privé (`owner_id`), plus
/// `profiles` et `providers`.
fn definition_table_specs() -> Vec<TableSpec> {
    vec![
        TableSpec {
            suffix: "catalog_agents",
            columns: vec![
                id(),
                unique_text("agent_id"),
                text("kind"),
                text("profile"),
                json("definition"),
                timestamp("created_at"),
            ],
            primary_key: &[],
        },
        TableSpec {
            suffix: "agents",
            columns: vec![
                id(),
                owner("owner_id"),
                text("agent_id"),
                text("kind"),
                text("profile"),
                json("definition"),
                timestamp("created_at"),
            ],
            primary_key: &[],
        },
        TableSpec {
            suffix: "catalog_toolsets",
            columns: vec![
                id(),
                unique_text("toolset_id"),
                text("version"),
                opt_json("active"),
                opt_json("draft"),
                timestamp("created_at"),
            ],
            primary_key: &[],
        },
        TableSpec {
            suffix: "toolsets",
            columns: vec![
                id(),
                owner("owner_id"),
                text("toolset_id"),
                text("version"),
                opt_json("active"),
                opt_json("draft"),
                timestamp("created_at"),
            ],
            primary_key: &[],
        },
        TableSpec {
            suffix: "profiles",
            columns: vec![
                id(),
                unique_text("name"),
                json("profile"),
                timestamp("created_at"),
            ],
            primary_key: &[],
        },
        TableSpec {
            suffix: "providers",
            columns: vec![
                id(),
                unique_text("name"),
                text("kind"),
                text("level"),
                json("provider"),
                timestamp("created_at"),
            ],
            primary_key: &[],
        },
    ]
}

/// Tables des stores d'exécution : types et contextes nommés (PK `name`), `sessions` et
/// `memories` (PK composées).
fn store_table_specs() -> Vec<TableSpec> {
    vec![
        TableSpec {
            suffix: "context_types",
            columns: vec![text("name"), json("context_type")],
            primary_key: &["name"],
        },
        TableSpec {
            suffix: "contexts",
            columns: vec![text("name"), json("context")],
            primary_key: &["name"],
        },
        TableSpec {
            suffix: "sessions",
            columns: vec![
                text("subject"),
                text("session_id"),
                text("kind"),
                text("state"),
                timestamp("updated_at"),
            ],
            primary_key: &["subject", "session_id"],
        },
        TableSpec {
            suffix: "memories",
            columns: vec![
                text("subject"),
                text("level"),
                text("reference"),
                text("memory_id"),
                json("memory"),
            ],
            primary_key: &["subject", "level", "reference", "memory_id"],
        },
    ]
}

/// Contraintes d'unicité composites posées comme index nommés (`Must` : le privé est
/// `UNIQUE(owner_id, …)`, pas une unicité monocolonne). Noms choisis à l'implémentation, la
/// spec ne les nomme pas (précédent `matrix::store::migration`).
const UNIQUE_INDEXES: [(&str, &str, &[&str]); 2] = [
    (
        "uq_miryad_lymaille_agents_owner_agent",
        "agents",
        &["owner_id", "agent_id"],
    ),
    (
        "uq_miryad_lymaille_toolsets_owner_toolset",
        "toolsets",
        &["owner_id", "toolset_id"],
    ),
];

/// Index de recherche (`Must`) : (`subject`, `updated_at`) sur `sessions`.
const INDEXES: [(&str, &str, &[&str]); 1] = [(
    "idx_miryad_lymaille_sessions_subject_updated",
    "sessions",
    &["subject", "updated_at"],
)];

impl MigrationName for Migration {
    fn name(&self) -> &str {
        NAME
    }
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        for spec in table_specs() {
            let mut statement = Table::create();
            statement
                .table(Alias::new(table_name(spec.suffix)))
                .if_not_exists();
            for column in spec.columns {
                statement.col(column);
            }
            if !spec.primary_key.is_empty() {
                let mut primary_key = Index::create();
                for column in spec.primary_key {
                    primary_key.col(Alias::new(*column));
                }
                statement.primary_key(&mut primary_key);
            }
            manager.create_table(statement.take()).await?;
        }
        for (name, suffix, columns) in UNIQUE_INDEXES {
            let mut index = Index::create();
            index
                .unique()
                .if_not_exists()
                .name(name)
                .table(Alias::new(table_name(suffix)));
            for column in columns {
                index.col(Alias::new(*column));
            }
            manager.create_index(index.take()).await?;
        }
        for (name, suffix, columns) in INDEXES {
            let mut index = Index::create();
            index
                .if_not_exists()
                .name(name)
                .table(Alias::new(table_name(suffix)));
            for column in columns {
                index.col(Alias::new(*column));
            }
            manager.create_index(index.take()).await?;
        }
        Ok(())
    }

    /// Remontée stricte, non idempotent (règle de `src/migration/mod.sdd`) : `drop_table` sans
    /// `if_exists`, tables dans l'ordre inverse de `up`.
    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        for spec in table_specs().iter().rev() {
            manager
                .drop_table(
                    Table::drop()
                        .table(Alias::new(table_name(spec.suffix)))
                        .to_owned(),
                )
                .await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{Migration, TABLE_PREFIX, migrations};
    use sea_orm::{ConnectionTrait, Database, DatabaseConnection};
    use sea_orm_migration::prelude::*;

    /// Les dix tables du `Must`, suffixe + colonnes déclarées, dans l'ordre de `up`.
    const TABLES: [(&str, &[&str]); 10] = [
        (
            "catalog_agents",
            &["id", "agent_id", "kind", "profile", "definition", "created_at"],
        ),
        (
            "agents",
            &[
                "id",
                "owner_id",
                "agent_id",
                "kind",
                "profile",
                "definition",
                "created_at",
            ],
        ),
        (
            "catalog_toolsets",
            &["id", "toolset_id", "version", "active", "draft", "created_at"],
        ),
        (
            "toolsets",
            &[
                "id",
                "owner_id",
                "toolset_id",
                "version",
                "active",
                "draft",
                "created_at",
            ],
        ),
        ("profiles", &["id", "name", "profile", "created_at"]),
        (
            "providers",
            &["id", "name", "kind", "level", "provider", "created_at"],
        ),
        ("context_types", &["name", "context_type"]),
        ("contexts", &["name", "context"]),
        (
            "sessions",
            &["subject", "session_id", "kind", "state", "updated_at"],
        ),
        (
            "memories",
            &["subject", "level", "reference", "memory_id", "memory"],
        ),
    ];

    struct LymailleMigrator;

    impl MigratorTrait for LymailleMigrator {
        fn migrations() -> Vec<Box<dyn MigrationTrait>> {
            migrations()
        }
    }

    fn table(suffix: &str) -> String {
        format!("{TABLE_PREFIX}{suffix}")
    }

    async fn fresh_db() -> DatabaseConnection {
        Database::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite connects")
    }

    /// Ligne d'`agents` (privé) : `owner_id` + `agent_id` et les colonnes NOT NULL.
    async fn insert_agent(
        db: &DatabaseConnection,
        owner_id: i64,
        agent_id: &str,
    ) -> Result<(), sea_orm::DbErr> {
        db.execute_unprepared(&format!(
            "INSERT INTO {TABLE_PREFIX}agents (owner_id, agent_id, kind, profile, definition, created_at) \
             VALUES ({owner_id}, '{agent_id}', 'work', 'p', '{{}}', '2026-11-03T00:00:00Z')"
        ))
        .await
        .map(|_| ())
    }

    /// Ligne de `catalog_agents` : `agent_id` (`G_…`) et les colonnes NOT NULL, sans propriétaire.
    async fn insert_catalog_agent(db: &DatabaseConnection, agent_id: &str) -> Result<(), sea_orm::DbErr> {
        db.execute_unprepared(&format!(
            "INSERT INTO {TABLE_PREFIX}catalog_agents (agent_id, kind, profile, definition, created_at) \
             VALUES ('{agent_id}', 'work', 'p', '{{}}', '2026-11-03T00:00:00Z')"
        ))
        .await
        .map(|_| ())
    }

    /// Scenario : « up crée les dix tables avec le préfixe ». Sonde aussi l'index
    /// (`subject`, `updated_at`) de `sessions` que le `Must` déclare.
    #[tokio::test]
    async fn up_creates_the_ten_prefixed_tables_with_their_columns() {
        let db = fresh_db().await;
        LymailleMigrator::up(&db, None)
            .await
            .expect("up returns Ok(()) over an empty in-memory base");
        let manager = SchemaManager::new(&db);
        for (suffix, columns) in TABLES {
            let name = table(suffix);
            assert!(
                manager
                    .has_table(name.as_str())
                    .await
                    .expect("has_table probe succeeds"),
                "la table {name} existe après up"
            );
            for column in columns {
                assert!(
                    manager
                        .has_column(name.as_str(), *column)
                        .await
                        .expect("has_column probe succeeds"),
                    "la table {name} porte la colonne {column}"
                );
            }
        }
        assert!(
            manager
                .has_index(
                    table("sessions").as_str(),
                    "idx_miryad_lymaille_sessions_subject_updated"
                )
                .await
                .expect("has_index probe succeeds"),
            "l'index (subject, updated_at) existe sur miryad_lymaille_sessions"
        );
    }

    /// Scenario : « down les supprime toutes ». Précondition explicite : up a posé les tables,
    /// sinon le test passerait trivialement sur une base vide.
    #[tokio::test]
    async fn down_drops_every_lymaille_table() {
        let db = fresh_db().await;
        LymailleMigrator::up(&db, None).await.expect("up returns Ok(())");
        let manager = SchemaManager::new(&db);
        for (suffix, _) in TABLES {
            let name = table(suffix);
            assert!(
                manager
                    .has_table(name.as_str())
                    .await
                    .expect("has_table probe succeeds"),
                "précondition : {name} existe après up"
            );
        }
        LymailleMigrator::down(&db, None)
            .await
            .expect("down returns Ok(())");
        for (suffix, _) in TABLES {
            let name = table(suffix);
            assert!(
                !manager
                    .has_table(name.as_str())
                    .await
                    .expect("has_table probe succeeds"),
                "down supprime {name}"
            );
        }
    }

    /// `Must` : « `up` sur une base où une table existe déjà est idempotent ». Le rejeu est
    /// effectif : le second `up` passe par `Migration.up` **directement** (motif du précédent
    /// `m20260930_000001_index_api_tokens_subject`), sinon le `Migrator` ne trouve aucun pending
    /// dans sa table de tracking et n'émet aucun DDL — le test ne dirait rien de
    /// l'`if_not_exists`.
    #[tokio::test]
    async fn up_is_replayable_on_a_database_where_the_tables_already_exist() {
        let db = fresh_db().await;
        let manager = SchemaManager::new(&db);
        LymailleMigrator::up(&db, None).await.expect("first up succeeds");
        Migration
            .up(&manager)
            .await
            .expect("replayed up returns Ok(()) thanks to if_not_exists");
        for (suffix, _) in TABLES {
            let name = table(suffix);
            assert!(
                manager
                    .has_table(name.as_str())
                    .await
                    .expect("has_table probe succeeds"),
                "{name} subsiste au rejeu de up"
            );
        }
    }

    /// Scenario : « l'unicité de `agent_id` est par table, et par propriétaire pour le privé ».
    #[tokio::test]
    async fn agent_id_is_unique_per_table_and_per_owner_for_the_private_table() {
        let db = fresh_db().await;
        LymailleMigrator::up(&db, None).await.expect("up returns Ok(())");

        insert_agent(&db, 1, "x")
            .await
            .expect("first private agent x for owner 1 inserts");
        let second = insert_agent(&db, 1, "x").await;
        assert!(
            second.is_err(),
            "le second x du même propriétaire échoue — UNIQUE(owner_id, agent_id)"
        );
        insert_agent(&db, 2, "x")
            .await
            .expect("x chez un propriétaire différent coexiste — l'unicité est par propriétaire");

        insert_catalog_agent(&db, "G_x")
            .await
            .expect("first catalog agent G_x inserts");
        let second = insert_catalog_agent(&db, "G_x").await;
        assert!(
            second.is_err(),
            "le second G_x du catalogue échoue — UNIQUE(agent_id) au catalogue"
        );
    }
}
