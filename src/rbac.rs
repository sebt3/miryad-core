use sea_orm::entity::prelude::*;
use sea_orm::{DatabaseConnection, ModelTrait};

use crate::resource::{AccessPolicy, MiryadResource};
use crate::users::group::{is_admin, is_member};
use crate::users::user;

/// `record` doit déjà être chargé — cette fonction ne fait pas de requête pour le récupérer,
/// elle évalue une politique contre un enregistrement en main (cf. feature 4 pour le filtrage de
/// liste, hors-scope ici).
pub async fn can_read<E>(
    db: &DatabaseConnection,
    user: &user::Model,
    record: &E::Model,
) -> Result<bool, DbErr>
where
    E: MiryadResource,
    E::Model: ModelTrait<Entity = E>,
{
    evaluate::<E>(db, E::read_policy(), E::owner_column(), user, record).await
}

pub async fn can_write<E>(
    db: &DatabaseConnection,
    user: &user::Model,
    record: &E::Model,
) -> Result<bool, DbErr>
where
    E: MiryadResource,
    E::Model: ModelTrait<Entity = E>,
{
    evaluate::<E>(db, E::write_policy(), E::owner_column(), user, record).await
}

async fn evaluate<E>(
    db: &DatabaseConnection,
    policy: AccessPolicy,
    owner_column: Option<E::Column>,
    user: &user::Model,
    record: &E::Model,
) -> Result<bool, DbErr>
where
    E: EntityTrait,
    E::Model: ModelTrait<Entity = E>,
{
    if policy == AccessPolicy::Public {
        return Ok(true);
    }
    // L'admin l'emporte toujours sur les autres politiques (cf. feature 1, doc du trait
    // MiryadResource : "+ les membres du groupe admin" sur OwnerOnly et Group(name)).
    if is_admin(db, user.id).await? {
        return Ok(true);
    }

    match policy {
        AccessPolicy::Public => unreachable!("handled above"),
        AccessPolicy::AdminOnly => Ok(false),
        AccessPolicy::Group(name) => is_member(db, user.id, name).await,
        AccessPolicy::OwnerOnly => {
            // Contrat feature 1 : `owner_column() == None` avec `OwnerOnly` est un comportement
            // non défini au niveau du trait — on choisit de refuser plutôt que de risquer un
            // accès non voulu (fail-closed).
            let Some(col) = owner_column else {
                return Ok(false);
            };
            let owner_value = record.get(col);
            Ok(owner_value == sea_orm::Value::from(user.id))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migration::Migrator;
    use crate::users::{resolve_user, sync_groups_from_oidc};
    use sea_orm_migration::MigratorTrait;

    mod recipe {
        use crate::resource::{AccessPolicy, MiryadResource};
        use sea_orm::entity::prelude::*;

        #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
        #[sea_orm(table_name = "recipes")]
        pub struct Model {
            #[sea_orm(primary_key)]
            pub id: i32,
            pub title: String,
            pub owner_id: i32,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {}

        impl ActiveModelBehavior for ActiveModel {}

        impl MiryadResource for Entity {
            fn resource_name() -> &'static str {
                "recipes"
            }
            fn read_policy() -> AccessPolicy {
                AccessPolicy::Public
            }
            fn write_policy() -> AccessPolicy {
                AccessPolicy::OwnerOnly
            }
            fn owner_column() -> Option<Column> {
                Some(Column::OwnerId)
            }
        }
    }

    mod ingredient {
        use crate::resource::{AccessPolicy, MiryadResource};
        use sea_orm::entity::prelude::*;

        #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
        #[sea_orm(table_name = "ingredients")]
        pub struct Model {
            #[sea_orm(primary_key)]
            pub id: i32,
            pub name: String,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {}

        impl ActiveModelBehavior for ActiveModel {}

        impl MiryadResource for Entity {
            fn resource_name() -> &'static str {
                "ingredients"
            }
            fn read_policy() -> AccessPolicy {
                AccessPolicy::Group("editors")
            }
            fn write_policy() -> AccessPolicy {
                AccessPolicy::AdminOnly
            }
            fn owner_column() -> Option<Column> {
                None
            }
        }
    }

    async fn test_db() -> DatabaseConnection {
        let db = sea_orm::Database::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite connects");
        Migrator::up(&db, None).await.expect("migrations apply cleanly");
        db
    }

    #[tokio::test]
    async fn owner_only_allows_owner_denies_others_allows_admin() {
        let db = test_db().await;
        let owner = resolve_user(&db, "owner", None).await.expect("resolve");
        let other = resolve_user(&db, "other", None).await.expect("resolve");
        let admin = resolve_user(&db, "admin-user", None).await.expect("resolve");
        sync_groups_from_oidc(&db, admin.id, &["admin".to_string()])
            .await
            .expect("sync");

        let record = recipe::Model {
            id: 1,
            title: "Tarte".to_string(),
            owner_id: owner.id,
        };

        assert!(can_write::<recipe::Entity>(&db, &owner, &record).await.unwrap());
        assert!(!can_write::<recipe::Entity>(&db, &other, &record).await.unwrap());
        assert!(can_write::<recipe::Entity>(&db, &admin, &record).await.unwrap());
    }

    #[tokio::test]
    async fn public_policy_always_allows() {
        let db = test_db().await;
        let anyone = resolve_user(&db, "anyone", None).await.expect("resolve");
        let record = recipe::Model {
            id: 1,
            title: "Tarte".to_string(),
            owner_id: 999,
        };
        assert!(can_read::<recipe::Entity>(&db, &anyone, &record).await.unwrap());
    }

    #[tokio::test]
    async fn group_policy_allows_member_and_admin_denies_others() {
        let db = test_db().await;
        let member = resolve_user(&db, "member", None).await.expect("resolve");
        sync_groups_from_oidc(&db, member.id, &["editors".to_string()])
            .await
            .expect("sync");
        let admin = resolve_user(&db, "admin-user", None).await.expect("resolve");
        sync_groups_from_oidc(&db, admin.id, &["admin".to_string()])
            .await
            .expect("sync");
        let stranger = resolve_user(&db, "stranger", None).await.expect("resolve");

        let record = ingredient::Model {
            id: 1,
            name: "Sel".to_string(),
        };

        assert!(
            can_read::<ingredient::Entity>(&db, &member, &record)
                .await
                .unwrap()
        );
        assert!(
            can_read::<ingredient::Entity>(&db, &admin, &record)
                .await
                .unwrap()
        );
        assert!(
            !can_read::<ingredient::Entity>(&db, &stranger, &record)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn admin_only_denies_non_admin() {
        let db = test_db().await;
        let stranger = resolve_user(&db, "stranger", None).await.expect("resolve");
        let record = ingredient::Model {
            id: 1,
            name: "Sel".to_string(),
        };
        assert!(
            !can_write::<ingredient::Entity>(&db, &stranger, &record)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn owner_only_without_owner_column_is_fail_closed() {
        // `ingredient` déclare `owner_column() -> None`. Si son `write_policy` était `OwnerOnly`
        // (contrat non respecté d'après feature 1), l'évaluateur doit refuser, pas paniquer ni
        // autoriser par défaut.
        let db = test_db().await;
        let stranger = resolve_user(&db, "stranger", None).await.expect("resolve");
        let record = ingredient::Model {
            id: 1,
            name: "Sel".to_string(),
        };
        let result = evaluate::<ingredient::Entity>(
            &db,
            AccessPolicy::OwnerOnly,
            ingredient::Entity::owner_column(),
            &stranger,
            &record,
        )
        .await
        .expect("evaluation does not error");
        assert!(!result);
    }
}
