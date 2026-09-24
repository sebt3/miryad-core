//! Crate d'intégration rattachée à `src/workflow/definition.sdd` : le `Scenario` « un second
//! appel à configure_policy panique » exige sa propre politique initiale dans un processus où
//! aucune autre pose n'a eu lieu (`OnceLock` : une seule pose par processus, `Tasks` de la spec :
//! « les tests qui la couvrent l'isolent dans un processus dédié »). Dédoublé de
//! `tests/workflow_policy.rs` parce que les deux `Scenario` posent des premières politiques
//! différentes et partagent sinon le même processus.

#![cfg(feature = "workflow")]

use miryad_core::resource::AccessPolicy;
use miryad_core::resource::MiryadResource;
use miryad_core::workflow::definition::Entity;
use miryad_core::workflow::definition::WorkflowPolicy;
use miryad_core::workflow::definition::configure_policy;

/// Scenario « un second appel à configure_policy panique » : ce test pose lui-même sa première
/// politique (`Public`/`Public`), la vérifie lue par `read_policy`/`write_policy` avant tout
/// incident — puis le second appel doit paniquer, quelle que soit la valeur proposée. Si un jour
/// le second appel réussissait silencieusement, aucune panique n'atteindrait `#[should_panic]` et
/// le test passerait au rouge.
#[test]
#[should_panic(expected = "configure_policy() déjà appelée")]
fn second_appel_configure_policy_panique() {
    configure_policy(WorkflowPolicy {
        read: AccessPolicy::Public,
        write: AccessPolicy::Public,
    });
    assert_eq!(Entity::read_policy(), AccessPolicy::Public);
    assert_eq!(Entity::write_policy(), AccessPolicy::Public);

    configure_policy(WorkflowPolicy {
        read: AccessPolicy::AdminOnly,
        write: AccessPolicy::AdminOnly,
    });

    // Inatteignable tant que le second appel panique : ligne de garde qui échoue nommément si la
    // panique disparaissait sans que le test ne s'en aperçoive autrement.
    assert_eq!(
        Entity::read_policy(),
        AccessPolicy::Public,
        "une pose double ne doit jamais remplacer la première politique"
    );
}
