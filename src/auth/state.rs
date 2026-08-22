use std::sync::Arc;

use cookie::Key;

use crate::auth::oidc::OidcClientTrait;

/// État minimal requis par le sous-routeur `auth_router` et l'extracteur `AuthUser`.
/// L'app consommatrice compose son propre `AppState` autour (pattern `FromRef` d'axum) —
/// miryad-core n'impose aucune structure d'état concrète.
#[derive(Clone)]
pub struct MiryadAuthState {
    pub oidc_client: Arc<dyn OidcClientTrait>,
    pub cookie_key: Key,
    pub post_login_redirect: String,
    pub post_logout_redirect: String,
}
