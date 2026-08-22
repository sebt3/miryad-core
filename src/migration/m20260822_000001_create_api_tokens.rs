use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(ApiToken::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(ApiToken::Id)
                            .integer()
                            .not_null()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(ColumnDef::new(ApiToken::Subject).string().not_null())
                    .col(ColumnDef::new(ApiToken::Name).string().not_null())
                    .col(
                        ColumnDef::new(ApiToken::TokenHash)
                            .string()
                            .not_null()
                            .unique_key(),
                    )
                    .col(
                        ColumnDef::new(ApiToken::CreatedAt)
                            .timestamp_with_time_zone()
                            .not_null(),
                    )
                    .col(ColumnDef::new(ApiToken::ExpiresAt).timestamp_with_time_zone())
                    .col(ColumnDef::new(ApiToken::LastUsedAt).timestamp_with_time_zone())
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(ApiToken::Table).to_owned())
            .await
    }
}

#[derive(DeriveIden)]
enum ApiToken {
    #[sea_orm(iden = "miryad_api_tokens")]
    Table,
    Id,
    Subject,
    Name,
    TokenHash,
    CreatedAt,
    ExpiresAt,
    LastUsedAt,
}
