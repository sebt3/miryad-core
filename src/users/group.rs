use chrono::Utc;
use sea_orm::entity::prelude::*;
use sea_orm::{ConnectionTrait, Set};

use crate::users::membership;

/// Nom du groupe admin, pré-câblé (seedé par migration) mais sans rien de spécial au niveau
/// schéma — juste la convention lue par l'évaluateur RBAC (`rbac::is_admin`).
pub const ADMIN_GROUP_NAME: &str = "admin";

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "miryad_groups")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    #[sea_orm(unique)]
    pub name: String,
    pub created_at: DateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

pub type Group = Entity;

/// Get-or-create par nom — un groupe cité dans un claim `groups` mais jamais vu est créé à la
/// volée (pas de registre préalable des noms de groupe autorisés).
pub async fn ensure_group<C: ConnectionTrait>(db: &C, name: &str) -> Result<i32, DbErr> {
    if let Some(existing) = Entity::find().filter(Column::Name.eq(name)).one(db).await? {
        return Ok(existing.id);
    }

    let active = ActiveModel {
        name: Set(name.to_string()),
        created_at: Set(Utc::now()),
        ..Default::default()
    };

    match active.insert(db).await {
        Ok(model) => Ok(model.id),
        Err(_) => Entity::find()
            .filter(Column::Name.eq(name))
            .one(db)
            .await?
            .map(|g| g.id)
            .ok_or_else(|| DbErr::RecordNotFound(format!("group {name} vanished"))),
    }
}

pub async fn is_admin<C: ConnectionTrait>(db: &C, user_id: i32) -> Result<bool, DbErr> {
    is_member(db, user_id, ADMIN_GROUP_NAME).await
}

pub async fn is_member<C: ConnectionTrait>(db: &C, user_id: i32, group_name: &str) -> Result<bool, DbErr> {
    let Some(group) = Entity::find().filter(Column::Name.eq(group_name)).one(db).await? else {
        return Ok(false);
    };

    let exists = membership::Entity::find()
        .filter(membership::Column::UserId.eq(user_id))
        .filter(membership::Column::GroupId.eq(group.id))
        .one(db)
        .await?
        .is_some();
    Ok(exists)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migration::Migrator;
    use crate::users::membership::sync_group_memberships;
    use crate::users::user::resolve_user;
    use sea_orm_migration::MigratorTrait;

    async fn test_db() -> DatabaseConnection {
        let db = sea_orm::Database::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite connects");
        Migrator::up(&db, None).await.expect("migrations apply cleanly");
        db
    }

    #[tokio::test]
    async fn admin_group_is_seeded_by_migration() {
        let db = test_db().await;
        let admin_group = Entity::find()
            .filter(Column::Name.eq(ADMIN_GROUP_NAME))
            .one(&db)
            .await
            .expect("query succeeds");
        assert!(admin_group.is_some());
    }

    #[tokio::test]
    async fn is_member_true_for_member_false_otherwise() {
        let db = test_db().await;
        let user = resolve_user(&db, "sub-1", None).await.expect("resolve succeeds");
        sync_group_memberships(&db, user.id, &["editors".to_string()])
            .await
            .expect("sync succeeds");

        assert!(is_member(&db, user.id, "editors").await.expect("query succeeds"));
        assert!(!is_member(&db, user.id, "admin").await.expect("query succeeds"));
    }

    #[tokio::test]
    async fn is_member_false_for_unknown_group() {
        let db = test_db().await;
        let user = resolve_user(&db, "sub-1", None).await.expect("resolve succeeds");
        assert!(
            !is_member(&db, user.id, "does-not-exist")
                .await
                .expect("query succeeds")
        );
    }
}
