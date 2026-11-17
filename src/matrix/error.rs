//! Erreur du client Matrix (`src/matrix/error.sdd`).

use crate::matrix::store::StoreError;

/// Erreur du client Matrix. Chaque variante porte un code `MRD-MATRIX-NNN` ; aucune ne contient
/// de jeton, de passphrase ni de contenu de message.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum MatrixError {
    /// Configuration invalide.
    #[error("MRD-MATRIX-001: invalid configuration: {0}")]
    Config(String),
    /// Aucune session Matrix pour ce compte.
    #[error("MRD-MATRIX-002: no matrix session for this account (log in or import a session first)")]
    NotLoggedIn,
    /// Erreur du store de chiffrement.
    #[error("MRD-MATRIX-003: store error: {0}")]
    Store(#[from] StoreError),
    /// Échec de connexion.
    #[error("MRD-MATRIX-004: login failed: {0}")]
    Login(String),
    /// Identifiant Matrix mal formé.
    #[error("MRD-MATRIX-005: invalid matrix identifier: {0}")]
    InvalidId(String),
    /// Salon inconnu du compte, non rejoignable, ou refus de permission.
    #[error("MRD-MATRIX-006: room unavailable: {0}")]
    RoomUnavailable(String),
    /// Erreur réseau ou serveur ; `retryable` indique si un nouvel essai a un sens.
    #[error("MRD-MATRIX-007: network error: {message}")]
    Network {
        /// Description sans contenu de message ni jeton.
        message: String,
        /// Vrai si un nouvel essai est pertinent (transport coupé, délai, 5xx, 429).
        retryable: bool,
    },
    /// Toute autre erreur de `matrix-sdk`, non retryable.
    #[error("MRD-MATRIX-008: matrix sdk error: {0}")]
    Sdk(String),
    /// Événement visé par une réponse ou une édition inconnu du serveur, ou illisible pour le
    /// compte ; porte l'identifiant de l'événement, jamais son contenu. Non retryable.
    #[error("MRD-MATRIX-009: event not found: {0}")]
    EventNotFound(String),
    /// Édition d'un événement envoyé par un autre compte (interdit par la spécification Matrix) ;
    /// porte l'identifiant de l'événement. Non retryable.
    #[error("MRD-MATRIX-010: not the author of event: {0}")]
    NotAuthor(String),
}

impl MatrixError {
    /// Vrai si un nouvel essai de l'opération a un sens (décision consommée par `matrix_send`).
    #[must_use]
    pub fn retryable(&self) -> bool {
        match self {
            Self::Network { retryable, .. } => *retryable,
            Self::Store(StoreError::Db(_)) => true,
            _ => false,
        }
    }
}

/// Traduit une erreur `matrix-sdk` en [`MatrixError`] (`src/matrix/error.sdd`). La décision se
/// fait par le statut HTTP et le type de transport, jamais par sous-chaîne de message.
pub(crate) fn from_sdk(e: &matrix_sdk::Error) -> MatrixError {
    match e {
        matrix_sdk::Error::Http(http) => from_http(http),
        _ => MatrixError::Sdk(e.to_string()),
    }
}

fn from_http(e: &matrix_sdk::HttpError) -> MatrixError {
    match e {
        matrix_sdk::HttpError::Cached(inner) => from_http(inner),
        matrix_sdk::HttpError::Reqwest(transport)
            if transport.is_timeout() || transport.is_connect() || transport.is_request() =>
        {
            MatrixError::Network {
                message: "connection failed or timed out".into(),
                retryable: true,
            }
        }
        _ => match e.as_client_api_error() {
            Some(api) => from_status(api.status_code.as_u16()),
            None => MatrixError::Sdk(e.to_string()),
        },
    }
}

fn from_status(status: u16) -> MatrixError {
    match status {
        429 => MatrixError::Network {
            message: "rate limited (HTTP 429)".into(),
            retryable: true,
        },
        500..=599 => MatrixError::Network {
            message: format!("server error (HTTP {status})"),
            retryable: true,
        },
        403 => MatrixError::RoomUnavailable("forbidden (HTTP 403)".into()),
        404 => MatrixError::RoomUnavailable("not found (HTTP 404)".into()),
        _ => MatrixError::Sdk(format!("HTTP {status}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use matrix_sdk::HttpError;
    use ruma::api::EndpointError;
    use ruma::api::error::FromHttpResponseError;
    use sea_orm::DbErr;

    /// Erreur `matrix-sdk` construite par le chemin public réel : réponse HTTP → `RumaApiError`
    /// (`from_http_response` de ruma, qui garde le statut même pour un corps non-JSON).
    fn sdk_http(status: u16, body: &str) -> matrix_sdk::Error {
        let response = ruma::exports::http::Response::builder()
            .status(status)
            .body(body.as_bytes().to_vec())
            .expect("réponse de test valide");
        let api_error = matrix_sdk::RumaApiError::from_http_response(response);
        matrix_sdk::Error::Http(Box::new(HttpError::Api(Box::new(FromHttpResponseError::Server(
            api_error,
        )))))
    }

    // Scenario: les codes sont stables
    #[test]
    fn codes_are_stable() {
        let cases = [
            (MatrixError::Config("x".into()), "MRD-MATRIX-001"),
            (MatrixError::NotLoggedIn, "MRD-MATRIX-002"),
            (
                MatrixError::Store(StoreError::Cipher("x".into())),
                "MRD-MATRIX-003",
            ),
            (MatrixError::Login("x".into()), "MRD-MATRIX-004"),
            (MatrixError::InvalidId("x".into()), "MRD-MATRIX-005"),
            (MatrixError::RoomUnavailable("x".into()), "MRD-MATRIX-006"),
            (
                MatrixError::Network {
                    message: "x".into(),
                    retryable: true,
                },
                "MRD-MATRIX-007",
            ),
            (MatrixError::Sdk("x".into()), "MRD-MATRIX-008"),
            (MatrixError::EventNotFound("$ev".into()), "MRD-MATRIX-009"),
            (MatrixError::NotAuthor("$ev".into()), "MRD-MATRIX-010"),
        ];
        for (err, code) in cases {
            let msg = err.to_string();
            assert!(msg.starts_with(code), "attendu préfixe {code}, obtenu : {msg}");
        }
    }

    // Scenario: les codes sont stables — Display d'EventNotFound/NotAuthor porte l'identifiant de
    // l'événement, jamais son contenu (amendement 2026-11-16, issue #87).
    #[test]
    fn event_variants_display_carries_event_id_not_content() {
        let msg = MatrixError::EventNotFound("$AbCdef:example.org".into()).to_string();
        assert_eq!(
            msg, "MRD-MATRIX-009: event not found: $AbCdef:example.org",
            "Display d'EventNotFound"
        );
        let msg = MatrixError::NotAuthor("$AbCdef:example.org".into()).to_string();
        assert_eq!(
            msg, "MRD-MATRIX-010: not the author of event: $AbCdef:example.org",
            "Display de NotAuthor"
        );
    }

    // Scenario: la retryabilité suit le type d'erreur
    #[test]
    fn retryable_follows_error_kind() {
        let cases = [
            (
                MatrixError::Network {
                    message: "x".into(),
                    retryable: true,
                },
                true,
            ),
            (
                MatrixError::Network {
                    message: "x".into(),
                    retryable: false,
                },
                false,
            ),
            (MatrixError::RoomUnavailable("x".into()), false),
            (MatrixError::Login("x".into()), false),
            (
                MatrixError::Store(StoreError::Db(DbErr::Custom("boom".into()))),
                true,
            ),
            (MatrixError::Store(StoreError::Cipher("x".into())), false),
            (MatrixError::EventNotFound("$ev".into()), false),
            (MatrixError::NotAuthor("$ev".into()), false),
        ];
        for (err, expected) in cases {
            assert_eq!(err.retryable(), expected, "variante : {err}");
        }
    }

    // Scenario: 429 et 5xx sont retryables, 403 et 404 ne le sont pas
    #[test]
    fn from_sdk_429_is_retryable_network() {
        let err = from_sdk(&sdk_http(
            429,
            r#"{"errcode":"M_LIMIT_EXCEEDED","error":"Too many requests","retry_after_ms":1000}"#,
        ));
        assert!(
            matches!(err, MatrixError::Network { retryable: true, .. }),
            "obtenu : {err:?}"
        );
    }

    #[test]
    fn from_sdk_502_is_retryable_network() {
        // Corps non-JSON, comme le renvoie un reverse proxy : le statut doit tout de même passer.
        let err = from_sdk(&sdk_http(502, "<html>Bad Gateway</html>"));
        assert!(
            matches!(err, MatrixError::Network { retryable: true, .. }),
            "obtenu : {err:?}"
        );
    }

    #[test]
    fn from_sdk_403_is_room_unavailable() {
        let err = from_sdk(&sdk_http(403, r#"{"errcode":"M_FORBIDDEN","error":"Forbidden"}"#));
        assert!(matches!(err, MatrixError::RoomUnavailable(_)), "obtenu : {err:?}");
    }

    #[test]
    fn from_sdk_404_is_room_unavailable() {
        let err = from_sdk(&sdk_http(404, r#"{"errcode":"M_NOT_FOUND","error":"Not found"}"#));
        assert!(matches!(err, MatrixError::RoomUnavailable(_)), "obtenu : {err:?}");
    }
}
