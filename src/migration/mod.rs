//! Migrations internes (`miryad_*`) — à appliquer via [`Migrator`](crate::migration::Migrator)::`up`.

mod m20260822_000001_create_api_tokens;
mod m20260822_000002_create_users_groups;
mod m20260822_000003_seed_admin_group;
#[cfg(feature = "workflow")]
mod m20260923_000001_create_workflow_definitions;
mod m20260930_000001_index_api_tokens_subject;

use sea_orm::sea_query::IntoIden;

/// Migrateur interne miryad-core (tables `miryad_*`, tracking table dédiée).
///
/// **Attention — `fresh` et `reset` héritées de `sea_orm_migration` restent destructeurs
/// au-delà de `miryad_*`** : `fresh` droppe **toutes** les tables de la connexion, celles de
/// l'application consommatrice comprises. Pas de wrapper — ne jamais les appeler sur une
/// connexion partagée en production (arbitré par Sébastien le 2026-09-29, ./mod.sdd `Must`).
///
/// **`steps` `Some(n)` peut laisser le moteur dans un état partiel** — par exemple une base à
/// jour de la migration 002 sans le groupe `admin` posé par le seed 003. C'est une
/// responsabilité du caller : aucun wrapper ne force `None` ; n'appeler `up`/`down` avec
/// `Some(n)` que si cet état intermédiaire est celui voulu (arbitré par Sébastien le
/// 2026-09-29, ./mod.sdd `Must`).
pub struct Migrator;

impl sea_orm_migration::MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn sea_orm_migration::MigrationTrait>> {
        vec![
            Box::new(m20260822_000001_create_api_tokens::Migration),
            Box::new(m20260822_000002_create_users_groups::Migration),
            Box::new(m20260822_000003_seed_admin_group::Migration),
            #[cfg(feature = "workflow")]
            Box::new(m20260923_000001_create_workflow_definitions::Migration),
            Box::new(m20260930_000001_index_api_tokens_subject::Migration),
        ]
    }

    /// Table de suivi dédiée, distincte du défaut `seaql_migrations` — une app consommatrice
    /// compose ce `Migrator` avec son propre `MigratorTrait` métier sur la même connexion
    /// (pattern documenté). Sans ça, `sea_orm_migration::Migrator::up()` valide que *toute*
    /// entrée de la table de suivi est connue du migrateur en cours d'exécution : le second
    /// migrateur à tourner échoue sur les entrées laissées par le premier.
    fn migration_table_name() -> sea_orm::DynIden {
        sea_orm::sea_query::Alias::new("seaql_migrations_miryad_core").into_iden()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sea_orm::Database;
    use sea_orm_migration::MigratorTrait as _;
    use sea_orm_migration::prelude::*;

    /// Migrateur indépendant représentant l'app consommatrice — utilise le
    /// `migration_table_name()` par défaut (`seaql_migrations`), comme documenté pour le pattern
    /// "composer deux `MigratorTrait` sur la même connexion".
    struct AppMigrator;

    impl MigratorTrait for AppMigrator {
        fn migrations() -> Vec<Box<dyn MigrationTrait>> {
            vec![Box::new(AppMigration)]
        }
    }

    #[derive(DeriveMigrationName)]
    struct AppMigration;

    #[async_trait::async_trait]
    impl MigrationTrait for AppMigration {
        async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
            manager
                .create_table(
                    Table::create()
                        .table(Alias::new("app_widgets"))
                        .col(
                            ColumnDef::new(Alias::new("id"))
                                .integer()
                                .not_null()
                                .primary_key(),
                        )
                        .to_owned(),
                )
                .await
        }

        async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
            manager
                .drop_table(Table::drop().table(Alias::new("app_widgets")).to_owned())
                .await
        }
    }

    /// Base `SQLite` en mémoire vide — canal de test de la batterie de `/tooling.sdd`, sans base
    /// externe (pattern du test inline existant, même harnais que les sœurs `m20260923_*` et
    /// `m20260930_*`).
    async fn fresh_db() -> sea_orm::DatabaseConnection {
        sea_orm::Database::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite connects")
    }

    /// Stems enregistrés dans `migrations()`, en ordre d'enregistrement : les trois internes en
    /// ordre figé, puis sous `workflow` la 004 gated, puis l'index de 2026-09-30 (dernière
    /// position réelle, arbitrage du 2026-09-29 tracé dans ./mod.sdd).
    /// L'ordre d'enregistrement coïncide avec l'ordre lexicographique des versions (règle
    /// `mYYYYMMDD_NNNNNN_nom` de ./mod.sdd), donc aussi avec le tri `@Order::Asc` de
    /// `get_migration_models` (vérifié au source `exec.rs:26`).
    fn registered_stems() -> Vec<&'static str> {
        let mut stems = vec![
            "m20260822_000001_create_api_tokens",
            "m20260822_000002_create_users_groups",
            "m20260822_000003_seed_admin_group",
        ];
        #[cfg(feature = "workflow")]
        stems.push("m20260923_000001_create_workflow_definitions");
        stems.push("m20260930_000001_index_api_tokens_subject");
        stems
    }

    /// Versions rendues par `Migrator::get_migration_models` (table de tracking dédiée, tri
    /// `version` ascendant vérifié au source de sea-orm-migration 2.0.2).
    async fn dedicated_versions(db: &sea_orm::DatabaseConnection) -> Vec<String> {
        Migrator::get_migration_models(db)
            .await
            .expect("dedicated tracking table is readable")
            .into_iter()
            .map(|model| model.version)
            .collect()
    }

    /// Lecture SQL brute de la colonne `version` d'une table de tracking (défaut ou dédiée) —
    /// la sonde échoue en `DbErr` quand la table est absente.
    async fn raw_versions(
        db: &sea_orm::DatabaseConnection,
        table: &str,
    ) -> Result<Vec<String>, sea_orm::DbErr> {
        let rows = db
            .query_all_raw(sea_orm::Statement::from_sql_and_values(
                sea_orm::DbBackend::Sqlite,
                format!("SELECT version FROM {table} ORDER BY version ASC"),
                Vec::<sea_orm::Value>::new(),
            ))
            .await?;
        rows.iter()
            .map(|row| row.try_get::<String>("", "version"))
            .collect()
    }

    /// Scenario : « @migrations enregistre les trois internes en ordre figé puis les évolutions
    /// chronologiques ». Contrat vérifié : la liste compte exactement les enregistrements des
    /// specs filles — quatre, cinq sous `workflow` — dans l'ordre figé des trois internes suivi
    /// des évolutions chronologiques (la 004 gated sous `workflow`, l'index de 2026-09-30 en
    /// dernière position), chaque entrée au statut `Pending` avant tout contact base.
    #[tokio::test]
    async fn get_migration_files_registers_the_registered_migrations_in_frozen_order() {
        let files = Migrator::get_migration_files();
        let names: Vec<&str> = files.iter().map(sea_orm_migration::Migration::name).collect();
        assert_eq!(
            names,
            registered_stems(),
            "les trois internes en ordre figé, puis les enregistrements chronologiques"
        );
        assert!(
            files
                .iter()
                .all(|file| file.status() == sea_orm_migration::MigrationStatus::Pending),
            "chaque entry porte le statut Pending avant tout contact base"
        );
    }

    /// Scenario : « le tracking dédié porte toutes les entrées sous
    /// `seaql_migrations_miryad_core` ». Contrat vérifié : `install` auto-crée la table dédiée au
    /// premier `up`, et `get_migration_models` rend exactement les enregistrements — quatre
    /// versions, cinq sous `workflow` — dans l'ordre du tri `version` ascendant.
    #[tokio::test]
    async fn the_dedicated_tracking_table_carries_the_registered_versions() {
        let db = fresh_db().await;
        Migrator::up(&db, None)
            .await
            .expect("up returns Ok(()) over an empty in-memory base");
        let manager = SchemaManager::new(&db);
        assert!(
            manager
                .has_table("seaql_migrations_miryad_core")
                .await
                .expect("has_table probe succeeds"),
            "la table de tracking dédiée est auto-créée par install au premier up"
        );
        assert_eq!(
            dedicated_versions(&db).await,
            registered_stems(),
            "le tracking dédié porte exactement les enregistrements, version triée ascendante"
        );
    }

    /// Scenario : « le défaut `seaql_migrations` n'est jamais créé par le Migrator miryad-core ».
    #[tokio::test]
    async fn the_default_seaql_migrations_is_never_created_by_the_miryad_core_migrator() {
        let db = fresh_db().await;
        Migrator::up(&db, None)
            .await
            .expect("up returns Ok(()) over an empty in-memory base");
        let manager = SchemaManager::new(&db);
        assert!(
            !manager
                .has_table("seaql_migrations")
                .await
                .expect("has_table probe succeeds"),
            "la sonde répond false : le Migrator n'écrit que dans sa table dédiée"
        );
        assert!(
            raw_versions(&db, "seaql_migrations").await.is_err(),
            "la lecture SQL brute de `seaql_migrations` échoue en DbErr, table absente"
        );
    }

    /// Scenario : « steps Some(1) n'applique que la tête de l'ordre ».
    #[tokio::test]
    async fn steps_some_one_applies_only_the_head_of_the_registration_order() {
        let db = fresh_db().await;
        Migrator::up(&db, Some(1))
            .await
            .expect("up Some(1) applies the head and returns Ok(())");
        let manager = SchemaManager::new(&db);
        assert!(
            manager
                .has_table("miryad_api_tokens")
                .await
                .expect("has_table probe succeeds"),
            "la tête de l'ordre (001) est appliquée"
        );
        assert!(
            !manager
                .has_table("miryad_users")
                .await
                .expect("has_table probe succeeds"),
            "la suite de l'ordre (002) n'est pas touchée"
        );
        assert_eq!(
            dedicated_versions(&db).await,
            vec!["m20260822_000001_create_api_tokens"],
            "le tracking rend la seule ligne de la tête"
        );
    }

    /// Scenario : « steps Some(0) installe le tracking sans rien appliquer ».
    #[tokio::test]
    async fn steps_some_zero_installs_the_tracking_table_without_applying_anything() {
        let db = fresh_db().await;
        Migrator::up(&db, Some(0))
            .await
            .expect("up Some(0) returns Ok(()) with an empty application");
        let manager = SchemaManager::new(&db);
        assert!(
            manager
                .has_table("seaql_migrations_miryad_core")
                .await
                .expect("has_table probe succeeds"),
            "Some(0) installe quand même la table de tracking"
        );
        assert!(
            dedicated_versions(&db).await.is_empty(),
            "zéro ligne rendue par get_migration_models"
        );
        for table in [
            "miryad_api_tokens",
            "miryad_users",
            "miryad_groups",
            "miryad_group_memberships",
        ] {
            assert!(
                !manager.has_table(table).await.expect("has_table probe succeeds"),
                "aucune table `miryad_*` des filles n'existe : {table}"
            );
        }
    }

    /// Scenario : « un second up applique rien quand tout est appliqué ». Contrat vérifié : le
    /// second `up` retourne `Ok(())`, `get_pending_migrations` rend une liste vide, le snapshot
    /// avant/après du tracking est inchangé et celui-ci compte exactement les enregistrements —
    /// quatre lignes, cinq sous `workflow`.
    #[tokio::test]
    async fn a_second_up_applies_nothing_when_everything_is_already_applied() {
        let db = fresh_db().await;
        Migrator::up(&db, None)
            .await
            .expect("first up applies everything");
        let before = dedicated_versions(&db).await;
        Migrator::up(&db, None)
            .await
            .expect("second up returns Ok(()) without applying anything");
        assert!(
            Migrator::get_pending_migrations(&db)
                .await
                .expect("pending list is readable")
                .is_empty(),
            "get_pending_migrations rend une liste vide"
        );
        assert_eq!(
            dedicated_versions(&db).await,
            before,
            "le second up laisse le tracking inchangé — aucun rejeu, aucune ligne ajoutée"
        );
        assert_eq!(
            before,
            registered_stems(),
            "le tracking d'une base appliquée porte exactement les enregistrements"
        );
    }

    /// Scenario : « down d'un pas défait la dernière appliquée, l'index `api_tokens` ». Contrat
    /// vérifié : `exec_down_with` itère les appliquées en ordre inverse de `migrations()`
    /// (vérifié au source sea-orm-migration 2.0.2, `exec.rs:305`), donc `down(Some(1))` défait
    /// `m20260930_000001_index_api_tokens_subject` — et lui seule — sans toucher au seed 003,
    /// `miryad_groups` intacte.
    #[tokio::test]
    async fn down_one_step_undoes_the_last_applied_entry() {
        let db = fresh_db().await;
        Migrator::up(&db, None)
            .await
            .expect("up applies every registered migration");
        Migrator::down(&db, Some(1))
            .await
            .expect("down Some(1) returns Ok(())");
        let applied: Vec<String> = Migrator::get_applied_migrations(&db)
            .await
            .expect("applied list is readable")
            .iter()
            .map(sea_orm_migration::Migration::name)
            .map(str::to_string)
            .collect();
        let pending: Vec<String> = Migrator::get_pending_migrations(&db)
            .await
            .expect("pending list is readable")
            .iter()
            .map(sea_orm_migration::Migration::name)
            .map(str::to_string)
            .collect();
        let mut expected_applied = registered_stems();
        let last_registered = expected_applied
            .pop()
            .expect("the registration list is never empty");
        assert_eq!(
            last_registered, "m20260930_000001_index_api_tokens_subject",
            "la dernière appliquée en ordre d'enregistrement est l'index de 2026-09-30, non la seed 003"
        );
        assert_eq!(
            applied, expected_applied,
            "get_applied_migrations rend tout sauf la dernière, en ordre @migrations"
        );
        assert_eq!(
            pending,
            vec![last_registered.to_string()],
            "get_pending_migrations rend la seule entrée défaite"
        );
        let manager = SchemaManager::new(&db);
        assert!(
            manager
                .has_table("miryad_groups")
                .await
                .expect("has_table probe succeeds"),
            "le rollback de 002 n'a pas été touché"
        );
    }

    /// Scenario : « la consommatrice installée en tête ne contamine pas le tracking miryad-core ».
    #[tokio::test]
    async fn an_app_migrator_installed_first_does_not_contaminate_the_miryad_core_tracking() {
        let db = fresh_db().await;
        AppMigrator::up(&db, None)
            .await
            .expect("the app migrator installs first under the default seaql_migrations");
        Migrator::up(&db, None)
            .await
            .expect("Ok(()) without DbErr::Custom of missing version — no contamination");
        let manager = SchemaManager::new(&db);
        for table in [
            "miryad_api_tokens",
            "miryad_users",
            "miryad_groups",
            "miryad_group_memberships",
        ] {
            assert!(
                manager.has_table(table).await.expect("has_table probe succeeds"),
                "les tables de schéma `miryad_*` des trois filles sont posées : {table}"
            );
        }
        assert_eq!(
            dedicated_versions(&db).await,
            registered_stems(),
            "les entrées miryad ne sont couchées que dans `seaql_migrations_miryad_core`"
        );
        assert_eq!(
            raw_versions(&db, "seaql_migrations")
                .await
                .expect("the default table exists, created by the app migrator"),
            vec!["mod".to_string()],
            "`seaql_migrations` ne contient que l'entrée du migrateur applicatif"
        );
    }

    /// Scenario : « la quatrième migration n'existe que sous la feature workflow » (amendement
    /// 2026-09-23 — dixième `Scenario` de ./mod.sdd). Contrat vérifié sur les deux graphes : le
    /// tracking porte exactement les enregistrements du graphe de features courant — quatre
    /// entrées sans `workflow`, cinq avec — et `miryad_workflow_definitions` n'existe que sous
    /// `workflow`, son entrée étant la quatrième, après les trois internes et avant l'index.
    #[tokio::test]
    async fn the_workflow_definitions_entry_exists_only_under_the_workflow_feature() {
        let db = fresh_db().await;
        Migrator::up(&db, None)
            .await
            .expect("up applies the registrations on a fresh base");
        let manager = SchemaManager::new(&db);
        let has_workflow_table = manager
            .has_table("miryad_workflow_definitions")
            .await
            .expect("has_table probe succeeds");
        let tracked = dedicated_versions(&db).await;
        assert_eq!(
            tracked,
            registered_stems(),
            "le tracking porte exactement les enregistrements du graphe de features courant"
        );
        #[cfg(not(feature = "workflow"))]
        assert!(
            !has_workflow_table,
            "sans `workflow`, la table `miryad_workflow_definitions` n'existe pas"
        );
        #[cfg(feature = "workflow")]
        {
            assert!(
                has_workflow_table,
                "avec `workflow`, la table `miryad_workflow_definitions` existe"
            );
            assert_eq!(
                tracked.get(3).map(String::as_str),
                Some("m20260923_000001_create_workflow_definitions"),
                "l'entrée workflow est la quatrième, après les trois internes"
            );
        }
    }

    /// Scenario : « le migrateur consommatrice au tracking défaut s'installe après
    /// miryad-core ». Contrat vérifié : l'`AppMigrator` au `migration_table_name` par défaut
    /// compose après le `Migrator` miryad-core sans collision, `has_table` répond `true` pour
    /// `seaql_migrations`, et un rejeu du `Migrator` miryad-core reste `Ok(())`.
    #[tokio::test]
    async fn composes_with_an_independent_migrator_sharing_the_default_table_name() {
        let db = Database::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite connects");

        Migrator::up(&db, None)
            .await
            .expect("miryad-core migrations apply cleanly");
        // Nom de tracking de l'`AppMigration` : `#[derive(DeriveMigrationName)]` enregistre le
        // stem du fichier source déclarant (`get_file_stem(file!())`, vérifié au source de
        // sea-orm-macros/sea-orm-migration 2.0.2). Ce test est inline dans `src/migration/mod.rs`,
        // donc l'entrée est couchée dans `seaql_migrations` sous le nom `mod` — stem de ./mod.rs.
        // Inoffensif tant que les tables de test (`app_widgets`) restent distinctes des tables
        // `miryad_*` : aucun renommage, arbitré par Sébastien le 2026-09-29 (./mod.sdd `Must`).
        AppMigrator::up(&db, None)
            .await
            .expect("app migrator composes without colliding on the tracking table");

        // Then du Scenario : c'est l'`AppMigrator` qui installe la table de suivi par défaut.
        let manager = SchemaManager::new(&db);
        assert!(
            manager
                .has_table("seaql_migrations")
                .await
                .expect("has_table probe succeeds"),
            "`has_table` répond true pour `seaql_migrations` après le run de l'app"
        );

        // Régression : réappliquer le migrateur miryad-core après que l'app ait tourné son propre
        // migrateur (table de suivi par défaut) ne doit pas non plus échouer.
        Migrator::up(&db, None)
            .await
            .expect("miryad-core migrator remains idempotent alongside an app migrator");
    }
}
