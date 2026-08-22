use miryad_core::resource::{AccessPolicy, MiryadResource};
use sea_orm::entity::prelude::*;

/// Entité d'exemple avec propriétaire — cas "recettes partagées en lecture,
/// modifiables par leur auteur uniquement".
mod recipe {
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
}

impl MiryadResource for recipe::Entity {
    fn resource_name() -> &'static str {
        "recipes"
    }

    fn read_policy() -> AccessPolicy {
        AccessPolicy::Public
    }

    fn write_policy() -> AccessPolicy {
        AccessPolicy::OwnerOnly
    }

    fn owner_column() -> Option<<Self as EntityTrait>::Column> {
        Some(recipe::Column::OwnerId)
    }
}

/// Entité d'exemple sans propriétaire — référentiel partagé, réservé aux admins.
mod ingredient {
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
}

impl MiryadResource for ingredient::Entity {
    fn resource_name() -> &'static str {
        "ingredients"
    }

    fn read_policy() -> AccessPolicy {
        AccessPolicy::AdminOnly
    }

    fn write_policy() -> AccessPolicy {
        AccessPolicy::AdminOnly
    }

    fn owner_column() -> Option<<Self as EntityTrait>::Column> {
        None
    }
}

#[test]
fn recipe_declares_owner_only_write_with_public_read() {
    assert_eq!(recipe::Entity::resource_name(), "recipes");
    assert_eq!(recipe::Entity::read_policy(), AccessPolicy::Public);
    assert_eq!(recipe::Entity::write_policy(), AccessPolicy::OwnerOnly);
    assert!(matches!(
        recipe::Entity::owner_column(),
        Some(recipe::Column::OwnerId)
    ));
}

#[test]
fn ingredient_has_no_owner_and_is_admin_only() {
    assert_eq!(ingredient::Entity::resource_name(), "ingredients");
    assert_eq!(ingredient::Entity::read_policy(), AccessPolicy::AdminOnly);
    assert_eq!(ingredient::Entity::write_policy(), AccessPolicy::AdminOnly);
    assert!(ingredient::Entity::owner_column().is_none());
}
