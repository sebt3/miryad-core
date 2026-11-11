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
    /// Session, brouillon ou serveur introuvable **ou non visible** pour l'appelant (amendement
    /// lot 2, DSD `DEC-24`) ; le texte ne distingue jamais les deux, `{0}` est expurgé.
    #[error("MRD-LYMAILLE-010: not found: {0}")]
    NotFound(String),
    /// Conflit d'état : tour déjà actif, mxid déjà lié (amendement lot 2, DSD `DEC-24`) ;
    /// `{0}` est expurgé.
    #[error("MRD-LYMAILLE-011: conflict: {0}")]
    Conflict(String),
    /// Panne de sérialisation sur le canal SSE (`../http/sse.rs`, tâche du 2026-11-05) ; `{0}`
    /// est un texte expurgé, sans contenu de session. Code propre, distinct de `-009` (`Store`).
    #[error("MRD-LYMAILLE-012: serialization error: {0}")]
    Serialization(String),
    /// Exécuteur de script : délai dépassé (amendement du 2026-11-11, DSD `scripts-hote#FR-7`) ;
    /// non retryable, sans contenu de script ni de ligne.
    #[error("MRD-LYMAILLE-013: script exceeded its time limit")]
    ScriptTimeout,
    /// Exécuteur de script : limite d'opérations dépassée (amendement du 2026-11-11, DSD
    /// `scripts-hote#FR-7`) ; non retryable, sans contenu de script ni de ligne.
    #[error("MRD-LYMAILLE-014: script exceeded its operation limit")]
    ScriptOperationLimit,
    /// Exécuteur de script : résultat dépassant la taille autorisée (amendement du 2026-11-11,
    /// DSD `scripts-hote#FR-7`) ; non retryable, sans contenu de script ni de ligne.
    #[error("MRD-LYMAILLE-015: script result exceeds its size limit")]
    ScriptResultTooLarge,
    /// Exécuteur de script : le script a échoué (amendement du 2026-11-11, DSD
    /// `scripts-hote#FR-7`) ; `{0}` est le message d'erreur Rhai, sans source. Non retryable.
    #[error("MRD-LYMAILLE-016: script failed: {0}")]
    ScriptFailed(String),
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

    // Scenario: les codes sont stables — tâche `Serialization` (2026-11-05) : `-001` à `-012` ;
    // amendement du 2026-11-11 (DSD `scripts-hote#FR-7`) : désormais `-001` à `-016`.
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
            (LymailleError::NotFound("x".into()), "MRD-LYMAILLE-010"),
            (LymailleError::Conflict("x".into()), "MRD-LYMAILLE-011"),
            (LymailleError::Serialization("x".into()), "MRD-LYMAILLE-012"),
            // Amendement du 2026-11-11 (DSD `scripts-hote#FR-7`) : exécuteur de script.
            (LymailleError::ScriptTimeout, "MRD-LYMAILLE-013"),
            (LymailleError::ScriptOperationLimit, "MRD-LYMAILLE-014"),
            (LymailleError::ScriptResultTooLarge, "MRD-LYMAILLE-015"),
            (LymailleError::ScriptFailed("x".into()), "MRD-LYMAILLE-016"),
        ];
        for (err, code) in cases {
            let msg = err.to_string();
            assert!(msg.starts_with(code), "attendu préfixe {code}, obtenu : {msg}");
        }
    }

    // Amendement lot 2 (DSD `DEC-24`) : message exact des variantes `NotFound` et `Conflict`,
    // sans distinction introuvable/non visible dans le texte, et `Debug` qui ne porte que ce
    // qu'a fourni l'appelant.
    #[test]
    fn amendement_lot_2_messages_et_debug() {
        let not_found = LymailleError::NotFound("session s-1".into());
        assert_eq!(not_found.to_string(), "MRD-LYMAILLE-010: not found: session s-1");
        let debug = format!("{not_found:?}");
        assert_eq!(
            debug, r#"NotFound("session s-1")"#,
            "le Debug ne porte que ce qu'a fourni l'appelant"
        );

        let conflict = LymailleError::Conflict("tour déjà actif".into());
        assert_eq!(
            conflict.to_string(),
            "MRD-LYMAILLE-011: conflict: tour déjà actif"
        );
        let debug = format!("{conflict:?}");
        assert_eq!(
            debug, r#"Conflict("tour déjà actif")"#,
            "le Debug ne porte que ce qu'a fourni l'appelant"
        );
    }

    // Tâche `Serialization` (arbitrage du 2026-11-05) : message et `Debug` même forme que les
    // autres variantes (code, texte contractuel, texte expurgé fourni par l'appelant, rien de
    // plus), et panne de sérialisation non rejouable.
    #[test]
    fn serialization_message_debug_et_retryabilite() {
        let err = LymailleError::Serialization("event payload rejected by encoder".into());
        assert_eq!(
            err.to_string(),
            "MRD-LYMAILLE-012: serialization error: event payload rejected by encoder"
        );
        let debug = format!("{err:?}");
        assert_eq!(
            debug, r#"Serialization("event payload rejected by encoder")"#,
            "le Debug ne porte que ce qu'a fourni l'appelant"
        );
        assert!(!err.retryable(), "une panne de sérialisation ne se rejoue pas");
    }

    // Amendement du 2026-11-11 (DSD `scripts-hote#FR-7`) : message exact et `Debug` même forme
    // que les autres variantes (code, texte contractuel, message Rhai fourni par l'appelant sans
    // source de script, rien de plus), et les quatre exécutions de script non rejouables.
    #[test]
    fn amendement_scripts_hote_messages_debug_et_retryabilite() {
        let timeout = LymailleError::ScriptTimeout;
        assert_eq!(
            timeout.to_string(),
            "MRD-LYMAILLE-013: script exceeded its time limit"
        );
        assert_eq!(format!("{timeout:?}"), "ScriptTimeout");
        assert!(!timeout.retryable(), "un dépassement de délai ne se rejoue pas");

        let operations = LymailleError::ScriptOperationLimit;
        assert_eq!(
            operations.to_string(),
            "MRD-LYMAILLE-014: script exceeded its operation limit"
        );
        assert_eq!(format!("{operations:?}"), "ScriptOperationLimit");
        assert!(
            !operations.retryable(),
            "un dépassement de limite d'opérations ne se rejoue pas"
        );

        let too_large = LymailleError::ScriptResultTooLarge;
        assert_eq!(
            too_large.to_string(),
            "MRD-LYMAILLE-015: script result exceeds its size limit"
        );
        assert_eq!(format!("{too_large:?}"), "ScriptResultTooLarge");
        assert!(!too_large.retryable(), "un résultat trop gros ne se rejoue pas");

        let failed = LymailleError::ScriptFailed("0:1 - 1:2: syntax error".into());
        assert_eq!(
            failed.to_string(),
            "MRD-LYMAILLE-016: script failed: 0:1 - 1:2: syntax error"
        );
        assert_eq!(
            format!("{failed:?}"),
            r#"ScriptFailed("0:1 - 1:2: syntax error")"#,
            "le Debug ne porte que le message Rhai fourni par l'appelant, sans source"
        );
        assert!(!failed.retryable(), "un échec du script ne se rejoue pas");
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
            // Amendement lot 2 (DSD `DEC-24`) : NotFound et Conflict ne se rejouent pas.
            (LymailleError::NotFound("x".into()), false),
            (LymailleError::Conflict("x".into()), false),
            // Tâche `Serialization` (2026-11-05) : une panne de sérialisation ne se rejoue pas.
            (LymailleError::Serialization("x".into()), false),
            // Amendement du 2026-11-11 (DSD `scripts-hote#FR-7`) : les quatre variantes
            // d'exécuteur de script ne se rejouent pas.
            (LymailleError::ScriptTimeout, false),
            (LymailleError::ScriptOperationLimit, false),
            (LymailleError::ScriptResultTooLarge, false),
            (LymailleError::ScriptFailed("x".into()), false),
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
        assert_eq!(
            debug, r#"SecretUnresolved("LLM_API_KEY")"#,
            "le Debug ne porte que la référence fournie par l'appelant, rien d'autre"
        );
    }
}
