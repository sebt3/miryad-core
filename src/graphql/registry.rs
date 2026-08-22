use std::collections::HashMap;

use sea_orm::Iden;

use crate::resource::{AccessPolicy, MiryadResource};

/// Politique d'une entité, indexée par son nom (`resource_name()`) plutôt que par son type —
/// Seaography identifie une entité par nom à l'exécution (`LifecycleHooksInterface`), alors que
/// `MiryadResource`/`rbac.rs` sont génériques sur le type à la compilation.
#[derive(Debug, Clone)]
pub struct EntityPolicy {
    pub read: AccessPolicy,
    pub write: AccessPolicy,
    /// Nom de colonne (pas la valeur `Column` typée) — `entity_filter` construit sa condition par
    /// nom brut, sans connaître `E::Column` au runtime.
    pub owner_column: Option<String>,
}

#[derive(Debug, Default)]
pub struct PolicyRegistry(HashMap<&'static str, EntityPolicy>);

impl PolicyRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// À appeler une fois par entité montée, en parallèle de
    /// `seaography::Builder::register_entity::<E>()`.
    pub fn register<E: MiryadResource>(&mut self) -> &mut Self {
        self.0.insert(
            E::resource_name(),
            EntityPolicy {
                read: E::read_policy(),
                write: E::write_policy(),
                owner_column: E::owner_column().map(|c| c.to_string()),
            },
        );
        self
    }

    pub fn get(&self, entity: &str) -> Option<&EntityPolicy> {
        self.0.get(entity)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    mod recipe {
        use crate::resource::{AccessPolicy, MiryadResource};
        use sea_orm::entity::prelude::*;

        #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
        #[sea_orm(table_name = "recipes")]
        pub struct Model {
            #[sea_orm(primary_key)]
            pub id: i32,
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

    #[test]
    fn register_then_lookup_returns_expected_policy() {
        let mut registry = PolicyRegistry::new();
        registry.register::<recipe::Entity>();

        let policy = registry.get("recipes").expect("registered entity found");
        assert_eq!(policy.read, AccessPolicy::Public);
        assert_eq!(policy.write, AccessPolicy::OwnerOnly);
        assert_eq!(policy.owner_column.as_deref(), Some("owner_id"));
    }

    #[test]
    fn unregistered_entity_returns_none() {
        let registry = PolicyRegistry::new();
        assert!(registry.get("does-not-exist").is_none());
    }
}
