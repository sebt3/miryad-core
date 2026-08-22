use sea_orm::entity::prelude::*;
use sea_orm::{Condition, DatabaseConnection, ModelTrait};

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

/// Autorisation de créer un nouvel enregistrement (feature 4) — il n'y en a pas encore à
/// comparer, donc pas de vérification par propriétaire : `OwnerOnly` autorise toujours la
/// création (le créateur devient le propriétaire), seules `Group`/`AdminOnly` filtrent selon
/// l'appartenance.
pub async fn can_create<E>(db: &DatabaseConnection, user: &user::Model) -> Result<bool, DbErr>
where
    E: MiryadResource,
{
    match E::write_policy() {
        AccessPolicy::Public | AccessPolicy::OwnerOnly => Ok(true),
        AccessPolicy::AdminOnly => is_admin(db, user.id).await,
        AccessPolicy::Group(name) => {
            if is_admin(db, user.id).await? {
                return Ok(true);
            }
            is_member(db, user.id, name).await
        }
    }
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

/// Résultat de l'évaluation RBAC pour une opération de liste (feature 4) — contrairement à
/// `can_read`/`can_write`, il n'y a pas encore d'enregistrement précis à comparer, donc pas de
/// simple booléen : soit on ne filtre pas, soit on filtre par propriétaire, soit c'est refusé
/// avant même de construire une requête.
#[derive(Debug)]
pub enum ListAccess {
    /// Politique publique, ou appelant admin — aucune restriction à appliquer.
    Unrestricted,
    /// Politique `OwnerOnly` pour un appelant non-admin — condition à ajouter à la requête de
    /// liste (`WHERE owner_column = user.id`).
    FilterByOwner(Condition),
    /// Politique `Group`/`AdminOnly` sans l'appartenance requise — pas de requête à exécuter.
    Forbidden,
}

pub async fn list_access<E>(db: &DatabaseConnection, user: &user::Model) -> Result<ListAccess, DbErr>
where
    E: MiryadResource,
{
    let policy = E::read_policy();

    if policy == AccessPolicy::Public {
        return Ok(ListAccess::Unrestricted);
    }
    if is_admin(db, user.id).await? {
        return Ok(ListAccess::Unrestricted);
    }

    match policy {
        AccessPolicy::Public => unreachable!("handled above"),
        AccessPolicy::AdminOnly => Ok(ListAccess::Forbidden),
        AccessPolicy::Group(name) => {
            if is_member(db, user.id, name).await? {
                Ok(ListAccess::Unrestricted)
            } else {
                Ok(ListAccess::Forbidden)
            }
        }
        AccessPolicy::OwnerOnly => match E::owner_column() {
            Some(col) => Ok(ListAccess::FilterByOwner(Condition::all().add(col.eq(user.id)))),
            // Même contrat fail-closed que `evaluate` : pas de colonne, pas d'accès.
            None => Ok(ListAccess::Forbidden),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migration::Migrator;
    use crate::users::{resolve_user, sync_group_memberships};
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

    /// `OwnerOnly` en lecture *et* écriture — `recipe`/`ingredient` ne couvrent pas ce cas
    /// (`recipe` est public en lecture), nécessaire pour tester `ListAccess::FilterByOwner`.
    mod note {
        use crate::resource::{AccessPolicy, MiryadResource};
        use sea_orm::entity::prelude::*;

        #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
        #[sea_orm(table_name = "notes")]
        pub struct Model {
            #[sea_orm(primary_key)]
            pub id: i32,
            pub body: String,
            pub owner_id: i32,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {}

        impl ActiveModelBehavior for ActiveModel {}

        impl MiryadResource for Entity {
            fn resource_name() -> &'static str {
                "notes"
            }
            fn read_policy() -> AccessPolicy {
                AccessPolicy::OwnerOnly
            }
            fn write_policy() -> AccessPolicy {
                AccessPolicy::OwnerOnly
            }
            fn owner_column() -> Option<Column> {
                Some(Column::OwnerId)
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
        sync_group_memberships(&db, admin.id, &["admin".to_string()])
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
        sync_group_memberships(&db, member.id, &["editors".to_string()])
            .await
            .expect("sync");
        let admin = resolve_user(&db, "admin-user", None).await.expect("resolve");
        sync_group_memberships(&db, admin.id, &["admin".to_string()])
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

    #[tokio::test]
    async fn list_access_public_is_unrestricted() {
        let db = test_db().await;
        let anyone = resolve_user(&db, "anyone", None).await.expect("resolve");
        assert!(matches!(
            list_access::<recipe::Entity>(&db, &anyone).await.unwrap(),
            ListAccess::Unrestricted
        ));
    }

    #[tokio::test]
    async fn list_access_owner_only_filters_for_non_admin_but_not_admin() {
        let db = test_db().await;
        let owner = resolve_user(&db, "owner", None).await.expect("resolve");
        let admin = resolve_user(&db, "admin-user", None).await.expect("resolve");
        sync_group_memberships(&db, admin.id, &["admin".to_string()])
            .await
            .expect("sync");

        match list_access::<note::Entity>(&db, &owner).await.unwrap() {
            ListAccess::FilterByOwner(_) => (),
            other => panic!("expected FilterByOwner for a non-admin, got {other:?}"),
        }
        assert!(matches!(
            list_access::<note::Entity>(&db, &admin).await.unwrap(),
            ListAccess::Unrestricted
        ));
    }

    #[tokio::test]
    async fn list_access_group_policy_forbidden_without_membership() {
        let db = test_db().await;
        let stranger = resolve_user(&db, "stranger", None).await.expect("resolve");
        let member = resolve_user(&db, "member", None).await.expect("resolve");
        sync_group_memberships(&db, member.id, &["editors".to_string()])
            .await
            .expect("sync");

        assert!(matches!(
            list_access::<ingredient::Entity>(&db, &stranger).await.unwrap(),
            ListAccess::Forbidden
        ));
        // `ingredient.read_policy()` est `Group("editors")`.
        assert!(matches!(
            list_access::<ingredient::Entity>(&db, &member).await.unwrap(),
            ListAccess::Unrestricted
        ));
    }
}
