//! Crate d'intégration rattachée à `src/workflow/definition.sdd` : le `Scenario` « un second
//! appel à `configure_policy` rend une erreur » exige sa propre politique initiale dans un processus
//! où aucune autre pose n'a eu lieu (`OnceLock` : une seule pose par processus, `Tasks` de la spec :
//! « les tests qui la couvrent l'isolent dans un processus dédié »). Dédoublé de
//! `tests/workflow_policy.rs` parce que les deux `Scenario` posent des premières politiques
//! différentes et partagent sinon le même processus.

#![cfg(feature = "workflow")]

use miryad_core::resource::AccessPolicy;
use miryad_core::resource::MiryadResource;
use miryad_core::workflow::WorkflowError;
use miryad_core::workflow::definition::Entity;
use miryad_core::workflow::definition::WorkflowPolicy;
use miryad_core::workflow::definition::configure_policy;

/// Scenario « un second appel à `configure_policy` rend une erreur » : ce test pose lui-même sa
/// première politique (`Public`/`Public`), la vérifie lue par `read_policy`/`write_policy` — puis
/// le second appel doit rendre `PolicyAlreadySet`, quelle que soit la valeur proposée, et la
/// première politique reste la politique effective.
#[test]
fn second_appel_configure_policy_rend_une_erreur() {
    configure_policy(WorkflowPolicy {
        read: AccessPolicy::Public,
        write: AccessPolicy::Public,
    })
    .expect("la première pose doit réussir");
    assert_eq!(Entity::read_policy(), AccessPolicy::Public);
    assert_eq!(Entity::write_policy(), AccessPolicy::Public);

    let second = configure_policy(WorkflowPolicy {
        read: AccessPolicy::AdminOnly,
        write: AccessPolicy::AdminOnly,
    });
    assert!(
        matches!(second, Err(WorkflowError::PolicyAlreadySet)),
        "un second appel devait rendre PolicyAlreadySet : {second:?}"
    );
    assert_eq!(
        second.map_err(|e| e.to_string()),
        Err("MRD-WORKFLOW-005: workflow policy already configured".to_string())
    );
    assert_eq!(
        Entity::read_policy(),
        AccessPolicy::Public,
        "une pose double ne doit jamais remplacer la première politique"
    );
    assert_eq!(Entity::write_policy(), AccessPolicy::Public);
}
