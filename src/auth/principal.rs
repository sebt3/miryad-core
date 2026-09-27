/// Identité unifiée d'une requête authentifiée, quelle que soit la source (cookie de session ou
/// token API) — c'est ce type que REST/GraphQL/MCP consomment (feature 4+), pas `AuthUser` qui
/// reste spécifique au flow navigateur (feature 2a).
#[derive(Debug, Clone)]
pub struct AuthPrincipal {
    pub subject: String,
    pub email: Option<String>,
    /// Claim OIDC standard `preferred_username` — identifiant nommé garanti unique par le
    /// fournisseur (contrairement à `email`), posé sur le chemin session, `None` sur le chemin
    /// token API.
    pub preferred_username: Option<String>,
    pub source: PrincipalSource,
}

#[derive(Debug, Clone)]
pub enum PrincipalSource {
    Session { id_token: String },
    ApiToken { token_id: i32 },
}

#[cfg(test)]
mod tests {
    // Zéro ligne `use` ici aussi — contrat `Done when` de `principal.sdd` (« le fichier ne
    // contient toujours aucune @use ») : tout est qualifié par `super::`.

    /// `Scenario` : « Construction session : quatre champs publics lisibles à leurs
    /// valeurs exactes ».
    #[test]
    fn construction_session_quatre_champs_publics_lus_a_leurs_valeurs_exactes() {
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
    /// `token_id` conservé » — la forme exacte de `validate_token`.
    #[test]
    fn construction_token_api_email_et_preferred_username_none_et_token_id_conserve() {
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
    fn discrimination_source_chaque_variante_lie_sa_charge_utile() {
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
    /// n'existe pas, la comparaison se fait champ à champ.
    #[test]
    fn clone_copie_proonde_aux_champs_independants() {
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

    /// `Scenario` : « `Debug` : identité, source et charge utile verbatim ».
    #[test]
    fn debug_identite_source_et_charge_utile_verbatim() {
        let token_principal = super::AuthPrincipal {
            subject: "svc".to_string(),
            email: None,
            preferred_username: None,
            source: super::PrincipalSource::ApiToken { token_id: 42 },
        };
        let session_principal = super::AuthPrincipal {
            subject: "svc".to_string(),
            email: None,
            preferred_username: None,
            source: super::PrincipalSource::Session {
                id_token: "jwt-brut".to_string(),
            },
        };

        let rendu_token = format!("{token_principal:?}");
        for attendu in ["AuthPrincipal", "svc", "ApiToken", "42", "preferred_username"] {
            assert!(
                rendu_token.contains(attendu),
                "`Debug` du principal token doit contenir {attendu:?} : {rendu_token}"
            );
        }
        let rendu_session = format!("{session_principal:?}");
        assert!(
            rendu_session.contains("Session") && rendu_session.contains("jwt-brut"),
            "`Debug` du principal session doit contenir la variante et la charge utile \
             verbatim : {rendu_session}"
        );
    }

    /// `Scenario` : « `as_deref` sur `email` et `preferred_username` : la forme de lecture
    /// des surfaces ».
    #[test]
    fn as_deref_sur_email_et_preferred_username_forme_de_lecture_des_surfaces() {
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

    /// `Scenario` : « `preferred_username` absent : `None` relivré sans normalisation ».
    #[test]
    fn preferred_username_absent_none_relivre_sans_normalisation() {
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
    /// quelles ».
    #[test]
    fn validation_absente_valeurs_vides_ou_malformees_passent_telles_quelles() {
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
