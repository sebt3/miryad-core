//! Crate d'intégration rattachée à `src/resource.sdd` — exerce le contrat `MiryadResource`,
//! `AccessPolicy` et `HookError` : métadonnées relues telles quelles, hooks de création, de mise
//! à jour et de suppression, et parité de lecture des surfaces.

// Famille panic/unwrap/indexation tolérée dans cette crate de test : en-tête d'exemption
// équivalent à celui de `src/lib.rs`, posé d'après le `Must` de `tooling.sdd` — une crate
// d'intégration n'hérite pas des attributs de la librairie. Groupes `pedantic` et `cargo`
// restent `deny` sous `cfg(test)`.
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::dbg_macro,
        clippy::todo,
        clippy::unimplemented,
        clippy::print_stdout,
        clippy::print_stderr,
        clippy::arithmetic_side_effects,
        clippy::indexing_slicing,
        clippy::unwrap_in_result,
        clippy::panic_in_result_fn
    )
)]

use miryad_core::auth::{AuthPrincipal, PrincipalSource};
use miryad_core::resource::{AccessPolicy, HookError, MiryadResource};
use sea_orm::ActiveValue::Set;
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

/// Message porté par le rejet no-op du hook `before_update` de `gadget` (test).
const LABEL_NO_OP_MESSAGE: &str = "label must change on update";

/// Message porté par le rejet « locked » du hook `before_delete` de `widget` (test).
const LOCKED_REJECTION_MESSAGE: &str = "locked widgets must not be deleted";

/// Entité d'exemple pour le surcharge de `before_update` — le hook refuse la transition
/// no-op sur `label` (comparer avant/après n'est possible que parce que `existing` est prêté).
mod gadget {
    use super::{AccessPolicy, AuthPrincipal, HookError, LABEL_NO_OP_MESSAGE, MiryadResource};
    use sea_orm::ActiveValue;
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "gadgets")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i32,
        pub label: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}

    impl MiryadResource for Entity {
        fn resource_name() -> &'static str {
            "gadgets"
        }

        fn read_policy() -> AccessPolicy {
            AccessPolicy::Public
        }

        fn write_policy() -> AccessPolicy {
            AccessPolicy::AdminOnly
        }

        fn owner_column() -> Option<<Self as EntityTrait>::Column> {
            None
        }

        fn before_update(
            active: ActiveModel,
            existing: &Self::Model,
            _principal: &AuthPrincipal,
        ) -> Result<ActiveModel, HookError> {
            let no_op = match &active.label {
                ActiveValue::Set(label) | ActiveValue::Unchanged(label) => label == &existing.label,
                ActiveValue::NotSet => false,
            };
            if no_op {
                return Err(HookError::new(LABEL_NO_OP_MESSAGE));
            }
            Ok(active)
        }
    }
}

/// Entité d'exemple pour la surcharge de `before_delete` — le hook refuse la suppression d'une
/// ligne verrouillée avec un code applicatif libre.
mod widget {
    use super::{AccessPolicy, AuthPrincipal, HookError, LOCKED_REJECTION_MESSAGE, MiryadResource};
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "widgets")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i32,
        pub status: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}

    impl MiryadResource for Entity {
        fn resource_name() -> &'static str {
            "widgets"
        }

        fn read_policy() -> AccessPolicy {
            AccessPolicy::Public
        }

        fn write_policy() -> AccessPolicy {
            AccessPolicy::AdminOnly
        }

        fn owner_column() -> Option<<Self as EntityTrait>::Column> {
            None
        }

        fn before_delete(existing: &Self::Model, _principal: &AuthPrincipal) -> Result<(), HookError> {
            if existing.status == "locked" {
                return Err(HookError::with_code("WIDGET-LOCKED", LOCKED_REJECTION_MESSAGE));
            }
            Ok(())
        }
    }
}

fn session_principal() -> AuthPrincipal {
    AuthPrincipal {
        subject: "user-1".to_string(),
        email: Some("user-1@example.test".to_string()),
        preferred_username: Some("user-1".to_string()),
        source: PrincipalSource::Session {
            id_token: "id-token".to_string(),
        },
    }
}

fn api_principal() -> AuthPrincipal {
    AuthPrincipal {
        subject: "svc-1".to_string(),
        email: None,
        preferred_username: None,
        source: PrincipalSource::ApiToken { token_id: 42 },
    }
}

#[test]
fn default_before_update_is_identity_and_ignores_existing() {
    let active = recipe::ActiveModel {
        id: Set(3),
        title: Set("beetroot soup".to_string()),
        owner_id: Set(9),
    };
    // `existing` volontairement différente de l'ActiveModel posé sur toutes ses colonnes : le
    // défaut ne doit rien en lire, sinon l'identité serait observable comme une mutation.
    let existing = recipe::Model {
        id: 99,
        title: "old title".to_string(),
        owner_id: 1,
    };

    for principal in [session_principal(), api_principal()] {
        let outcome = recipe::Entity::before_update(active.clone(), &existing, &principal);
        assert!(
            matches!(outcome, Ok(ref unchanged) if *unchanged == active),
            "le défaut de before_update doit rendre l'ActiveModel inchangé, champ par champ"
        );
    }
}

#[test]
fn before_update_override_can_compare_existing_and_reject() {
    let principal = session_principal();
    let existing = gadget::Model {
        id: 1,
        label: "draft".to_string(),
    };

    let no_op = gadget::ActiveModel {
        id: Set(1),
        label: Set("draft".to_string()),
    };
    let rejected = gadget::Entity::before_update(no_op.clone(), &existing, &principal);
    assert!(
        rejected.is_err(),
        "une transition no-op sur label doit être rejetée par l'override"
    );
    if let Err(ref error) = rejected {
        assert!(error.code.is_none(), "le rejet est un HookError sans code");
        assert_eq!(error.message, LABEL_NO_OP_MESSAGE);
    }

    let changed = gadget::ActiveModel {
        id: Set(1),
        label: Set("published".to_string()),
    };
    let accepted = gadget::Entity::before_update(changed.clone(), &existing, &principal);
    assert!(
        matches!(accepted, Ok(ref passed) if *passed == changed),
        "un label qui change passe inchangé"
    );
}

#[test]
fn default_before_delete_is_ok_and_ignores_existing() {
    let existing = ingredient::Model {
        id: 4,
        name: "salt".to_string(),
    };

    for principal in [session_principal(), api_principal()] {
        assert!(
            matches!(ingredient::Entity::before_delete(&existing, &principal), Ok(())),
            "le défaut de before_delete autorise, quel que soit le principal"
        );
    }
}

#[test]
fn before_delete_override_rejects_with_free_form_error() {
    let principal = api_principal();

    let locked = widget::Model {
        id: 1,
        status: "locked".to_string(),
    };
    let rejected = widget::Entity::before_delete(&locked, &principal);
    assert!(
        rejected.is_err(),
        "une ligne « locked » doit être rejetée par l'override"
    );
    if let Err(ref error) = rejected {
        assert_eq!(
            error.code.as_deref(),
            Some("WIDGET-LOCKED"),
            "le code applicatif libre traverse le hook intact"
        );
        assert_eq!(error.message, LOCKED_REJECTION_MESSAGE);
    }

    let open = widget::Model {
        id: 2,
        status: "open".to_string(),
    };
    assert!(
        matches!(widget::Entity::before_delete(&open, &principal), Ok(())),
        "une ligne hors verrouillage passe"
    );
}
