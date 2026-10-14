//! Erreur propre du store de chiffrement (`src/matrix/store/error.sdd`).

use matrix_sdk_crypto::store::CryptoStoreError;
use sea_orm::DbErr;

/// Erreur du store de chiffrement `SeaORM`.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum StoreError {
    /// Échec de la base de données (traverse nu, non recodé).
    #[error("MRD-MXSTORE-001: database error: {0}")]
    Db(#[from] DbErr),
    /// Échec de chiffrement du store.
    #[error("MRD-MXSTORE-002: store cipher error: {0}")]
    Cipher(String),
    /// Échec de (dé)sérialisation.
    #[error("MRD-MXSTORE-003: serialization error: {0}")]
    Serialization(String),
    /// Donnée absente ou indéchiffrable.
    #[error("MRD-MXSTORE-004: corrupt store data: {0}")]
    Corrupt(String),
}

impl From<StoreError> for CryptoStoreError {
    fn from(err: StoreError) -> Self {
        CryptoStoreError::backend(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Scenario: les codes sont stables
    #[test]
    fn codes_are_stable() {
        let cases = [
            (StoreError::Db(DbErr::Custom("boom".into())), "MRD-MXSTORE-001"),
            (StoreError::Cipher("x".into()), "MRD-MXSTORE-002"),
            (StoreError::Serialization("x".into()), "MRD-MXSTORE-003"),
            (StoreError::Corrupt("x".into()), "MRD-MXSTORE-004"),
        ];
        for (err, code) in cases {
            let msg = err.to_string();
            assert!(msg.starts_with(code), "attendu préfixe {code}, obtenu : {msg}");
        }
    }

    // Scenario: la conversion vers CryptoStoreError conserve le message
    #[test]
    fn cipher_converts_to_crypto_store_error_keeping_message() {
        let err: CryptoStoreError = StoreError::Cipher("x".into()).into();
        assert!(
            matches!(err, CryptoStoreError::Backend(_)),
            "variante inattendue : {err:?}"
        );
        assert!(err.to_string().contains("MRD-MXSTORE-002"), "message : {err}");
    }
}
