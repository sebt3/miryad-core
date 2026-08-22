use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(User::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(User::Id)
                            .integer()
                            .not_null()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(ColumnDef::new(User::Subject).string().not_null().unique_key())
                    .col(ColumnDef::new(User::Email).string())
                    .col(ColumnDef::new(User::DisplayName).string())
                    .col(
                        ColumnDef::new(User::CreatedAt)
                            .timestamp_with_time_zone()
                            .not_null(),
                    )
                    .to_owned(),
            )
            .await?;

        manager
            .create_table(
                Table::create()
                    .table(Group::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(Group::Id)
                            .integer()
                            .not_null()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(ColumnDef::new(Group::Name).string().not_null().unique_key())
                    .col(
                        ColumnDef::new(Group::CreatedAt)
                            .timestamp_with_time_zone()
                            .not_null(),
                    )
                    .to_owned(),
            )
            .await?;

        manager
            .create_table(
                Table::create()
                    .table(GroupMembership::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(GroupMembership::Id)
                            .integer()
                            .not_null()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(ColumnDef::new(GroupMembership::UserId).integer().not_null())
                    .col(ColumnDef::new(GroupMembership::GroupId).integer().not_null())
                    .foreign_key(
                        ForeignKey::create()
                            .from(GroupMembership::Table, GroupMembership::UserId)
                            .to(User::Table, User::Id)
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .from(GroupMembership::Table, GroupMembership::GroupId)
                            .to(Group::Table, Group::Id)
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .index(
                        Index::create()
                            .unique()
                            .col(GroupMembership::UserId)
                            .col(GroupMembership::GroupId),
                    )
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(GroupMembership::Table).to_owned())
            .await?;
        manager
            .drop_table(Table::drop().table(Group::Table).to_owned())
            .await?;
        manager
            .drop_table(Table::drop().table(User::Table).to_owned())
            .await
    }
}

#[derive(DeriveIden)]
enum User {
    #[sea_orm(iden = "miryad_users")]
    Table,
    Id,
    Subject,
    Email,
    DisplayName,
    CreatedAt,
}

#[derive(DeriveIden)]
enum Group {
    #[sea_orm(iden = "miryad_groups")]
    Table,
    Id,
    Name,
    CreatedAt,
}

#[derive(DeriveIden)]
enum GroupMembership {
    #[sea_orm(iden = "miryad_group_memberships")]
    Table,
    Id,
    UserId,
    GroupId,
}
