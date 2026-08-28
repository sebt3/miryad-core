//! Migrations internes (`miryad_*`) — à appliquer via [`Migrator`](crate::migration::Migrator)::`up`.

mod m20260822_000001_create_api_tokens;
mod m20260822_000002_create_users_groups;
mod m20260822_000003_seed_admin_group;

use sea_orm::sea_query::IntoIden;

/// Migrateur interne miryad-core (tables `miryad_*`, tracking table dédiée).
pub struct Migrator;

impl sea_orm_migration::MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn sea_orm_migration::MigrationTrait>> {
        vec![
            Box::new(m20260822_000001_create_api_tokens::Migration),
            Box::new(m20260822_000002_create_users_groups::Migration),
            Box::new(m20260822_000003_seed_admin_group::Migration),
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

    #[tokio::test]
    async fn composes_with_an_independent_migrator_sharing_the_default_table_name() {
        let db = Database::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite connects");

        Migrator::up(&db, None)
            .await
            .expect("miryad-core migrations apply cleanly");
        AppMigrator::up(&db, None)
            .await
            .expect("app migrator composes without colliding on the tracking table");

        // Régression : réappliquer le migrateur miryad-core après que l'app ait tourné son propre
        // migrateur (table de suivi par défaut) ne doit pas non plus échouer.
        Migrator::up(&db, None)
            .await
            .expect("miryad-core migrator remains idempotent alongside an app migrator");
    }
}
