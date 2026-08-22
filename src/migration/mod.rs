mod m20260822_000001_create_api_tokens;
mod m20260822_000002_create_users_groups;
mod m20260822_000003_seed_admin_group;

pub struct Migrator;

impl sea_orm_migration::MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn sea_orm_migration::MigrationTrait>> {
        vec![
            Box::new(m20260822_000001_create_api_tokens::Migration),
            Box::new(m20260822_000002_create_users_groups::Migration),
            Box::new(m20260822_000003_seed_admin_group::Migration),
        ]
    }
}
