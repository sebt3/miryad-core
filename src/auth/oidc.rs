use openidconnect::{
    AuthorizationCode, ClientId, ClientSecret, CsrfToken, EndpointMaybeSet, EndpointNotSet, EndpointSet,
    HttpRequest, HttpResponse, IssuerUrl, Nonce, RedirectUrl, Scope, TokenResponse,
    core::{CoreAuthenticationFlow, CoreClient, CoreProviderMetadata},
};

use crate::auth::config::OidcConfig;
use crate::auth::error::AuthError;

type BuiltCoreClient = CoreClient<
    EndpointSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointMaybeSet,
    EndpointMaybeSet,
>;

/// Identité extraite d'un `id_token` OIDC vérifié — c'est ce qui finit dans le cookie de session
/// (cf. `auth::cookie`). Ne porte pas les groupes : ceux-ci sont éphémères, consommés une seule
/// fois par `sync_group_memberships` au login (cf. `OidcLoginResult`), jamais persistés ici.
pub struct OidcIdentity {
    pub id_token: String,
    /// Claim `sub` — identifiant stable, ce que `users::resolve_user` utilise pour lier/créer un
    /// `User`.
    pub subject: String,
    /// Claim `email` — pas garanti par tous les fournisseurs/scopes, donc optionnel.
    pub email: Option<String>,
}

/// Résultat complet d'un échange de code réussi. `groups` (claim `groups`, spécifique à
/// Authentik — pas un claim OIDC standard) pilote la synchronisation des appartenances de groupe
/// en base (cf. feature 3, `users::sync_group_memberships`) ; il n'est jamais persisté tel quel.
pub struct OidcLoginResult {
    pub identity: OidcIdentity,
    pub groups: Vec<String>,
}

#[async_trait::async_trait]
pub trait OidcClientTrait: Send + Sync {
    fn authorization_url(&self) -> (openidconnect::url::Url, CsrfToken, Nonce);
    async fn exchange_code(&self, code: &str, expected_nonce: &Nonce) -> Result<OidcLoginResult, AuthError>;
}

pub struct OidcClient {
    inner: BuiltCoreClient,
    http_client: reqwest::Client,
    scopes: Vec<String>,
}

fn build_http_client(config: &OidcConfig) -> Result<reqwest::Client, AuthError> {
    let mut builder = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none());

    if let Some(ref ca_pem) = config.ca_cert {
        let cert = reqwest::Certificate::from_pem(ca_pem.as_bytes())
            .map_err(|e| AuthError::Oidc(format!("MRD-AUTH-007: invalid OIDC CA cert: {e}")))?;
        builder = builder.add_root_certificate(cert);
    }

    builder
        .build()
        .map_err(|e| AuthError::Oidc(format!("MRD-AUTH-008: failed to build OIDC HTTP client: {e}")))
}

async fn send_http_request(
    client: &reqwest::Client,
    request: HttpRequest,
) -> Result<HttpResponse, reqwest::Error> {
    let (parts, body) = request.into_parts();
    let mut builder = client.request(
        reqwest::Method::from_bytes(parts.method.as_str().as_bytes()).unwrap_or(reqwest::Method::GET),
        parts.uri.to_string(),
    );
    for (name, value) in &parts.headers {
        builder = builder.header(name, value);
    }
    let response = builder.body(body).send().await?;

    let status = response.status();
    let mut response_builder = openidconnect::http::Response::builder().status(status);
    if let Some(headers) = response_builder.headers_mut() {
        headers.extend(response.headers().clone());
    }
    let body = response.bytes().await?.to_vec();
    Ok(response_builder
        .body(body)
        .expect("MRD-AUTH-009: failed to build HTTP response from a valid status+headers"))
}

/// Extrait le claim `groups` du payload d'un JWT déjà vérifié (signature/expiration validées en
/// amont par `openidconnect`) — même technique que `cookie::extract_exp_claim` : on décode le
/// payload base64 nous-mêmes plutôt que de reconfigurer `CoreClient` avec des `AdditionalClaims`
/// génériques pour un seul champ non-standard. Absent ou malformé → liste vide, pas une erreur
/// (tous les fournisseurs/apps ne portent pas ce claim).
fn extract_groups_claim(jwt: &str) -> Vec<String> {
    let parts: Vec<&str> = jwt.split('.').collect();
    if parts.len() != 3 {
        return Vec::new();
    }
    use base64::Engine;
    let Ok(payload_json) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(parts[1]) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&payload_json) else {
        return Vec::new();
    };
    value
        .get("groups")
        .and_then(|g| g.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

impl OidcClient {
    pub async fn new(config: &OidcConfig) -> Result<Self, AuthError> {
        let issuer_url = IssuerUrl::new(config.issuer_url.clone())
            .map_err(|e| AuthError::Oidc(format!("MRD-AUTH-004: invalid OIDC issuer URL: {e:?}")))?;

        let http_client = build_http_client(config)?;

        let provider_metadata = {
            let client = http_client.clone();
            CoreProviderMetadata::discover_async(issuer_url, &move |req: HttpRequest| {
                let client = client.clone();
                async move { send_http_request(&client, req).await }
            })
            .await
            .map_err(|e| AuthError::Oidc(format!("MRD-AUTH-005: OIDC discovery failed: {e:?}")))?
        };

        let redirect_url = RedirectUrl::new(config.redirect_url.clone())
            .map_err(|e| AuthError::Oidc(format!("MRD-AUTH-006: invalid OIDC redirect URL: {e:?}")))?;

        let client = CoreClient::from_provider_metadata(
            provider_metadata,
            ClientId::new(config.client_id.clone()),
            Some(ClientSecret::new(config.client_secret.clone())),
        )
        .set_redirect_uri(redirect_url);

        Ok(Self {
            inner: client,
            http_client,
            scopes: config.scopes.clone(),
        })
    }
}

#[async_trait::async_trait]
impl OidcClientTrait for OidcClient {
    fn authorization_url(&self) -> (openidconnect::url::Url, CsrfToken, Nonce) {
        let mut auth_request = self.inner.authorize_url(
            CoreAuthenticationFlow::AuthorizationCode,
            CsrfToken::new_random,
            Nonce::new_random,
        );
        for scope in &self.scopes {
            if scope != "openid" {
                auth_request = auth_request.add_scope(Scope::new(scope.clone()));
            }
        }
        auth_request.url()
    }

    async fn exchange_code(&self, code: &str, expected_nonce: &Nonce) -> Result<OidcLoginResult, AuthError> {
        let client = self.http_client.clone();
        let exchange_fn = move |req: HttpRequest| {
            let client = client.clone();
            async move { send_http_request(&client, req).await }
        };

        let token_response = self
            .inner
            .exchange_code(AuthorizationCode::new(code.to_string()))
            .map_err(|e| {
                tracing::error!("MRD-AUTH-009: OIDC code exchange config error: {e}");
                AuthError::Oidc(e.to_string())
            })?
            .request_async(&exchange_fn)
            .await
            .map_err(|e| {
                tracing::error!("MRD-AUTH-009: OIDC code exchange failed: {e:#}");
                AuthError::Oidc(e.to_string())
            })?;

        let id_token = token_response
            .id_token()
            .ok_or_else(|| AuthError::Oidc("MRD-AUTH-010: no id_token in response".to_string()))?;

        let id_token_claims = id_token
            .claims(&self.inner.id_token_verifier(), expected_nonce)
            .map_err(|e| AuthError::Oidc(format!("MRD-AUTH-011: token verification failed: {e}")))?;

        let subject = id_token_claims.subject().to_string();
        let email = id_token_claims.email().map(|e| e.to_string());
        let id_token_string = id_token.to_string();
        let groups = extract_groups_claim(&id_token_string);

        Ok(OidcLoginResult {
            identity: OidcIdentity {
                id_token: id_token_string,
                subject,
                email,
            },
            groups,
        })
    }
}

#[cfg(test)]
#[derive(Debug)]
pub struct MockOidcClient;

#[cfg(test)]
#[async_trait::async_trait]
impl OidcClientTrait for MockOidcClient {
    fn authorization_url(&self) -> (openidconnect::url::Url, CsrfToken, Nonce) {
        // Construction locale, sans I/O — sûr à exercer dans les tests du routeur.
        let url = openidconnect::url::Url::parse("https://issuer.example.com/authorize")
            .expect("static URL is valid");
        (url, CsrfToken::new_random(), Nonce::new_random())
    }

    async fn exchange_code(
        &self,
        _code: &str,
        _expected_nonce: &Nonce,
    ) -> Result<OidcLoginResult, AuthError> {
        unimplemented!("MockOidcClient::exchange_code")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_groups_claim_reads_present_array() {
        use base64::Engine;
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(r#"{"sub":"u1","groups":["admin","editors"]}"#);
        let jwt = format!("header.{payload}.sig");
        assert_eq!(extract_groups_claim(&jwt), vec!["admin", "editors"]);
    }

    #[test]
    fn extract_groups_claim_defaults_to_empty_when_absent() {
        use base64::Engine;
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(r#"{"sub":"u1"}"#);
        let jwt = format!("header.{payload}.sig");
        assert!(extract_groups_claim(&jwt).is_empty());
    }

    #[test]
    fn extract_groups_claim_defaults_to_empty_when_malformed() {
        assert!(extract_groups_claim("not-a-jwt").is_empty());
    }
}
