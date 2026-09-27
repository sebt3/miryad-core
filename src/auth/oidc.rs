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
    /// Claim standard `preferred_username` — identifiant nommé garanti unique par le
    /// fournisseur, optionnel comme `email` (cf. `oidc.sdd`, ajout du 2026-09-27).
    pub preferred_username: Option<String>,
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
        let preferred_username = id_token_claims.preferred_username().map(|u| u.to_string());
        let id_token_string = id_token.to_string();
        let groups = extract_groups_claim(&id_token_string);

        Ok(OidcLoginResult {
            identity: OidcIdentity {
                id_token: id_token_string,
                subject,
                email,
                preferred_username,
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

    // ---- Fixture mock-IdP loopback (stratégie pré-arbitrage `[?]`, consigne Sébastien
    // ---- 2026-09-27 : zéro nouveau crate — std::net + HMAC-SHA256 bâti sur sha2 existant).

    /// `HMAC-SHA256` (RFC 2104) construit sur `sha2::Sha256` — signature `HS256` des `id_token`
    /// du mock `IdP`, sans dépendance `hmac` (aucune dépendance nouvelle hors `Depends on`).
    fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
        use sha2::{Digest, Sha256};
        let mut key_block = [0u8; 64];
        if key.len() > 64 {
            key_block[..32].copy_from_slice(&Sha256::digest(key));
        } else {
            key_block[..key.len()].copy_from_slice(key);
        }
        let inner = Sha256::new()
            .chain_update(key_block.map(|b| b ^ 0x36))
            .chain_update(message)
            .finalize();
        let outer = Sha256::new()
            .chain_update(key_block.map(|b| b ^ 0x5c))
            .chain_update(inner)
            .finalize();
        let mut out = [0u8; 32];
        out.copy_from_slice(&outer);
        out
    }

    fn b64url(data: &[u8]) -> String {
        use base64::Engine;
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data)
    }

    /// `id_token` HS256 signé avec `secret` sur le payload JSON passé verbatim.
    fn sign_id_token(payload_json: &str, secret: &str) -> String {
        let signing_input = format!(
            "{}.{}",
            b64url(br#"{"alg":"HS256","typ":"JWT"}"#),
            b64url(payload_json.as_bytes())
        );
        format!(
            "{signing_input}.{}",
            b64url(&hmac_sha256(secret.as_bytes(), signing_input.as_bytes()))
        )
    }

    /// Décodage du payload d'un JWT tri-segment (lecture de test uniquement).
    fn decode_jwt_payload(jwt: &str) -> serde_json::Value {
        use base64::Engine;
        let payload = jwt.split('.').nth(1).expect("fixture JWT has three segments");
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(payload)
            .expect("fixture payload decodes");
        serde_json::from_slice(&bytes).expect("fixture payload is JSON")
    }

    /// `IdP` fake loopback minimal (flux `std::net`, `Connection: close`) servant discovery,
    /// JWKS vide et un `/token` 200 avec `id_token` `HS256` — fixture des deux `Scenario`
    /// d'échange introduits le 2026-09-27.
    struct MockIdP {
        issuer: String,
        /// Corps du dernier `POST /token` reçu (forme urlencoded brute).
        token_body: std::sync::Arc<std::sync::Mutex<String>>,
        /// Valeur brute de l'en-tête `Authorization` du dernier `POST /token`.
        token_authorization: std::sync::Arc<std::sync::Mutex<String>>,
    }

    fn http_ok(body: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    impl MockIdP {
        /// Réserve le port loopback, déduit l'`issuer`, laisse l'appelant signer l'`id_token`
        /// sur cet issuer (`make_id_token` est appelée avant le spawn), puis sert l'`IdP`.
        fn start(make_id_token: impl FnOnce(&str) -> String) -> Self {
            use std::io::Read;
            use std::io::Write;

            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("loopback listener binds");
            let port = listener
                .local_addr()
                .expect("listener has a local address")
                .port();
            let issuer = format!("http://127.0.0.1:{port}");
            let id_token = make_id_token(&issuer);
            let discovery_document = http_ok(&format!(
                r#"{{"issuer":"{issuer}","authorization_endpoint":"{issuer}/authorize","token_endpoint":"{issuer}/token","jwks_uri":"{issuer}/jwks","response_types_supported":["code"],"subject_types_supported":["public"],"id_token_signing_alg_values_supported":["HS256"]}}"#
            ));
            let token_body = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
            let token_authorization = std::sync::Arc::new(std::sync::Mutex::new(String::new()));

            let body_sink = std::sync::Arc::clone(&token_body);
            let auth_sink = std::sync::Arc::clone(&token_authorization);
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { continue };
                    let mut raw = Vec::new();
                    let mut chunk = [0u8; 4096];
                    while let Ok(n) = stream.read(&mut chunk)
                        && n > 0
                    {
                        raw.extend_from_slice(&chunk[..n]);
                        if raw.windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                    let head_end = raw
                        .windows(4)
                        .position(|w| w == b"\r\n\r\n")
                        .map_or(raw.len(), |p| p + 4);
                    let head = String::from_utf8_lossy(&raw[..head_end]).to_string();
                    let content_length: usize = head
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            if name.eq_ignore_ascii_case("content-length") {
                                value.trim().parse().ok()
                            } else {
                                None
                            }
                        })
                        .unwrap_or(0);
                    while raw.len() < head_end + content_length && stream.read(&mut chunk).unwrap_or(0) > 0 {
                        raw.extend_from_slice(&chunk);
                    }
                    let request_line = head.lines().next().unwrap_or_default().to_string();
                    let path = request_line
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or_default()
                        .to_string();
                    let body = String::from_utf8_lossy(&raw[head_end..head_end + content_length]).to_string();
                    let authorization = head
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            if name.eq_ignore_ascii_case("authorization") {
                                Some(value.trim().to_string())
                            } else {
                                None
                            }
                        })
                        .unwrap_or_default();

                    let response = match path.as_str() {
                        "/.well-known/openid-configuration" => discovery_document.clone(),
                        "/jwks" => http_ok(r#"{"keys":[]}"#),
                        "/token" => {
                            *body_sink.lock().expect("test mutex is not poisoned") = body;
                            *auth_sink.lock().expect("test mutex is not poisoned") = authorization;
                            http_ok(&format!(
                                r#"{{"access_token":"access-token","token_type":"Bearer","id_token":"{id_token}"}}"#
                            ))
                        }
                        _ => "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            .to_string(),
                    };
                    let _ = stream.write_all(response.as_bytes());
                    let _ = stream.flush();
                }
            });

            Self {
                issuer,
                token_body,
                token_authorization,
            }
        }

        fn config(&self, client_id: &str, client_secret: &str) -> OidcConfig {
            OidcConfig {
                issuer_url: self.issuer.clone(),
                client_id: client_id.to_string(),
                client_secret: client_secret.to_string(),
                redirect_url: "http://app.local/callback".to_string(),
                scopes: vec!["email".to_string()],
                ca_cert: None,
                post_login_redirect: "/".to_string(),
                post_logout_redirect: "/".to_string(),
            }
        }
    }

    /// Payload `id_token` du mock `IdP` : claims du happy path (`preferred_username` présent
    /// ou omis selon le `Scenario`).
    fn mock_id_token_payload(issuer: &str, client_id: &str, nonce: &str, with_username: bool) -> String {
        let exp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after epoch")
            .as_secs()
            + 31_536_000;
        let preferred_username = if with_username {
            r#","preferred_username":"alice""#
        } else {
            ""
        };
        // `iat` est exigé à la désérialisation des `IdTokenClaims` d'openidconnect (source
        // openidconnect-4.0.1 `id_token/mod.rs`) — absent, l'échange échoue en erreur de parse.
        let iat = exp - 60;
        format!(
            r#"{{"iss":"{issuer}","aud":["{client_id}"],"sub":"user-1","nonce":"{nonce}","iat":{iat},"exp":{exp},"email":"user-1@example.com"{preferred_username},"groups":["admin","editors"]}}"#
        )
    }

    /// `Scenario` : « Échange de code réussi rend le jeton brut, les claims et les groups »
    /// (mis à jour 2026-09-27 : `preferred_username` = `Some("alice")`).
    #[tokio::test]
    async fn exchange_code_happy_path_returns_preferred_username_claim() {
        let client_id = "test-client";
        let client_secret = "test-secret";
        let expected_nonce = Nonce::new("nonce-fixe".to_string());

        // L'`issuer` n'est connu qu'après le binding du port : `start` appelle la closure
        // de signature avant de servir, l'id_token est donc signé sur le bon issuer.
        let mut signed = String::new();
        let idp = MockIdP::start(|issuer| {
            let payload = mock_id_token_payload(issuer, client_id, expected_nonce.secret(), true);
            signed = sign_id_token(&payload, client_secret);
            signed.clone()
        });
        let id_token = signed;

        let client = OidcClient::new(&idp.config(client_id, client_secret))
            .await
            .expect("mock IdP discovery succeeds");

        let result = client
            .exchange_code("the-code", &expected_nonce)
            .await
            .expect("exchange succeeds against the mock IdP");

        assert_eq!(result.identity.id_token, id_token);
        assert_eq!(result.identity.subject, "user-1");
        assert_eq!(
            result.identity.email.as_deref(),
            Some("user-1@example.com"),
            "Scenario du 2026-09-27 : `email` inchangé à côté du nouveau champ"
        );
        assert_eq!(
            result.identity.preferred_username.as_deref(),
            Some("alice"),
            "Scenario « Échange de code réussi » (2026-09-27) : claim standard restitué en `Some`"
        );
        assert_eq!(result.groups, vec!["admin".to_string(), "editors".to_string()]);

        let authorization = idp
            .token_authorization
            .lock()
            .expect("test mutex is not poisoned")
            .clone();
        assert!(
            authorization.starts_with("Basic "),
            "le POST /token porte l'authentification Basic : {authorization:?}"
        );
        let body = idp.token_body.lock().expect("test mutex is not poisoned").clone();
        assert!(body.contains("grant_type=authorization_code"), "{body}");
        assert!(body.contains("code=the-code"), "{body}");
        assert!(
            body.contains("redirect_uri=http%3A%2F%2Fapp.local%2Fcallback"),
            "le corps porte le redirect_uri configuré : {body}"
        );
        assert!(!body.contains("code_verifier"), "PKCE jamais activé : {body}");
    }

    /// `Scenario` : « Fournisseur sans claim `preferred_username` rend None » (2026-09-27).
    #[tokio::test]
    async fn provider_without_preferred_username_claim_yields_none() {
        let client_id = "test-client";
        let client_secret = "test-secret";
        let expected_nonce = Nonce::new("nonce-fixe".to_string());

        let mut signed = String::new();
        let idp = MockIdP::start(|issuer| {
            let payload = mock_id_token_payload(issuer, client_id, expected_nonce.secret(), false);
            signed = sign_id_token(&payload, client_secret);
            signed.clone()
        });
        assert!(
            decode_jwt_payload(&signed).get("preferred_username").is_none(),
            "fixture : le claim est absent de l'id_token servi"
        );

        let client = OidcClient::new(&idp.config(client_id, client_secret))
            .await
            .expect("mock IdP discovery succeeds");

        let result = client
            .exchange_code("the-code", &expected_nonce)
            .await
            .expect("exchange succeeds even without the standard claim");

        assert_eq!(result.identity.subject, "user-1");
        assert_eq!(
            result.identity.preferred_username, None,
            "Scenario « Fournisseur sans claim preferred_username » (2026-09-27) : \
             même posture que l'absence d'`email`"
        );
    }
}
