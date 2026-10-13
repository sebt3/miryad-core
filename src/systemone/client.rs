//! Client HTTP vers `POST /v1/systemone` (API System One, format TypeSafe). Contrat :
//! `src/systemone/client.sdd`. Le client ne type ni les questions ni les réponses : `state`,
//! `questions` et `answers` restent des `serde_json::Value` opaques.

use std::fmt;
use std::time::Duration;
use std::time::Instant;

use serde::Deserialize;
use serde::Serialize;
use serde_json::Map;
use serde_json::Value;

use super::error::SystemOneError;

/// Nombre maximal de caractères conservés du corps d'une réponse en erreur (`client.sdd`).
const ERROR_BODY_MAX_CHARS: usize = 512;
/// Octets lus au plus pour un corps d'erreur : 4 octets au plus par caractère, donc 512
/// caractères sont toujours atteints avant cette borne quand le corps est assez long.
const ERROR_BODY_PROBE_BYTES: usize = ERROR_BODY_MAX_CHARS * 4;

/// Configuration du client. `api_key` n'apparaît jamais dans `Debug`.
pub struct SystemOneConfig {
    /// Racine de l'API, sans le chemin `/v1/systemone` (ex. `https://ai.kydah.example`).
    pub base_url: String,
    /// Clé Bearer ; absente, aucun en-tête `Authorization` n'est envoyé.
    pub api_key: Option<String>,
    /// Délai total de la requête, en secondes ; `0` est refusé à la construction du client.
    pub timeout_secs: u64,
}

impl SystemOneConfig {
    /// Configuration sans clé et avec un délai de 30 secondes.
    #[must_use]
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            api_key: None,
            timeout_secs: 30,
        }
    }
}

impl fmt::Debug for SystemOneConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SystemOneConfig")
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("timeout_secs", &self.timeout_secs)
            .finish()
    }
}

/// Corps de `POST /v1/systemone`, sérialisé tel quel.
#[derive(Debug, Clone, Serialize)]
pub struct SystemOneRequest {
    /// Identifiant du modèle de décision.
    pub model: String,
    /// État soumis au modèle : chaîne, objet ou tableau.
    pub state: Value,
    /// Questions indexées par identifiant ; forme opaque pour ce client.
    pub questions: Map<String, Value>,
}

/// Réponse de `POST /v1/systemone`. Les champs inconnus sont ignorés.
#[derive(Debug, Clone, Deserialize)]
pub struct SystemOneResponse {
    /// Modèle ayant répondu.
    pub model: String,
    /// Réponses indexées par identifiant de question ; forme opaque pour ce client.
    pub answers: Map<String, Value>,
    /// Consommation en tokens, si l'API la renvoie.
    #[serde(default)]
    pub usage: Option<Value>,
}

/// Client HTTP System One. Sans état hors son pool `reqwest`.
#[derive(Clone)]
pub struct SystemOneClient {
    http: reqwest::Client,
    url: reqwest::Url,
}

impl SystemOneClient {
    /// Construit le client à partir de sa configuration.
    ///
    /// # Errors
    ///
    /// `SystemOneError::Config` si `base_url` n'est pas une URL `http`/`https` valide ou si
    /// `timeout_secs` vaut `0`.
    pub fn new(config: SystemOneConfig) -> Result<Self, SystemOneError> {
        let SystemOneConfig {
            base_url,
            api_key,
            timeout_secs,
        } = config;
        if timeout_secs == 0 {
            return Err(SystemOneError::Config(
                "timeout_secs doit être supérieur à 0".to_string(),
            ));
        }
        let base = base_url.trim_end_matches('/');
        let url = reqwest::Url::parse(&format!("{base}/v1/systemone"))
            .map_err(|err| SystemOneError::Config(format!("base_url invalide : {err}")))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(SystemOneError::Config(format!(
                "schéma `{}` refusé, http ou https attendu",
                url.scheme()
            )));
        }
        let mut headers = reqwest::header::HeaderMap::new();
        if let Some(key) = api_key.as_deref() {
            let mut value = reqwest::header::HeaderValue::from_str(&format!("Bearer {key}"))
                .map_err(|_| SystemOneError::Config("clé API non transportable en en-tête".to_string()))?;
            value.set_sensitive(true);
            headers.insert(reqwest::header::AUTHORIZATION, value);
        }
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(timeout_secs))
            .redirect(reqwest::redirect::Policy::none())
            .default_headers(headers)
            .build()
            .map_err(|err| SystemOneError::Config(format!("client HTTP : {err}")))?;
        Ok(Self { http, url })
    }

    /// Envoie une requête `POST /v1/systemone` et renvoie la réponse décodée.
    ///
    /// # Errors
    ///
    /// `Transport` pour un échec réseau ou un délai dépassé, `Status` pour un statut hors
    /// `2xx` (corps tronqué à 512 caractères), `InvalidResponse` pour un `2xx` non décodable.
    pub async fn ask(&self, request: &SystemOneRequest) -> Result<SystemOneResponse, SystemOneError> {
        let started = Instant::now();
        tracing::debug!(url = %self.url, "requête System One");
        let response = self
            .http
            .post(self.url.clone())
            .json(request)
            .send()
            .await
            .map_err(SystemOneError::Transport)?;
        let status = response.status();
        tracing::debug!(
            url = %self.url,
            status = status.as_u16(),
            elapsed = ?started.elapsed(),
            "réponse System One"
        );
        if !status.is_success() {
            let body = read_error_body(response).await.unwrap_or_default();
            return Err(SystemOneError::Status {
                status: status.as_u16(),
                body,
            });
        }
        let bytes = response.bytes().await.map_err(SystemOneError::Transport)?;
        serde_json::from_slice::<SystemOneResponse>(&bytes)
            .map_err(|err| SystemOneError::InvalidResponse(err.to_string()))
    }
}

/// Lit le début d'un corps d'erreur, en perte UTF-8, tronqué à `ERROR_BODY_MAX_CHARS`
/// caractères. Une lecture échouée remonte en `Err` : l'appelant la transforme en corps vide.
async fn read_error_body(mut response: reqwest::Response) -> Result<String, reqwest::Error> {
    let mut collected: Vec<u8> = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        collected.extend_from_slice(&chunk);
        if collected.len() >= ERROR_BODY_PROBE_BYTES {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&collected)
        .chars()
        .take(ERROR_BODY_MAX_CHARS)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::SystemOneClient;
    use super::SystemOneConfig;

    /// Scenario « la configuration invalide est refusée à la construction ».
    #[test]
    fn configuration_invalide_refusee_a_la_construction() {
        let mut ftp = SystemOneConfig::new("ftp://x");
        ftp.timeout_secs = 30;
        assert!(matches!(
            SystemOneClient::new(ftp),
            Err(super::super::error::SystemOneError::Config(_))
        ));

        let bogus = SystemOneConfig::new("pas une url");
        assert!(matches!(
            SystemOneClient::new(bogus),
            Err(super::super::error::SystemOneError::Config(_))
        ));

        let mut zero = SystemOneConfig::new("https://ai.kydah.example");
        zero.timeout_secs = 0;
        assert!(matches!(
            SystemOneClient::new(zero),
            Err(super::super::error::SystemOneError::Config(_))
        ));
    }

    /// Tâche « unitaires pour new » : `base_url` avec ou sans `/` final donne la même URL.
    #[test]
    fn base_url_avec_ou_sans_slash_final_donne_la_meme_url() {
        let sans = SystemOneClient::new(SystemOneConfig::new("http://127.0.0.1:8080"))
            .expect("configuration valide");
        let avec = SystemOneClient::new(SystemOneConfig::new("http://127.0.0.1:8080/"))
            .expect("configuration valide");
        assert_eq!(sans.url.as_str(), "http://127.0.0.1:8080/v1/systemone");
        assert_eq!(avec.url.as_str(), sans.url.as_str());
    }

    /// Scenario « la clé n'apparaît jamais dans Debug ».
    #[test]
    fn la_cle_n_apparait_jamais_dans_debug() {
        let config = SystemOneConfig {
            api_key: Some("secret".to_string()),
            ..SystemOneConfig::new("https://ai.kydah.example")
        };
        let rendered = format!("{config:?}");
        assert!(!rendered.contains("secret"), "fuite de clé : {rendered}");
        assert!(rendered.contains("<redacted>"), "marqueur absent : {rendered}");
    }
}
