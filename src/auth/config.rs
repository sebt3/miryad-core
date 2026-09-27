/// Configuration du client OIDC — fournie par l'application consommatrice, jamais lue par
/// miryad-core depuis l'environnement ou un fichier (ça reste la responsabilité de l'app).
///
/// Huit champs, aucun constructeur ni validation ici : toute valeur invalide (URL, PEM, secret)
/// n'émerge qu'à la construction du client dans [`oidc`](super::oidc), avec les codes
/// `MRD-AUTH-004` à `MRD-AUTH-008`.
#[derive(Clone)]
pub struct OidcConfig {
    pub issuer_url: String,
    pub client_id: String,
    pub client_secret: Option<String>,
    pub redirect_url: String,
    pub scopes: Vec<String>,
    /// Certificat CA additionnel, contenu PEM (pas un chemin de fichier).
    pub ca_cert: Option<String>,
    /// Bornes de la phase de connexion (TCP+TLS) du client HTTP interne — posée sur
    /// `reqwest::ClientBuilder::connect_timeout` (`oidc::build_http_client`). Défaut
    /// applicatif recommandé : `5s`.
    pub connect_timeout: std::time::Duration,
    /// Bornes de la requête HTTP complète (discovery, JWKS, échange de code) du client HTTP
    /// interne — posée sur `reqwest::ClientBuilder::timeout` (`oidc::build_http_client`).
    /// Défaut applicatif recommandé : `15s`.
    pub timeout: std::time::Duration,
}

/// `Debug` manuel arbitré le 2026-09-27 : tous les champs verbatim SAUF `client_secret`, rendu
/// `Some("[REDACTED]")` quand présent et `None` quand absent — un `{:?}` accidentel ne doit
/// jamais exposer le secret (`config.sdd` `Must`).
impl std::fmt::Debug for OidcConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OidcConfig")
            .field("issuer_url", &self.issuer_url)
            .field("client_id", &self.client_id)
            .field(
                "client_secret",
                &self.client_secret.as_ref().map(|_| "[REDACTED]"),
            )
            .field("redirect_url", &self.redirect_url)
            .field("scopes", &self.scopes)
            .field("ca_cert", &self.ca_cert)
            .field("connect_timeout", &self.connect_timeout)
            .field("timeout", &self.timeout)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fixture : les huit champs de la table de `Must` (config.sdd, révisée 2026-09-27),
    /// `client_secret` en `Some`. Les URL finissent en `.invalid` (RFC 6761 — jamais résolu).
    fn config_with_secret(secret: &str) -> OidcConfig {
        OidcConfig {
            issuer_url: "https://issuer.invalid".to_string(),
            client_id: "cid".to_string(),
            client_secret: Some(secret.to_string()),
            redirect_url: "https://app.invalid/callback".to_string(),
            scopes: vec!["email".to_string()],
            ca_cert: None,
            connect_timeout: std::time::Duration::from_secs(5),
            timeout: std::time::Duration::from_secs(15),
        }
    }

    /// `Scenario` : « construction par littéral des huit champs obligatoires » — le littéral
    /// énumère les huit champs (tout champ manquant casse la compilation du test lui-même,
    /// `#[derive]`-`Default`-absent étant affirmé par la construction de la fixture) et la
    /// lecture par emprunt rend exactement la valeur déposée.
    #[test]
    fn literal_of_eight_fields_round_trips_every_value() {
        let config = config_with_secret("s3cr3t");
        assert_eq!(config.issuer_url, "https://issuer.invalid");
        assert_eq!(config.client_id, "cid");
        assert_eq!(config.client_secret.as_deref(), Some("s3cr3t"));
        assert_eq!(config.redirect_url, "https://app.invalid/callback");
        assert_eq!(config.scopes, vec!["email".to_string()]);
        assert_eq!(config.ca_cert, None);
        assert_eq!(config.connect_timeout, std::time::Duration::from_secs(5));
        assert_eq!(config.timeout, std::time::Duration::from_secs(15));
    }

    /// `Scenario` : « Clone produit une copie indépendante, Debug rédige le secret » — les deux
    /// sorties `{:?}` contiennent `client_secret: Some("[REDACTED]")` et jamais `s3cr3t`, et la
    /// mutation d'un champ du clone ne touche pas l'original.
    #[test]
    fn clone_is_independent_and_debug_redacts_secret() {
        let original = config_with_secret("s3cr3t");
        let mut cloned = original.clone();
        cloned.issuer_url.push_str("-mutated");

        let original_out = format!("{original:?}");
        let cloned_out = format!("{cloned:?}");
        for rendered in [original_out.as_str(), cloned_out.as_str()] {
            assert!(
                rendered.contains(r#"client_secret: Some("[REDACTED]")"#),
                "le Debug doit rédiger le secret : {rendered}"
            );
            assert!(
                !rendered.contains("s3cr3t"),
                "le secret ne doit jamais paraître dans un `{{:?}}` : {rendered}"
            );
        }
        assert_eq!(
            original.issuer_url, "https://issuer.invalid",
            "le clone est une copie indépendante (arbitré 2026-09-27)"
        );
    }

    /// `Scenario` : « Debug sur `client_secret` absent reste None ».
    #[test]
    fn debug_with_absent_client_secret_renders_none() {
        let config = OidcConfig {
            client_secret: None,
            ..config_with_secret("unused")
        };
        let rendered = format!("{config:?}");
        assert!(
            rendered.contains("client_secret: None"),
            "None doit rester rendu None, sans rédaction : {rendered}"
        );
    }

    /// `Scenario` : « la configuration voyage entre tâches par sa nature Send et Sync » —
    /// borne de compilation instantiée sur `OidcConfig`.
    #[test]
    fn oidc_config_satisfies_send_and_sync() {
        fn assert_send_sync_static<T: Send + Sync + 'static>() {}
        assert_send_sync_static::<OidcConfig>();
    }
}
