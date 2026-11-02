//! Installation idempotente du provider TLS `rustls` (`src/lymaille/tls.sdd`).
//!
//! `lymaille` avorte si l'hôte n'a pas installé de provider avant son premier appel TLS
//! (DSD `FR-5`, `DEC-4`) ; `ring` est le provider arbitré (2026-11-02). Le montage de l'état
//! de l'application appelle [`install_crypto_provider`] avant tout appel d'inférence ou MCP.

/// Installe `ring` comme provider `rustls` par défaut du processus, si aucun provider n'est
/// déjà posé. Idempotente, appelable depuis plusieurs threads, sans jamais paniquer : un `Err`
/// de `install_default` signifie seulement qu'une autre partie du processus a posé son
/// provider, et n'émet qu'une trace `debug`.
pub fn install_crypto_provider() {
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

    // Le provider par défaut de rustls est un verrou global au processus, posé une seule fois
    // et jamais retirable. Dans un binaire de tests qui partage ce processus, aucun test ne
    // peut garantir un état « aucun provider installé » préalable ni retirer le provider d'un
    // autre. Ce verrou sérialise les deux tests du module ; leurs assertions tiennent dans
    // tout ordre et tout entrelacement.
    static PROVIDER_LOCK: Mutex<()> = Mutex::new(());

    fn provider_lock() -> MutexGuard<'static, ()> {
        PROVIDER_LOCK.lock().unwrap_or_else(PoisonError::into_inner)
    }

    // Scenario: deux appels successifs réussissent
    #[test]
    fn two_successive_calls_succeed() {
        let _guard = provider_lock();
        let before = CryptoProvider::get_default().is_some();
        install_crypto_provider();
        install_crypto_provider();
        let current = CryptoProvider::get_default();
        assert!(
            current.is_some(),
            "deux appels doivent laisser un provider par défaut"
        );
        if !before {
            // Ce test est le premier à poser le provider : les appels doivent l'avoir posé
            // lui-même, complet (toutes les suites de `ring`), et être idempotents.
            assert_eq!(
                current.map(|provider| provider.cipher_suites.len()),
                Some(rustls::crypto::ring::default_provider().cipher_suites.len()),
                "le provider posé doit être celui de `ring`, non tronqué par un second appel"
            );
        }
    }

    // Scenario: un provider déjà installé n'est pas une erreur
    #[test]
    fn already_installed_provider_is_not_an_error() {
        let _guard = provider_lock();
        // Provider de test identifiable : celui de `ring`, amputé de ses suites de
        // chiffrement sauf la première (garde-fou : `ring` en offre plusieurs).
        let mut test_provider = rustls::crypto::ring::default_provider();
        assert!(
            test_provider.cipher_suites.len() > 1,
            "provider de test non identifiable : {} suites",
            test_provider.cipher_suites.len()
        );
        test_provider.cipher_suites.truncate(1);
        // Si un autre test a posé son provider avant, l'installation échoue : le contrat
        // « pas de panic, rien n'est remplacé » doit alors aussi tenir — d'où l'assertion
        // d'invariance inconditionnelle ci-dessous, identique dans les deux branches.
        let installed_by_test = test_provider.install_default().is_ok();

        // Empreinte du provider par défaut au début du Scenario (« Given un provider
        // installé ») : celui du test s'il a gagné le verrou global, sinon celui d'un autre
        // test — l'invariance s'affirme sur l'un comme sur l'autre.
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
