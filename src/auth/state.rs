use std::sync::Arc;

use cookie::Key;
use sea_orm::DatabaseConnection;

use crate::auth::oidc::OidcClientTrait;

/// État minimal requis par le sous-routeur `auth_router` et les extracteurs `AuthUser`/
/// `AuthPrincipal`. L'app consommatrice compose son propre `AppState` autour (pattern `FromRef`
/// d'axum) — miryad-core n'impose aucune structure d'état concrète.
#[derive(Clone)]
pub struct MiryadAuthState {
    pub oidc_client: Arc<dyn OidcClientTrait>,
    pub cookie_key: Key,
    pub post_login_redirect: String,
    pub post_logout_redirect: String,
    /// Requis pour valider un token API (feature 2b) — non utilisé par le flow cookie seul (2a).
    pub db: DatabaseConnection,
}
