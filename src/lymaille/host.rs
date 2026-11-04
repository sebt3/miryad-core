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

/// Extension de l'hôte pour les serveurs MCP et les contextes de niveau projet. Objet-trait :
/// l'application le fournit en [`SharedHost`] (`Arc<dyn LymailleHost>`). Les deux méthodes ont un
/// défaut — l'hôte n'implémente que ce qu'il fournit (`Must` de ./host.sdd).
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
