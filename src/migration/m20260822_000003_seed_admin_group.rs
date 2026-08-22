use sea_orm_migration::prelude::*;

use crate::users::group::{ADMIN_GROUP_NAME, ensure_group};

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    /// Seed idempotent — passe par le même `ensure_group` que la synchronisation OIDC (feature 3)
    /// plutôt que du SQL brut, pour rester portable SQLite/Postgres sans dupliquer la logique de
    /// conversion `DateTimeUtc`.
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        ensure_group(manager.get_connection(), ADMIN_GROUP_NAME).await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared("DELETE FROM miryad_groups WHERE name = 'admin'")
            .await?;
        Ok(())
    }
}
