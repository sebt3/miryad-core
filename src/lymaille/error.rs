//! Erreur de l'hôte `lymaille` (`src/lymaille/error.sdd`).

/// Erreur interne du module `lymaille`. Chaque variante porte un code `MRD-LYMAILLE-NNN` et
/// aucune ne contient de secret, de contenu de session ni de prompt : les causes sont
/// transportées en `String` déjà expurgées, jamais un `DbErr` ni un type de `lymaille`.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum LymailleError {
    /// Le `subject` de la session OIDC n'a aucun compte local : refus explicite, jamais un
    /// principal vide (DSD `DEC-3`).
    #[error("MRD-LYMAILLE-001: no local account for this session subject")]
    UnknownSubject,
    /// Droit manquant à l'appel : module réservé à l'admin, écriture au catalogue.
    #[error("MRD-LYMAILLE-002: forbidden: {0}")]
    Forbidden(String),
    /// Identifiant utilisant le préfixe réservé du catalogue (DSD `DEC-9`) ; `{0}` est
    /// l'identifiant, jamais un secret.
    #[error("MRD-LYMAILLE-003: identifier uses the reserved catalog prefix: {0}")]
    ReservedPrefix(String),
    /// Définition invalide.
    #[error("MRD-LYMAILLE-004: invalid definition: {0}")]
    InvalidDefinition(String),
    /// Référence de secret non résolue ; `{0}` est la référence (nom de variable par défaut),
    /// jamais la valeur.
    #[error("MRD-LYMAILLE-005: secret reference could not be resolved: {0}")]
    SecretUnresolved(String),
    /// Échec de construction du client d'inférence.
    #[error("MRD-LYMAILLE-006: inference error: {0}")]
    Inference(String),
    /// Échec du run batch ; `message` est expurgé, `retryable` porte la décision de nouvel essai.
    #[error("MRD-LYMAILLE-007: agent run failed: {message}")]
    Run {
        /// Description expurgée, sans contenu de session ni prompt.
        message: String,
        /// Vrai si un nouvel essai du run a un sens.
        retryable: bool,
    },
    /// Configuration invalide.
    #[error("MRD-LYMAILLE-008: invalid configuration: {0}")]
    Config(String),
    /// Panne d'une opération de stockage hors des traits de `lymaille` ; texte expurgé.
    #[error("MRD-LYMAILLE-009: store error: {0}")]
    Store(String),
}

impl LymailleError {
    /// Vrai si un nouvel essai de l'opération a un sens (décision consommée par le step
    /// `subagent`, `src/workflow/subagent_step.sdd`).
    #[must_use]
    pub fn retryable(&self) -> bool {
        match self {
            Self::Run { retryable, .. } => *retryable,
            Self::Store(_) => true,
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Scenario: les codes sont stables
    #[test]
    fn codes_are_stable() {
        let cases = [
            (LymailleError::UnknownSubject, "MRD-LYMAILLE-001"),
            (LymailleError::Forbidden("x".into()), "MRD-LYMAILLE-002"),
            (LymailleError::ReservedPrefix("x".into()), "MRD-LYMAILLE-003"),
            (LymailleError::InvalidDefinition("x".into()), "MRD-LYMAILLE-004"),
            (LymailleError::SecretUnresolved("x".into()), "MRD-LYMAILLE-005"),
            (LymailleError::Inference("x".into()), "MRD-LYMAILLE-006"),
            (
                LymailleError::Run {
                    message: "x".into(),
                    retryable: false,
                },
                "MRD-LYMAILLE-007",
            ),
            (LymailleError::Config("x".into()), "MRD-LYMAILLE-008"),
            (LymailleError::Store("x".into()), "MRD-LYMAILLE-009"),
        ];
        for (err, code) in cases {
            let msg = err.to_string();
            assert!(msg.starts_with(code), "attendu préfixe {code}, obtenu : {msg}");
        }
    }

    // Scenario: la retryabilité suit la variante
    #[test]
    fn retryable_follows_variant() {
        let cases = [
            (
                LymailleError::Run {
                    message: "x".into(),
                    retryable: true,
                },
                true,
            ),
            (
                LymailleError::Run {
                    message: "x".into(),
                    retryable: false,
                },
                false,
            ),
            (LymailleError::Store("x".into()), true),
            (LymailleError::Inference("x".into()), false),
            (LymailleError::Forbidden("x".into()), false),
            (LymailleError::SecretUnresolved("x".into()), false),
        ];
        for (err, expected) in cases {
            assert_eq!(err.retryable(), expected, "variante : {err}");
        }
    }

    // Scenario: aucune valeur secrète dans l'erreur
    #[test]
    fn no_secret_value_in_error() {
        let err = LymailleError::SecretUnresolved("LLM_API_KEY".into());
        // Le message est exactement le code, le texte contractuel et la référence fournie par
        // l'appelant — rien d'autre.
        assert_eq!(
            err.to_string(),
            "MRD-LYMAILLE-005: secret reference could not be resolved: LLM_API_KEY"
        );
        let debug = format!("{err:?}");
        assert!(
            debug.contains("LLM_API_KEY"),
            "la référence doit rester visible dans le Debug : {debug}"
        );
    }
}
