mod m20260822_000001_create_api_tokens;

pub struct Migrator;

impl sea_orm_migration::MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn sea_orm_migration::MigrationTrait>> {
        vec![Box::new(m20260822_000001_create_api_tokens::Migration)]
    }
}
