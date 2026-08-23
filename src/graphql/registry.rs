use std::any::Any;
use std::collections::HashMap;

use sea_orm::{ActiveModelTrait, Iden};

use crate::auth::AuthPrincipal;
use crate::resource::{AccessPolicy, HookError, MiryadResource};

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
    /// Appelle `E::before_create` sur l'`ActiveModel` type-erasé que fournit
    /// `before_active_model_save` (Seaography) — monomorphisé sur `E` à l'enregistrement, comme
    /// le reste de cette structure. `pub(crate)` : appelé uniquement par `graphql::hooks`.
    pub(crate) before_create: fn(&mut dyn Any, &AuthPrincipal) -> Result<(), HookError>,
}

fn call_before_create<E: MiryadResource>(
    active_model: &mut dyn Any,
    principal: &AuthPrincipal,
) -> Result<(), HookError> {
    let concrete = active_model
        .downcast_mut::<E::ActiveModel>()
        .expect("active_model type matches the entity registered under this name");
    let current = std::mem::replace(concrete, <E::ActiveModel as ActiveModelTrait>::default());
    *concrete = E::before_create(current, principal)?;
    Ok(())
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
                before_create: call_before_create::<E>,
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
    use crate::auth::PrincipalSource;

    mod recipe {
        use crate::resource::{AccessPolicy, HookError, MiryadResource};
        use sea_orm::ActiveValue::Set;
        use sea_orm::entity::prelude::*;

        #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
        #[sea_orm(table_name = "recipes")]
        pub struct Model {
            #[sea_orm(primary_key)]
            pub id: i32,
            pub owner_id: i32,
            pub title: String,
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

            fn before_create(
                active: ActiveModel,
                _principal: &crate::auth::AuthPrincipal,
            ) -> Result<ActiveModel, HookError> {
                let title = match &active.title {
                    sea_orm::ActiveValue::Set(v) | sea_orm::ActiveValue::Unchanged(v) => v.clone(),
                    sea_orm::ActiveValue::NotSet => String::new(),
                };
                if title.is_empty() {
                    return Err(HookError::with_code("RECIPE-001", "title must not be empty"));
                }
                let mut active = active;
                active.title = Set(title.to_uppercase());
                Ok(active)
            }
        }
    }

    fn principal() -> AuthPrincipal {
        AuthPrincipal {
            subject: "alice".to_string(),
            email: None,
            source: PrincipalSource::ApiToken { token_id: 0 },
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

    #[test]
    fn before_create_dispatch_mutates_the_downcast_active_model() {
        use sea_orm::ActiveValue::Set;

        let mut registry = PolicyRegistry::new();
        registry.register::<recipe::Entity>();
        let policy = registry.get("recipes").expect("registered");

        let mut active = recipe::ActiveModel {
            id: sea_orm::ActiveValue::NotSet,
            owner_id: sea_orm::ActiveValue::NotSet,
            title: Set("tarte".to_string()),
        };
        (policy.before_create)(&mut active, &principal()).expect("hook succeeds");

        assert_eq!(active.title, Set("TARTE".to_string()));
    }

    #[test]
    fn before_create_dispatch_propagates_hook_error() {
        let mut registry = PolicyRegistry::new();
        registry.register::<recipe::Entity>();
        let policy = registry.get("recipes").expect("registered");

        let mut active = recipe::ActiveModel {
            id: sea_orm::ActiveValue::NotSet,
            owner_id: sea_orm::ActiveValue::NotSet,
            title: sea_orm::ActiveValue::Set(String::new()),
        };
        let err = (policy.before_create)(&mut active, &principal()).expect_err("hook rejects");

        assert_eq!(err.code.as_deref(), Some("RECIPE-001"));
        assert_eq!(err.message, "title must not be empty");
    }
}
