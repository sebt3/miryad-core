//! Crate d'intégration rattachée à `src/workflow/definition.sdd` : le `Scenario`
//! « `configure_policy` change la politique effective » pose la cellule `OnceLock` `POLICY` de
//! ./definition.rs et ne peut donc s'exercer que dans un processus de test dédié (`Tasks` de la
//! spec : « les tests qui la couvrent l'isolent dans un processus dédié »). Ce binaire est ce
//! processus : un seul `#[test]`, seul appelant de `configure_policy` de ce processus.

#![cfg(feature = "workflow")]

use miryad_core::resource::AccessPolicy;
use miryad_core::resource::MiryadResource;
use miryad_core::workflow::definition::Column;
use miryad_core::workflow::definition::Entity;
use miryad_core::workflow::definition::WorkflowPolicy;
use miryad_core::workflow::definition::configure_policy;

/// Scenario « `configure_policy` change la politique effective » : une pose unique de
/// `Public`/`OwnerOnly` remplace le défaut `AdminOnly`/`AdminOnly` lu par `read_policy` et
/// `write_policy` ; `owner_column` reste `Some(Column::OwnerId)`, indépendant de la politique.
#[test]
fn configure_policy_change_la_politique_effective() {
    configure_policy(WorkflowPolicy {
        read: AccessPolicy::Public,
        write: AccessPolicy::OwnerOnly,
    })
    .expect("première pose du processus : ne peut pas échouer");
    assert_eq!(Entity::read_policy(), AccessPolicy::Public);
    assert_eq!(Entity::write_policy(), AccessPolicy::OwnerOnly);
    assert!(
        matches!(Entity::owner_column(), Some(Column::OwnerId)),
        "`owner_column` est une déclaration, pas une conséquence de la politique posée"
    );
}
