//! Point d'extension de l'application hôte (`src/lymaille/host.sdd`).
//!
//! Le seul endroit par lequel l'application consommatrice fournit à miryad-core ce qu'elle seule
//! connaît (DSD `DEC-27`, `FR-14`) : le serveur MCP propre à un contexte — URL dynamique, jeton
//! minté à la demande, autorité inconnue de la crate — et les contextes de niveau projet
//! (`H_…`, jamais stockés). Le trait est optionnel : sans lui, le comportement est celui de
//! `DEC-20` seul. L'emploi de ces types est dans ./mcp_access.sdd et ./store/toolset.sdd.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use zeroize::Zeroizing;

use ::lymaille::ToolsetContext;

use super::identity::CallerPrincipal;

/// Extension de l'hôte pour les serveurs MCP, les contextes de niveau projet, les droits de
/// script de ces contextes, le profil par défaut des agents et le jeton `forward` hors session
/// interactive. Objet-trait : l'application le fournit en [`SharedHost`]
/// (`Arc<dyn LymailleHost>`). Toutes les méthodes ont un défaut — l'hôte n'implémente que ce qu'il
/// fournit (`Must` de ./host.sdd).
#[async_trait]
pub trait LymailleHost: Send + Sync {
    /// Rend la décision de l'hôte sur le rôle MCP `request.role` du contexte `request.context`.
    /// Défaut : `Ok(McpDecision::Unprovided)` — l'hôte ne pourvoit rien, repli sur la liaison
    /// classique de `mcp_servers` (DSD `DEC-20`).
    ///
    /// # Errors
    ///
    /// `HostError` si la résolution d'hôte échoue — erreur sans contenu : l'hôte journalise
    /// lui-même sa panne, miryad-core n'en propage ni n'en trace aucun texte.
    async fn resolve_mcp(
        &self,
        _request: &McpRequest<'_>,
        _caller: &CallerPrincipal,
    ) -> Result<McpDecision, HostError> {
        Ok(McpDecision::Unprovided)
    }

    /// Rend le contexte projet identifié par `id` (préfixe `H_…`, ./identity.sdd), l'hôte y
    /// tranche aussi l'autorité de `caller` : `None` = absent **ou** interdit, sans distinction.
    /// Défaut : `Ok(None)`. Jamais de connexion base : l'hôte applique ses propres règles.
    ///
    /// # Errors
    ///
    /// `HostError` si la résolution d'hôte échoue — erreur sans contenu, cf.
    /// [`LymailleHost::resolve_mcp`].
    async fn resolve_context(
        &self,
        _id: &str,
        _caller: &CallerPrincipal,
    ) -> Result<Option<ToolsetContext>, HostError> {
        Ok(None)
    }

    /// Rend les droits de script que l'hôte accorde au contexte `id` (identifiant `H_…`,
    /// ./identity.sdd) pour `caller` (issue #70) : équivalent, pour un `H_…`, de la colonne
    /// `script_grants` d'un type de contexte (`./definitions/context_type.sdd`). Défaut :
    /// `Ok(None)` — aucun droit, donc l'échec fermé actuel de `./script_runner.sdd`. L'hôte ne peut
    /// accorder que ce que `check_grants` laisse passer (./identity.sdd, `FR-9`) : c'est le
    /// consommateur (`./script_runner.sdd`) qui applique cette porte.
    ///
    /// # Errors
    ///
    /// `HostError` si la résolution d'hôte échoue — erreur sans contenu, cf.
    /// [`LymailleHost::resolve_mcp`] ; la crate la rend en échec fermé, sans texte de l'hôte.
    async fn script_grants(
        &self,
        _context: &str,
        _caller: &CallerPrincipal,
    ) -> Result<Option<ScriptGrants>, HostError> {
        Ok(None)
    }

    /// Rend le nom du profil de modèle par défaut : celui que reçoivent les agents qui n'en
    /// désignent pas dans leur définition (issue #75, ./host.sdd — la colonne `profile` vide des
    /// tables d'agents, ./definitions/agent.sdd). Relu à **chaque** ouverture de session et à
    /// chaque exécution de step, jamais mis en cache, persisté ni journalisé par la crate.
    /// Défaut : `Ok(None)` — aucun profil par défaut ; l'appelant rend alors l'erreur nommant la
    /// cause (./interactive/assemble.sdd, ../../`workflow/subagent_step.sdd`).
    ///
    /// # Errors
    ///
    /// `HostError` si la résolution d'hôte échoue — erreur sans contenu, cf.
    /// [`LymailleHost::resolve_mcp`] ; la crate ne la propage ni ne la trace, elle rend sa phrase
    /// fixe.
    async fn default_profile(&self) -> Result<Option<String>, HostError> {
        Ok(None)
    }

    /// Rend le jeton qu'un serveur MCP en mode `forward` doit recevoir quand aucun jeton
    /// interactif n'existe (step batch, script de widget, pont — issue #82). L'audience, la durée
    /// et la portée du jeton (échange RFC 8693 ou autre) sont l'affaire exclusive de l'hôte ;
    /// `McpGrant::expires_at` ne s'y applique pas. Jamais persisté, journalisé ni mis en cache
    /// par la crate (`Must not` de ./host.sdd) ; il n'atteint que le serveur `forward` demandé.
    /// Défaut : `Ok(None)` — aucun fournisseur : comportement actuel, échec typé non retryable
    /// (`./mcp_access.sdd`).
    ///
    /// # Errors
    ///
    /// `HostError` si la résolution d'hôte échoue — erreur sans contenu, cf.
    /// [`LymailleHost::resolve_mcp`] ; la crate ne la propage ni ne la trace, elle rend sa phrase
    /// fixe.
    async fn forward_token(
        &self,
        _request: &McpRequest<'_>,
        _caller: &CallerPrincipal,
    ) -> Result<Option<Zeroizing<String>>, HostError> {
        Ok(None)
    }
}

/// Requête de [`LymailleHost::resolve_mcp`] : l'identifiant du contexte de la session et le rôle
/// MCP demandé par un toolset accordé.
#[derive(Clone, Copy, Debug)]
pub struct McpRequest<'a> {
    /// Identifiant du contexte de la session.
    pub context: &'a str,
    /// Rôle MCP demandé par un toolset accordé.
    pub role: &'a str,
}

/// Décision de l'hôte sur un rôle MCP de [`McpRequest`].
#[derive(Clone, Debug)]
pub enum McpDecision {
    /// L'hôte ne pourvoit pas ce rôle : repli sur la liaison classique de `mcp_servers`.
    Unprovided,
    /// Refus d'autorité de l'hôte, **sans repli**.
    Denied,
    /// Serveur et jeton à employer.
    Granted(McpGrant),
}

/// Serveur MCP et jeton de courte durée fournis par l'hôte. Ni l'URL ni le jeton ne sont mis en
/// cache, persistés ou tracés (`Must not` de ./host.sdd) : le `Debug` est manuel et opaque.
#[derive(Clone)]
pub struct McpGrant {
    /// URL du serveur MCP propre au contexte.
    pub url: String,
    /// Jeton minté à la demande — `Zeroizing`, jamais un `String` nu.
    pub token: Zeroizing<String>,
    /// Instant d'expiration, si l'hôte en pose un.
    pub expires_at: Option<DateTime<Utc>>,
}

impl std::fmt::Debug for McpGrant {
    /// Opaque par construction : ni l'URL ni le jeton n'y figurent (`Must` de ./host.sdd).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpGrant").finish_non_exhaustive()
    }
}

impl McpGrant {
    /// Vrai si `expires_at` est présent et `<= now` — expiré à la seconde exacte de
    /// l'expiration ; sans `expires_at`, un jeton n'expire jamais (DSD `FR-14`).
    #[must_use]
    pub fn is_expired(&self, now: DateTime<Utc>) -> bool {
        self.expires_at.is_some_and(|at| at <= now)
    }
}

/// Les droits de script accordés à un contexte, même forme que la colonne `script_grants` d'un
/// type de contexte (objet à exactement les deux clés `toolsets` et `modules`) : les noms de
/// toolsets (version active) et de modules d'hôte (`Must` de ./host.sdd, amendement du 2026-11-13
/// — posé par la tâche provenance de ./store/publish.sdd, issue #73 ; la méthode
/// `LymailleHost::script_grants` qui le rend est la tâche #70 de ./host.sdd, posée avec elle).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScriptGrants {
    /// Toolsets du catalogue (`G_…`, version active) accordés.
    pub toolsets: Vec<String>,
    /// Noms de modules d'hôte accordés.
    pub modules: Vec<String>,
}

/// Erreur de l'hôte, **sans contenu** (`Must` de ./host.sdd) : l'hôte journalise lui-même sa
/// panne ; miryad-core n'en propage ni n'en trace aucun texte.
#[derive(Clone, Copy, Debug, thiserror::Error)]
#[error("host resolution failed")]
pub struct HostError;

/// L'hôte partagé par le montage de l'état.
pub type SharedHost = std::sync::Arc<dyn LymailleHost>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lymaille::identity::CallerPrincipal;
    use chrono::{TimeZone as _, Utc};
    use std::sync::Arc;

    fn caller() -> CallerPrincipal {
        CallerPrincipal {
            user_id: 1,
            subject: "sub-test".to_string(),
            is_admin: false,
        }
    }

    /// Scenario « un hôte vide ne pourvoit rien » : un hôte qui n'implémente aucune méthode,
    /// exercé via `SharedHost` = `Arc<dyn LymailleHost>` — la compilation de ce type d'objet est
    /// aussi la preuve de l'objet-trait du `Must`.
    #[tokio::test]
    async fn un_hote_vide_ne_pourvoit_rien() {
        struct EmptyHost;
        #[async_trait]
        impl LymailleHost for EmptyHost {}

        let host: SharedHost = std::sync::Arc::new(EmptyHost);
        let request = McpRequest {
            context: "G_prod",
            role: "fetch",
        };
        let decision = host
            .resolve_mcp(&request, &caller())
            .await
            .expect("le défaut de resolve_mcp ne peut pas échouer");
        assert!(
            matches!(decision, McpDecision::Unprovided),
            "attendu Unprovided, obtenu : {decision:?}"
        );
        let context = host
            .resolve_context("H_projet", &caller())
            .await
            .expect("le défaut de resolve_context ne peut pas échouer");
        assert!(context.is_none(), "attendu None, obtenu : {context:?}");
    }

    /// Scenario « les trois méthodes ajoutées ont un défaut neutre » (./host.sdd, issues #70,
    /// #75, #82) : un hôte qui n'implémente que les méthodes d'origine rend `Ok(None)` pour
    /// `script_grants`, `default_profile` et `forward_token` — aucun droit, aucun profil, aucun
    /// jeton ; les trois sont bien les méthode à défaut du trait.
    #[tokio::test]
    async fn les_trois_methodes_ajoutees_ont_un_defaut_neutre() {
        struct EmptyHost;
        #[async_trait]
        impl LymailleHost for EmptyHost {}

        let host: SharedHost = std::sync::Arc::new(EmptyHost);
        let droits = host
            .script_grants("H_sandbox_alice_web", &caller())
            .await
            .expect("le défaut de script_grants ne peut pas échouer");
        assert!(droits.is_none(), "aucun droit par défaut, obtenu : {droits:?}");
        let rendu = host
            .default_profile()
            .await
            .expect("le défaut de default_profile ne peut pas échouer");
        assert!(rendu.is_none(), "attendu None, obtenu : {rendu:?}");
        let request = McpRequest {
            context: "G_prod",
            role: "fetch",
        };
        let jeton = host
            .forward_token(&request, &caller())
            .await
            .expect("le défaut de forward_token ne peut pas échouer");
        assert!(jeton.is_none(), "aucun jeton par défaut, obtenu : {jeton:?}");
    }

    /// `Scenario` de la tâche « Issue #70 » de ./host.sdd : un hôte factice accorde des droits de
    /// script à un contexte `H_…` pour `alice`, et `None` pour tout autre contexte.
    #[tokio::test]
    async fn un_hote_accorde_des_droits_de_script() {
        struct HoteDroits;
        #[async_trait]
        impl LymailleHost for HoteDroits {
            async fn script_grants(
                &self,
                context: &str,
                _caller: &CallerPrincipal,
            ) -> Result<Option<ScriptGrants>, HostError> {
                if context == "H_sandbox_alice_web" {
                    Ok(Some(ScriptGrants {
                        toolsets: vec!["G_ops".to_owned()],
                        modules: vec!["view".to_owned()],
                    }))
                } else {
                    Ok(None)
                }
            }
        }

        let host: SharedHost = std::sync::Arc::new(HoteDroits);
        let rendus = host
            .script_grants("H_sandbox_alice_web", &caller())
            .await
            .expect("l'hôte accorde sans panne");
        let attendu = ScriptGrants {
            toolsets: vec!["G_ops".to_owned()],
            modules: vec!["view".to_owned()],
        };
        assert_eq!(rendus, Some(attendu), "les droits accordés traversent");
        let autre = host
            .script_grants("H_autre", &caller())
            .await
            .expect("l'hôte répond sans panne");
        assert!(autre.is_none(), "un autre contexte n'a aucun droit : {autre:?}");
    }

    /// Capture des traces (patron des tests de `./mcp_access.rs`) : preuve négative sur le texte
    /// consigné — le jeton rendu par `forward_token` n'y paraît jamais.
    #[derive(Default)]
    struct CapturedTraces(std::sync::Mutex<Vec<String>>);

    struct CaptureLayer(std::sync::Arc<CapturedTraces>);

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for CaptureLayer {
        fn on_event(&self, event: &tracing::Event<'_>, _ctx: tracing_subscriber::layer::Context<'_, S>) {
            if !event.metadata().target().starts_with("miryad_core") {
                return;
            }
            let mut rendered = String::new();
            event.record(&mut RenderVisitor(&mut rendered));
            self.0
                .0
                .lock()
                .expect("verrou de la capture non empoisonne")
                .push(rendered);
        }
    }

    struct RenderVisitor<'a>(&'a mut String);

    impl tracing::field::Visit for RenderVisitor<'_> {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            use std::fmt::Write as _;
            let _ = write!(self.0, " {}={value:?}", field.name());
        }
    }

    /// `Scenario` « `forward_token` ne divulgue pas le jeton » (./host.sdd, issue #82 ; validation
    /// indépendante du 2026-11-16) : la sentinelle rendue par l'hôte factice n'apparaît dans
    /// AUCUN rendu `Debug` du résultat (le `Result` nu, l'`Option<Zeroizing<String>>` qu'il
    /// porte, le `McpGrant` et le `McpDecision` qui la portent), dans aucun `Display`, et dans
    /// aucune trace émise pendant les appels d'hôte. Témoin positif d'abord : la détection de
    /// containment est prouvée effective sur le `Debug` dérivé d'une struct qui porte la sentinelle,
    /// et sur une trace de passage, sans quoi les assertions négatives ne prouveraient rien. `Display` de `HostError` reste la
    /// phrase fixe. Le `Debug` de `Zeroizing<String>` est opaque en `zeroize` >= 1.9.1 (version
    /// du graphe, verrou direct de `/Cargo.toml`) : si un retour en 1.9.0 faisait réapparaître la
    /// valeur dans `{rendu:?}`, ce test vire au rouge — c'est la preuve demandée par le `[?]` du
    /// cycle #82, sans verrou de dépendance posé hors `Owns`.
    #[tokio::test]
    async fn forward_token_ne_divulgue_pas_le_jeton() {
        use tracing_subscriber::layer::SubscriberExt as _;

        const SENTINELLE: &str = "secret-token";

        #[derive(Debug)]
        struct HoteJeton {
            panne: bool,
        }
        #[async_trait]
        impl LymailleHost for HoteJeton {
            async fn forward_token(
                &self,
                _request: &McpRequest<'_>,
                _caller: &CallerPrincipal,
            ) -> Result<Option<Zeroizing<String>>, HostError> {
                // Trace fixe de passage : témoin positif — la capture voit bien les événements
                // émis pendant l'appel d'hôte.
                tracing::info!("hote factice : jeton rendu");
                if self.panne {
                    return Err(HostError);
                }
                Ok(Some(Zeroizing::new(SENTINELLE.to_owned())))
            }
        }

        // Témoin positif : un `Debug` dérivé qui porte la sentinelle doit être détecté par le
        // contrôle de containment — un `contains` qui ne matcherait jamais rendrait toutes les
        // assertions négatives vides. Le témoin passe par un rendu `Debug` de struct, la forme
        // réelle d'une fuite, et non par la chaîne nue (qui ne prouvait que `contains` sur lui-même).
        #[derive(Debug)]
        struct Fuyard(String);
        let fuyard = Fuyard(SENTINELLE.to_owned());
        // Le champ est lu ici : `Debug` dérivé ne compte pas pour `dead_code`.
        assert_eq!(fuyard.0, SENTINELLE, "témoin : le champ porte la sentinelle");
        assert!(
            format!("{fuyard:?}").contains(SENTINELLE),
            "témoin : la détection de containment voit la sentinelle dans le Debug dérivé d'une struct"
        );

        let captured = std::sync::Arc::new(CapturedTraces::default());
        let subscriber = tracing_subscriber::registry::Registry::default()
            .with(CaptureLayer(std::sync::Arc::clone(&captured)));
        let _garde = tracing::subscriber::set_default(subscriber);

        let request = McpRequest {
            context: "G_prod",
            role: "fetch",
        };
        let concret = HoteJeton { panne: false };
        // Le `Debug` de l'impl de l'hôte : elle ne garde aucun jeton en champ, rien à divulguer.
        let rendu_impl = format!("{concret:?}");
        assert!(
            !rendu_impl.contains(SENTINELLE),
            "le Debug de l'impl de l'hôte ne contient pas le jeton : {rendu_impl}"
        );

        let hote: SharedHost = Arc::new(concret);
        let rendu_resultat = hote.forward_token(&request, &caller()).await;

        // Le `Debug` du résultat complet (`Result<Option<Zeroizing<String>>, HostError>`) ne
        // contient pas la sentinelle — ce que l'ancien test, réduit à `rendu.is_some()`, ne
        // prouvait pas.
        let rendu_brut = format!("{rendu_resultat:?}");
        assert!(
            !rendu_brut.contains(SENTINELLE),
            "le Debug du résultat rendu porte le jeton : {rendu_brut}"
        );
        let rendu = rendu_resultat.expect("l'hôte fournit le jeton");
        // Le `Debug` de l'`Option<Zeroizing<String>>` seule : opacité de `Zeroizing` dans le
        // graphe courant (`zeroize` 1.9.1), verrouillée par ce test.
        let rendu_jeton = format!("{rendu:?}");
        assert!(
            !rendu_jeton.contains(SENTINELLE),
            "le Debug du Zeroizing rendu porte le jeton : {rendu_jeton}"
        );

        // La sentinelle logée dans un `McpGrant` (et dans le `McpDecision` qui l'enveloppe) ne
        // transparaît dans aucun rendu : ni jeton ni URL.
        let grant = McpGrant {
            url: "https://sbx.example/mcp".to_owned(),
            token: Zeroizing::new(SENTINELLE.to_owned()),
            expires_at: None,
        };
        let rendu_grant = format!("{grant:?}");
        assert!(
            !rendu_grant.contains(SENTINELLE) && !rendu_grant.contains("sbx.example"),
            "le Debug du McpGrant porte le jeton ou l'URL : {rendu_grant}"
        );
        let rendu_decision = format!("{:?}", McpDecision::Granted(grant));
        assert!(
            !rendu_decision.contains(SENTINELLE) && !rendu_decision.contains("sbx.example"),
            "le Debug du McpDecision Granted porte le jeton ou l'URL : {rendu_decision}"
        );

        let hote_en_panne: SharedHost = Arc::new(HoteJeton { panne: true });
        let erreur = hote_en_panne
            .forward_token(&request, &caller())
            .await
            .expect_err("l'hôte panne rend HostError");
        assert_eq!(format!("{erreur}"), "host resolution failed");
        assert_eq!(format!("{erreur:?}"), "HostError");
        assert!(
            !format!("{erreur}").contains(SENTINELLE),
            "le Display de HostError ne porte rien : {erreur}"
        );

        // Volet traces : la trace de passage a bien été capturée (témoin positif), et aucune des
        // lignes capturées pendant les deux appels d'hôte ne contient la sentinelle.
        let lignes = captured
            .0
            .lock()
            .expect("verrou de la capture non empoisonne")
            .clone();
        assert!(
            lignes.iter().any(|l| l.contains("hote factice")),
            "témoin : la capture voit les traces de l'appel d'hôte : {lignes:?}"
        );
        assert!(
            !lignes.iter().any(|l| l.contains(SENTINELLE)),
            "le jeton apparaît dans une trace émise pendant l'appel d'hôte : {lignes:?}"
        );
    }

    /// `Scenario` de la tâche « Issue #75 » de ./host.sdd (moitié « défaut → `None` » du
    /// Scenario « les méthodes ajoutées ont un défaut neutre ») : un hôte qui n'implémente que les méthodes
    /// d'origine rend `Ok(None)` — aucun profil par défaut, et `default_profile` est bien la
    /// quatrième méthode à défaut du trait.
    #[tokio::test]
    async fn default_profile_a_un_defaut_neutre() {
        struct EmptyHost;
        #[async_trait]
        impl LymailleHost for EmptyHost {}

        let host: SharedHost = std::sync::Arc::new(EmptyHost);
        let rendu = host
            .default_profile()
            .await
            .expect("le défaut de default_profile ne peut pas échouer");
        assert!(rendu.is_none(), "attendu None, obtenu : {rendu:?}");
    }

    /// `Scenario` de la tâche « Issue #75 » de ./host.sdd (moitié « hôte qui désigne un profil ») :
    /// l'hôte qui implémente `default_profile` pourvoit le nom que la crate relit à chaque
    /// ouverture de session et à chaque exécution de step.
    #[tokio::test]
    async fn un_hote_designe_le_profil_par_defaut() {
        struct HoteProfil;
        #[async_trait]
        impl LymailleHost for HoteProfil {
            async fn default_profile(&self) -> Result<Option<String>, HostError> {
                Ok(Some("qwen".to_owned()))
            }
        }

        let host: SharedHost = std::sync::Arc::new(HoteProfil);
        let rendu = host
            .default_profile()
            .await
            .expect("l'hôte désigne un profil sans panne");
        assert!(
            matches!(&rendu, Some(nom) if nom == "qwen"),
            "le nom désigné traverse : {rendu:?}"
        );
    }

    /// Scenario « l'expiration est exclusive » : faux avant `t`, vrai à `t` et après ; sans
    /// `expires_at`, toujours faux.
    #[tokio::test]
    async fn lexpiration_est_exclusive() {
        let t = Utc.with_ymd_and_hms(2026, 11, 4, 12, 0, 0).unwrap();
        let grant = McpGrant {
            url: "https://sbx.example/mcp".to_string(),
            token: Zeroizing::new("s3cret".to_string()),
            expires_at: Some(t),
        };
        assert!(
            !grant.is_expired(t - chrono::Duration::seconds(1)),
            "expiré avant t"
        );
        assert!(grant.is_expired(t), "non expiré à t");
        assert!(
            grant.is_expired(t + chrono::Duration::seconds(1)),
            "non expiré après t"
        );
        let sans_expiration = McpGrant {
            url: "https://sbx.example/mcp".to_string(),
            token: Zeroizing::new("s3cret".to_string()),
            expires_at: None,
        };
        assert!(
            !sans_expiration.is_expired(t - chrono::Duration::seconds(1)),
            "un jeton sans expiration ne doit jamais expirer"
        );
        assert!(
            !sans_expiration.is_expired(t),
            "un jeton sans expiration ne doit jamais expirer"
        );
        assert!(
            !sans_expiration.is_expired(t + chrono::Duration::seconds(1)),
            "un jeton sans expiration ne doit jamais expirer"
        );
    }

    /// Scenario « Debug ne divulgue ni jeton ni URL » : le `Debug` de `McpGrant` est opaque.
    #[test]
    fn debug_ne_divulgue_ni_jeton_ni_url() {
        let grant = McpGrant {
            url: "https://sbx.example/mcp".to_string(),
            token: Zeroizing::new("s3cret".to_string()),
            expires_at: None,
        };
        let rendu = format!("{grant:?}");
        assert!(
            !rendu.contains("s3cret"),
            "le jeton transite par le Debug : {rendu}"
        );
        assert!(
            !rendu.contains("sbx.example"),
            "l'URL transite par le Debug : {rendu}"
        );
    }

    /// Scenario « `HostError` ne porte aucun texte » : `Display` et `Debug` fixes, rien venu de
    /// l'hôte.
    #[test]
    fn hosterror_ne_porte_aucun_texte() {
        let erreur = HostError;
        assert_eq!(format!("{erreur}"), "host resolution failed");
        assert_eq!(format!("{erreur:?}"), "HostError");
    }
}
