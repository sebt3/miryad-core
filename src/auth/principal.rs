/// Identité unifiée d'une requête authentifiée, quelle que soit la source (cookie de session ou
/// token API) — c'est ce type que REST/GraphQL/MCP consomment (feature 4+), pas `AuthUser` qui
/// reste spécifique au flow navigateur (feature 2a).
#[derive(Debug, Clone)]
pub struct AuthPrincipal {
    pub subject: String,
    pub email: Option<String>,
    pub source: PrincipalSource,
}

#[derive(Debug, Clone)]
pub enum PrincipalSource {
    Session { id_token: String },
    ApiToken { token_id: i32 },
}
