//! Migrations du store de chiffrement `matrix-sdk` (`src/matrix/store/migration.sdd`).
//!
//! Phase test-first : les tests des `Scenario` sont écrits avant le comportement de production.

use sea_orm_migration::prelude::*;

/// Préfixe unique des tables du store (`src/matrix/store/mod.sdd`, `TABLE_PREFIX`).
const TABLE_PREFIX: &str = "miryad_matrix_";

/// Nom de migration `SeaORM` (`src/matrix/store/migration.sdd` `Must`), explicite : le
/// `DeriveMigrationName` donnerait le stem du fichier (`migration`).
const NAME: &str = "m20261006_000001_create_matrix_store";

/// Migrations du store à assembler par l'hôte (`src/migration/mod.sdd`, amendement `matrix`).
#[must_use]
pub fn migrations() -> Vec<Box<dyn sea_orm_migration::MigrationTrait>> {
    vec![Box::new(Migration)]
}

/// Migration unique du store : pose les tables puis les index de recherche.
struct Migration;

/// Nom de table complet : `TABLE_PREFIX` + suffixe.
fn table_name(suffix: &str) -> String {
    format!("{TABLE_PREFIX}{suffix}")
}

/// Colonne texte `NOT NULL` (clés, identifiants de recherche).
fn text(name: &str) -> ColumnDef {
    ColumnDef::new(Alias::new(name)).text().not_null().to_owned()
}

/// Colonne texte nullable.
fn opt_text(name: &str) -> ColumnDef {
    ColumnDef::new(Alias::new(name)).text().to_owned()
}

/// Colonne `BLOB` nullable (payload chiffré).
fn blob(name: &str) -> ColumnDef {
    ColumnDef::new(Alias::new(name)).binary().to_owned()
}

/// Colonne `BOOLEAN NOT NULL`.
fn flag(name: &str) -> ColumnDef {
    ColumnDef::new(Alias::new(name)).boolean().not_null().to_owned()
}

/// Colonne `BOOLEAN NOT NULL DEFAULT false`.
fn flag_default_false(name: &str) -> ColumnDef {
    ColumnDef::new(Alias::new(name))
        .boolean()
        .not_null()
        .default(false)
        .to_owned()
}

/// Colonne `BIGINT NOT NULL`.
fn big(name: &str) -> ColumnDef {
    ColumnDef::new(Alias::new(name))
        .big_integer()
        .not_null()
        .to_owned()
}

/// Spécification d'une table : suffixe, colonnes, colonnes de la clé primaire composite.
struct TableSpec {
    suffix: &'static str,
    columns: Vec<ColumnDef>,
    primary_key: &'static [&'static str],
}

/// Tables du store, dans l'ordre de création (`down` le parcourt à l'envers).
/// `seq` de `secrets_inbox` n'est pas auto-incrémenté (arbitrage 2026-10-14, `migration.sdd`).
fn table_specs() -> Vec<TableSpec> {
    let mut specs = session_table_specs();
    specs.extend(state_table_specs());
    specs
}

/// Tables de sessions et d'appareils (Olm, Megolm, appareils, identités).
fn session_table_specs() -> Vec<TableSpec> {
    vec![
        TableSpec {
            suffix: "kv",
            columns: vec![text("account_key"), text("key"), blob("value")],
            primary_key: &["account_key", "key"],
        },
        TableSpec {
            suffix: "olm_sessions",
            columns: vec![
                text("account_key"),
                text("session_id"),
                opt_text("sender_key"),
                blob("payload"),
            ],
            primary_key: &["account_key", "session_id"],
        },
        TableSpec {
            suffix: "inbound_group_sessions",
            columns: vec![
                text("account_key"),
                text("room_id"),
                text("session_id"),
                opt_text("sender_key"),
                ColumnDef::new(Alias::new("sender_data_type"))
                    .small_integer()
                    .to_owned(),
                flag_default_false("backed_up"),
                blob("payload"),
            ],
            primary_key: &["account_key", "room_id", "session_id"],
        },
        TableSpec {
            suffix: "outbound_group_sessions",
            columns: vec![text("account_key"), text("room_id"), blob("payload")],
            primary_key: &["account_key", "room_id"],
        },
        TableSpec {
            suffix: "withheld_sessions",
            columns: vec![
                text("account_key"),
                text("room_id"),
                text("session_id"),
                blob("payload"),
            ],
            primary_key: &["account_key", "room_id", "session_id"],
        },
        TableSpec {
            suffix: "devices",
            columns: vec![
                text("account_key"),
                text("user_id"),
                text("device_id"),
                blob("payload"),
            ],
            primary_key: &["account_key", "user_id", "device_id"],
        },
        TableSpec {
            suffix: "identities",
            columns: vec![text("account_key"), text("user_id"), blob("payload")],
            primary_key: &["account_key", "user_id"],
        },
    ]
}

/// Tables d'état : suivi, secrets, salons, verrous.
fn state_table_specs() -> Vec<TableSpec> {
    vec![
        TableSpec {
            suffix: "tracked_users",
            columns: vec![text("account_key"), text("user_id"), flag("dirty")],
            primary_key: &["account_key", "user_id"],
        },
        TableSpec {
            suffix: "message_hashes",
            columns: vec![text("account_key"), text("hash")],
            primary_key: &["account_key", "hash"],
        },
        TableSpec {
            suffix: "secret_requests",
            columns: vec![
                text("account_key"),
                text("request_id"),
                text("info_key"),
                flag("sent_out"),
                blob("payload"),
            ],
            primary_key: &["account_key", "request_id"],
        },
        TableSpec {
            suffix: "secrets_inbox",
            columns: vec![
                text("account_key"),
                text("secret_name"),
                big("seq"),
                blob("payload"),
            ],
            primary_key: &["account_key", "secret_name", "seq"],
        },
        TableSpec {
            suffix: "room_settings",
            columns: vec![text("account_key"), text("room_id"), blob("payload")],
            primary_key: &["account_key", "room_id"],
        },
        TableSpec {
            suffix: "room_key_bundles",
            columns: vec![
                text("account_key"),
                text("room_id"),
                text("sender_user_id"),
                blob("payload"),
                flag("pending"),
            ],
            primary_key: &["account_key", "room_id", "sender_user_id"],
        },
        TableSpec {
            suffix: "room_keys_downloaded",
            columns: vec![text("account_key"), text("room_id")],
            primary_key: &["account_key", "room_id"],
        },
        TableSpec {
            suffix: "leases",
            columns: vec![
                text("account_key"),
                text("key"),
                text("holder"),
                big("expiration_ms"),
                big("generation"),
            ],
            primary_key: &["account_key", "key"],
        },
    ]
}

/// Index de recherche : (nom, suffixe de table, colonnes). Noms choisis à l'implémentation
/// (non fixés par la spec), préfixés `idx_miryad_matrix_`.
const INDEXES: [(&str, &str, &[&str]); 7] = [
    (
        "idx_miryad_matrix_olm_sender",
        "olm_sessions",
        &["account_key", "sender_key"],
    ),
    (
        "idx_miryad_matrix_inbound_backed_up",
        "inbound_group_sessions",
        &["account_key", "backed_up"],
    ),
    (
        "idx_miryad_matrix_inbound_sender",
        "inbound_group_sessions",
        &["account_key", "sender_key", "sender_data_type", "session_id"],
    ),
    (
        "idx_miryad_matrix_inbound_room",
        "inbound_group_sessions",
        &["account_key", "room_id"],
    ),
    (
        "idx_miryad_matrix_devices_user",
        "devices",
        &["account_key", "user_id"],
    ),
    (
        "idx_miryad_matrix_secret_requests_info",
        "secret_requests",
        &["account_key", "info_key"],
    ),
    (
        "idx_miryad_matrix_secret_requests_sent",
        "secret_requests",
        &["account_key", "sent_out"],
    ),
];

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
            statement.table(Alias::new(table_name(spec.suffix)));
            for column in spec.columns {
                statement.col(column);
            }
            let mut primary_key = Index::create();
            for column in spec.primary_key {
                primary_key.col(Alias::new(*column));
            }
            statement.primary_key(&mut primary_key);
            manager.create_table(statement.take()).await?;
        }
        for (name, suffix, columns) in INDEXES {
            let mut index = Index::create();
            index.name(name).table(Alias::new(table_name(suffix)));
            for column in columns {
                index.col(Alias::new(*column));
            }
            manager.create_index(index.take()).await?;
        }
        Ok(())
    }

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
    use super::{TABLE_PREFIX, migrations};
    use sea_orm::Database;
    use sea_orm_migration::prelude::*;

    /// Les quinze tables du `Must`, sans préfixe.
    const TABLES: [&str; 15] = [
        "kv",
        "olm_sessions",
        "inbound_group_sessions",
        "outbound_group_sessions",
        "withheld_sessions",
        "devices",
        "identities",
        "tracked_users",
        "message_hashes",
        "secret_requests",
        "secrets_inbox",
        "room_settings",
        "room_key_bundles",
        "room_keys_downloaded",
        "leases",
    ];

    /// Index déclarés sur `inbound_group_sessions` (`Must`) — noms choisis à l'implémentation,
    /// la spec ne les nomme pas.
    const INBOUND_INDEXES: [&str; 3] = [
        "idx_miryad_matrix_inbound_backed_up",
        "idx_miryad_matrix_inbound_sender",
        "idx_miryad_matrix_inbound_room",
    ];

    struct StoreMigrator;

    impl MigratorTrait for StoreMigrator {
        fn migrations() -> Vec<Box<dyn MigrationTrait>> {
            migrations()
        }
    }

    fn table(suffix: &str) -> String {
        format!("{TABLE_PREFIX}{suffix}")
    }

    async fn fresh_db() -> sea_orm::DatabaseConnection {
        Database::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite connects")
    }

    /// Scenario : « up crée les quinze tables avec le préfixe ».
    #[tokio::test]
    async fn up_creates_the_fifteen_prefixed_tables_each_with_account_key() {
        let db = fresh_db().await;
        StoreMigrator::up(&db, None)
            .await
            .expect("up returns Ok(()) over an empty in-memory base");
        let manager = SchemaManager::new(&db);
        for suffix in TABLES {
            let name = table(suffix);
            assert!(
                manager
                    .has_table(name.as_str())
                    .await
                    .expect("has_table probe succeeds"),
                "la table {name} existe après up"
            );
            assert!(
                manager
                    .has_column(name.as_str(), "account_key")
                    .await
                    .expect("has_column probe succeeds"),
                "la table {name} porte la colonne account_key"
            );
        }
    }

    /// Scenario : « down les supprime toutes ». Précondition explicite : up a posé les tables,
    /// sinon le test passerait trivialement sur une base vide.
    #[tokio::test]
    async fn down_drops_every_matrix_table() {
        let db = fresh_db().await;
        StoreMigrator::up(&db, None).await.expect("up returns Ok(())");
        let manager = SchemaManager::new(&db);
        for suffix in TABLES {
            let name = table(suffix);
            assert!(
                manager
                    .has_table(name.as_str())
                    .await
                    .expect("has_table probe succeeds"),
                "précondition : {name} existe après up"
            );
        }
        StoreMigrator::down(&db, None).await.expect("down returns Ok(())");
        for suffix in TABLES {
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

    /// Scenario : « les index de recherche existent ».
    #[tokio::test]
    async fn the_three_declared_inbound_group_sessions_indexes_exist() {
        let db = fresh_db().await;
        StoreMigrator::up(&db, None).await.expect("up returns Ok(())");
        let manager = SchemaManager::new(&db);
        let inbound = table("inbound_group_sessions");
        for index in INBOUND_INDEXES {
            assert!(
                manager
                    .has_index(inbound.as_str(), index)
                    .await
                    .expect("has_index probe succeeds"),
                "l'index {index} existe sur {inbound}"
            );
        }
    }
}
