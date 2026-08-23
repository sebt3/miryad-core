use std::any::Any;

use async_graphql::dynamic::ResolverContext;
use sea_orm::Condition;
use sea_orm::sea_query::{Alias, Expr, ExprTrait};
use seaography::{GuardAction, LifecycleHooksInterface, OperationType};

use crate::auth::AuthPrincipal;
use crate::graphql::principal::GraphQlPrincipal;
use crate::graphql::registry::{EntityPolicy, PolicyRegistry};
use crate::resource::AccessPolicy;

/// Pont entre `seaography::LifecycleHooksInterface` (hooks synchrones, un accès DB par requête
/// déjà fait en amont via `GraphQlPrincipal`) et `rbac.rs` (feature 3) — même politique
/// qu'en REST, pas un second système d'autorisation.
pub struct MiryadHooks {
    registry: PolicyRegistry,
}

impl MiryadHooks {
    pub fn new(registry: PolicyRegistry) -> Self {
        Self { registry }
    }

    fn policy_for(&self, entity: &str) -> Option<&EntityPolicy> {
        self.registry.get(entity)
    }
}

fn is_admin(ctx: &ResolverContext) -> bool {
    ctx.data::<GraphQlPrincipal>()
        .map(|p| p.is_admin)
        .unwrap_or(false)
}

fn is_member(ctx: &ResolverContext, group_name: &str) -> bool {
    ctx.data::<GraphQlPrincipal>()
        .map(|p| p.groups.contains(group_name))
        .unwrap_or(false)
}

#[async_trait::async_trait]
impl LifecycleHooksInterface for MiryadHooks {
    fn entity_guard(&self, ctx: &ResolverContext, entity: &str, action: OperationType) -> GuardAction {
        let Some(policy) = self.policy_for(entity) else {
            return GuardAction::Allow;
        };
        let applicable = match action {
            OperationType::Read => policy.read,
            OperationType::Create | OperationType::Update | OperationType::Delete => policy.write,
        };

        if is_admin(ctx) {
            return GuardAction::Allow;
        }

        match applicable {
            AccessPolicy::Public => GuardAction::Allow,
            // Le filtrage par propriétaire se fait via entity_filter (une clause de requête),
            // pas ici — sauf à la création, où il n'y a pas encore de ligne à filtrer.
            AccessPolicy::OwnerOnly => {
                if action == OperationType::Create && policy.owner_column.is_none() {
                    // Contrat feature 1 : OwnerOnly sans owner_column est fail-closed.
                    GuardAction::Block(Some("MRD-GQL-001: OwnerOnly without owner_column".to_string()))
                } else {
                    GuardAction::Allow
                }
            }
            AccessPolicy::AdminOnly => GuardAction::Block(None),
            AccessPolicy::Group(name) => {
                if is_member(ctx, name) {
                    GuardAction::Allow
                } else {
                    GuardAction::Block(None)
                }
            }
        }
    }

    fn entity_filter(&self, ctx: &ResolverContext, entity: &str, action: OperationType) -> Option<Condition> {
        let policy = self.policy_for(entity)?;
        let applicable = match action {
            OperationType::Read => policy.read,
            OperationType::Create | OperationType::Update | OperationType::Delete => policy.write,
        };
        if applicable != AccessPolicy::OwnerOnly || is_admin(ctx) {
            return None;
        }

        let owner_column = policy.owner_column.as_ref()?;
        let user_id = ctx.data::<GraphQlPrincipal>().ok()?.user_id;
        Some(Condition::all().add(Expr::col(Alias::new(owner_column.as_str())).eq(user_id)))
    }

    /// Miroir GraphQL de `rest::core::create` appelant `E::before_create` (feature 7b) — create
    /// only, Seaography ne déclenche ce hook que sur un insert ("only insert for now", cf.
    /// `docs/features/7b-hooks-metier-crud.md`).
    fn before_active_model_save(
        &self,
        ctx: &ResolverContext,
        entity: &str,
        action: OperationType,
        active_model: &mut dyn Any,
    ) -> GuardAction {
        if action != OperationType::Create {
            return GuardAction::Allow;
        }
        let Some(policy) = self.policy_for(entity) else {
            return GuardAction::Allow;
        };
        let Ok(principal) = ctx.data::<AuthPrincipal>() else {
            // AuthPrincipal doit toujours être injecté par graphql_handler — absence = bug
            // d'intégration côté app, pas une raison métier de bloquer silencieusement.
            return GuardAction::Block(Some(
                "MRD-GQL-002: AuthPrincipal missing from request data".to_string(),
            ));
        };

        match (policy.before_create)(active_model, principal) {
            Ok(()) => GuardAction::Allow,
            // Seaography ne transporte qu'une chaîne — cf. question d'implémentation dans
            // docs/features/7b-hooks-metier-crud.md sur l'absence d'extensions structurées ici.
            Err(err) => GuardAction::Block(Some(match err.code {
                Some(code) => format!("{code}: {}", err.message),
                None => err.message,
            })),
        }
    }
}
