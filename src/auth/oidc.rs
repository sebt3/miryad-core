use std::sync::atomic::{AtomicBool, Ordering};

use openidconnect::{
    AuthorizationCode, ClaimsVerificationError, ClientId, ClientSecret, CsrfToken, EndpointMaybeSet,
    EndpointNotSet, EndpointSet, HttpRequest, HttpResponse, IssuerUrl, Nonce, PkceCodeChallenge,
    PkceCodeVerifier, RedirectUrl, Scope, SignatureVerificationError, TokenResponse,
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
#[derive(Clone, Debug)]
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
/// `Clone`/`Debug` (arbitrage 2026-09-27) : requis par le mode configurable de `MockOidcClient`,
/// qui rend verbatim une copie du résultat fourni à la construction.
#[derive(Clone, Debug)]
pub struct OidcLoginResult {
    pub identity: OidcIdentity,
    pub groups: Vec<String>,
}

#[async_trait::async_trait]
pub trait OidcClientTrait: Send + Sync {
    /// URL d'autorisation à flux code fraîche : `state`, `nonce` et `PkceCodeVerifier` sont
    /// générés au même appel (`PkceCodeChallenge::new_random_sha256`, arbitré 2026-09-27),
    /// non persistés ici — l'appelant doit conserver le quatuor pour son propre callback.
    fn authorization_url(&self) -> (openidconnect::url::Url, CsrfToken, Nonce, PkceCodeVerifier);
    async fn exchange_code(
        &self,
        code: &str,
        expected_nonce: &Nonce,
        pkce_verifier: &PkceCodeVerifier,
    ) -> Result<OidcLoginResult, AuthError>;
}

pub struct OidcClient {
    inner: BuiltCoreClient,
    http_client: reqwest::Client,
    scopes: Vec<String>,
    /// Signal de rotation JWKS (arbitré 2026-09-27) : armé par `exchange_code` lorsque la
    /// vérification de l'`id_token` échoue pour la cause précise « clé/`kid` introuvable »
    /// (`ClaimsVerificationError::SignatureVerification(NoMatchingKey)` de la librairie, pas
    /// une correspondance de texte). Pas de re-fetch automatique : l'app câble la lecture de
    /// ce signal dans son readiness, Kubernetes redémarre le pod, un `OidcClient` neuf refait
    /// la discovery avec un JWKS frais (cf. `oidc.sdd` `Must`).
    jwks_rotation_needed: AtomicBool,
}

/// Validation de `ca_cert` AVANT toute construction du client HTTP (`oidc.sdd`, ordre figé
/// `004` → `007`/`008` → `005` → `006`) : le contenu doit porter au moins un bloc PEM
/// `CERTIFICATE` décodable, itéré via `rustls_pki_types::pem::PemObject` (la porte
/// `reqwest::Certificate::from_pem` reste ensuite la source du `007` pour un PEM structurellement
/// valide mais refusé par rustls). Aucun chemin fichier, aucune variable d'environnement.
fn validate_ca_cert_pem(ca_pem: &str) -> Result<(), AuthError> {
    use rustls_pki_types::pem::PemObject as _;

    let mut first_error: Option<String> = None;
    for item in rustls_pki_types::CertificateDer::pem_slice_iter(ca_pem.as_bytes()) {
        match item {
            // Un seul bloc CERTIFICATE décodable suffit (le champ est additif, cf. `Must`).
            Ok(_certificate) => return Ok(()),
            Err(e) => {
                if first_error.is_none() {
                    first_error = Some(e.to_string());
                }
            }
        }
    }
    Err(AuthError::Oidc(format!(
        "MRD-AUTH-007: invalid OIDC CA cert: {}",
        first_error.unwrap_or_else(|| "no decodable PEM CERTIFICATE block".to_string())
    )))
}

fn build_http_client(config: &OidcConfig) -> Result<reqwest::Client, AuthError> {
    if let Some(ref ca_pem) = config.ca_cert {
        validate_ca_cert_pem(ca_pem)?;
    }

    // Timeouts bornés depuis `OidcConfig` (arbitré 2026-09-27, `config.sdd` `Must`) : plus
    // jamais d'attente réseau non bornée ; `Policy::none` laisse les `3xx` remonter tels quels.
    let mut builder = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(config.connect_timeout)
        .timeout(config.timeout);

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
            // `Some(secret)` → authentification `Basic` à l'échange comme avant ; `None` = client
            // public, l'authentification `Basic` est omise et la possession du code repose sur
            // PKCE `S256` seul (arbitré 2026-09-27, config.sdd `Must`).
            config
                .client_secret
                .as_ref()
                .map(|secret| ClientSecret::new(secret.clone())),
        )
        .set_redirect_uri(redirect_url);

        Ok(Self {
            inner: client,
            http_client,
            scopes: config.scopes.clone(),
            jwks_rotation_needed: AtomicBool::new(false),
        })
    }

    /// Lecture du signal de rotation JWKS (arbitré par Sébastien le 2026-09-27) : `true` dès
    /// qu'une vérification d'`id_token` a échoué parce qu'aucune clé du JWKS (snapshot de la
    /// discovery) ne correspond au `kid`/à l'algorithme du token — la seule cause qui justifie
    /// un renouvellement du JWKS par redémarrage. Les autres rejets `MRD-AUTH-011` (expiration,
    /// audience, issuer, nonce, signature) ne l'arment jamais. Une fois armé, le signal reste
    /// armé ; c'est à l'application de le câbler dans son readiness HTTP (hors périmètre).
    #[must_use]
    pub fn jwks_rotation_needed(&self) -> bool {
        self.jwks_rotation_needed.load(Ordering::Relaxed)
    }
}

#[async_trait::async_trait]
impl OidcClientTrait for OidcClient {
    fn authorization_url(&self) -> (openidconnect::url::Url, CsrfToken, Nonce, PkceCodeVerifier) {
        // PKCE `S256` inconditionnel (arbitré 2026-09-27) : la paire (challenge, verifier) naît
        // au même appel que le CSRF et le nonce ; le verifier n'est pas persisté ici, l'appelant
        // le conserve comme le nonce pour le callback qui continuera cet appel.
        let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();
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
        auth_request = auth_request.set_pkce_challenge(pkce_challenge);
        let (url, csrf_token, nonce) = auth_request.url();
        (url, csrf_token, nonce, pkce_verifier)
    }

    async fn exchange_code(
        &self,
        code: &str,
        expected_nonce: &Nonce,
        pkce_verifier: &PkceCodeVerifier,
    ) -> Result<OidcLoginResult, AuthError> {
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
                AuthError::Oidc(format!("MRD-AUTH-009: OIDC code exchange config error: {e}"))
            })?
            // `PkceCodeVerifier` n'est pas `Clone` chez oauth2 5.0.0 (vérifié source vendue) :
            // reconstruction par `secret()`, même valeur sur le fil.
            .set_pkce_verifier(PkceCodeVerifier::new(pkce_verifier.secret().clone()))
            .request_async(&exchange_fn)
            .await
            .map_err(|e| {
                tracing::error!("MRD-AUTH-009: OIDC code exchange failed: {e:#}");
                AuthError::Oidc(format!("MRD-AUTH-009: OIDC code exchange failed: {e}"))
            })?;

        let id_token = token_response
            .id_token()
            .ok_or_else(|| AuthError::Oidc("MRD-AUTH-010: no id_token in response".to_string()))?;

        let id_token_claims = id_token
            .claims(&self.inner.id_token_verifier(), expected_nonce)
            .map_err(|e| {
                // Signal de rotation JWKS : variant précis « clé introuvable » de la librairie
                // (vérifié dans la source vendue openidconnect-4.0.1, `verification::signing_key`),
                // jamais une correspondance de texte (arbitré 2026-09-27).
                if matches!(
                    &e,
                    ClaimsVerificationError::SignatureVerification(SignatureVerificationError::NoMatchingKey)
                ) {
                    self.jwks_rotation_needed.store(true, Ordering::Relaxed);
                }
                AuthError::Oidc(format!("MRD-AUTH-011: token verification failed: {e}"))
            })?;

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

/// Contrat de test des appelants (`oidc.sdd`) : URL d'émission statique sans E/S, secrets frais
/// par appel, `exchange_code` volontairement `unimplemented!` (panique nommée = assertion
/// d'absence) par défaut. Mode configurable ajouté le 2026-09-27 : `with_login_result` rend ce
/// résultat verbatim, pour le scenario « callback succès » de `mod.sdd` sans fournisseur réseau.
#[cfg(test)]
#[derive(Debug, Default)]
pub struct MockOidcClient {
    /// `None` = mode panique (défaut) ; `Some` = mode configurable.
    exchange_result: Option<OidcLoginResult>,
}

#[cfg(test)]
impl MockOidcClient {
    /// Mode par défaut : `exchange_code` panique (`unimplemented!` nommé).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Mode configurable (arbitré 2026-09-27) : `exchange_code` rend `Ok` d'une copie du
    /// résultat fourni, sans panique.
    #[must_use]
    pub fn with_login_result(result: OidcLoginResult) -> Self {
        Self {
            exchange_result: Some(result),
        }
    }
}

#[cfg(test)]
#[async_trait::async_trait]
impl OidcClientTrait for MockOidcClient {
    fn authorization_url(&self) -> (openidconnect::url::Url, CsrfToken, Nonce, PkceCodeVerifier) {
        // Construction locale, sans I/O — sûr à exercer dans les tests du routeur. Le verifier
        // PKCE suit le même patron que CSRF/nonce : chaîne fraîche par appel.
        let url = openidconnect::url::Url::parse("https://issuer.example.com/authorize")
            .expect("static URL is valid");
        let verifier = PkceCodeVerifier::new(CsrfToken::new_random().secret().clone());
        (url, CsrfToken::new_random(), Nonce::new_random(), verifier)
    }

    async fn exchange_code(
        &self,
        _code: &str,
        _expected_nonce: &Nonce,
        _pkce_verifier: &PkceCodeVerifier,
    ) -> Result<OidcLoginResult, AuthError> {
        match &self.exchange_result {
            Some(result) => Ok(result.clone()),
            None => unimplemented!("MockOidcClient::exchange_code"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    // ---- Helpers contrat ----

    /// Extrait la charge d'un refus `Err` sans imposer `Debug` au `Ok` (bound de `expect_err`)
    /// et sans `clippy::err_expect` (`.err().expect()`).
    trait ContractErr {
        fn contract_err(self, why: &str) -> AuthError;
    }

    impl<T> ContractErr for Result<T, AuthError> {
        fn contract_err(self, why: &str) -> AuthError {
            match self {
                Err(err) => err,
                Ok(_) => panic!("un refus `AuthError::Oidc` était attendu : {why}"),
            }
        }
    }

    /// Charge utile d'une `AuthError::Oidc` (préfixe `MRD-AUTH-003` ajouté par `error.rs` au
    /// `Display`, jamais ici) — les tests des `Raises` affirment le préfixe exact de la charge.
    fn oidc_payload(err: &AuthError) -> String {
        match err {
            AuthError::Oidc(payload) => payload.clone(),
            other => panic!("expected AuthError::Oidc, got {other:?}"),
        }
    }

    fn query_param(url: &openidconnect::url::Url, key: &str) -> Option<String> {
        url.query_pairs()
            .find(|(name, _)| name.as_ref() == key)
            .map(|(_, value)| value.to_string())
    }

    /// `PKCE S256` : `BASE64URL_NOPAD(SHA256(verifier))` (RFC 7636) — recalcul du challenge
    /// depuis le verifier retourné, pour prouver la cohérence de l'URL.
    fn s256_challenge(verifier: &str) -> String {
        use base64::Engine;
        use sha2::{Digest, Sha256};
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
    }

    /// Capture des traces `tracing` émises par cette crate (`target` préfixé `miryad_core`),
    /// via `tracing-subscriber` en dev-dependency (stratégie fixture arbitrée 2026-09-27).
    /// Le `DefaultGuard` est thread-local : sans effet sur les tests voisins.
    #[derive(Clone, Default)]
    struct CapturedTraces(Arc<Mutex<Vec<String>>>);

    impl CapturedTraces {
        fn lines(&self) -> Vec<String> {
            self.0.lock().expect("test mutex is not poisoned").clone()
        }
    }

    struct CaptureLayer(Arc<Mutex<Vec<String>>>);

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for CaptureLayer {
        fn on_event(&self, event: &tracing::Event<'_>, _ctx: tracing_subscriber::layer::Context<'_, S>) {
            if !event.metadata().target().starts_with("miryad_core") {
                return;
            }
            let mut visitor = MessageVisitor::default();
            event.record(&mut visitor);
            self.0
                .lock()
                .expect("test mutex is not poisoned")
                .push(visitor.finish());
        }
    }

    #[derive(Default)]
    struct MessageVisitor {
        rendered: String,
    }

    impl MessageVisitor {
        fn finish(&self) -> String {
            self.rendered.clone()
        }
    }

    impl tracing::field::Visit for MessageVisitor {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            use std::fmt::Write as _;

            if !self.rendered.is_empty() {
                self.rendered.push(' ');
            }
            let _ = write!(&mut self.rendered, "{}={value:?}", field.name());
        }
    }

    fn capture_traces() -> (tracing::subscriber::DefaultGuard, CapturedTraces) {
        use tracing_subscriber::layer::SubscriberExt as _;
        let captured = CapturedTraces::default();
        let subscriber =
            tracing_subscriber::registry::Registry::default().with(CaptureLayer(Arc::clone(&captured.0)));
        let guard = tracing::subscriber::set_default(subscriber);
        (guard, captured)
    }

    // ---- Fixtures JWT (HS256 signé au runtime, `hmac`+`sha2` dépendances normales) ----

    /// `HMAC-SHA256` (RFC 2104) via la crate `hmac` — signature `HS256` des `id_token` du mock
    /// `IdP` (réutilisée depuis le poivre de `token.rs`, `Depends on` de `oidc.sdd`).
    fn hmac_sha256(key: &[u8], message: &[u8]) -> Vec<u8> {
        use hmac::Mac;
        <hmac::Hmac<sha2::Sha256> as Mac>::new_from_slice(key)
            .expect("HMAC accepts any key length")
            .chain_update(message)
            .finalize()
            .into_bytes()
            .to_vec()
    }

    fn b64url(data: &[u8]) -> String {
        use base64::Engine;
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data)
    }

    fn sign_jwt(payload_json: &str, header_json: &str, secret: &str) -> String {
        let signing_input = format!(
            "{}.{}",
            b64url(header_json.as_bytes()),
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

    /// Payload `id_token` paramétrable : défauts = happy path (claims parfaitement valides sur
    /// l'`issuer` et le `client_id` fournis, `nonce` attendu, `exp` très futur).
    #[derive(Clone)]
    struct ClaimsFixture {
        /// Claim `iss` — défaut : issuer de la metadata.
        iss: Option<String>,
        /// Claim `aud` (mono-audience) — défaut : `client_id` configuré.
        aud: Option<String>,
        /// Claim `nonce` — défaut : nonce attendu passé à `build_signed_token`.
        nonce: Option<String>,
        /// `exp = now + exp_secs_from_now` (négatif = déjà expiré).
        exp_secs_from_now: i64,
        preferred_username: bool,
        /// `alg` du JOSE header — `RS256` + JWKS sans clé = variante `NoMatchingKey` amont.
        alg: &'static str,
        kid: Option<String>,
    }

    impl Default for ClaimsFixture {
        fn default() -> Self {
            Self {
                iss: None,
                aud: None,
                nonce: None,
                exp_secs_from_now: 31_536_000,
                preferred_username: true,
                alg: "HS256",
                kid: None,
            }
        }
    }

    fn build_signed_token(
        fixture: &ClaimsFixture,
        issuer: &str,
        client_id: &str,
        expected_nonce: &str,
        secret: &str,
    ) -> String {
        let iss = fixture.iss.clone().unwrap_or_else(|| issuer.to_string());
        let aud = fixture.aud.clone().unwrap_or_else(|| client_id.to_string());
        let nonce = fixture
            .nonce
            .clone()
            .unwrap_or_else(|| expected_nonce.to_string());
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after epoch")
            .as_secs()
            .cast_signed();
        let exp = now + fixture.exp_secs_from_now;
        // `iat` est exigé à la désérialisation des `IdTokenClaims` d'openidconnect (source
        // openidconnect-4.0.1 `id_token/mod.rs`) — absent, l'échange échoue en erreur de parse.
        let iat = exp - 60;
        let preferred_username = if fixture.preferred_username {
            r#","preferred_username":"alice""#
        } else {
            ""
        };
        let payload = format!(
            r#"{{"iss":"{iss}","aud":["{aud}"],"sub":"user-1","nonce":"{nonce}","iat":{iat},"exp":{exp},"email":"user-1@example.com"{preferred_username},"groups":["admin","editors"]}}"#
        );
        let kid_part = fixture
            .kid
            .as_ref()
            .map_or(String::new(), |kid| format!(r#","kid":"{kid}""#));
        let header = format!(r#"{{"alg":"{}","typ":"JWT"{kid_part}}}"#, fixture.alg);
        sign_jwt(&payload, &header, secret)
    }

    // ---- Mock IdP loopback (tokio::net, fixture arbitrée `[x]` 2026-09-27) ----

    /// Comportement du endpoint `/token` du fournisseur factice.
    #[derive(Clone)]
    enum TokenMode {
        /// `200` JSON avec l'`id_token` courant du slot (et rejet `400` si le `code_verifier`
        /// exigé diverge — comportement d'un fournisseur PKCE conforme).
        IdToken,
        /// `200` JSON sans champ `id_token`.
        NoIdToken,
        /// `400` erreur OAuth `invalid_grant`.
        OAuthError,
        /// `302` avec `Location` — `Policy::none` doit le laisser passer sans le suivre.
        Redirect(String),
        /// Accepte la connexion, ne répond jamais (épreuve du `timeout` total).
        Silent,
    }

    /// Plan du mock `IdP` : chemins servis, contenu du document de discovery, JWKS, mode token.
    #[derive(Clone)]
    struct IdPPlan {
        doc_paths: Vec<String>,
        jwks_paths: Vec<String>,
        /// Claim `issuer` du document de discovery — défaut : base loopback de l'`IdP`.
        doc_issuer: Option<String>,
        /// Valeur `jwks_uri` du document — défaut : base + premier `jwks_path`.
        jwks_uri: Option<String>,
        /// Contenu de `id_token_signing_alg_values_supported` (liste JSON sans crochets).
        algs_json: String,
        token_endpoint: bool,
        jwks_json: String,
        token_mode: TokenMode,
    }

    impl Default for IdPPlan {
        fn default() -> Self {
            Self {
                doc_paths: vec!["/.well-known/openid-configuration".to_string()],
                jwks_paths: vec!["/jwks".to_string()],
                doc_issuer: None,
                jwks_uri: None,
                algs_json: r#""HS256""#.to_string(),
                token_endpoint: true,
                jwks_json: r#"{"keys":[]}"#.to_string(),
                token_mode: TokenMode::IdToken,
            }
        }
    }

    struct IdPState {
        document: String,
        doc_paths: Vec<String>,
        jwks_paths: Vec<String>,
        jwks_json: String,
        token_mode: TokenMode,
        /// `id_token` servi par `/token` (slot rempli par le test, y compris après
        /// `authorization_url` pour embarquer le nonce réel du couple).
        id_token: Arc<Mutex<String>>,
        /// `code_verifier` exigé du fournisseur factice (`None` = n'exige rien).
        expected_code_verifier: Arc<Mutex<Option<String>>>,
        requests: Arc<Mutex<Vec<String>>>,
        token_body: Arc<Mutex<String>>,
        token_authorization: Arc<Mutex<String>>,
    }

    struct MockIdP {
        issuer: String,
        state: Arc<IdPState>,
    }

    fn http_ok(body: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    fn http_status(status_line: &str, extra_headers: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status_line}\r\n{extra_headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    fn oauth_error_body() -> String {
        r#"{"error":"invalid_grant","error_description":"code verifier mismatch"}"#.to_string()
    }

    impl MockIdP {
        /// Bind loopback synchrone (l'`issuer` est connu avant tout spawn), puis service asynchrone
        /// `tokio::net`. L'`id_token` servi est un slot rempli via [`MockIdP::set_id_token`].
        fn start(plan: IdPPlan) -> Self {
            let std_listener = std::net::TcpListener::bind("127.0.0.1:0").expect("loopback listener binds");
            let port = std_listener
                .local_addr()
                .expect("listener has a local address")
                .port();
            std_listener
                .set_nonblocking(true)
                .expect("listener switches to nonblocking");
            let listener =
                tokio::net::TcpListener::from_std(std_listener).expect("tokio listener adopts std listener");
            let issuer = format!("http://127.0.0.1:{port}");
            let doc_issuer = plan.doc_issuer.clone().unwrap_or_else(|| issuer.clone());
            let jwks_uri = plan.jwks_uri.clone().unwrap_or_else(|| {
                format!(
                    "{}{}",
                    issuer,
                    plan.jwks_paths.first().cloned().unwrap_or_default()
                )
            });
            let token_endpoint_field = if plan.token_endpoint {
                format!(r#","token_endpoint":"{issuer}/token""#)
            } else {
                String::new()
            };
            let document = format!(
                r#"{{"issuer":"{doc_issuer}","authorization_endpoint":"{doc_issuer}/authorize"{token_endpoint_field},"jwks_uri":"{jwks_uri}","response_types_supported":["code"],"subject_types_supported":["public"],"id_token_signing_alg_values_supported":[{}]}}"#,
                plan.algs_json
            );
            let state = Arc::new(IdPState {
                document,
                doc_paths: plan.doc_paths,
                jwks_paths: plan.jwks_paths,
                jwks_json: plan.jwks_json,
                token_mode: plan.token_mode,
                id_token: Arc::new(Mutex::new(String::new())),
                expected_code_verifier: Arc::new(Mutex::new(None)),
                requests: Arc::new(Mutex::new(Vec::new())),
                token_body: Arc::new(Mutex::new(String::new())),
                token_authorization: Arc::new(Mutex::new(String::new())),
            });
            tokio::spawn(serve_idp(listener, Arc::clone(&state)));

            Self { issuer, state }
        }

        fn set_id_token(&self, jwt: String) {
            *self.state.id_token.lock().expect("test mutex is not poisoned") = jwt;
        }

        fn expect_code_verifier(&self, verifier: &str) {
            *self
                .state
                .expected_code_verifier
                .lock()
                .expect("test mutex is not poisoned") = Some(verifier.to_string());
        }

        fn requests(&self) -> Vec<String> {
            self.state
                .requests
                .lock()
                .expect("test mutex is not poisoned")
                .clone()
        }

        fn token_body(&self) -> String {
            self.state
                .token_body
                .lock()
                .expect("test mutex is not poisoned")
                .clone()
        }

        fn token_authorization(&self) -> String {
            self.state
                .token_authorization
                .lock()
                .expect("test mutex is not poisoned")
                .clone()
        }

        fn config(&self, client_id: &str, client_secret: &str) -> OidcConfig {
            Self::config_at(&self.issuer, client_id, client_secret)
        }

        fn config_at(issuer_url: &str, client_id: &str, client_secret: &str) -> OidcConfig {
            OidcConfig {
                issuer_url: issuer_url.to_string(),
                client_id: client_id.to_string(),
                client_secret: Some(client_secret.to_string()),
                redirect_url: "http://app.local/callback".to_string(),
                scopes: vec!["email".to_string()],
                ca_cert: None,
                connect_timeout: std::time::Duration::from_secs(5),
                timeout: std::time::Duration::from_secs(15),
            }
        }
    }

    async fn serve_idp(listener: tokio::net::TcpListener, state: Arc<IdPState>) {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            let state = Arc::clone(&state);
            tokio::spawn(async move {
                handle_idp_connection(stream, state).await;
            });
        }
    }

    struct IdPRequest {
        head: String,
        path: String,
        body: String,
    }

    enum IdPAnswer {
        Response(String),
        /// Retient le socket sans jamais répondre (`TokenMode::Silent`).
        Hold,
    }

    async fn handle_idp_connection(mut stream: tokio::net::TcpStream, state: Arc<IdPState>) {
        use tokio::io::AsyncWriteExt;

        let Some(request) = read_idp_request(&mut stream).await else {
            return;
        };
        state
            .requests
            .lock()
            .expect("test mutex is not poisoned")
            .push(request.path.clone());
        match build_idp_answer(&state, &request) {
            // Fuite volontaire du socket : `forget` n'exécute pas `Drop`, la connexion reste
            // ouverte sans réponse — seule façon d'éprouver le `timeout` total côté client.
            IdPAnswer::Hold => std::mem::forget(stream),
            IdPAnswer::Response(response) => {
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            }
        }
    }

    /// Lecture d'une requête HTTP/1.1 brute (head complet + corps `Content-Length`).
    async fn read_idp_request(stream: &mut tokio::net::TcpStream) -> Option<IdPRequest> {
        use tokio::io::AsyncReadExt;

        let mut raw: Vec<u8> = Vec::new();
        let mut buf = [0u8; 4096];
        let head_end = loop {
            if let Some(pos) = raw.windows(4).position(|window| window == b"\r\n\r\n") {
                break pos + 4;
            }
            match stream.read(&mut buf).await {
                Ok(0) | Err(_) => return None,
                Ok(n) => raw.extend_from_slice(&buf[..n]),
            }
        };
        let head = String::from_utf8_lossy(&raw[..head_end]).to_string();
        let content_length: usize = header_value(&head, "content-length")
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);
        while raw.len() < head_end + content_length {
            match stream.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => raw.extend_from_slice(&buf[..n]),
            }
        }
        let path = head
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap_or_default()
            .to_string();
        let body_end = (head_end + content_length).min(raw.len());
        let body = String::from_utf8_lossy(&raw[head_end..body_end]).to_string();
        Some(IdPRequest { head, path, body })
    }

    fn header_value<'a>(head: &'a str, name: &str) -> Option<&'a str> {
        head.lines().skip(1).find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case(name).then(|| value.trim())
        })
    }

    fn build_idp_answer(state: &IdPState, request: &IdPRequest) -> IdPAnswer {
        if state.doc_paths.contains(&request.path) {
            return IdPAnswer::Response(http_ok(&state.document));
        }
        if state.jwks_paths.contains(&request.path) {
            return IdPAnswer::Response(http_ok(&state.jwks_json));
        }
        if request.path != "/token" {
            return IdPAnswer::Response(http_status("404 Not Found", "", ""));
        }
        *state.token_body.lock().expect("test mutex is not poisoned") = request.body.clone();
        *state
            .token_authorization
            .lock()
            .expect("test mutex is not poisoned") = header_value(&request.head, "authorization")
            .unwrap_or_default()
            .to_string();

        match &state.token_mode {
            TokenMode::IdToken => {
                let expected = state
                    .expected_code_verifier
                    .lock()
                    .expect("test mutex is not poisoned")
                    .clone();
                let verifier_ok = match &expected {
                    Some(expected) => request.body.contains(&format!("code_verifier={expected}")),
                    None => true,
                };
                if verifier_ok {
                    let id_token = state.id_token.lock().expect("test mutex is not poisoned").clone();
                    IdPAnswer::Response(http_ok(&format!(
                        r#"{{"access_token":"access-token","token_type":"Bearer","id_token":"{id_token}"}}"#
                    )))
                } else {
                    // Rejet d'un fournisseur `PKCE` conforme (`code_verifier` divergent).
                    IdPAnswer::Response(http_status(
                        "400 Bad Request",
                        "Content-Type: application/json\r\n",
                        &oauth_error_body(),
                    ))
                }
            }
            TokenMode::NoIdToken => IdPAnswer::Response(http_ok(
                r#"{"access_token":"access-token","token_type":"Bearer"}"#,
            )),
            TokenMode::OAuthError => IdPAnswer::Response(http_status(
                "400 Bad Request",
                "Content-Type: application/json\r\n",
                &oauth_error_body(),
            )),
            TokenMode::Redirect(location) => {
                IdPAnswer::Response(http_status("302 Found", &format!("Location: {location}\r\n"), ""))
            }
            TokenMode::Silent => IdPAnswer::Hold,
        }
    }

    /// Faux endpoint `Location` d'une redirection `302` : compte toute visite. `Policy::none`
    /// impose `0` visite après un échange.
    fn start_honeypot() -> (String, Arc<std::sync::atomic::AtomicUsize>) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::io::AsyncWriteExt;

        let std_listener = std::net::TcpListener::bind("127.0.0.1:0").expect("honeypot binds");
        let port = std_listener.local_addr().expect("honeypot address").port();
        std_listener.set_nonblocking(true).expect("honeypot nonblocking");
        let listener = tokio::net::TcpListener::from_std(std_listener).expect("honeypot tokio listener");
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_task = Arc::clone(&hits);
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    continue;
                };
                hits_task.fetch_add(1, Ordering::Relaxed);
                tokio::spawn(async move {
                    let _ = stream.write_all(http_status("200 OK", "", "").as_bytes()).await;
                });
            }
        });
        (format!("http://127.0.0.1:{port}/jamais-visitee"), hits)
    }

    /// Construction client + couple `authorization_url` (le verifier et le nonce « valides » que
    /// la spec exige par défaut de tout `Scenario` d'échange).
    async fn client_with_pair(
        idp: &MockIdP,
        client_id: &str,
        client_secret: &str,
    ) -> (OidcClient, Nonce, PkceCodeVerifier) {
        let client = OidcClient::new(&idp.config(client_id, client_secret))
            .await
            .expect("mock IdP discovery succeeds");
        let (_url, _csrf, nonce, verifier) = client.authorization_url();
        (client, nonce, verifier)
    }

    // ---- Les 31 `Scenario` de `oidc.sdd` (numérotation = ordre du fichier de spec) ----

    /// `Scenario` 1 : « Issuer impossible à parser, refus 004 sans aucun contact réseau ».
    #[tokio::test]
    async fn new_rejects_unparsable_issuer_004_before_any_network() {
        let config = OidcConfig {
            issuer_url: "pas une url".to_string(),
            client_id: "cid".to_string(),
            client_secret: None,
            redirect_url: "http://app.local/callback".to_string(),
            scopes: vec![],
            ca_cert: None,
            connect_timeout: std::time::Duration::from_secs(5),
            timeout: std::time::Duration::from_secs(15),
        };

        let err = OidcClient::new(&config)
            .await
            .contract_err("issuer unparsable must be refused");
        let payload = oidc_payload(&err);
        assert!(
            payload.starts_with("MRD-AUTH-004: invalid OIDC issuer URL: "),
            "charge utile attendue : {payload}"
        );
        assert!(
            err.to_string()
                .starts_with("MRD-AUTH-003: OIDC error: MRD-AUTH-004: "),
            "double préfixe rendu par error.rs : {err}"
        );
        // L'erreur précède `build_http_client` comme la discovery (ordre figé) : aucune requête
        // n'a pu partir — vérifié par lecture de l'ordre dans `new`, observable ici par le fait
        // qu'aucun réseau n'est nécessaire pour que ce test passe.
    }

    /// `Scenario` 2 : « Schéma http accepté au parse de l'issuer, c'est la discovery qui refuse 005 ».
    #[tokio::test]
    async fn http_issuer_scheme_passes_parse_and_fails_discovery_005() {
        let config = OidcConfig {
            issuer_url: "http://127.0.0.1:9".to_string(),
            client_id: "cid".to_string(),
            client_secret: None,
            redirect_url: "http://app.local/callback".to_string(),
            scopes: vec![],
            ca_cert: None,
            connect_timeout: std::time::Duration::from_secs(5),
            timeout: std::time::Duration::from_secs(15),
        };

        let err = OidcClient::new(&config)
            .await
            .contract_err("closed port discovery must fail");
        let payload = oidc_payload(&err);
        assert!(
            payload.starts_with("MRD-AUTH-005: OIDC discovery failed: "),
            "{payload}"
        );
        assert!(
            !payload.contains("MRD-AUTH-004"),
            "http n'est pas un motif de refus au parse : {payload}"
        );
    }

    /// `Scenario` 3 : « CA PEM invalide refusée 007 avant tout réseau » — issuer syntaxiquement
    /// valide derrière, port fermé derrière : c'est bien `007` qui tombe, preuve que la validation
    /// PEM précède la construction du client et la discovery.
    #[tokio::test]
    async fn invalid_ca_pem_refused_007_before_any_network() {
        let idp = MockIdP::start(IdPPlan::default());
        let mut config = idp.config("cid", "secret");
        config.issuer_url = "http://127.0.0.1:9".to_string();
        config.ca_cert = Some("pas-du-pem".to_string());

        let err = OidcClient::new(&config)
            .await
            .contract_err("garbage CA must be refused");
        let payload = oidc_payload(&err);
        assert!(
            payload.starts_with("MRD-AUTH-007: invalid OIDC CA cert: "),
            "{payload}"
        );

        // Second motif, plus fin : bloc `CERTIFICATE` présent mais base64 indécodable → refus
        // par l'itérateur PEM (`validate_ca_cert_pem`), pas par reqwest.
        config.ca_cert =
            Some("-----BEGIN CERTIFICATE-----\n%%pas-du-base64%%\n-----END CERTIFICATE-----\n".to_string());
        let err = OidcClient::new(&config)
            .await
            .contract_err("undecodable PEM block must be refused");
        let payload = oidc_payload(&err);
        assert!(
            payload.starts_with("MRD-AUTH-007: invalid OIDC CA cert: "),
            "{payload}"
        );
    }

    /// `Scenario` 3 (garde inverse) : un PEM `CERTIFICATE` valide passe la validation et laisse
    /// la discovery faire son travail (`005`, pas `007`) — la validation n'est pas un filtre
    /// qui refuse tout.
    #[tokio::test]
    async fn valid_ca_pem_passes_validation_then_discovery_fails_005() {
        const VALID_TEST_CA: &str = concat!(
            "-----BEGIN CERTIFICATE-----\n",
            "MIIDEzCCAfugAwIBAgIUXA6CRlQCkQH41aW/bm5UlCyJbp8wDQYJKoZIhvcNAQEL\n",
            "BQAwGTEXMBUGA1UEAwwObWlyeWFkLXRlc3QtY2EwHhcNMjYwOTI3MjAwMDI1WhcN\n",
            "MzYwOTI0MjAwMDI1WjAZMRcwFQYDVQQDDA5taXJ5YWQtdGVzdC1jYTCCASIwDQYJ\n",
            "KoZIhvcNAQEBBQADggEPADCCAQoCggEBAMkKaHolBH7ZBGSE8mUrBPspzzgNn8Sv\n",
            "nSGqV7WX1Xr14E8M3MLc4n5UiZGzPQ+b/co4tHELdV6I0PeXk3DVb5FPr6QtJXhC\n",
            "RcJEysZGFZlU1fBjW4gpNBNsnK2B6+uD0mTr8ib/B5l0LZEGfuJ/4B3eE1AeuFAc\n",
            "nSmG+1PZTtkBWpdMQqDEtbzT5uROkoSYuOa06iZsDWovle5Yvt8tNpCF4FzvQ0Uq\n",
            "ApqhsFqUE4wvaRkBzEMS6EWfp9gKXEuJEI4cv/WdGHcbfB2i5zdK4bTf2Di7hQPm\n",
            "t/vysSQg68WBCfx9S9WjX+HVX9nAMiUMWqrrgsFC8y0FlBKg7dQ5OKcCAwEAAaNT\n",
            "MFEwHQYDVR0OBBYEFKxY7SIEN1S4v52hPgfCsx2WwxpaMB8GA1UdIwQYMBaAFKxY\n",
            "7SIEN1S4v52hPgfCsx2WwxpaMA8GA1UdEwEB/wQFMAMBAf8wDQYJKoZIhvcNAQEL\n",
            "BQADggEBABzmXfRnlM2s/SApiOeNlvbUdHJ6dURzv4HCOgrF3d8CdZnvUA/SuN6t\n",
            "cxtylt6wRiXxVcjVA84WF3Iahw+2tk1ChgZO+bARnOuLs1uSPCaa6ajGuw410UMx\n",
            "YzSw5fiWcivgEec6cNeq2ocGMRaXHOCCNuOVdXXS8puiu5Y6c5oXOTtf+krdSJTN\n",
            "GDdB1r95vAH+Amc7jiJYg6IvArUCUwc9HCV78QHQWx/9jXgT3EeXRNpxIlHynaJJ\n",
            "3zHywcLc5e+In4p2NlDM+Bv3NJ8JdjQqQiakMYjSeikImISaJNifcc7kuQj0Pc30\n",
            "IvS7xrsAIkcFXy4r1/D3/IW7LkYyD6U=\n",
            "-----END CERTIFICATE-----\n",
        );

        let mut config = OidcConfig {
            issuer_url: "http://127.0.0.1:9".to_string(),
            client_id: "cid".to_string(),
            client_secret: None,
            redirect_url: "http://app.local/callback".to_string(),
            scopes: vec![],
            ca_cert: Some(VALID_TEST_CA.to_string()),
            connect_timeout: std::time::Duration::from_secs(5),
            timeout: std::time::Duration::from_secs(15),
        };
        // La validation PEM doit passer : l'erreur qui suit est la discovery (`005`), pas `007`.
        let err = OidcClient::new(&config)
            .await
            .contract_err("closed port behind valid CA still fails discovery");
        let payload = oidc_payload(&err);
        assert!(
            payload.starts_with("MRD-AUTH-005: OIDC discovery failed: "),
            "{payload}"
        );

        // Et le même CA valide derrière un IdP fonctionnel ne casse pas la construction.
        let idp = MockIdP::start(IdPPlan::default());
        config.issuer_url = idp.issuer.clone();
        assert!(
            OidcClient::new(&config).await.is_ok(),
            "CA valide = client construit"
        );
    }

    /// `Scenario` 4 : « JWKS inaccessible fait échouer toute la construction en 005 » —
    /// l'`IdP` sert le document (une fois) puis le `jwks_uri` désigne un port fermé.
    #[tokio::test]
    async fn inaccessible_jwks_fails_construction_005_after_document_is_served() {
        let plan = IdPPlan {
            jwks_uri: Some("http://127.0.0.1:9/keys".to_string()),
            ..IdPPlan::default()
        };
        let idp = MockIdP::start(plan);

        let err = OidcClient::new(&idp.config("cid", "secret"))
            .await
            .contract_err("inaccessible JWKS must fail construction");
        let payload = oidc_payload(&err);
        assert!(
            payload.starts_with("MRD-AUTH-005: OIDC discovery failed: "),
            "{payload}"
        );
        assert_eq!(
            idp.requests(),
            vec!["/.well-known/openid-configuration".to_string()],
            "le document a bien été servi une fois : le refus vient du fetch du jwks_uri"
        );
    }

    /// `Scenario` 5 : « Document de discovery d'un issuer différent refusé 005 ».
    #[tokio::test]
    async fn discovery_document_declaring_other_issuer_refused_005() {
        let plan = IdPPlan {
            doc_issuer: Some("http://issuer-intrus.invalid".to_string()),
            ..IdPPlan::default()
        };
        let idp = MockIdP::start(plan);

        let err = OidcClient::new(&idp.config("cid", "secret"))
            .await
            .contract_err("issuer mismatch must be refused");
        let payload = oidc_payload(&err);
        assert!(
            payload.starts_with("MRD-AUTH-005: OIDC discovery failed: "),
            "{payload}"
        );
        assert!(
            idp.requests()
                .contains(&"/.well-known/openid-configuration".to_string()),
            "le document a été servi avant le refus de validation"
        );
    }

    /// `Scenario` 6 — ⚠ SCENARIO PROUVÉ FAUX PAR LA SOURCE AMONT, EN ATTENTE D'ARBITRAGE
    /// (question écrite dans le rapport d'implémentation 2026-09-27). La spec attend une
    /// jointure d'URL relative (`GET /.well-known/openid-configuration`, dernier segment du
    /// chemin effacé, « vérifiée dans la source amont »). La source vendue `openidconnect-4.0.1`
    /// dit le contraire : `types/mod.rs` (`IssuerUrl::join`) concatène `issuer + "/" + suffix`
    /// quand l'issuer ne finit pas par `/`. La jointure est dans la librairie, hors de portée de
    /// ce fichier (qui délègue toute la discovery). Ce test verrouille la RÉALITÉ observable
    /// (GET sur `/reverse/.well-known/…`, puis refus `005` car le document servi déclare l'issuer
    /// sans `/reverse`) ; il sera réaligné sur l'arbitrage de la spec, à l'unisson du mot de
    /// Sébastien — la spec ou le test bougera, jamais un faux contrat.
    #[tokio::test]
    async fn issuer_path_without_trailing_slash_currently_joins_by_concatenation_005() {
        let plan = IdPPlan {
            doc_paths: vec!["/reverse/.well-known/openid-configuration".to_string()],
            jwks_paths: vec!["/reverse/keys".to_string()],
            ..IdPPlan::default()
        };
        let idp = MockIdP::start(plan);
        let issuer_with_path = format!("{}/reverse", idp.issuer);

        let err = OidcClient::new(&MockIdP::config_at(&issuer_with_path, "cid", "secret"))
            .await
            .contract_err("the served document declares the slashless issuer: validation must refuse it");
        let requests = idp.requests();
        assert_eq!(
            requests,
            vec!["/reverse/.well-known/openid-configuration".to_string()],
            "réalité amont : `IssuerUrl::join` concatène — la spec, elle, attend un GET sur la racine"
        );
        let payload = oidc_payload(&err);
        assert!(
            payload.starts_with("MRD-AUTH-005: OIDC discovery failed: "),
            "{payload}"
        );
    }

    /// `Scenario` 7 : « Callback invalide : surface après discovery, signalé 006 ».
    #[tokio::test]
    async fn invalid_redirect_url_surfaces_006_only_after_successful_discovery() {
        let idp = MockIdP::start(IdPPlan::default());
        let mut config = idp.config("cid", "secret");
        config.redirect_url = "pas une url".to_string();

        let err = OidcClient::new(&config)
            .await
            .contract_err("unparsable callback must be refused");
        let payload = oidc_payload(&err);
        assert!(
            payload.starts_with("MRD-AUTH-006: invalid OIDC redirect URL: "),
            "{payload}"
        );
        let requests = idp.requests();
        assert!(
            requests.contains(&"/.well-known/openid-configuration".to_string())
                && requests.contains(&"/jwks".to_string()),
            "006 est post-réseau par construction : {requests:?}"
        );
    }

    /// `Scenario` 8 : « URL d'autorisation : flux code, state, nonce, openid, callback, PKCE S256 ».
    #[tokio::test]
    async fn authorization_url_carries_code_flow_state_nonce_openid_callback_and_pkce_s256() {
        let idp = MockIdP::start(IdPPlan::default());
        let mut config = idp.config("test-client", "test-secret");
        config.scopes = vec!["email".to_string(), "profile".to_string()];
        let client = OidcClient::new(&config).await.expect("discovery succeeds");

        let (url, csrf, nonce, verifier) = client.authorization_url();

        assert!(
            url.as_str().starts_with(&format!("{}/authorize?", idp.issuer)),
            "l'URL vise l'authorization_endpoint : {url}"
        );
        assert_eq!(query_param(&url, "response_type").as_deref(), Some("code"));
        assert_eq!(query_param(&url, "client_id").as_deref(), Some("test-client"));
        assert_eq!(
            query_param(&url, "state").as_deref(),
            Some(csrf.secret().as_str())
        );
        assert_eq!(
            query_param(&url, "nonce").as_deref(),
            Some(nonce.secret().as_str())
        );
        assert_eq!(
            query_param(&url, "redirect_uri").as_deref(),
            Some("http://app.local/callback")
        );
        let scope = query_param(&url, "scope").expect("scope param present");
        let scope_tokens: Vec<&str> = scope.split(' ').collect();
        assert_eq!(
            scope_tokens.first().copied(),
            Some("openid"),
            "injecté par la librairie en tête : {scope}"
        );
        assert!(
            scope_tokens.contains(&"email") && scope_tokens.contains(&"profile"),
            "{scope}"
        );
        assert_eq!(
            query_param(&url, "code_challenge_method").as_deref(),
            Some("S256")
        );
        assert_eq!(
            query_param(&url, "code_challenge").as_deref(),
            Some(s256_challenge(verifier.secret()).as_str()),
            "le challenge de l'URL est bien dérivé du verifier retourné (RFC 7636)"
        );
        // Le fichier ne stocke ni state, ni nonce, ni verifier : structuré par l'absence de tout
        // champ d'état sur `OidcClient` hormis `jwks_rotation_needed` (lecture statique).
    }

    /// `Scenario` 9 : « Scope openid demandé une seule fois même si la config liste openid ».
    #[tokio::test]
    async fn openid_scope_appears_exactly_once_when_configured() {
        let idp = MockIdP::start(IdPPlan::default());
        let mut config = idp.config("test-client", "test-secret");
        config.scopes = vec!["openid".to_string(), "email".to_string()];
        let client = OidcClient::new(&config).await.expect("discovery succeeds");

        let (url, _csrf, _nonce, _verifier) = client.authorization_url();
        let scope = query_param(&url, "scope").expect("scope param present");
        assert_eq!(
            scope.split(' ').filter(|token| *token == "openid").count(),
            1,
            "jamais openid+openid : le filtre `!= \"openid\"` tient sa promesse : {scope}"
        );
        assert!(scope.split(' ').any(|token| token == "email"), "{scope}");
    }

    /// `Scenario` 10 : « State, nonce et verifier PKCE frais à chaque demande d'URL ».
    #[tokio::test]
    async fn state_nonce_and_verifier_are_fresh_on_every_authorization_url_call() {
        let idp = MockIdP::start(IdPPlan::default());
        let client = OidcClient::new(&idp.config("test-client", "test-secret"))
            .await
            .expect("discovery succeeds");

        let (url1, csrf1, nonce1, verifier1) = client.authorization_url();
        let (url2, csrf2, nonce2, verifier2) = client.authorization_url();

        let first = [csrf1.secret(), nonce1.secret(), verifier1.secret()];
        let second = [csrf2.secret(), nonce2.secret(), verifier2.secret()];
        for secret in first.iter().chain(second.iter()) {
            assert!(!secret.is_empty(), "les secrets rendus sont non vides");
        }
        for a in first {
            for b in second {
                assert_ne!(a, b, "aucun secret du premier appel ne réapparaît dans le second");
            }
        }
        for (url, csrf, nonce, verifier) in [
            &(url1, &csrf1, &nonce1, &verifier1),
            &(url2, &csrf2, &nonce2, &verifier2),
        ] {
            assert_eq!(query_param(url, "state").as_deref(), Some(csrf.secret().as_str()));
            assert_eq!(
                query_param(url, "nonce").as_deref(),
                Some(nonce.secret().as_str())
            );
            assert_eq!(
                query_param(url, "code_challenge").as_deref(),
                Some(s256_challenge(verifier.secret()).as_str()),
                "chaque URL porte son propre challenge"
            );
        }
    }

    /// `Scenario` 11 : « Échange de code réussi rend le jeton brut, les claims et les groups »
    /// (+ trace absente, + `code_verifier` sur le fil, nonce/verifier du couple `authorization_url`).
    #[tokio::test]
    async fn exchange_code_happy_path_returns_preferred_username_claim() {
        let client_id = "test-client";
        let client_secret = "test-secret";
        let idp = MockIdP::start(IdPPlan::default());
        let (client, nonce, verifier) = client_with_pair(&idp, client_id, client_secret).await;

        let token = build_signed_token(
            &ClaimsFixture::default(),
            &idp.issuer,
            client_id,
            nonce.secret(),
            client_secret,
        );
        idp.set_id_token(token.clone());

        let (guard, captured) = capture_traces();
        let result = client
            .exchange_code("the-code", &nonce, &verifier)
            .await
            .expect("exchange succeeds against the mock IdP");
        drop(guard);

        assert!(
            captured.lines().is_empty(),
            "l'échange réussi ne pose aucune trace : {:?}",
            captured.lines()
        );
        assert_eq!(
            result.identity.id_token, token,
            "le JWT brut est restitué verbatim"
        );
        assert_eq!(result.identity.subject, "user-1");
        assert_eq!(
            result.identity.email.as_deref(),
            Some("user-1@example.com"),
            "`email` inchangé à côté du nouveau champ"
        );
        assert_eq!(
            result.identity.preferred_username.as_deref(),
            Some("alice"),
            "Scenario « Échange de code réussi » (2026-09-27) : claim standard restitué en `Some`"
        );
        assert_eq!(result.groups, vec!["admin".to_string(), "editors".to_string()]);

        let authorization = idp.token_authorization();
        assert!(
            authorization.starts_with("Basic "),
            "le POST /token porte l'authentification Basic : {authorization:?}"
        );
        let body = idp.token_body();
        assert!(body.contains("grant_type=authorization_code"), "{body}");
        assert!(body.contains("code=the-code"), "{body}");
        assert!(
            body.contains("redirect_uri=http%3A%2F%2Fapp.local%2Fcallback"),
            "le corps porte le redirect_uri configuré : {body}"
        );
        assert!(
            body.contains(&format!("code_verifier={}", verifier.secret())),
            "PKCE S256 (arbitré 2026-09-27) : le corps porte le code_verifier : {body}"
        );
    }

    /// `Scenario` 12 : « Verifier PKCE divergent rejette l'échange » — le fournisseur conforme
    /// vérifie le challenge ; le rejet remonte par le chemin `009`.
    #[tokio::test]
    async fn divergent_pkce_verifier_is_rejected_by_the_provider() {
        let client_id = "test-client";
        let client_secret = "test-secret";
        let idp = MockIdP::start(IdPPlan::default());
        let (client, nonce, verifier) = client_with_pair(&idp, client_id, client_secret).await;
        idp.expect_code_verifier(verifier.secret());
        let token = build_signed_token(
            &ClaimsFixture::default(),
            &idp.issuer,
            client_id,
            nonce.secret(),
            client_secret,
        );
        idp.set_id_token(token);

        let divergent = PkceCodeVerifier::new("un-verifier-qui-ne-correspond-pas-au-challenge".to_string());
        let err = client
            .exchange_code("the-code", &nonce, &divergent)
            .await
            .contract_err("a provider that checked the challenge must reject the divergent verifier");
        assert!(
            err.to_string()
                .starts_with("MRD-AUTH-003: OIDC error: MRD-AUTH-009: OIDC code exchange failed: "),
            "rejet du fournisseur sur le chemin d'échec d'échange : {err}"
        );
    }

    /// `Scenario` 13 : « Fournisseur sans claim `preferred_username` rend None ».
    #[tokio::test]
    async fn provider_without_preferred_username_claim_yields_none() {
        let client_id = "test-client";
        let client_secret = "test-secret";
        let idp = MockIdP::start(IdPPlan::default());
        let (client, nonce, verifier) = client_with_pair(&idp, client_id, client_secret).await;

        let token = build_signed_token(
            &ClaimsFixture {
                preferred_username: false,
                ..ClaimsFixture::default()
            },
            &idp.issuer,
            client_id,
            nonce.secret(),
            client_secret,
        );
        assert!(
            decode_jwt_payload(&token).get("preferred_username").is_none(),
            "fixture : le claim est absent de l'id_token servi"
        );
        idp.set_id_token(token);

        let result = client
            .exchange_code("the-code", &nonce, &verifier)
            .await
            .expect("exchange succeeds even without the standard claim");

        assert_eq!(result.identity.subject, "user-1");
        assert_eq!(
            result.identity.preferred_username, None,
            "Scenario « Fournisseur sans claim preferred_username » (2026-09-27) : même posture que l'absence d'`email`"
        );
    }

    /// `Scenario` 14 : « Réponse de tokens sans `id_token`, rejet 010 » — charge utile littérale.
    #[tokio::test]
    async fn token_response_without_id_token_rejected_with_exact_010_payload() {
        let idp = MockIdP::start(IdPPlan {
            token_mode: TokenMode::NoIdToken,
            ..IdPPlan::default()
        });
        let (client, nonce, verifier) = client_with_pair(&idp, "cid", "secret").await;

        let err = client
            .exchange_code("the-code", &nonce, &verifier)
            .await
            .contract_err("a token response without id_token must be refused");
        assert_eq!(oidc_payload(&err), "MRD-AUTH-010: no id_token in response");
    }

    /// `Scenario` 15 : « Nonce du `id_token` divergent du nonce attendu, rejet 011 ».
    #[tokio::test]
    async fn divergent_nonce_claim_rejected_011() {
        let idp = MockIdP::start(IdPPlan::default());
        let (client, nonce, verifier) = client_with_pair(&idp, "test-client", "test-secret").await;
        let token = build_signed_token(
            &ClaimsFixture {
                nonce: Some("un-nonce-de-dubble-login-voisin".to_string()),
                ..ClaimsFixture::default()
            },
            &idp.issuer,
            "test-client",
            nonce.secret(),
            "test-secret",
        );
        idp.set_id_token(token);

        let err = client
            .exchange_code("the-code", &nonce, &verifier)
            .await
            .contract_err("divergent nonce must be refused");
        assert!(
            oidc_payload(&err).starts_with("MRD-AUTH-011: token verification failed: "),
            "tout motif de rejet se confond en 011 : {}",
            oidc_payload(&err)
        );
    }

    /// `Scenario` 16 : « `id_token` expiré, rejet 011 sans tolérance d'horloge ».
    #[tokio::test]
    async fn expired_id_token_rejected_011_without_clock_leeway() {
        let idp = MockIdP::start(IdPPlan::default());
        let (client, nonce, verifier) = client_with_pair(&idp, "test-client", "test-secret").await;
        let token = build_signed_token(
            &ClaimsFixture {
                exp_secs_from_now: -60,
                ..ClaimsFixture::default()
            },
            &idp.issuer,
            "test-client",
            nonce.secret(),
            "test-secret",
        );
        idp.set_id_token(token);

        let err = client
            .exchange_code("the-code", &nonce, &verifier)
            .await
            .contract_err("expired token must be refused");
        assert!(
            oidc_payload(&err).starts_with("MRD-AUTH-011: token verification failed: "),
            "{}",
            oidc_payload(&err)
        );
    }

    /// `Scenario` 17 : « `id_token` à signature invalide, rejet 011 » — et la cause crypto
    /// (`CryptoError`) n'arme pas le signal de rotation, seule `NoMatchingKey` le fait.
    #[tokio::test]
    async fn invalid_signature_rejected_011_and_does_not_arm_rotation_signal() {
        let idp = MockIdP::start(IdPPlan::default());
        let (client, nonce, verifier) = client_with_pair(&idp, "test-client", "test-secret").await;
        let token = build_signed_token(
            &ClaimsFixture::default(),
            &idp.issuer,
            "test-client",
            nonce.secret(),
            "un-autre-secret-qui-ne-verifie-pas",
        );
        idp.set_id_token(token);

        let err = client
            .exchange_code("the-code", &nonce, &verifier)
            .await
            .contract_err("a bad MAC is a refusal, never a silent tolerance");
        assert!(
            oidc_payload(&err).starts_with("MRD-AUTH-011: token verification failed: "),
            "{}",
            oidc_payload(&err)
        );
        assert!(
            !client.jwks_rotation_needed(),
            "un échec de signature n'est pas un indice de JWKS périmé"
        );
    }

    /// `Scenario` 18 : « `id_token` destiné à un autre client, rejet 011 ».
    #[tokio::test]
    async fn id_token_for_another_client_audience_rejected_011() {
        let idp = MockIdP::start(IdPPlan::default());
        let (client, nonce, verifier) = client_with_pair(&idp, "test-client", "test-secret").await;
        let token = build_signed_token(
            &ClaimsFixture {
                aud: Some("autre-client".to_string()),
                ..ClaimsFixture::default()
            },
            &idp.issuer,
            "test-client",
            nonce.secret(),
            "test-secret",
        );
        idp.set_id_token(token);

        let err = client
            .exchange_code("the-code", &nonce, &verifier)
            .await
            .contract_err("aud without our client_id must be refused");
        assert!(
            oidc_payload(&err).starts_with("MRD-AUTH-011: token verification failed: "),
            "{}",
            oidc_payload(&err)
        );
    }

    /// `Scenario` 19 : « `id_token` d'un autre issuer, rejet 011 » — et le message ne révèle rien
    /// du token au-delà du motif de la librairie.
    #[tokio::test]
    async fn id_token_from_another_issuer_rejected_011() {
        let idp = MockIdP::start(IdPPlan::default());
        let (client, nonce, verifier) = client_with_pair(&idp, "test-client", "test-secret").await;
        let token = build_signed_token(
            &ClaimsFixture {
                iss: Some("http://issuer-etranger.invalid".to_string()),
                ..ClaimsFixture::default()
            },
            &idp.issuer,
            "test-client",
            nonce.secret(),
            "test-secret",
        );
        idp.set_id_token(token);

        let err = client
            .exchange_code("the-code", &nonce, &verifier)
            .await
            .contract_err("foreign issuer must be refused");
        let payload = oidc_payload(&err);
        assert!(
            payload.starts_with("MRD-AUTH-011: token verification failed: "),
            "{payload}"
        );
        assert!(
            !payload.contains("user-1@example.com"),
            "le message ne divulgue pas le contenu du token : {payload}"
        );
    }

    /// `Scenario` 20 : « Réponse d'erreur du endpoint de tokens : 009 en tête du payload,
    /// trace 009, 502 sur le fil ».
    #[tokio::test]
    async fn token_endpoint_error_carries_009_payload_and_trace_and_renders_502() {
        use axum::response::IntoResponse;

        let idp = MockIdP::start(IdPPlan {
            token_mode: TokenMode::OAuthError,
            ..IdPPlan::default()
        });
        let (client, nonce, verifier) = client_with_pair(&idp, "test-client", "test-secret").await;

        let (guard, captured) = capture_traces();
        let err = client
            .exchange_code("the-code", &nonce, &verifier)
            .await
            .contract_err("400 invalid_grant must be refused");
        drop(guard);

        let display = err.to_string();
        assert!(
            display.starts_with("MRD-AUTH-003: OIDC error: MRD-AUTH-009: OIDC code exchange failed: "),
            "double préfixe 003+009 (arbitré 2026-09-27) : {display}"
        );
        assert!(
            display.len() > "MRD-AUTH-003: OIDC error: MRD-AUTH-009: OIDC code exchange failed: ".len(),
            "le message amont nu suit le préfixe : {display}"
        );
        let traces = captured.lines();
        assert!(
            traces
                .iter()
                .any(|line| line.contains("MRD-AUTH-009: OIDC code exchange failed")),
            "la trace accompagne le préfixe, elle ne le remplace pas : {traces:?}"
        );

        let rendered = err.into_response();
        assert_eq!(
            rendered.status(),
            axum::http::StatusCode::BAD_GATEWAY,
            "contrat de rendering de ./error.rs : 502"
        );
    }

    /// `Scenario` 21 : « Réponse 3xx du endpoint de tokens : non suivie, même échec qu'une 400 ».
    #[tokio::test]
    async fn token_endpoint_redirect_is_not_followed_and_fails_as_009() {
        let (honeypot, hits) = start_honeypot();
        let idp = MockIdP::start(IdPPlan {
            token_mode: TokenMode::Redirect(honeypot),
            ..IdPPlan::default()
        });
        let (client, nonce, verifier) = client_with_pair(&idp, "test-client", "test-secret").await;

        let err = client
            .exchange_code("the-code", &nonce, &verifier)
            .await
            .contract_err("a 302 left un-followed is a non-success response");
        assert!(
            err.to_string()
                .starts_with("MRD-AUTH-003: OIDC error: MRD-AUTH-009: OIDC code exchange failed: "),
            "même chemin que la réponse-400 : {err}"
        );
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "Policy::none : la cible de Location n'a jamais été visitée"
        );
    }

    /// `Scenario` 22 : « Provider sans `token_endpoint` : construction acceptée, échange refusé
    /// en configuration » (009 config error + trace, aucun appel /token).
    #[tokio::test]
    async fn provider_without_token_endpoint_refuses_exchange_as_config_error_009() {
        let idp = MockIdP::start(IdPPlan {
            token_endpoint: false,
            ..IdPPlan::default()
        });
        let (client, nonce, verifier) = client_with_pair(&idp, "test-client", "test-secret").await;

        let (guard, captured) = capture_traces();
        let err = client
            .exchange_code("the-code", &nonce, &verifier)
            .await
            .contract_err("missing token_endpoint must refuse the exchange locally");
        drop(guard);

        assert!(
            err.to_string()
                .starts_with("MRD-AUTH-003: OIDC error: MRD-AUTH-009: OIDC code exchange config error: "),
            "ConfigurationError oauth2 derrière le préfixe 003+009 : {err}"
        );
        let traces = captured.lines();
        assert!(
            traces
                .iter()
                .any(|line| line.contains("MRD-AUTH-009: OIDC code exchange config error")),
            "{traces:?}"
        );
        assert!(
            !idp.requests().contains(&"/token".to_string()),
            "le refus est purement local : {:?}",
            idp.requests()
        );
    }

    /// `Scenario` 23 : « Timeout de connexion configuré déclenche 005 sans attente indéfinie ».
    /// L'`issuer` pointe un trou noir réseau (`192.0.2.0/24`, TEST-NET-1, non routable) : selon
    /// l'environnement le SYN se perd (timeout ~200ms) ou la route manque immédiatement — le
    /// contrat tenu ici est « erreur 005, main rendue en durée bornée », jamais l'attente infinie.
    #[tokio::test]
    async fn configured_connect_timeout_bounds_discovery_failure_005() {
        let mut config = OidcConfig {
            issuer_url: "http://192.0.2.1".to_string(),
            client_id: "cid".to_string(),
            client_secret: None,
            redirect_url: "http://app.local/callback".to_string(),
            scopes: vec![],
            ca_cert: None,
            connect_timeout: std::time::Duration::from_millis(200),
            timeout: std::time::Duration::from_secs(2),
        };

        let started = std::time::Instant::now();
        let err = OidcClient::new(&config)
            .await
            .contract_err("black-holed issuer must fail");
        let elapsed = started.elapsed();

        let payload = oidc_payload(&err);
        assert!(
            payload.starts_with("MRD-AUTH-005: OIDC discovery failed: "),
            "{payload}"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(10),
            "retour borné (connect_timeout 200ms configuré), pas d'attente indéfinie : {elapsed:?}"
        );

        // Inverse : sans le timeout configuré ci-dessus, le champ `timeout` seul bornerait —
        // vérification que les deux champs voyagent bien vers le ClientBuilder (test 25).
        config.connect_timeout = std::time::Duration::from_secs(5);
        let started = std::time::Instant::now();
        let still_bounded = OidcClient::new(&config)
            .await
            .contract_err("still bounded by timeout 2s");
        assert!(
            oidc_payload(&still_bounded).starts_with("MRD-AUTH-005: "),
            "borne en timeout total : {:?}",
            started.elapsed()
        );
    }

    /// `Scenario` 24 : « Timeout total configuré déclenche 009 sur un endpoint de tokens qui
    /// ne répond jamais ».
    #[tokio::test]
    async fn configured_total_timeout_bounds_token_exchange_009() {
        let idp = MockIdP::start(IdPPlan {
            token_mode: TokenMode::Silent,
            ..IdPPlan::default()
        });
        let mut config = idp.config("test-client", "test-secret");
        config.connect_timeout = std::time::Duration::from_secs(5);
        config.timeout = std::time::Duration::from_millis(200);
        let client = OidcClient::new(&config).await.expect("discovery succeeds fast");
        let (_url, _csrf, nonce, verifier) = client.authorization_url();

        let started = std::time::Instant::now();
        let err = client
            .exchange_code("the-code", &nonce, &verifier)
            .await
            .contract_err("an endpoint that never answers must time out");
        let elapsed = started.elapsed();

        assert!(
            err.to_string()
                .starts_with("MRD-AUTH-003: OIDC error: MRD-AUTH-009: OIDC code exchange failed: "),
            "{err}"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "timeout 200ms posé : la main revient bornée : {elapsed:?}"
        );
    }

    /// `Scenario` 25 : « Timeouts non configurés retombent sur les défauts » — depuis
    /// l'arbitrage `config.sdd` 2026-09-27, les deux champs `Duration` sont OBLIGATOIRES dans
    /// `OidcConfig` (pas de `Default`) : un `OidcConfig` « qui ne les fixe pas » est
    /// infalsifiable par le type ; les défauts 5s/15s sont ceux que l'app pose, et ce test prouve
    /// que ces valeurs-là sont lues et posées sans rien casser (construction + échange complets
    /// sous 5s/15s). L'épreuve comportementale du câblage `ClientBuilder` est portée par les
    /// scenarios 23/24 aux valeurs 200ms.
    #[tokio::test]
    async fn documented_default_timeouts_flow_from_config_into_working_client() {
        let idp = MockIdP::start(IdPPlan::default());
        let config = OidcConfig {
            issuer_url: idp.issuer.clone(),
            client_id: "test-client".to_string(),
            client_secret: Some("test-secret".to_string()),
            redirect_url: "http://app.local/callback".to_string(),
            scopes: vec!["email".to_string()],
            ca_cert: None,
            connect_timeout: std::time::Duration::from_secs(5),
            timeout: std::time::Duration::from_secs(15),
        };

        let client = OidcClient::new(&config)
            .await
            .expect("5s/15s build a working bounded client");
        let (_url, _csrf, nonce, verifier) = client.authorization_url();
        let token = build_signed_token(
            &ClaimsFixture::default(),
            &idp.issuer,
            "test-client",
            nonce.secret(),
            "test-secret",
        );
        idp.set_id_token(token);
        assert!(
            client.exchange_code("the-code", &nonce, &verifier).await.is_ok(),
            "les défauts 5s/15s ne coupent pas un échange local"
        );
    }

    /// `Scenario` 26 : « Clé JWKS introuvable arme le signal de rotation » — variante précise
    /// `SignatureVerification(NoMatchingKey)` : `id_token` `RS256` dont aucun clé du JWKS
    /// (vide) ne porte le `kid`.
    #[tokio::test]
    async fn jwks_key_not_found_rejects_011_and_arms_rotation_signal() {
        let plan = IdPPlan {
            algs_json: r#""HS256","RS256""#.to_string(),
            ..IdPPlan::default()
        };
        let idp = MockIdP::start(plan);
        let (client, nonce, verifier) = client_with_pair(&idp, "test-client", "test-secret").await;
        assert!(
            !client.jwks_rotation_needed(),
            "le signal est désarmé à la construction"
        );

        let token = build_signed_token(
            &ClaimsFixture {
                alg: "RS256",
                kid: Some("kid-apres-rotation".to_string()),
                ..ClaimsFixture::default()
            },
            &idp.issuer,
            "test-client",
            nonce.secret(),
            "test-secret",
        );
        idp.set_id_token(token);

        let err = client
            .exchange_code("the-code", &nonce, &verifier)
            .await
            .contract_err("no key matches the kid: verification must fail");
        assert!(
            oidc_payload(&err).starts_with("MRD-AUTH-011: "),
            "{}",
            oidc_payload(&err)
        );
        assert!(
            client.jwks_rotation_needed(),
            "la cause « clé/kid introuvable » arme le signal de readiness"
        );
    }

    /// `Scenario` 26 (seconde branche) : « une claim expirée ne l'arme pas » — l'expiration est
    /// un rejet de claim ordinaire, pas un indice de JWKS périmé.
    #[tokio::test]
    async fn expired_token_rejection_does_not_arm_rotation_signal() {
        let plan = IdPPlan {
            algs_json: r#""HS256","RS256""#.to_string(),
            ..IdPPlan::default()
        };
        let idp = MockIdP::start(plan);
        let (client, nonce, verifier) = client_with_pair(&idp, "test-client", "test-secret").await;

        let token = build_signed_token(
            &ClaimsFixture {
                exp_secs_from_now: -60,
                ..ClaimsFixture::default()
            },
            &idp.issuer,
            "test-client",
            nonce.secret(),
            "test-secret",
        );
        idp.set_id_token(token);

        let err = client
            .exchange_code("the-code", &nonce, &verifier)
            .await
            .contract_err("expired token must be refused");
        assert!(
            oidc_payload(&err).starts_with("MRD-AUTH-011: "),
            "{}",
            oidc_payload(&err)
        );
        assert!(
            !client.jwks_rotation_needed(),
            "l'expiration n'arme jamais le signal de rotation"
        );
    }

    /// `Scenario` 27 : « Claim groups filtre silencieusement les entrées non-string ».
    #[test]
    fn groups_claim_filters_non_string_entries_in_order() {
        use base64::Engine;
        let payload =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(r#"{"groups":["admin",42,"editors"]}"#);
        let jwt = format!("header.{payload}.sig");
        assert_eq!(extract_groups_claim(&jwt), vec!["admin", "editors"]);
    }

    /// `Scenario` 28 : « Claim groups non-array, liste vide ».
    #[test]
    fn groups_claim_non_array_yields_empty_list() {
        use base64::Engine;
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(r#"{"groups":"admin"}"#);
        let jwt = format!("header.{payload}.sig");
        assert!(extract_groups_claim(&jwt).is_empty());
    }

    /// `Scenario` 29 : « JWT hors trois segments, liste vide » — deux et quatre segments
    /// (le quatrième protège l'indexation de `parts[1]`).
    #[test]
    fn jwt_without_three_segments_yields_empty_list() {
        assert!(extract_groups_claim("a.b").is_empty(), "deux segments");
        assert!(extract_groups_claim("a.b.c.d").is_empty(), "quatre segments");
    }

    // ——— Les trois tests inline préexistants (couvrent le cas `not-a-jwt` du Scenario 29 et
    // ——— les dégradations silencieuses du claim `groups`) ———

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

    /// `Scenario` 30 : « Mock de test sans réseau : URL statique, couples frais, échange
    /// volontairement paniquant » (`catch_unwind` sur l'`unimplemented!` nommé).
    #[test]
    fn mock_default_mode_serves_static_url_with_fresh_secrets_and_panics_on_exchange() {
        let mock = MockOidcClient::new();
        let (url1, csrf1, nonce1, verifier1) = mock.authorization_url();
        let (url2, csrf2, nonce2, verifier2) = mock.authorization_url();

        assert_eq!(url1.as_str(), "https://issuer.example.com/authorize");
        assert_eq!(url2.as_str(), "https://issuer.example.com/authorize");

        let first = [csrf1.secret(), nonce1.secret(), verifier1.secret()];
        let second = [csrf2.secret(), nonce2.secret(), verifier2.secret()];
        for secret in first.iter().chain(second.iter()) {
            assert!(!secret.is_empty());
        }
        for a in first {
            for b in second {
                assert_ne!(a, b, "couples frais par appel, sans E/S");
            }
        }

        // Mode panique : assertion d'absence des tests de ./mod.rs, ./dual.rs, /src/rest/*.rs.
        let quiet_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("test runtime builds");
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.block_on(mock.exchange_code(
                "n-importe",
                &Nonce::new("n-importe".to_string()),
                &PkceCodeVerifier::new("n-importe".to_string()),
            ))
        }));
        std::panic::set_hook(quiet_hook);
        assert!(
            outcome.is_err(),
            "le mode par défaut doit paniquer via unimplemented! nommé"
        );
    }

    /// `Scenario` 31 : « Mock configuré pour un échange réussi rend le résultat fourni »
    /// (mode configurable arbitré 2026-09-27, verbatim, sans panique).
    #[tokio::test]
    async fn mock_configured_mode_returns_the_provided_login_result_verbatim() {
        let provided = OidcLoginResult {
            identity: OidcIdentity {
                id_token: "a.b.c".to_string(),
                subject: "alice".to_string(),
                email: Some("alice@example.com".to_string()),
                preferred_username: Some("alice".to_string()),
            },
            groups: vec!["editors".to_string()],
        };
        let mock = MockOidcClient::with_login_result(provided);

        let result = mock
            .exchange_code(
                "n-importe",
                &Nonce::new("n-importe".to_string()),
                &PkceCodeVerifier::new("n-importe".to_string()),
            )
            .await
            .expect("le mode configurable rend Ok sans panique");

        assert_eq!(result.identity.subject, "alice");
        assert_eq!(result.identity.id_token, "a.b.c");
        assert_eq!(result.identity.email.as_deref(), Some("alice@example.com"));
        assert_eq!(result.identity.preferred_username.as_deref(), Some("alice"));
        assert_eq!(result.groups, vec!["editors".to_string()]);
    }
}
