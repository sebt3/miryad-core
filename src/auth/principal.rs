/// Identité unifiée d'une requête authentifiée, quelle que soit la source (cookie de session ou
/// token API) — c'est ce type que REST/GraphQL/MCP consomment (feature 4+), pas `AuthUser` qui
/// reste spécifique au flow navigateur (feature 2a).
///
/// `Debug` est un `impl` manuel (arbitré 2026-09-27) : même sortie que le dérivé, sauf
/// l'`id_token` de session rédigé `"[REDACTED]"` — un `{:?}` accidentel ne doit jamais
/// reproduire le JWT brut.
#[derive(Clone)]
pub struct AuthPrincipal {
    /// Claim `sub` de l'`id_token` (session) ou de la ligne `miryad_api_tokens` (token API) :
    /// clé d'identité et de propriété, relue par `resolve_user` et le self-service de
    /// `rest/tokens.rs`. Type passoire à producteurs de confiance (arbitré 2026-09-27) : vide,
    /// casse, espaces et unicité ne sont contrôlés ni ici ni chez les deux producteurs
    /// (`auth/dual.rs`, `auth/token.rs`).
    pub subject: String,
    /// Claim `email` de l'`id_token` au login, `None` pour tout token API (métadonnée de la
    /// ligne `users`, jamais une credential — arbitré 2026-09-27). Lu via `as_deref`.
    pub email: Option<String>,
    /// Claim OIDC standard `preferred_username` — identifiant nommé garanti unique par le
    /// fournisseur (contrairement à `email`), posé sur le chemin session, `None` sur le chemin
    /// token API.
    pub preferred_username: Option<String>,
    /// Source d'authentification de la requête : c'est la seule divergence visible entre
    /// appelants REST/GraphQL/MCP, qui consomment le même type par ailleurs.
    pub source: PrincipalSource,
}

impl std::fmt::Debug for AuthPrincipal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthPrincipal")
            .field("subject", &self.subject)
            .field("email", &self.email)
            .field("preferred_username", &self.preferred_username)
            .field("source", &self.source)
            .finish()
    }
}

/// Charge utile de la source d'authentification derrière un [`AuthPrincipal`] : le JWT brut de
/// session ou la clé primaire du token API. Aucun des deux champs n'est lu par la crate — ils
/// sont portés pour les apps consommatrices (audit par `token_id`, claims tirés d'`id_token`).
///
/// `Debug` est un `impl` manuel (arbitré 2026-09-27) : même sortie que le dérivé, sauf
/// l'`id_token` de la variante [`PrincipalSource::Session`] rendu `"[REDACTED]"` — cohérent
/// avec la rédaction de `client_secret` sur `OidcConfig`. `token_id` n'est pas un secret, il
/// reste verbatim.
#[derive(Clone)]
pub enum PrincipalSource {
    /// Session navigateur issue du cookie chiffré : l'`id_token` OIDC brut, sans revalidation
    /// ici (producteur : `auth/dual.rs` via `extract_session`).
    Session {
        /// `id_token` OIDC brut (`header.payload.signature`, ou chaîne vide en fixture) ;
        /// rédigé `"[REDACTED]"` par le `Debug` manuel.
        id_token: String,
    },
    /// Token API validé par `auth/token.rs` : les champs `email` et `preferred_username` de
    /// l'[`AuthPrincipal`] porteur sont alors `None` codé en dur.
    ApiToken {
        /// Clé primaire `i32` de la ligne `miryad_api_tokens` (type figé par l'égalité avec
        /// `Model::id`) ; verbatim dans le `Debug` — pas un secret.
        token_id: i32,
    },
}

impl std::fmt::Debug for PrincipalSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // `id_token` rédigé (arbitré 2026-09-27, principal.sdd) : jamais le JWT brut.
            Self::Session { .. } => f
                .debug_struct("Session")
                .field("id_token", &"[REDACTED]")
                .finish(),
            Self::ApiToken { token_id } => f.debug_struct("ApiToken").field("token_id", token_id).finish(),
        }
    }
}

#[cfg(test)]
mod tests {
    // Zéro ligne `use` ici aussi — contrat `Done when` de `principal.sdd` (« le fichier ne
    // contient toujours aucune @use ») : tout est qualifié par `super::`.

    /// `Scenario` : « Construction session : quatre champs publics lisibles à leurs
    /// valeurs exactes » — les champs publics sont le contrat de lecture, sans accesseur.
    #[test]
    fn session_fields_readable_verbatim() {
        let principal = super::AuthPrincipal {
            subject: "alice".to_string(),
            email: Some("alice@example.org".to_string()),
            preferred_username: Some("alice".to_string()),
            source: super::PrincipalSource::Session {
                id_token: "jwt-brut".to_string(),
            },
        };

        assert_eq!(principal.subject, "alice");
        assert_eq!(principal.email.as_deref(), Some("alice@example.org"));
        assert_eq!(principal.preferred_username.as_deref(), Some("alice"));
        let id_token = match &principal.source {
            super::PrincipalSource::Session { id_token } => id_token,
            super::PrincipalSource::ApiToken { .. } => panic!("source construite en Session"),
        };
        assert_eq!(id_token, "jwt-brut");
    }

    /// `Scenario` : « Construction token API : `email` et `preferred_username` `None` et
    /// `token_id` conservé » — la forme exacte de `validate_token`. `email` `None` n'affirme
    /// pas l'absence de login : c'est la valeur posée par le producteur, `preferred_username`
    /// `None` de même.
    #[test]
    fn token_api_source_none_email_none_preferred_username() {
        let principal = super::AuthPrincipal {
            subject: "user-123".to_string(),
            email: None,
            preferred_username: None,
            source: super::PrincipalSource::ApiToken { token_id: 7 },
        };

        assert_eq!(principal.subject, "user-123");
        assert_eq!(principal.email, None);
        assert_eq!(principal.preferred_username, None);
        let token_id = match &principal.source {
            super::PrincipalSource::ApiToken { token_id } => *token_id,
            super::PrincipalSource::Session { .. } => panic!("source construite en ApiToken"),
        };
        assert_eq!(token_id, 7);
    }

    /// `Scenario` : « Discrimination de `source` : chaque variante lie sa charge utile
    /// exacte » — l'appariement exhaustif est le seul troisième état impossible.
    #[test]
    fn source_discrimination_binds_exact_payloads() {
        let session = super::AuthPrincipal {
            subject: "s".to_string(),
            email: None,
            preferred_username: None,
            source: super::PrincipalSource::Session {
                id_token: "jwt-brut".to_string(),
            },
        };
        let api = super::AuthPrincipal {
            subject: "s".to_string(),
            email: None,
            preferred_username: None,
            source: super::PrincipalSource::ApiToken { token_id: 42 },
        };

        let charge_sessionne = match &session.source {
            super::PrincipalSource::Session { id_token } => id_token.as_str(),
            super::PrincipalSource::ApiToken { .. } => panic!("premier principal en Session"),
        };
        let charge_api = match &api.source {
            super::PrincipalSource::Session { .. } => panic!("second principal en ApiToken"),
            super::PrincipalSource::ApiToken { token_id } => *token_id,
        };
        assert_eq!(charge_sessionne, "jwt-brut");
        assert_eq!(charge_api, 42);
    }

    /// `Scenario` : « `Clone` : copie profonde aux champs indépendants » — `PartialEq`
    /// n'existe pas, la comparaison se fait champ à champ ; `Clone` réalloue les `String`.
    #[test]
    fn clone_is_deep_and_independent() {
        let original = super::AuthPrincipal {
            subject: "alice".to_string(),
            email: Some("a@b".to_string()),
            preferred_username: Some("alice".to_string()),
            source: super::PrincipalSource::Session {
                id_token: "jwt-brut".to_string(),
            },
        };
        let mut copie = original.clone();

        assert_eq!(copie.subject, original.subject);
        assert_eq!(copie.email, original.email);
        assert_eq!(copie.preferred_username, original.preferred_username);
        let id_token_de_la_copie = match &copie.source {
            super::PrincipalSource::Session { id_token } => id_token.clone(),
            super::PrincipalSource::ApiToken { .. } => panic!("copie en Session"),
        };
        assert_eq!(id_token_de_la_copie, "jwt-brut");

        copie.subject.push_str("-suffixe");
        if let super::PrincipalSource::Session { id_token } = &mut copie.source {
            id_token.push_str("-suffixe");
        }
        assert_eq!(original.subject, "alice");
        let id_token_originale = match &original.source {
            super::PrincipalSource::Session { id_token } => id_token.as_str(),
            super::PrincipalSource::ApiToken { .. } => panic!("original en Session"),
        };
        assert_eq!(id_token_originale, "jwt-brut");
    }

    /// `Scenario` : « `Debug` : identité et source verbatim, `token_id` verbatim » — `token_id`
    /// n'est pas un secret (arbitré 2026-09-27).
    #[test]
    fn debug_token_variant_verbatim_token_id() {
        let principal = super::AuthPrincipal {
            subject: "svc".to_string(),
            email: None,
            preferred_username: None,
            source: super::PrincipalSource::ApiToken { token_id: 42 },
        };

        let rendu = format!("{principal:?}");
        for attendu in ["AuthPrincipal", "svc", "ApiToken", "42", "preferred_username"] {
            assert!(
                rendu.contains(attendu),
                "`Debug` du principal token doit contenir {attendu:?} : {rendu}"
            );
        }
    }

    /// `Scenario` : « `Debug` rédige `id_token` sur la variante Session » — la sortie contient
    /// la variante mais jamais le JWT brut, remplacé par le littéral `"[REDACTED]"`
    /// (arbitré 2026-09-27, cohérent avec `client_secret` de `OidcConfig`).
    #[test]
    fn debug_session_redacts_id_token() {
        let principal = super::AuthPrincipal {
            subject: "svc".to_string(),
            email: None,
            preferred_username: None,
            source: super::PrincipalSource::Session {
                id_token: "jwt-brut".to_string(),
            },
        };

        let rendu = format!("{principal:?}");
        assert!(
            rendu.contains("Session"),
            "`Debug` doit nommer la variante Session : {rendu}"
        );
        assert!(
            !rendu.contains("jwt-brut"),
            "`Debug` ne doit jamais reproduire l'`id_token` brut : {rendu}"
        );
        assert!(
            rendu.contains("\"[REDACTED]\""),
            "le littéral `\"[REDACTED]\"` doit rendre l'emplacement d'`id_token` : {rendu}"
        );
    }

    /// `Scenario` : « `as_deref` sur `email` et `preferred_username` : la forme de lecture
    /// des surfaces » — sans valeur de défaut inventée par le type.
    #[test]
    fn as_deref_shape_for_email_and_preferred_username() {
        let complet = super::AuthPrincipal {
            subject: "alice".to_string(),
            email: Some("a@b".to_string()),
            preferred_username: Some("alice".to_string()),
            source: super::PrincipalSource::Session {
                id_token: String::new(),
            },
        };
        let vides = super::AuthPrincipal {
            subject: "alice".to_string(),
            email: None,
            preferred_username: None,
            source: super::PrincipalSource::Session {
                id_token: String::new(),
            },
        };

        assert_eq!(complet.email.as_deref(), Some("a@b"));
        assert_eq!(complet.preferred_username.as_deref(), Some("alice"));
        assert_eq!(vides.email.as_deref(), None);
        assert_eq!(vides.preferred_username.as_deref(), None);
    }

    /// `Scenario` : « `preferred_username` absent : `None` relivré sans normalisation » —
    /// l'absence est une information, la présence est rendue verbatim (unicité garantie par
    /// le fournisseur OIDC, en amont de ce fichier).
    #[test]
    fn preferred_username_absent_or_verbatim_no_normalisation() {
        let absent = super::AuthPrincipal {
            subject: "s".to_string(),
            email: None,
            preferred_username: None,
            source: super::PrincipalSource::Session {
                id_token: String::new(),
            },
        };
        let libre = super::AuthPrincipal {
            subject: "s".to_string(),
            email: None,
            preferred_username: Some("Any Name/x".to_string()),
            source: super::PrincipalSource::Session {
                id_token: String::new(),
            },
        };

        assert_eq!(
            absent.preferred_username.as_deref(),
            None,
            "l'absence est une information, pas une valeur de repli inventée par le type"
        );
        assert_eq!(
            libre.preferred_username.as_deref(),
            Some("Any Name/x"),
            "chaîne libre rendue verbatim : le type ne valide, ne trie et ne normalise rien"
        );
    }

    /// `Scenario` : « Validation absente : valeurs vides ou malformées passent telles
    /// quelles » — type passoire à producteurs de confiance (arbitré 2026-09-27).
    #[test]
    fn no_validation_empty_and_malformed_pass_through() {
        let passerelle = super::AuthPrincipal {
            subject: String::new(),
            email: Some("pas-un-email".to_string()),
            preferred_username: Some("  mal formé  ".to_string()),
            source: super::PrincipalSource::Session {
                id_token: String::new(),
            },
        };
        let fixture = super::AuthPrincipal {
            subject: "fixture".to_string(),
            email: None,
            preferred_username: None,
            source: super::PrincipalSource::ApiToken { token_id: 0 },
        };

        assert_eq!(passerelle.subject, "");
        assert_eq!(passerelle.email.as_deref(), Some("pas-un-email"));
        assert_eq!(passerelle.preferred_username.as_deref(), Some("  mal formé  "));
        assert!(
            matches!(passerelle.source, super::PrincipalSource::Session { .. }),
            "`id_token` vide accepté tel quel"
        );
        assert!(
            matches!(fixture.source, super::PrincipalSource::ApiToken { token_id: 0 }),
            "fixture des registres GraphQL/MCP : token_id 0 rendu inchangé"
        );
    }
}
