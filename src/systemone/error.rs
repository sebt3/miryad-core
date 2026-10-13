//! Taxonomie d'erreur du client System One : configuration, transport, statut HTTP rejeté,
//! réponse invalide, sous codes `MRD-SYSTEMONE-NNN`. Compile seulement sous la feature `systemone`.

use thiserror::Error;

/// Erreur du client `systemone`. Seul `client.rs` construit ces variantes.
///
/// `reqwest::Error` embarqué interdit `Clone` et `PartialEq` : le type ne dérive que `Debug`
/// et `Error`. Pas de `From<reqwest::Error>` : le client choisit explicitement la variante.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SystemOneError {
    /// `MRD-SYSTEMONE-001` — `Config` : `base_url`, `timeout_secs` ou schéma refusés à la
    /// construction du client.
    #[error("MRD-SYSTEMONE-001: invalid configuration: {0}")]
    Config(String),
    /// `MRD-SYSTEMONE-002` — `Transport` : échec réseau ou délai dépassé pendant `ask`.
    #[error("MRD-SYSTEMONE-002: transport error: {0}")]
    Transport(reqwest::Error),
    /// `MRD-SYSTEMONE-003` — `Status` : réponse hors `2xx`. `body` est tronqué à 512 caractères
    /// par le client à la construction, jamais par cette erreur.
    #[error("MRD-SYSTEMONE-003: system one rejected request (status {status}): {body}")]
    Status {
        /// Code numérique observé sur la réponse.
        status: u16,
        /// Début du corps de réponse, tronqué à 512 caractères.
        body: String,
    },
    /// `MRD-SYSTEMONE-004` — `InvalidResponse` : réponse `2xx` non désérialisable en
    /// `SystemOneResponse`, ou corps illisible.
    #[error("MRD-SYSTEMONE-004: invalid response: {0}")]
    InvalidResponse(String),
}

impl SystemOneError {
    /// Indique si rejouer la requête peut aboutir. `Config` et `InvalidResponse` sont
    /// déterministes ; `Transport` n'est retryable que sur délai ou connexion ; `Status` l'est
    /// pour `429`, `529` et tout `5xx`.
    #[must_use]
    pub fn retryable(&self) -> bool {
        match self {
            Self::Config(_) | Self::InvalidResponse(_) => false,
            Self::Transport(err) => err.is_timeout() || err.is_connect(),
            Self::Status { status, .. } => *status == 429 || *status == 529 || *status >= 500,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::SystemOneError;

    /// Scenario « les codes et messages sont stables » : une variante de chaque sorte, chaque
    /// `to_string()` commence par son code `MRD-SYSTEMONE-001` à `-004`.
    #[test]
    fn codes_et_messages_sont_stables() {
        let Err(builder_err) = reqwest::Client::new().get("http://[::1").build() else {
            panic!("une URL invalide devait échouer à la construction de la requête");
        };
        let cases = [
            (
                SystemOneError::Config("x".to_string()),
                "MRD-SYSTEMONE-001: invalid configuration: x",
            ),
            (
                SystemOneError::Transport(builder_err),
                "MRD-SYSTEMONE-002: transport error: ",
            ),
            (
                SystemOneError::Status {
                    status: 422,
                    body: "boom".to_string(),
                },
                "MRD-SYSTEMONE-003: system one rejected request (status 422): boom",
            ),
            (
                SystemOneError::InvalidResponse("y".to_string()),
                "MRD-SYSTEMONE-004: invalid response: y",
            ),
        ];
        for (err, prefix) in cases {
            let rendered = err.to_string();
            assert!(
                rendered.starts_with(prefix),
                "rendu inattendu, préfixe attendu `{prefix}` : {rendered}"
            );
        }
    }

    /// Scenario « la retryabilité suit le statut » : `429`, `529`, `500`, `503` retryables ;
    /// `401`, `422`, `404` non retryables.
    #[test]
    fn retryabilite_suit_le_statut() {
        let status = |status: u16| SystemOneError::Status {
            status,
            body: String::new(),
        };
        for code in [429, 529, 500, 503] {
            assert!(status(code).retryable(), "{code} doit être retryable");
        }
        for code in [401, 422, 404] {
            assert!(!status(code).retryable(), "{code} ne doit pas être retryable");
        }
    }

    /// Scenario « config et réponse invalides ne se rejouent pas » : `Config` et
    /// `InvalidResponse` ne sont jamais retryables.
    #[test]
    fn config_et_reponse_invalides_ne_se_rejouent_pas() {
        assert!(!SystemOneError::Config("x".to_string()).retryable());
        assert!(!SystemOneError::InvalidResponse("y".to_string()).retryable());
    }
}
