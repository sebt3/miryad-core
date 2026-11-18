//! Installation idempotente du provider TLS `rustls` pour le client Matrix
//! (`src/matrix/tls.sdd`).
//!
//! Sous `matrix-sdk` 0.19 avec `default-features = false`, aucun provider `rustls` n'est posé et
//! `reqwest` 0.13 refuse de construire un client TLS tant que rien n'est installé (panique
//! « No rustls crypto provider is configured »). `ring` est le provider arbitré (politique du
//! projet, DSD `lymaille` `DEC-4`, repris côté `matrix` par DSD `matrix` `DEC-4`) :
//! [`install_crypto_provider`] le pose une fois pour toutes, et `build_client` de ./client.rs
//! l'appelle avant de construire le client.

/// Installe `ring` comme provider `rustls` par défaut du processus, si aucun provider n'est déjà
/// posé. Idempotente, appelable depuis plusieurs threads, sans jamais paniquer : un `Err` de
/// `install_default` signifie seulement qu'une autre partie du processus a posé son provider — y
/// compris celui de `lymaille`, les deux features étant indépendantes — et n'émet qu'une trace
/// `debug`.
pub(crate) fn install_crypto_provider() {
    if rustls::crypto::ring::default_provider()
        .install_default()
        .is_err()
    {
        tracing::debug!("provider TLS rustls déjà installé : `ring` n'est pas posé (idempotent)");
    }
}

#[cfg(test)]
mod tests {
    use super::install_crypto_provider;
    use rustls::crypto::CryptoProvider;
    use std::sync::{Mutex, MutexGuard, PoisonError};

    // Le provider par défaut de rustls est un verrou global au processus, posé une seule fois et
    // jamais retirable. Dans un binaire de tests qui partage ce processus, aucun test ne peut
    // garantir un état « aucun provider installé » préalable ni retirer le provider d'un autre. Ce
    // verrou sérialise les tests du module ; leurs assertions tiennent dans tout ordre et tout
    // entrelacement.
    //
    // Limite (mesurée, même limitation que ../lymaille/tls.rs) : `lymaille::tls` a son propre verrou,
    // car les deux features sont indépendantes. Sous une combinaison qui les compile toutes deux, les
    // modules s'exécutent donc en parallèle : seule l'invariance du provider en place est affirmée
    // ici, jamais « c'est nous qui l'avons posé ».
    static PROVIDER_LOCK: Mutex<()> = Mutex::new(());

    fn provider_lock() -> MutexGuard<'static, ()> {
        PROVIDER_LOCK.lock().unwrap_or_else(PoisonError::into_inner)
    }

    // Scenario: deux appels successifs réussissent
    #[test]
    fn two_successive_calls_succeed() {
        let _guard = provider_lock();
        install_crypto_provider();
        install_crypto_provider();
        let current = CryptoProvider::get_default();
        assert!(
            current.is_some(),
            "deux appels doivent laisser un provider par défaut"
        );
        let suites = current.map_or(0, |provider| provider.cipher_suites.len());
        assert!(
            suites > 0,
            "le provider par défaut doit offrir au moins une suite de chiffrement"
        );
    }

    // Scenario: un provider déjà installé n'est pas une erreur
    #[test]
    fn already_installed_provider_is_not_an_error() {
        let _guard = provider_lock();
        // Provider de test identifiable : celui de `ring`, amputé de ses suites de chiffrement
        // sauf la première (garde-fou : `ring` en offre plusieurs).
        let mut test_provider = rustls::crypto::ring::default_provider();
        assert!(
            test_provider.cipher_suites.len() > 1,
            "provider de test non identifiable : {} suites",
            test_provider.cipher_suites.len()
        );
        test_provider.cipher_suites.truncate(1);
        // Si un autre test a posé son provider avant, l'installation échoue : le contrat « pas de
        // panic, rien n'est remplacé » doit alors aussi tenir — d'où l'assertion d'invariance
        // inconditionnelle ci-dessous, identique dans les deux branches.
        let installed_by_test = test_provider.install_default().is_ok();

        // Empreinte du provider par défaut au début du Scenario (« Given un provider installé ») :
        // celui du test s'il a gagné le verrou global, sinon celui d'un autre test — l'invariance
        // s'affirme sur l'un comme sur l'autre.
        let initial = CryptoProvider::get_default().expect("étape Given : un provider doit être posé");
        let initial_ptr = std::sync::Arc::as_ptr(initial);
        let initial_suites = initial.cipher_suites.len();

        install_crypto_provider();

        let current = CryptoProvider::get_default().expect("aucun panic et un provider par défaut subsiste");
        assert!(
            std::ptr::eq(initial_ptr, std::sync::Arc::as_ptr(current)),
            "le provider par défaut ne doit jamais être remplacé par install_crypto_provider"
        );
        assert_eq!(
            current.cipher_suites.len(),
            initial_suites,
            "l'empreinte du provider par défaut ne doit pas bouger"
        );
        if installed_by_test {
            assert_eq!(
                initial_suites, 1,
                "le provider posé par le test doit rester le provider par défaut"
            );
        }
    }

    // Must (`tls.sdd`) : appelable depuis plusieurs threads, sans panic.
    #[test]
    fn callable_from_multiple_threads() {
        let handles: Vec<_> = (0..4)
            .map(|_| std::thread::spawn(install_crypto_provider))
            .collect();
        for handle in handles {
            assert!(
                handle.join().is_ok(),
                "un fil a paniqué en installant le provider"
            );
        }
        assert!(
            CryptoProvider::get_default().is_some(),
            "au moins un des fils doit avoir posé le provider"
        );
    }
}
