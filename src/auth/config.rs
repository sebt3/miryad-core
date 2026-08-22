/// Configuration du client OIDC — fournie par l'application consommatrice, jamais lue par
/// miryad-core depuis l'environnement ou un fichier (ça reste la responsabilité de l'app).
pub struct OidcConfig {
    pub issuer_url: String,
    pub client_id: String,
    pub client_secret: String,
    pub redirect_url: String,
    pub scopes: Vec<String>,
    /// Certificat CA additionnel, contenu PEM (pas un chemin de fichier).
    pub ca_cert: Option<String>,
    /// Où rediriger après un login réussi.
    pub post_login_redirect: String,
    /// Où rediriger après un logout.
    pub post_logout_redirect: String,
}
