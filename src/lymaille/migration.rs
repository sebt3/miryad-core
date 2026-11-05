//! Migrations SeaORM internes du module `lymaille` (`src/lymaille/migration.sdd`).
//!
//! Phase test-first : les tests des `Scenario` sont écrits avant le comportement de production.

use sea_orm_migration::prelude::*;

/// Nom de migration `SeaORM` (`src/lymaille/migration.sdd` `Must`), explicite : le
/// `DeriveMigrationName` donnerait le stem du fichier (`migration`).
const NAME: &str = "m20261103_000001_create_lymaille_tables";

/// Préfixe complet des tables du module (`src/lymaille/migration.sdd` `Must`).
const TABLE_PREFIX: &str = "miryad_lymaille_";

/// Migrations du module à assembler par l'hôte (`src/migration/mod.sdd`, amendement `lymaille`) :
/// le lot 1 puis le lot 2, en ordre chronologique (amendement lot 2 du
/// `src/lymaille/migration.sdd`).
#[must_use]
pub fn migrations() -> Vec<Box<dyn sea_orm_migration::MigrationTrait>> {
    vec![
        Box::new(Migration),
        Box::new(m20261104_000001_lymaille_lot2::Migration),
    ]
}

/// Première migration du module (lot 1) : pose les huit tables puis les index. Les tables
/// internes `context_types` et `contexts` ne sont plus dans sa liste : exception du 2026-11-04
/// (`/AGENTS.md`, feature jamais déployée), amendement en place autorisé, remplacées par les
/// tables du lot 2.
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

/// Colonne `INTEGER NOT NULL` sans sémantique de propriétaire (durées de `mcp_servers`,
/// amendement lot 2).
fn number(name: &str) -> ColumnDef {
    ColumnDef::new(Alias::new(name)).integer().not_null().to_owned()
}

/// Colonne texte nullable (références sans NOT NULL dans l'amendement lot 2 : `auth_ref`,
/// `matrix_rooms`, `script_errors`).
fn opt_text(name: &str) -> ColumnDef {
    ColumnDef::new(Alias::new(name)).text().to_owned()
}

/// Colonne `INTEGER` nullable (`turn` de `script_errors`, amendement lot 2).
fn opt_number(name: &str) -> ColumnDef {
    ColumnDef::new(Alias::new(name)).integer().to_owned()
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

/// Les huit tables du `Must`, dans l'ordre de `up` (`down` le parcourt à l'envers). Aucune
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

/// Tables des stores d'exécution : `sessions` et `memories` (PK composées). Les tables internes
/// `context_types` et `contexts` ne se créent plus : exception du 2026-11-04 (`/AGENTS.md`,
/// feature jamais déployée), amendement en place — remplacées par `catalog_context_types`,
/// `catalog_contexts` et `user_contexts` du lot 2 (DSD `DEC-20`).
fn store_table_specs() -> Vec<TableSpec> {
    vec![
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

/// Seconde migration du module — « Amendement lot 2 » du `src/lymaille/migration.sdd` : remplace
/// les deux tables internes du lot 1 (`context_types`, `contexts`, aucune donnée de production,
/// la feature n'est pas déployée) par les tables à identifiant entier lues par les stores du lot 2
/// (DSD `DEC-20`), et pose les tables MCP, journal d'écriture, audit et pont Matrix. Module nommé
/// du nom de suivi, comme les fichiers de `crate::migration` ; la migration du lot 1 reste
/// immuable, c'est ici que passe le changement de schéma.
///
/// Mêmes conventions que le lot 1 : préfixe `miryad_lymaille_`, `if_not_exists` sur les créations,
/// JSON en `ColumnDef::json()`, aucune `FOREIGN KEY`, aucun défaut serveur d'horodatage. Les
/// tables `matrix_*` sont créées même sans la feature `matrix` — le schéma ne dépend pas des
/// features. Colonnes d'horodatage toutes `NOT NULL` (peuplement à l'écriture, jamais serveur) ;
/// les colonnes que l'amendement liste sans `NOT NULL` (références du pont Matrix, colonnes de
/// `script_errors` hors `id`, `auth_ref`) restent nullables.
mod m20261104_000001_lymaille_lot2 {
    use sea_orm_migration::prelude::*;

    use super::{
        TableSpec, id, json, number, opt_json, opt_number, opt_text, owner, table_name, text, timestamp,
        unique_text,
    };

    /// Nom de migration (`src/lymaille/migration.sdd`, amendement lot 2), explicite : le
    /// `DeriveMigrationName` donnerait le stem du fichier parent (`migration`).
    const NAME: &str = "m20261104_000001_lymaille_lot2";

    /// Les deux tables internes remplacées par celles de ce `up`, dans l'ordre de déclaration du
    /// lot 1. Depuis l'exception du 2026-11-04 (`/AGENTS.md`, feature jamais déployée) le `up` du
    /// lot 1 ne les crée plus : leur suppression ici n'est plus qu'un nettoyage conditionnel
    /// (`if_exists`) pour une base ayant hérité d'un état antérieur à l'amendement.
    const REPLACED: [&str; 2] = ["context_types", "contexts"];

    /// Seconde migration du module, rendue par `super::migrations()` en second.
    pub(super) struct Migration;

    /// Colonnes de `catalog_context_types` : la forme catalogue (`id`, `name` unique, `version`,
    /// versions actives et brouillon, horodatage) plus les trois colonnes pilotes DEC-28 du
    /// réalignement R2 — nature, agent par défaut, ensemble des agents autorisés (modification en
    /// place actée le 2026-11-03, exception `/AGENTS.md`, migration jamais déployée).
    /// `default_agent` est nullable en base mais exigé posé par les hooks, comme `active`/`draft`
    /// des toolsets (`../definitions/context_type.sdd`).
    fn context_type_columns() -> Vec<ColumnDef> {
        vec![
            id(),
            unique_text("name"),
            text("version"),
            text("kind"),
            opt_text("default_agent"),
            json("allowed_agents"),
            opt_json("active"),
            opt_json("draft"),
            timestamp("created_at"),
        ]
    }

    /// Les dix tables de l'amendement, dans l'ordre de `up` (`down` le parcourt à l'envers).
    /// `catalog_context_types` et `catalog_contexts` suivent la forme catalogue (PK entière, nom
    /// unique) — le nom de `catalog_context_types` suit la grammaire `lymaille` sans préfixe `G_`
    /// (arbitrage (a) du 2026-11-04, `../definitions/context_type.sdd`), celui de
    /// `catalog_contexts` garde le `G_` du catalogue (`../definitions/catalog_context.sdd`) ;
    /// `user_contexts` la forme privée (`owner_id`, unicité par propriétaire) ;
    /// `write_journal` a une PK composée (spec `../store/write_journal.sdd`).
    fn table_specs() -> Vec<TableSpec> {
        vec![
            TableSpec {
                suffix: "catalog_context_types",
                columns: context_type_columns(),
                primary_key: &[],
            },
            TableSpec {
                suffix: "catalog_contexts",
                columns: vec![
                    id(),
                    unique_text("name"),
                    json("context"),
                    opt_text("context_type"),
                    timestamp("created_at"),
                ],
                primary_key: &[],
            },
            TableSpec {
                suffix: "user_contexts",
                columns: vec![
                    id(),
                    owner("owner_id"),
                    text("name"),
                    opt_text("context_type"),
                    json("context"),
                    timestamp("created_at"),
                ],
                primary_key: &[],
            },
            TableSpec {
                suffix: "mcp_servers",
                columns: vec![
                    id(),
                    unique_text("name"),
                    text("url"),
                    number("request_timeout_secs"),
                    number("connect_timeout_secs"),
                    text("auth_mode"),
                    opt_text("auth_ref"),
                    json("allowed_groups"),
                    timestamp("created_at"),
                ],
                primary_key: &[],
            },
            TableSpec {
                suffix: "write_journal",
                columns: vec![
                    text("run_key"),
                    text("step_key"),
                    text("op_key"),
                    json("result"),
                    timestamp("created_at"),
                ],
                primary_key: &["run_key", "step_key", "op_key"],
            },
            TableSpec {
                suffix: "script_errors",
                columns: vec![
                    id(),
                    opt_text("subject"),
                    opt_text("session_id"),
                    opt_number("turn"),
                    opt_text("class"),
                    opt_text("text"),
                    timestamp("at"),
                ],
                primary_key: &[],
            },
            TableSpec {
                suffix: "matrix_links",
                columns: vec![text("mxid"), text("subject"), timestamp("created_at")],
                primary_key: &["mxid"],
            },
            TableSpec {
                suffix: "matrix_link_codes",
                columns: vec![text("code_hash"), text("subject"), timestamp("expires_at")],
                primary_key: &["code_hash"],
            },
            TableSpec {
                suffix: "matrix_rooms",
                columns: vec![
                    text("room_id"),
                    opt_text("subject"),
                    opt_text("session_id"),
                    opt_text("agent_id"),
                    timestamp("updated_at"),
                ],
                primary_key: &["room_id"],
            },
            TableSpec {
                suffix: "matrix_events",
                columns: vec![text("event_id"), timestamp("seen_at")],
                primary_key: &["event_id"],
            },
        ]
    }

    /// Unicité privée posée comme index unique nommé (`UNIQUE(owner_id, name)` de `user_contexts`,
    /// même motif que `agents` et `toolsets` du lot 1).
    const UNIQUE_INDEXES: [(&str, &str, &[&str]); 1] = [(
        "uq_miryad_lymaille_user_contexts_owner_name",
        "user_contexts",
        &["owner_id", "name"],
    )];

    /// Index de recherche des tables du lot 2 (`script_errors.session_id`,
    /// `matrix_links.subject`, `matrix_link_codes.subject`, `matrix_events.seen_at`).
    const INDEXES: [(&str, &str, &[&str]); 4] = [
        (
            "idx_miryad_lymaille_script_errors_session",
            "script_errors",
            &["session_id"],
        ),
        (
            "idx_miryad_lymaille_matrix_links_subject",
            "matrix_links",
            &["subject"],
        ),
        (
            "idx_miryad_lymaille_matrix_link_codes_subject",
            "matrix_link_codes",
            &["subject"],
        ),
        (
            "idx_miryad_lymaille_matrix_events_seen_at",
            "matrix_events",
            &["seen_at"],
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
            // Suppression conditionnelle des tables internes héritées : depuis l'exception du
            // 2026-11-04 (`/AGENTS.md`) le `up` du lot 1 ne les crée plus, elles n'existent donc
            // sur aucune base neuve — `if_exists` garde ce `up` idempotent et vert dans les deux
            // états (règle du `Must` parent : les créations sont `if_not_exists`, les `down` sont
            // stricts, les suppressions d'`up` restent conditionnelles).
            for suffix in REPLACED {
                manager
                    .drop_table(
                        Table::drop()
                            .table(Alias::new(table_name(suffix)))
                            .if_exists()
                            .to_owned(),
                    )
                    .await?;
            }
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
            // Réalignement R10 (DSD `DEC-28`, arbitrage du 2026-11-05) : le lot 2 **ajoute** la
            // colonne `context` TEXT NULL à `sessions` (le contexte sur lequel la session est
            // ouverte, immuable après l'ouverture), par `ALTER TABLE … ADD COLUMN` portable —
            // jamais par une retouche du lot 1. Le rejeu du `Must` parent (idempotence) garde
            // l'ajout par `has_column` : sans ce garde, le second `up` échouerait sur une colonne
            // déjà présente.
            if !manager
                .has_column(table_name("sessions").as_str(), "context")
                .await?
            {
                manager
                    .alter_table(
                        Table::alter()
                            .table(Alias::new(table_name("sessions")))
                            .add_column(opt_text("context"))
                            .to_owned(),
                    )
                    .await?;
            }
            Ok(())
        }

        /// Remontée stricte, non idempotent (règle de `src/migration/mod.sdd`) : colonne `context`
        /// de `sessions` retirée d'abord (c'est le lot 2 qui l'a ajoutée — R10, chaîne `down`
        /// cohérente), puis `drop_table` sans `if_exists`, tables dans l'ordre inverse de `up`,
        /// **sans** recréer `context_types` ni `contexts` (clause de l'amendement lot 2 : la
        /// suppression des tables internes n'est annulée par aucun `down` — leur réapparition
        /// relève d'un acte manuel).
        async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
            manager
                .alter_table(
                    Table::alter()
                        .table(Alias::new(table_name("sessions")))
                        .drop_column(Alias::new("context"))
                        .to_owned(),
                )
                .await?;
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
}

#[cfg(test)]
mod tests {
    use super::{Migration, TABLE_PREFIX, migrations};
    use sea_orm::{ConnectionTrait, Database, DatabaseConnection};
    use sea_orm_migration::prelude::*;

    /// Les huit tables du `Must` du lot 1, suffixe + colonnes déclarées, dans l'ordre de `up`. Les
    /// deux tables internes `context_types` et `contexts` n'y figurent plus : exception du
    /// 2026-11-04 (`/AGENTS.md`, feature jamais déployée) — amendement en place de
    /// `m20261103_000001`, remplacées par les tables du lot 2 (DSD `DEC-20`).
    const TABLES: [(&str, &[&str]); 8] = [
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
        (
            "sessions",
            &["subject", "session_id", "kind", "state", "updated_at"],
        ),
        (
            "memories",
            &["subject", "level", "reference", "memory_id", "memory"],
        ),
    ];

    /// Les dix tables de l'« Amendement lot 2 », suffixe + colonnes déclarées, dans l'ordre de
    /// `up` (`down` le parcourt à l'envers). `catalog_context_types` porte les trois colonnes du
    /// réalignement R2 (DSD `DEC-28`, modification en place actée le 2026-11-03) : nature, agent
    /// par défaut, ensemble des agents autorisés. Les deux tables de contextes portent en plus
    /// `context_type` (TEXT nullable, référence par `name` à un type du catalogue — arbitrage
    /// `DEC-28` du 2026-11-05, modification en place, `/AGENTS.md`) ; la colonne `context` de
    /// `sessions` n'y figure pas : elle n'est pas déclarée par le lot 2, elle y est **ajoutée**
    /// par `ALTER TABLE` (son test propre, « les contextes portent leur type et les sessions leur
    /// contexte »).
    const TABLES_LOT2: [(&str, &[&str]); 10] = [
        (
            "catalog_context_types",
            &[
                "id",
                "name",
                "version",
                "kind",
                "default_agent",
                "allowed_agents",
                "active",
                "draft",
                "created_at",
            ],
        ),
        (
            "catalog_contexts",
            &["id", "name", "context", "context_type", "created_at"],
        ),
        (
            "user_contexts",
            &["id", "owner_id", "name", "context", "context_type", "created_at"],
        ),
        (
            "mcp_servers",
            &[
                "id",
                "name",
                "url",
                "request_timeout_secs",
                "connect_timeout_secs",
                "auth_mode",
                "auth_ref",
                "allowed_groups",
                "created_at",
            ],
        ),
        (
            "write_journal",
            &["run_key", "step_key", "op_key", "result", "created_at"],
        ),
        (
            "script_errors",
            &["id", "subject", "session_id", "turn", "class", "text", "at"],
        ),
        ("matrix_links", &["mxid", "subject", "created_at"]),
        ("matrix_link_codes", &["code_hash", "subject", "expires_at"]),
        (
            "matrix_rooms",
            &["room_id", "subject", "session_id", "agent_id", "updated_at"],
        ),
        ("matrix_events", &["event_id", "seen_at"]),
    ];

    /// Les deux tables internes que l'amendement lot 2 remplace par les tables à identifiant
    /// entier (`ContextTypeStore` et `ContextStore`). Depuis l'exception du 2026-11-04
    /// (`/AGENTS.md`) elles ne se créent plus nulle part : le `up` du lot 1 ne les pose plus et
    /// celles du lot 2 ne les suppriment que conditionnellement (`if exists`).
    const LOT1_REPLACED: [&str; 2] = ["context_types", "contexts"];

    /// Contrainte unique et index posés par le lot 2 : (nom d'index, suffixe de table). L'unicité
    /// `UNIQUE(owner_id, name)` de `user_contexts` est posée comme index unique (précédent lot 1),
    /// sondée comme les autres par `has_index`.
    const LOT2_INDEXES: [(&str, &str); 5] = [
        ("uq_miryad_lymaille_user_contexts_owner_name", "user_contexts"),
        ("idx_miryad_lymaille_script_errors_session", "script_errors"),
        ("idx_miryad_lymaille_matrix_links_subject", "matrix_links"),
        (
            "idx_miryad_lymaille_matrix_link_codes_subject",
            "matrix_link_codes",
        ),
        ("idx_miryad_lymaille_matrix_events_seen_at", "matrix_events"),
    ];

    /// Noms de suivi des deux entrées du module, dans l'ordre chronologique (`Amendement lot 2`).
    const MIGRATION_NAMES: [&str; 2] = [
        "m20261103_000001_create_lymaille_tables",
        "m20261104_000001_lymaille_lot2",
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

    /// La seconde entrée rendue par `migrations()` (le lot 2). Appelable avant implémentation :
    /// l'assertion de longueur échoue sur l'entrée manquante — comportement absent, pas erreur de
    /// construction — et la dispatch par `Box<dyn MigrationTrait>` évite de nommer le type ici.
    fn lot2() -> Box<dyn MigrationTrait> {
        let mut entries = migrations();
        assert_eq!(
            entries.len(),
            MIGRATION_NAMES.len(),
            "migrations() rend les deux entrées du module (amendement lot 2)"
        );
        entries.remove(1)
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

    /// Scenario : « up crée les tables du contrat avec le préfixe ». Huit tables depuis l'exception
    /// du 2026-11-04 (`/AGENTS.md`) : le `up` de `m20261103_000001` amendé en place ne crée plus
    /// `context_types` ni `contexts` — remplacées par les tables du lot 2, il doit en rester absent.
    /// Son `up` est appelé **directement** sur une base vide, figé hors migrateur. Sonde aussi
    /// l'index (`subject`, `updated_at`) de `sessions`.
    #[tokio::test]
    async fn first_up_creates_the_eight_prefixed_tables_and_not_the_internal_ones() {
        let db = fresh_db().await;
        let manager = SchemaManager::new(&db);
        Migration
            .up(&manager)
            .await
            .expect("le up du lot 1 retourne Ok(()) sur une base vide");
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
        for suffix in LOT1_REPLACED {
            let name = table(suffix);
            assert!(
                !manager
                    .has_table(name.as_str())
                    .await
                    .expect("has_table probe succeeds"),
                "le up amendé du lot 1 ne crée plus {name} (exception du 2026-11-04, /AGENTS.md)"
            );
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

    /// `Must` : « `up` sur une base où une table existe déjà est idempotent » — attribut du lot 1.
    /// Le rejeu est effectif : le second `up` passe par `Migration.up` **directement** (motif du
    /// précédent `m20260930_000001_index_api_tokens_subject`), sinon le `Migrator` n'émet aucun DDL.
    /// Depuis l'exception du 2026-11-04, le `up` du lot 1 ne crée que ses huit tables : rejoué après
    /// le lot 2, il ne peut plus reposer `context_types` ni `contexts`.
    #[tokio::test]
    async fn first_up_is_replayable_on_a_database_where_the_tables_already_exist() {
        let db = fresh_db().await;
        let manager = SchemaManager::new(&db);
        Migration.up(&manager).await.expect("premier up du lot 1 réussit");
        Migration
            .up(&manager)
            .await
            .expect("le up rejoué retourne Ok(()) grâce à if_not_exists");
        for (suffix, _) in TABLES {
            let name = table(suffix);
            assert!(
                manager
                    .has_table(name.as_str())
                    .await
                    .expect("has_table probe succeeds"),
                "{name} subsiste au rejeu du up du lot 1"
            );
        }
    }

    /// Scenario : « down les supprime toutes » — attribut du lot 1, sur une base qui ne l'a vu
    /// que lui : sa remontée stricte, tables dans l'ordre inverse de `up`, ne laisse aucune de ses
    /// huit tables. Depuis l'exception du 2026-11-04 (`/AGENTS.md`), son `up` ne crée plus
    /// `context_types` ni `contexts` et son `down` ne les dropperait que si son `up` les avait
    /// créées : la chaîne `down` complète sous `lymaille` est de nouveau cohérente, prouvée par
    /// les tests de remontée `crate::migration::m20260822_*`.
    #[tokio::test]
    async fn first_down_drops_every_lot1_table() {
        let db = fresh_db().await;
        let manager = SchemaManager::new(&db);
        Migration
            .up(&manager)
            .await
            .expect("le up du lot 1 pose ses huit tables");
        Migration
            .down(&manager)
            .await
            .expect("le down du lot 1 retourne Ok(()) sur une base lot 1 uniquement");
        for (suffix, _) in TABLES {
            let name = table(suffix);
            assert!(
                !manager
                    .has_table(name.as_str())
                    .await
                    .expect("has_table probe succeeds"),
                "le down du lot 1 supprime {name}"
            );
        }
    }

    /// Scenario (amendement lot 2) : `migrations()` rend `[lot 1, lot 2]` dans l'ordre
    /// chronologique — l'ordre d'assemblage de `crate::migration::Migrator` en dépend.
    #[test]
    fn migrations_returns_the_two_module_entries_in_chronological_order() {
        let entries = migrations();
        let names: Vec<&str> = entries.iter().map(|entry| entry.name()).collect();
        assert_eq!(
            names, MIGRATION_NAMES,
            "les deux entrées du module, en ordre chronologique"
        );
    }

    /// Scenario (amendement lot 2) : `up` pose les dix nouvelles tables avec leurs colonnes, laisse
    /// les huit tables du lot 1 en place, et ne laisse aucune trace de `context_types`/`contexts` :
    /// depuis l'exception du 2026-11-04 ces tables ne se créent nulle part, et le `up` du lot 2 ne
    /// les supprime que conditionnellement (`if exists`) au cas où une base les aurait héritées
    /// d'un état antérieur à l'amendement.
    #[tokio::test]
    async fn lot2_up_replaces_the_internal_tables_with_the_ten_new_tables() {
        let db = fresh_db().await;
        LymailleMigrator::up(&db, None)
            .await
            .expect("le up complet pose les schémas des deux lots");
        let manager = SchemaManager::new(&db);
        for (suffix, columns) in TABLES_LOT2 {
            let name = table(suffix);
            assert!(
                manager
                    .has_table(name.as_str())
                    .await
                    .expect("has_table probe succeeds"),
                "la table {name} du lot 2 existe après up"
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
        for suffix in LOT1_REPLACED {
            let name = table(suffix);
            assert!(
                !manager
                    .has_table(name.as_str())
                    .await
                    .expect("has_table probe succeeds"),
                "{name} n'existe nulle part après le up complet (exception du 2026-11-04, /AGENTS.md)"
            );
        }
        for (suffix, _) in TABLES {
            let name = table(suffix);
            assert!(
                manager
                    .has_table(name.as_str())
                    .await
                    .expect("has_table probe succeeds"),
                "le up du lot 2 ne touche pas la table {name} du lot 1"
            );
        }
    }

    /// Scenario (réalignement R2, DSD `DEC-28`, 2026-11-03) : `catalog_context_types` porte les
    /// trois colonnes pilotes de la session — nature (`work`|`config`), agent par défaut, ensemble
    /// des agents autorisés. Sondes `has_column` sur le schéma complet.
    #[tokio::test]
    async fn lot2_catalog_context_types_carries_the_three_session_columns() {
        let db = fresh_db().await;
        LymailleMigrator::up(&db, None)
            .await
            .expect("le up complet pose les schémas des deux lots");
        let manager = SchemaManager::new(&db);
        let name = table("catalog_context_types");
        for column in ["kind", "default_agent", "allowed_agents"] {
            assert!(
                manager
                    .has_column(name.as_str(), column)
                    .await
                    .expect("has_column probe succeeds"),
                "la table {name} porte la colonne {column} (réalignement R2, DSD `DEC-28`)"
            );
        }
    }

    /// Scenarios (réalignement R10, DSD `DEC-28`, arbitrage du 2026-11-05) : « les contextes
    /// portent leur type et les sessions leur contexte ». Après le `up` complet, `context_type`
    /// (TEXT nullable) existe sur `catalog_contexts` et `user_contexts` (modification en place du
    /// `up`, exception `/AGENTS.md`, tables jamais déployées), et `context` (TEXT nullable) existe
    /// sur `sessions` — ajoutée par `ALTER TABLE` par le lot 2, jamais par une retouche du lot 1.
    #[tokio::test]
    async fn contexts_carry_their_type_and_sessions_their_context() {
        let db = fresh_db().await;
        LymailleMigrator::up(&db, None)
            .await
            .expect("le up complet pose les schémas des deux lots");
        let manager = SchemaManager::new(&db);
        for suffix in ["catalog_contexts", "user_contexts"] {
            let name = table(suffix);
            assert!(
                manager
                    .has_column(name.as_str(), "context_type")
                    .await
                    .expect("has_column probe succeeds"),
                "la table {name} porte la colonne context_type (arbitrage DEC-28 du 2026-11-05)"
            );
        }
        assert!(
            manager
                .has_column(table("sessions").as_str(), "context")
                .await
                .expect("has_column probe succeeds"),
            "la table des sessions porte la colonne context ajoutée par le lot 2 (DEC-28)"
        );
    }

    /// Scenario (réalignement R10, DSD `DEC-28`) : la remontée du lot 2 retire ces colonnes —
    /// `context_type` part avec les tables de contextes (elles sont du lot 2), `context` est
    /// supprimée de `sessions` par le `down` du lot 2, qui l'a ajoutée : la chaîne `down` est
    /// cohérente avec le `up`, et un rejeu complet repose la colonne.
    #[tokio::test]
    async fn lot2_rollback_removes_the_columns_it_added() {
        let db = fresh_db().await;
        let manager = SchemaManager::new(&db);
        LymailleMigrator::up(&db, None)
            .await
            .expect("premier up complet réussit");
        LymailleMigrator::down(&db, Some(1))
            .await
            .expect("un pas de down défait le lot 2, dernière appliquée");
        for suffix in ["catalog_contexts", "user_contexts"] {
            assert!(
                !manager
                    .has_table(table(suffix).as_str())
                    .await
                    .expect("has_table probe succeeds"),
                "{suffix} est supprimée avec ses colonnes par le down du lot 2"
            );
        }
        assert!(
            manager
                .has_table(table("sessions").as_str())
                .await
                .expect("has_table probe succeeds"),
            "le down du lot 2 ne touche pas la table sessions du lot 1"
        );
        assert!(
            !manager
                .has_column(table("sessions").as_str(), "context")
                .await
                .expect("has_column probe succeeds"),
            "le down du lot 2 retire la colonne context qu'il avait ajoutée (chaîne down cohérente)"
        );
        LymailleMigrator::up(&db, None)
            .await
            .expect("le rejeu du up complet réussit après la remontée");
        assert!(
            manager
                .has_column(table("sessions").as_str(), "context")
                .await
                .expect("has_column probe succeeds"),
            "le rejeu du up repose la colonne context (ALTER gardé, idempotence du `Must` parent)"
        );
    }

    /// Scenarios (amendement lot 2, tâche « tests `has_table`/`has_index` d'abord ») : la
    /// contrainte `UNIQUE(owner_id, name)` de `user_contexts` et les quatre index de recherche
    /// (`script_errors.session_id`, `matrix_links.subject`, `matrix_link_codes.subject`,
    /// `matrix_events.seen_at`) sont posés.
    #[tokio::test]
    async fn lot2_up_poses_the_declared_unique_constraint_and_indexes() {
        let db = fresh_db().await;
        LymailleMigrator::up(&db, None)
            .await
            .expect("le up complet pose les schémas des deux lots");
        let manager = SchemaManager::new(&db);
        for (index, suffix) in LOT2_INDEXES {
            let name = table(suffix);
            assert!(
                manager
                    .has_index(name.as_str(), index)
                    .await
                    .expect("has_index probe succeeds"),
                "l'index {index} existe sur {name}"
            );
        }
    }

    /// `Must` (« mêmes conventions que ci-dessus », amendement lot 2) : le rejeu du `up` du lot 2
    /// est idempotent. Effectif par dispatch direct (`MigrationTrait::up` sur la seconde entrée) :
    /// les suppressions des tables internes passent par `if_exists` — elles n'existent plus après
    /// une première application — et les créations par `if_not_exists`.
    #[tokio::test]
    async fn lot2_up_is_replayable_on_a_database_where_it_already_ran() {
        let db = fresh_db().await;
        let manager = SchemaManager::new(&db);
        LymailleMigrator::up(&db, None)
            .await
            .expect("premier up complet réussit");
        lot2()
            .up(&manager)
            .await
            .expect("le up du lot 2 rejoué retourne Ok(()) — if_exists sur les suppressions, if_not_exists sur les créations");
        for (suffix, _) in TABLES_LOT2 {
            let name = table(suffix);
            assert!(
                manager
                    .has_table(name.as_str())
                    .await
                    .expect("has_table probe succeeds"),
                "{name} subsiste au rejeu du up du lot 2"
            );
        }
    }

    /// Scenario (amendement lot 2) : `down` du lot 2 supprime ses dix tables dans l'ordre inverse
    /// **sans** recréer `context_types` ni `contexts`, et sans toucher aux autres tables du lot 1.
    #[tokio::test]
    async fn lot2_down_undoes_lot2_without_recreating_the_internal_tables() {
        let db = fresh_db().await;
        let manager = SchemaManager::new(&db);
        LymailleMigrator::up(&db, None)
            .await
            .expect("le up complet pose les schémas des deux lots");
        lot2()
            .down(&manager)
            .await
            .expect("le down du lot 2 retourne Ok(())");
        for (suffix, _) in TABLES_LOT2 {
            let name = table(suffix);
            assert!(
                !manager
                    .has_table(name.as_str())
                    .await
                    .expect("has_table probe succeeds"),
                "le down du lot 2 supprime {name}"
            );
        }
        for suffix in LOT1_REPLACED {
            let name = table(suffix);
            assert!(
                !manager
                    .has_table(name.as_str())
                    .await
                    .expect("has_table probe succeeds"),
                "le down du lot 2 ne recrée pas {name} (clause « sans recréer » de l'amendement)"
            );
        }
        assert!(
            manager
                .has_table(table("sessions").as_str())
                .await
                .expect("has_table probe succeeds"),
            "les tables du lot 1 ne sont pas touchées par le down du lot 2"
        );
    }

    /// Scenario (amendement lot 2, par le `Migrator` d'un test) : `up` complet, remontée du lot 2
    /// (`down(Some(1))`), puis `up` complet laisse le schéma lot 2 sans `context_types`/`contexts`
    /// — le `down` du lot 2 ne les recrée pas (clause « sans recréer ») et son `up` ne les recrée
    /// pas davantage (`if_exists` sur les suppressions, pas de création).
    ///
    /// Le `down` complet (`None`) est de nouveau praticable depuis l'exception du 2026-11-04
    /// (`/AGENTS.md`) : il n'atteint plus de drop strict de ces deux tables, et il est exercé par
    /// les tests de remontée `crate::migration::m20260822_*`.
    #[tokio::test]
    async fn a_lot2_rollback_then_a_full_up_leaves_no_internal_context_tables() {
        let db = fresh_db().await;
        let manager = SchemaManager::new(&db);
        LymailleMigrator::up(&db, None)
            .await
            .expect("premier up complet réussit");
        LymailleMigrator::down(&db, Some(1))
            .await
            .expect("un pas de down défait le lot 2, dernière appliquée");
        LymailleMigrator::up(&db, None)
            .await
            .expect("le rejeu du up complet réussit");
        for (suffix, _) in TABLES_LOT2 {
            let name = table(suffix);
            assert!(
                manager
                    .has_table(name.as_str())
                    .await
                    .expect("has_table probe succeeds"),
                "{name} est reposée par le rejeu"
            );
        }
        for suffix in LOT1_REPLACED {
            let name = table(suffix);
            assert!(
                !manager
                    .has_table(name.as_str())
                    .await
                    .expect("has_table probe succeeds"),
                "{name} reste absente après down d'un pas puis up complet"
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
