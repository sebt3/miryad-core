//! Configuration du client Matrix (`src/matrix/config.sdd`).

use std::fmt;

use crate::matrix::error::MatrixError;

/// Configuration que l'application fournit pour construire un client Matrix (`src/matrix/config.sdd`).
#[derive(Clone)]
pub struct MatrixConfig {
    /// URL de base du serveur client-serveur, ex. `https://matrix.example.org`.
    pub homeserver_url: String,
    /// Protège le `StoreCipher` du compte. **Secret** : jamais affiché par `Debug`.
    pub store_passphrase: String,
    /// Identifiant stable et unique par réplica (typiquement le nom du pod), détenteur du bail.
    pub holder_name: String,
    /// Clé de compte dans les tables du store. Défaut `"default"`.
    pub account_key: String,
    /// Nom d'appareil affiché côté serveur. Défaut `"miryad"`.
    pub device_display_name: String,
    /// Identifiants `@user:server` dont les invitations sont acceptées. Vide : aucune invitation.
    pub auto_join_from: Vec<String>,
    /// Délai de long-poll de la synchronisation, en millisecondes. Défaut `30_000`.
    pub sync_timeout_ms: u64,
}

impl MatrixConfig {
    /// Construit une configuration avec les défauts de `src/matrix/config.sdd`.
    #[must_use]
    pub fn new(
        homeserver_url: impl Into<String>,
        store_passphrase: impl Into<String>,
        holder_name: impl Into<String>,
    ) -> Self {
        Self {
            homeserver_url: homeserver_url.into(),
            store_passphrase: store_passphrase.into(),
            holder_name: holder_name.into(),
            account_key: "default".into(),
            device_display_name: "miryad".into(),
            auto_join_from: Vec::new(),
            sync_timeout_ms: 30_000,
        }
    }

    /// Vérifie la cohérence de la configuration (`src/matrix/config.sdd`).
    ///
    /// # Errors
    ///
    /// Retourne [`MatrixError::Config`] nommant le champ fautif.
    pub fn validate(&self) -> Result<(), MatrixError> {
        validate_homeserver_url(&self.homeserver_url)?;
        require_non_blank("store_passphrase", &self.store_passphrase)?;
        require_non_blank("holder_name", &self.holder_name)?;
        require_non_blank("account_key", &self.account_key)?;
        for entry in &self.auto_join_from {
            validate_user_id(entry)?;
        }
        if !(SYNC_TIMEOUT_MIN_MS..=SYNC_TIMEOUT_MAX_MS).contains(&self.sync_timeout_ms) {
            return Err(config_error(
                "sync_timeout_ms",
                "must be between 1000 and 120000 milliseconds (inclusive)",
            ));
        }
        Ok(())
    }
}

impl fmt::Debug for MatrixConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MatrixConfig")
            .field("homeserver_url", &self.homeserver_url)
            .field("store_passphrase", &"<redacted>")
            .field("holder_name", &self.holder_name)
            .field("account_key", &self.account_key)
            .field("device_display_name", &self.device_display_name)
            .field("auto_join_from", &self.auto_join_from)
            .field("sync_timeout_ms", &self.sync_timeout_ms)
            .finish()
    }
}

const SYNC_TIMEOUT_MIN_MS: u64 = 1_000;
const SYNC_TIMEOUT_MAX_MS: u64 = 120_000;

/// Construit l'erreur de configuration en nommant le champ fautif. Ne cite jamais la valeur
/// (la passphrase est un secret).
fn config_error(field: &str, reason: &str) -> MatrixError {
    MatrixError::Config(format!("{field}: {reason}"))
}

fn validate_homeserver_url(raw: &str) -> Result<(), MatrixError> {
    let url = reqwest::Url::parse(raw).map_err(|_| config_error("homeserver_url", "must be a valid URL"))?;
    match url.scheme() {
        "http" | "https" => Ok(()),
        _ => Err(config_error("homeserver_url", "scheme must be http or https")),
    }
}

fn require_non_blank(field: &str, value: &str) -> Result<(), MatrixError> {
    if value.trim().is_empty() {
        return Err(config_error(field, "must not be blank"));
    }
    Ok(())
}

fn validate_user_id(entry: &str) -> Result<(), MatrixError> {
    let malformed = || config_error("auto_join_from", "each entry must have the form @local:domain");
    let rest = entry.strip_prefix('@').ok_or_else(malformed)?;
    match rest.split_once(':') {
        Some((local, domain)) if !local.is_empty() && !domain.is_empty() => Ok(()),
        _ => Err(malformed()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid() -> MatrixConfig {
        MatrixConfig::new("https://m.example", "p", "pod-1")
    }

    /// Extrait le message d'une erreur `MatrixError::Config` ; toute autre issue fait échouer le test.
    fn config_error(result: Result<(), MatrixError>) -> String {
        match result {
            Err(MatrixError::Config(message)) => message,
            other => panic!("attendu Err(Config), obtenu : {other:?}"),
        }
    }

    // Scenario: les défauts sont posés par new
    #[test]
    fn new_sets_defaults() {
        let c = MatrixConfig::new("https://m.example", "p", "pod-1");
        assert_eq!(c.homeserver_url, "https://m.example");
        assert_eq!(c.store_passphrase, "p");
        assert_eq!(c.holder_name, "pod-1");
        assert_eq!(c.account_key, "default");
        assert_eq!(c.device_display_name, "miryad");
        assert_eq!(c.auto_join_from, Vec::<String>::new());
        assert_eq!(c.sync_timeout_ms, 30_000);
    }

    #[test]
    fn valid_config_passes_validate() {
        assert!(valid().validate().is_ok());
    }

    // Scenario: validate refuse les valeurs invalides (URL)
    #[test]
    fn validate_refuses_non_http_url() {
        let mut c = valid();
        c.homeserver_url = "ftp://x".into();
        assert!(config_error(c.validate()).contains("homeserver_url"));
    }

    #[test]
    fn validate_refuses_unparsable_url() {
        let mut c = valid();
        c.homeserver_url = "pas une url".into();
        assert!(config_error(c.validate()).contains("homeserver_url"));
    }

    #[test]
    fn validate_refuses_url_without_host() {
        // Pour http/https, le parseur rejette déjà toute URL sans hôte (`empty host`).
        let mut c = valid();
        for raw in ["https:", "http://", "http:"] {
            c.homeserver_url = raw.into();
            assert!(
                config_error(c.validate()).contains("homeserver_url"),
                "entrée : {raw}"
            );
        }
    }

    #[test]
    fn validate_accepts_http_and_https_urls() {
        let mut c = valid();
        c.homeserver_url = "http://m.example:8008".into();
        assert!(c.validate().is_ok());
        c.homeserver_url = "https://m.example".into();
        assert!(c.validate().is_ok());
    }

    // Scenario: validate refuse les valeurs invalides (champs vides après trim)
    #[test]
    fn validate_refuses_blank_holder_name() {
        let mut c = valid();
        c.holder_name = " ".into();
        assert!(config_error(c.validate()).contains("holder_name"));
    }

    #[test]
    fn validate_refuses_blank_store_passphrase() {
        let mut c = valid();
        c.store_passphrase = "   ".into();
        assert!(config_error(c.validate()).contains("store_passphrase"));
    }

    #[test]
    fn validate_refuses_blank_account_key() {
        let mut c = valid();
        c.account_key = " \t ".into();
        assert!(config_error(c.validate()).contains("account_key"));
    }

    // Scenario: validate refuse les valeurs invalides (auto_join_from)
    #[test]
    fn validate_refuses_auto_join_entry_without_at_sign() {
        let mut c = valid();
        c.auto_join_from = vec!["alice".into()];
        assert!(config_error(c.validate()).contains("auto_join_from"));
    }

    #[test]
    fn validate_refuses_auto_join_entry_without_domain() {
        let mut c = valid();
        c.auto_join_from = vec!["@alice".into()];
        assert!(config_error(c.validate()).contains("auto_join_from"));
        c.auto_join_from = vec!["@alice:".into()];
        assert!(config_error(c.validate()).contains("auto_join_from"));
    }

    #[test]
    fn validate_refuses_auto_join_entry_with_empty_local_part() {
        let mut c = valid();
        c.auto_join_from = vec!["@:m.example".into()];
        assert!(config_error(c.validate()).contains("auto_join_from"));
    }

    #[test]
    fn validate_accepts_well_formed_auto_join_entries() {
        let mut c = valid();
        c.auto_join_from = vec!["@alice:m.example".into(), "@bob:other.example".into()];
        assert!(c.validate().is_ok());
    }

    // Scenario: validate refuse les valeurs invalides (sync_timeout_ms, bornes incluses)
    #[test]
    fn validate_refuses_sync_timeout_below_range() {
        let mut c = valid();
        c.sync_timeout_ms = 10;
        assert!(config_error(c.validate()).contains("sync_timeout_ms"));
        c.sync_timeout_ms = 999;
        assert!(config_error(c.validate()).contains("sync_timeout_ms"));
    }

    #[test]
    fn validate_refuses_sync_timeout_above_range() {
        let mut c = valid();
        c.sync_timeout_ms = 120_001;
        assert!(config_error(c.validate()).contains("sync_timeout_ms"));
    }

    #[test]
    fn validate_accepts_sync_timeout_bounds() {
        let mut c = valid();
        c.sync_timeout_ms = 1_000;
        assert!(c.validate().is_ok());
        c.sync_timeout_ms = 120_000;
        assert!(c.validate().is_ok());
    }

    // Scenario: la passphrase n'apparaît jamais dans Debug
    #[test]
    fn debug_redacts_store_passphrase() {
        let c = MatrixConfig::new("https://m.example", "secret", "pod-1");
        let shown = format!("{c:?}");
        assert!(!shown.contains("secret"), "fuite de passphrase : {shown}");
        assert!(shown.contains("<redacted>"), "absence de <redacted> : {shown}");
    }
}
