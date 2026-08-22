use sea_orm::entity::prelude::*;
use sea_orm::{ConnectionTrait, Set};

use crate::users::group::ensure_group;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "miryad_group_memberships")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    pub user_id: i32,
    pub group_id: i32,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

pub type GroupMembership = Entity;

/// Réconciliation complète des appartenances de `user_id` depuis un claim `groups` OIDC : les
/// groupes absents sont retirés, les nouveaux sont ajoutés (créés à la volée si inconnus). Seul
/// chemin d'écriture de cette table — pas d'API d'assignation manuelle (Authentik est la source
/// de vérité, cf. `docs/architecture.md`).
pub async fn sync_group_memberships<C: ConnectionTrait>(
    db: &C,
    user_id: i32,
    groups: &[String],
) -> Result<(), DbErr> {
    let mut wanted_group_ids = Vec::with_capacity(groups.len());
    for name in groups {
        wanted_group_ids.push(ensure_group(db, name).await?);
    }

    let current = Entity::find().filter(Column::UserId.eq(user_id)).all(db).await?;

    for membership in &current {
        if !wanted_group_ids.contains(&membership.group_id) {
            Entity::delete_by_id(membership.id).exec(db).await?;
        }
    }

    let current_group_ids: Vec<i32> = current.iter().map(|m| m.group_id).collect();
    for group_id in wanted_group_ids {
        if !current_group_ids.contains(&group_id) {
            let active = ActiveModel {
                user_id: Set(user_id),
                group_id: Set(group_id),
                ..Default::default()
            };
            active.insert(db).await?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migration::Migrator;
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
    async fn sync_adds_missing_groups_including_unknown_ones() {
        let db = test_db().await;
        let user = resolve_user(&db, "sub-1", None).await.expect("resolve succeeds");

        sync_group_memberships(&db, user.id, &["admin".to_string(), "editors".to_string()])
            .await
            .expect("sync succeeds");

        let memberships = Entity::find()
            .filter(Column::UserId.eq(user.id))
            .all(&db)
            .await
            .expect("query succeeds");
        assert_eq!(memberships.len(), 2);
    }

    #[tokio::test]
    async fn second_sync_with_fewer_groups_removes_stale_memberships() {
        let db = test_db().await;
        let user = resolve_user(&db, "sub-1", None).await.expect("resolve succeeds");

        sync_group_memberships(&db, user.id, &["admin".to_string(), "editors".to_string()])
            .await
            .expect("first sync succeeds");
        sync_group_memberships(&db, user.id, &["editors".to_string()])
            .await
            .expect("second sync succeeds");

        let memberships = Entity::find()
            .filter(Column::UserId.eq(user.id))
            .all(&db)
            .await
            .expect("query succeeds");
        assert_eq!(memberships.len(), 1);
    }

    #[tokio::test]
    async fn sync_is_idempotent() {
        let db = test_db().await;
        let user = resolve_user(&db, "sub-1", None).await.expect("resolve succeeds");

        sync_group_memberships(&db, user.id, &["editors".to_string()])
            .await
            .expect("first sync succeeds");
        sync_group_memberships(&db, user.id, &["editors".to_string()])
            .await
            .expect("second sync succeeds");

        let memberships = Entity::find()
            .filter(Column::UserId.eq(user.id))
            .all(&db)
            .await
            .expect("query succeeds");
        assert_eq!(memberships.len(), 1);
    }
}
