//! Droit d'usage des serveurs MCP et en-têtes d'authentification (`src/lymaille/mcp_access.sdd`).
//!
//! Contrôle d'**usage** des serveurs MCP (DSD `DEC-20`, `FR-4`, `FR-9`) et production de leurs
//! en-têtes d'authentification. Un serveur (`./definitions/mcp_server.rs`) porte `allowed_groups` ;
//! ici se décide, **à chaque appel et sur une relecture en base**, si un `subject` peut le lier ou
//! l'appeler (`all` = tout authentifié, `admin` = admin, sinon appartenance à l'un des groupes ;
//! aucun cache). Les hooks `MiryadResource` étant synchrones et sans connexion, ce contrôle
//! d'autorité est la défense en profondeur de `FR-9` à l'assemblage et à l'appel. Amendement
//! `DEC-27` : un serveur peut aussi être fourni par l'hôte (`./host.rs`), sans ligne `mcp_servers` ;
//! l'hôte tranche alors l'autorité et rend URL et jeton frais.
//!
//! Le secret ne transite jamais par la base, une erreur ou une trace ; `Denied`, l'expiration d'un
//! jeton d'hôte et `Err(HostError)` sont des échecs fermés, sans repli ; le repli sur la table
//! n'a lieu que sur `Unprovided`. « Absent » et « non autorisé » rendent le même texte,
//! `Forbidden("mcp server unavailable")`.

use std::sync::Arc;

use sea_orm::entity::prelude::*;
use zeroize::Zeroizing;

use ::lymaille::{McpAuth, McpError, McpHeaders};

use super::definitions::mcp_server;
use super::error::LymailleError;
use super::host::{LymailleHost, McpDecision, McpGrant, McpRequest, SharedHost};
use super::identity::{CallerPrincipal, Identity};
use super::secret::SecretResolver;
use crate::lymaille::store::db_text;
use crate::users::group;

/// Texte unique du refus d'usage — « absent » et « non autorisé » se confondent, pas d'oracle
/// d'existence (`Scenario` de la spec).
const SERVER_UNAVAILABLE: &str = "mcp server unavailable";

/// Refus d'un `forward` hors session interactive — non retryable (`retryable()` de `../error.sdd`
/// est `false` sur `Forbidden`, traduits en `StepError` non retryable par le step `subagent`).
const FORWARD_NEEDS_SESSION: &str = "forward auth needs an interactive session";

/// Erreur fixe d'une référence de secret introuvable — ni la valeur ni le nom de la référence
/// n'y figurent.
const AUTH_REF_UNRESOLVED: &str = "mcp auth reference unresolved";

/// Erreur fixe du refus de l'hôte dans `SeaHostMcpAuth::headers` — aucun texte de l'hôte.
const HOST_DENIED: &str = "mcp host denied";

/// Mode d'authentification d'un serveur, pur, depuis la colonne `auth_mode` de `Model`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum McpAuthMode {
    /// Aucun en-tête.
    None,
    /// `Authorization: Bearer` d'une référence résolue par le `SecretResolver` à chaque connexion.
    Static,
    /// Jeton de l'utilisateur d'une session **interactive**, jamais persisté.
    Forward,
}

/// Implémentation hôte de `lymaille::McpAuth` pour les serveurs de la table (modes `static` et
/// `forward`). Le secret vit dans un `Zeroizing`, n'entre dans aucune erreur ni trace, et le
/// `Debug` est opaque.
pub struct SeaMcpAuth {
    kind: AuthKind,
}

enum AuthKind {
    Static {
        resolver: Arc<dyn SecretResolver>,
        auth_ref: String,
    },
    Forward(Zeroizing<String>),
}

impl std::fmt::Debug for SeaMcpAuth {
    /// Opaque par construction : ni le jeton ni la référence ne figurent dans le `Debug`
    /// (`Must` de la spec).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SeaMcpAuth").finish_non_exhaustive()
    }
}

impl SeaMcpAuth {
    /// Mode `static` : la référence `auth_ref` est résolue par `resolver` **à chaque appel de
    /// `headers()`** — un jeton renouvelé est pris en compte au plus tard à la connexion suivante.
    #[must_use]
    pub fn new_static(resolver: Arc<dyn SecretResolver>, auth_ref: String) -> Self {
        Self {
            kind: AuthKind::Static { resolver, auth_ref },
        }
    }

    /// Mode `forward` : le jeton de la session interactive, rendu tel quel en
    /// `Authorization: Bearer`.
    #[must_use]
    pub fn new_forward(token: Zeroizing<String>) -> Self {
        Self {
            kind: AuthKind::Forward(token),
        }
    }
}

impl McpAuth for SeaMcpAuth {
    fn headers(&self) -> impl std::future::Future<Output = Result<McpHeaders, McpError>> + Send {
        // Mode synchrone par nature (résolution d'une référence, jeton déjà en main) : la
        // résolution a lieu à l'appel de `headers()`, rendu par `ready` — pas de `.await`
        // intérieur, l'implémentation `async fn` est refusée par `unused_async`.
        std::future::ready(match &self.kind {
            // Résolution à chaque appel — la valeur fraîche est injectée directement, aucun
            // cache. Une référence introuvable rend la phrase fixe, sans le nom de la
            // référence ni la valeur (`Must` de la spec) : l'erreur du résolveur, qui porte
            // la référence, est écartée ici.
            AuthKind::Static { resolver, auth_ref } => resolver
                .resolve(auth_ref)
                .map_err(|_erreur| McpError::Failed(AUTH_REF_UNRESOLVED.to_owned()))
                .map(|secret| McpHeaders::new().with_bearer(&secret)),
            AuthKind::Forward(token) => Ok(McpHeaders::new().with_bearer(token)),
        })
    }
}

/// Fabrique l'authentification correspondant au mode du serveur : `none` → `Ok(None)` (aucun
/// en-tête), `static` → `SeaMcpAuth` statique, `forward` avec jeton interactif → `SeaMcpAuth`
/// forward. `Forward` **sans** jeton interactif (step batch, ou session sans jeton) est refusé
/// non retryable.
///
/// # Errors
///
/// - `LymailleError::Forbidden` : `forward` sans session interactive (`FORWARD_NEEDS_SESSION`).
/// - `LymailleError::Config` : ligne `static` sans `auth_ref` — impossible après les hooks de
///   `../definitions/mcp_server.sdd`, branche de garde sans `unwrap`.
pub fn build_auth(
    model: &mcp_server::Model,
    resolver: Arc<dyn SecretResolver>,
    interactive_token: Option<Zeroizing<String>>,
) -> Result<Option<SeaMcpAuth>, LymailleError> {
    match auth_mode(&model.auth_mode) {
        // `none` → aucun objet : le connecteur n'ajoutera aucun en-tête.
        McpAuthMode::None => Ok(None),
        McpAuthMode::Static => match &model.auth_ref {
            Some(reference) => Ok(Some(SeaMcpAuth::new_static(resolver, reference.clone()))),
            // Les hooks de `../definitions/mcp_server.sdd` interdisent un `static` sans
            // référence : branche de garde sur une ligne corrompue, sans `unwrap`.
            None => Err(LymailleError::Config(
                "static mcp server auth requires an auth reference".to_string(),
            )),
        },
        McpAuthMode::Forward => match interactive_token {
            Some(token) => Ok(Some(SeaMcpAuth::new_forward(token))),
            None => Err(LymailleError::Forbidden(FORWARD_NEEDS_SESSION.to_string())),
        },
    }
}

/// Classification de la colonne `auth_mode` : les hooks de `../definitions/mcp_server.sdd`
/// n'écrivent que `none`, `static` ou `forward` ; toute autre valeur est traitée comme `none`
/// (aucune authentification inventée à partir d'une ligne invalide).
fn auth_mode(mode: &str) -> McpAuthMode {
    match mode {
        "static" => McpAuthMode::Static,
        "forward" => McpAuthMode::Forward,
        _ => McpAuthMode::None,
    }
}

/// Cherche le serveur MCP `name` et tranche le droit de l'employer : `allowed_groups` contient
/// `all`, ou contient `admin` et `caller.is_admin`, ou le caller est membre de l'un des groupes
/// (**relu en base**, aucun cache).
///
/// Connexion générique (amendement du 2026-11-11, DSD `hooks-async` `DEC-7`) : appelé par le
/// hook de `./definitions/user_context.sdd` avec la connexion de l'appel ; comportement inchangé.
///
/// # Errors
///
/// - `LymailleError::Forbidden` : serveur absent **ou** non autorisé — le même texte dans les
///   deux cas, pas d'oracle d'existence (`SERVER_UNAVAILABLE`).
/// - `LymailleError::Store` : `DbErr` d'une des lectures (`db_text` expurgé).
pub async fn server_for<C: ConnectionTrait>(
    db: &C,
    caller: &CallerPrincipal,
    name: &str,
) -> Result<mcp_server::Model, LymailleError> {
    let modele = mcp_server::Entity::find()
        .filter(mcp_server::Column::Name.eq(name))
        .one(db)
        .await
        .map_err(|erreur| LymailleError::Store(db_text(&erreur)))?
        .ok_or_else(|| LymailleError::Forbidden(SERVER_UNAVAILABLE.to_string()))?;
    if autorise(db, caller, &modele.allowed_groups).await? {
        Ok(modele)
    } else {
        // Absent et non autorisé rendent le même texte : pas d'oracle d'existence.
        Err(LymailleError::Forbidden(SERVER_UNAVAILABLE.to_string()))
    }
}

/// Décision d'appartenance sur `allowed_groups` (DSD `DEC-20`) : `all` pour tout authentifié,
/// `admin` pour un admin (`caller.is_admin`, reconstruit à chaque appel par `Identity`), sinon
/// appartenance à l'un des groupes — **relu en base à chaque appel, aucun cache** (DSD `FR-4`).
/// Une valeur hors du tableau de noms garanti par les hooks n'autorise personne.
async fn autorise<C: ConnectionTrait>(
    db: &C,
    caller: &CallerPrincipal,
    groupes: &serde_json::Value,
) -> Result<bool, LymailleError> {
    let Some(noms) = groupes.as_array() else {
        return Ok(false);
    };
    if noms.iter().any(|groupe| groupe.as_str() == Some("all")) {
        return Ok(true);
    }
    if noms.iter().any(|groupe| groupe.as_str() == Some("admin")) {
        return Ok(caller.is_admin);
    }
    for nom in noms.iter().filter_map(serde_json::Value::as_str) {
        if group::is_member(db, caller.user_id, nom)
            .await
            .map_err(|erreur| LymailleError::Store(db_text(&erreur)))?
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Liaison d'un rôle MCP : servie par l'hôte (`McpGrant`) ou par la table (`Model`).
#[derive(Clone, Debug)]
pub enum RoleBinding {
    /// Serveur et jeton fournis par l'hôte (DSD `DEC-27`) — priorité sur la table.
    Host(McpGrant),
    /// Serveur lié par la table `mcp_servers` (DSD `DEC-20`).
    Table(mcp_server::Model),
}

/// Tranche la liaison du rôle MCP `role` du contexte `context` : avec un hôte, sa décision fait
/// foi (`Granted` non expiré → `RoleBinding::Host` ; `Denied`, jeton expiré ou `Err(HostError)`
/// → refus **fermé**, sans repli, la panne étant tracée par une phrase fixe) ; `Unprovided` ou
/// absence d'hôte → liaison classique par `bound_server` (`server_for`) ou refus.
///
/// # Errors
///
/// - `LymailleError::Forbidden` : refus de l'hôte, jeton expiré, panne d'hôte, `bound_server`
///   absent, ou serveur lié absent/non autorisé — toujours `SERVER_UNAVAILABLE` pour les refus
///   d'usage (`Scenario` de la spec).
/// - `LymailleError::Store` : propagé de `server_for` sur le repli classique.
pub async fn resolve_role(
    host: Option<&dyn LymailleHost>,
    db: &DatabaseConnection,
    caller: &CallerPrincipal,
    context: &str,
    role: &str,
    bound_server: Option<&str>,
) -> Result<RoleBinding, LymailleError> {
    if let Some(host) = host {
        let request = McpRequest { context, role };
        match host.resolve_mcp(&request, caller).await {
            Ok(McpDecision::Granted(grant)) if !grant.is_expired(chrono::Utc::now()) => {
                return Ok(RoleBinding::Host(grant));
            }
            // `Denied`, jeton expiré : échec fermé, sans repli sur la table (DSD `DEC-27`).
            Ok(McpDecision::Granted(_) | McpDecision::Denied) => {
                return Err(LymailleError::Forbidden(SERVER_UNAVAILABLE.to_string()));
            }
            // `Unprovided` : l'hôte ne pourvoit pas ce rôle, repli sur la liaison classique.
            Ok(McpDecision::Unprovided) => {}
            Err(_erreur_hote) => {
                // Panne de l'hôte : refus fermé, tracé par une phrase fixe — `HostError` est
                // sans contenu (`./host.sdd`), aucun texte d'hôte ne peut y figurer.
                tracing::warn!("mcp host resolution failed");
                return Err(LymailleError::Forbidden(SERVER_UNAVAILABLE.to_string()));
            }
        }
    }
    match bound_server {
        Some(name) => Ok(RoleBinding::Table(server_for(db, caller, name).await?)),
        None => Err(LymailleError::Forbidden(SERVER_UNAVAILABLE.to_string())),
    }
}

/// Implémentation hôte de `lymaille::McpAuth` pour un rôle pourvu par l'hôte : à **chaque appel
/// de `headers()`**, principal relu par `Identity` (DSD `FR-4`) puis `resolve_mcp` ; aucun jeton
/// n'est gardé entre deux appels, le `Debug` est opaque.
pub struct SeaHostMcpAuth {
    host: SharedHost,
    identity: Identity,
    subject: String,
    context: String,
    role: String,
}

impl SeaHostMcpAuth {
    /// Attache l'authentification à l'hôte partagé, à la porte d'identité et au rôle
    /// (`context`/`role` de `McpRequest`). L'URL du client vient de `Host(g).url`, lue à
    /// l'assemblage, et ne transite jamais ici.
    #[must_use]
    pub fn new(host: SharedHost, identity: Identity, subject: String, context: String, role: String) -> Self {
        Self {
            host,
            identity,
            subject,
            context,
            role,
        }
    }
}

impl std::fmt::Debug for SeaHostMcpAuth {
    /// Opaque par construction : l'objet ne porte ni jeton ni URL, et ne rend rien qui puisse y
    /// mener.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SeaHostMcpAuth").finish_non_exhaustive()
    }
}

impl McpAuth for SeaHostMcpAuth {
    async fn headers(&self) -> Result<McpHeaders, McpError> {
        // Principal relu à chaque appel (DSD `FR-4`) : un droit retiré entre deux appels est
        // effectif au second. Une identité indéchiffrable est un refus, pas un repli.
        let Ok(caller) = self.identity.resolve(&self.subject).await else {
            return Err(McpError::Failed(HOST_DENIED.to_owned()));
        };
        let request = McpRequest {
            context: &self.context,
            role: &self.role,
        };
        // Autorité relue à chaque appel, jeton jamais gardé entre deux appels : seul un
        // `Granted` non expiré rend un en-tête ; `Unprovided` (rôle non pourvu), expiration
        // et panne rendent la phrase fixe, sans texte de l'hôte.
        match self.host.resolve_mcp(&request, &caller).await {
            Ok(McpDecision::Granted(grant)) if !grant.is_expired(chrono::Utc::now()) => {
                Ok(McpHeaders::new().with_bearer(&grant.token))
            }
            _ => Err(McpError::Failed(HOST_DENIED.to_owned())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lymaille::host::HostError;
    use crate::lymaille::store::test_support::test_db;
    use crate::users::membership::{self, sync_group_memberships};
    use crate::users::resolve_user;
    use sea_orm::ActiveValue::{NotSet, Set};
    use serde_json::json;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // ── Fixtures ────────────────────────────────────────────────────────────

    /// Base neuve migrée (SQLite mémoire, ou PostgreSQL si `MIRYAD_TEST_PG_URL`).
    async fn db_migre() -> DatabaseConnection {
        test_db().await
    }

    /// Compte local de test et principal non-admin correspondant.
    async fn compte(db: &DatabaseConnection, subject: &str) -> CallerPrincipal {
        let account = resolve_user(db, subject, None)
            .await
            .expect("compte local de test cree");
        CallerPrincipal {
            user_id: account.id,
            subject: subject.to_string(),
            is_admin: false,
        }
    }

    /// Ligne de `miryad_lymaille_mcp_servers` en `auth_mode = none` (le droit d'usage ne regarde
    /// que `name` et `allowed_groups`).
    async fn inserer_serveur(db: &DatabaseConnection, nom: &str, groupes: serde_json::Value) {
        let actif = mcp_server::ActiveModel {
            id: NotSet,
            name: Set(nom.to_string()),
            url: Set("https://mcp.example/".to_string()),
            request_timeout_secs: Set(30),
            connect_timeout_secs: Set(5),
            auth_mode: Set("none".to_string()),
            auth_ref: Set(None),
            allowed_groups: Set(groupes),
            created_at: Set(chrono::Utc::now()),
        };
        actif.insert(db).await.expect("ligne mcp_servers de test");
    }

    /// `Model` en mémoire pour les tests qui ne lisent pas la table.
    fn modele(
        nom: &str,
        auth_mode: &str,
        auth_ref: Option<&str>,
        groupes: serde_json::Value,
    ) -> mcp_server::Model {
        mcp_server::Model {
            id: 1,
            name: nom.to_string(),
            url: "https://mcp.example/".to_string(),
            request_timeout_secs: 30,
            connect_timeout_secs: 5,
            auth_mode: auth_mode.to_string(),
            auth_ref: auth_ref.map(str::to_owned),
            allowed_groups: groupes,
            created_at: chrono::Utc::now(),
        }
    }

    /// Résolveur scripté : chaque `Some(v)` rend `v`, chaque `None` simule la référence
    /// inconnue. Le compteur prouve l'absence de cache : une résolution par `headers()`.
    struct ScriptedResolver {
        appels: AtomicUsize,
        valeurs: std::sync::Mutex<VecDeque<Option<String>>>,
    }

    impl ScriptedResolver {
        fn with(valeurs: Vec<Option<String>>) -> Arc<Self> {
            Arc::new(Self {
                appels: AtomicUsize::new(0),
                valeurs: std::sync::Mutex::new(valeurs.into()),
            })
        }

        fn appels(&self) -> usize {
            self.appels.load(Ordering::SeqCst)
        }
    }

    impl SecretResolver for ScriptedResolver {
        fn resolve(&self, reference: &str) -> Result<Zeroizing<String>, LymailleError> {
            let _ = self.appels.fetch_add(1, Ordering::SeqCst);
            match self
                .valeurs
                .lock()
                .expect("verrou du resolverscripte")
                .pop_front()
            {
                Some(Some(valeur)) => Ok(Zeroizing::new(valeur)),
                Some(None) | None => Err(LymailleError::SecretUnresolved(reference.to_string())),
            }
        }
    }

    /// Hôte scripté : rend ses décisions dans l'ordre, `Unprovided` ensuite ; le compteur prouve
    /// la relecture à chaque appel.
    struct ScriptedHost {
        appels: AtomicUsize,
        decisions: std::sync::Mutex<VecDeque<Result<McpDecision, HostError>>>,
    }

    impl ScriptedHost {
        fn with(decisions: Vec<Result<McpDecision, HostError>>) -> Arc<Self> {
            Arc::new(Self {
                appels: AtomicUsize::new(0),
                decisions: std::sync::Mutex::new(decisions.into()),
            })
        }

        fn appels(&self) -> usize {
            self.appels.load(Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl LymailleHost for ScriptedHost {
        async fn resolve_mcp(
            &self,
            _request: &McpRequest<'_>,
            _caller: &CallerPrincipal,
        ) -> Result<McpDecision, HostError> {
            let _ = self.appels.fetch_add(1, Ordering::SeqCst);
            self.decisions
                .lock()
                .expect("verrou de l'hote scripte")
                .pop_front()
                .unwrap_or(Ok(McpDecision::Unprovided))
        }
    }

    fn grant(url: &str, token: &str) -> McpGrant {
        McpGrant {
            url: url.to_string(),
            token: Zeroizing::new(token.to_string()),
            expires_at: None,
        }
    }

    fn grant_expire(url: &str, token: &str, expires_at: chrono::DateTime<chrono::Utc>) -> McpGrant {
        McpGrant {
            url: url.to_string(),
            token: Zeroizing::new(token.to_string()),
            expires_at: Some(expires_at),
        }
    }

    // ── Scenarios « table » (DEC-20) ────────────────────────────────────────

    /// Scenario « un groupe autorise, l'appartenance est relue » : `Ok` tant qu'`alice` est
    /// membre de `ops`, `Forbidden("mcp server unavailable")` dès qu'elle en est retirée —
    /// relecture en base à chaque appel, aucun cache.
    #[tokio::test]
    async fn un_groupe_autorise_lappartenance_est_relu() {
        let db = db_migre().await;
        let alice = compte(&db, "alice").await;
        sync_group_memberships(&db, alice.user_id, &["ops".to_string()])
            .await
            .expect("alice membre de ops");
        inserer_serveur(&db, "srv", json!(["ops"])).await;

        let accord = server_for(&db, &alice, "srv").await;
        assert!(
            accord.is_ok(),
            "membre de `ops`, alice est autorisee : {accord:?}"
        );

        membership::Entity::delete_many()
            .filter(membership::Column::UserId.eq(alice.user_id))
            .exec(&db)
            .await
            .expect("retrait des appartenances d'alice");

        let refus = server_for(&db, &alice, "srv").await;
        assert!(
            matches!(&refus, Err(LymailleError::Forbidden(message))
                if message == SERVER_UNAVAILABLE),
            "retiree du groupe, alice est refusee au deuxieme appel : {refus:?}"
        );
    }

    /// Scenario « absent et non autorisé se confondent » : deux erreurs strictement identiques,
    /// pas d'oracle d'existence.
    #[tokio::test]
    async fn absent_et_non_autorise_se_confondent() {
        let db = db_migre().await;
        let bob = compte(&db, "bob").await;
        inserer_serveur(&db, "admin_srv", json!(["admin"])).await;

        let non_autorise = server_for(&db, &bob, "admin_srv")
            .await
            .expect_err("un non-admin ne lit pas un serveur `admin`");
        let absent = server_for(&db, &bob, "inexistant")
            .await
            .expect_err("un serveur inexistant refuse pareil");

        match (&non_autorise, &absent) {
            (LymailleError::Forbidden(premier), LymailleError::Forbidden(second)) => {
                assert_eq!(premier, second, "le texte d'erreur est identique");
                assert_eq!(premier, SERVER_UNAVAILABLE);
            }
            autre => panic!("deux `Forbidden` identiques attendues, obtenu : {autre:?}"),
        }
        assert_eq!(
            non_autorise.to_string(),
            absent.to_string(),
            "Display identique : aucune discrimination entre absent et non autorise"
        );
    }

    /// Scenario « all et admin » : un non-admin authentifié lit `["all"]`, pas `["admin"]`.
    #[tokio::test]
    async fn all_et_admin() {
        let db = db_migre().await;
        let bob = compte(&db, "bob").await;
        inserer_serveur(&db, "allsrv", json!(["all"])).await;
        inserer_serveur(&db, "adminsrv", json!(["admin"])).await;

        let accord = server_for(&db, &bob, "allsrv").await;
        assert!(
            accord.is_ok(),
            "`all` est accordé à tout authentifié : {accord:?}"
        );

        let refus = server_for(&db, &bob, "adminsrv").await;
        assert!(
            matches!(&refus, Err(LymailleError::Forbidden(message))
                if message == SERVER_UNAVAILABLE),
            "`admin` est refuse a un non-admin : {refus:?}"
        );
    }

    // ── Scenarios « SeaMcpAuth / build_auth » ───────────────────────────────

    /// Scenario « le secret statique est résolu à chaque connexion » : le résolveur est appelé
    /// une fois par `headers()` (aucun cache) ; chaque en-tête porte la valeur que le résolveur
    /// vient de rendre (`McpHeaders` n'affiche que la présence du porteur, l'injection directe
    /// de la valeur résolue est le contrat vérifié ici par le compteur).
    #[tokio::test]
    async fn le_secret_statique_est_resolu_a_chaque_connexion() {
        let resolver = ScriptedResolver::with(vec![
            Some("valeur-courante-1".to_string()),
            Some("valeur-courante-2".to_string()),
        ]);
        let auth = SeaMcpAuth::new_static(resolver.clone(), "MA_REF".to_string());

        let premier = auth.headers().await.expect("premiers en-têtes statiques");
        let second = auth.headers().await.expect("deuxièmes en-têtes statiques");

        assert_eq!(
            resolver.appels(),
            2,
            "la référence est résolue à chaque headers(), jamais cachee"
        );
        for entete in [&premier, &second] {
            assert!(
                format!("{entete:?}").contains("bearer: true"),
                "l'en-tête porte un porteur : {entete:?}"
            );
        }
    }

    /// Scenario « une référence introuvable ne fuit pas » : `McpError` fixe, sans le nom de la
    /// référence ni la valeur.
    #[tokio::test]
    async fn une_reference_introuvable_ne_fuit_pas() {
        let resolver = ScriptedResolver::with(vec![None]);
        let auth = SeaMcpAuth::new_static(resolver, "REF-MISTERIEUSE".to_string());

        let erreur = auth
            .headers()
            .await
            .expect_err("une référence inconnue doit refuser");
        let rendu = format!("{erreur}");
        assert!(
            rendu.contains(AUTH_REF_UNRESOLVED),
            "l'erreur est la phrase fixe : {rendu}"
        );
        assert!(
            !rendu.contains("REF-MISTERIEUSE"),
            "le nom de la référence ne fuit pas : {rendu}"
        );
        assert!(
            !format!("{erreur:?}").contains("REF-MISTERIEUSE"),
            "ni dans le Debug : {erreur:?}"
        );
    }

    /// Scenario « forward refuse hors session interactive » : `Forbidden` au texte exact, et
    /// **non retryable** (contrat du step `subagent`).
    #[test]
    fn forward_refuse_hors_session_interactive() {
        let srv = modele("fwd", "forward", None, json!(["all"]));
        let rendu = build_auth(&srv, ScriptedResolver::with(vec![]), None);
        match rendu {
            Err(erreur) => {
                match &erreur {
                    LymailleError::Forbidden(message) => {
                        assert_eq!(message, FORWARD_NEEDS_SESSION);
                    }
                    autre => panic!("`Forbidden` attendue, obtenu : {autre:?}"),
                }
                assert!(
                    !erreur.retryable(),
                    "le refus du forward hors session interactive est non retryable"
                );
            }
            Ok(auth) => panic!("un forward sans jeton interactif doit être refusé : {auth:?}"),
        }
    }

    /// Scenario « Debug est opaque » : ni le jeton forward ni la référence statique ne
    /// transparaissent dans `format!("{:?}")`.
    #[test]
    fn debug_est_opaque() {
        let forward = SeaMcpAuth::new_forward(Zeroizing::new("s3cret".to_string()));
        let rendu_forward = format!("{forward:?}");
        assert!(
            !rendu_forward.contains("s3cret"),
            "le jeton transite par le Debug : {rendu_forward}"
        );

        let resolver = ScriptedResolver::with(vec![Some("s3cret".to_string())]);
        let statique = SeaMcpAuth::new_static(resolver, "REF-SECRETE".to_string());
        let rendu_statique = format!("{statique:?}");
        assert!(
            !rendu_statique.contains("REF-SECRETE") && !rendu_statique.contains("s3cret"),
            "ni référence ni valeur dans le Debug statique : {rendu_statique}"
        );
    }

    // ── Scenarios DEC-27 (hôte) ─────────────────────────────────────────────

    /// Scenario « l'hôte pourvoit le rôle, il a la priorité sur la table » : `Host` avec l'URL
    /// et le jeton de l'hôte ; la table n'est pas lue — prouvé par un second appel où aucune
    /// ligne `mcp_servers` n'existe : seul un chemin qui ne la consulte pas peut aboutir.
    #[tokio::test]
    async fn lhote_pourvoit_le_role_il_a_la_priorite_sur_la_table() {
        let db = db_migre().await;
        let alice = compte(&db, "alice").await;
        inserer_serveur(&db, "sandbox", json!(["all"])).await;

        let hote = ScriptedHost::with(vec![
            Ok(McpDecision::Granted(grant("https://sbx/mcp", "jeton-hote"))),
            Ok(McpDecision::Granted(grant("https://sbx/mcp", "jeton-hote"))),
        ]);
        let lien = resolve_role(Some(&*hote), &db, &alice, "G_prod", "sandbox", Some("sandbox"))
            .await
            .expect("l'hôte pourvoit le rôle");
        match lien {
            RoleBinding::Host(accord) => {
                assert_eq!(accord.url, "https://sbx/mcp", "l'URL vient de l'hôte");
                assert_eq!(&*accord.token, "jeton-hote", "le jeton vient de l'hôte");
            }
            RoleBinding::Table(ligne) => {
                panic!("la table n'a pas la priorité, obtenu son serveur : {ligne:?}")
            }
        }

        // Deuxième clause « la table n'est pas lue » : plus aucune ligne `mcp_servers` et le
        // même appel aboutit encore en `Host` — un repli sur `server_for` échouerait.
        mcp_server::Entity::delete_many()
            .exec(&db)
            .await
            .expect("table vidée");
        let sans_table = resolve_role(Some(&*hote), &db, &alice, "G_prod", "sandbox", Some("sandbox"))
            .await
            .expect("l'hôte pourvoit toujours, table vidée");
        assert!(
            matches!(sans_table, RoleBinding::Host(_)),
            "la table n'est pas consultée quand l'hôte pourvoit : {sans_table:?}"
        );
    }

    /// Scenario « un rôle non pourvu retombe sur la liaison classique » : `Unprovided` →
    /// `Table(serveur)` avec `bound_server` ; `Forbidden` sans `bound_server`.
    #[tokio::test]
    async fn un_role_non_pourvu_retombe_sur_la_liaison_classique() {
        let db = db_migre().await;
        let alice = compte(&db, "alice").await;
        inserer_serveur(&db, "srv", json!(["all"])).await;

        let hote = ScriptedHost::with(vec![Ok(McpDecision::Unprovided)]);
        {
            let lien = resolve_role(Some(&*hote), &db, &alice, "G_prod", "srv", Some("srv"))
                .await
                .expect("Unprovided retombe sur la liaison classique");
            match lien {
                RoleBinding::Table(ligne) => assert_eq!(ligne.name, "srv"),
                RoleBinding::Host(accord) => {
                    panic!("repli sur la table attendu, obtenu un grant : {accord:?}")
                }
            }
        }

        let hote_sans_liaison = ScriptedHost::with(vec![Ok(McpDecision::Unprovided)]);
        let refus = resolve_role(Some(&*hote_sans_liaison), &db, &alice, "G_prod", "srv", None)
            .await
            .expect_err("Unprovided sans bound_server refuse");
        assert!(
            matches!(&refus, LymailleError::Forbidden(message)
                if message == SERVER_UNAVAILABLE),
            "sans liaison liée, même texte de refus : {refus:?}"
        );
    }

    /// Scenario « un refus de l'hôte n'a pas de repli » : `Denied` ferme l'appel — `Forbidden`
    /// alors que la table, elle, aurait autorisé (aucune consultation de repli).
    #[tokio::test]
    async fn un_refus_de_lhote_na_pas_de_repli() {
        let db = db_migre().await;
        let alice = compte(&db, "alice").await;
        inserer_serveur(&db, "srv", json!(["all"])).await;

        let hote = ScriptedHost::with(vec![Ok(McpDecision::Denied)]);
        let refus = resolve_role(Some(&*hote), &db, &alice, "G_prod", "srv", Some("srv"))
            .await
            .expect_err("`Denied` est un échec fermé, sans repli sur la table");
        assert!(
            matches!(&refus, LymailleError::Forbidden(message)
                if message == SERVER_UNAVAILABLE),
            "le refus de l'hôte rend le texte fermé : {refus:?}"
        );
    }

    /// Scenario « la panne de l'hôte refuse et ne fuit pas » : `Err(HostError)` rend le même
    /// refus qu'un serveur absent ; aucun texte d'hôte dans l'erreur (`HostError` est sans
    /// contenu, `./host.sdd`) — et la trace est la phrase fixe du source, lue à la relecture.
    #[tokio::test]
    async fn la_panne_de_lhote_refuse_et_ne_fuit_pas() {
        let db = db_migre().await;
        let alice = compte(&db, "alice").await;
        inserer_serveur(&db, "srv", json!(["all"])).await;

        let hote = ScriptedHost::with(vec![Err(HostError)]);
        let refus = resolve_role(Some(&*hote), &db, &alice, "G_prod", "srv", Some("srv"))
            .await
            .expect_err("la panne de l'hôte est un échec fermé");
        let absent = server_for(&db, &alice, "inexistant")
            .await
            .expect_err("serveur absent de référence");
        assert_eq!(
            refus.to_string(),
            absent.to_string(),
            "panne d'hôte et serveur absent rendent le même texte"
        );
        assert!(
            !refus.to_string().contains("host resolution"),
            "rien du texte de l'hôte ne remonte : {refus}"
        );
    }

    /// Scenario « un jeton déjà expiré est refusé » : `resolve_role` → `Forbidden` (échec fermé,
    /// pas de repli) ; `SeaHostMcpAuth::headers()` → `McpError` fixe.
    #[tokio::test]
    async fn un_jeton_deja_expire_est_refuse() {
        let db = db_migre().await;
        let alice = compte(&db, "alice").await;
        let passe = chrono::Utc::now() - chrono::Duration::hours(1);
        let expiré = grant_expire("https://sbx/mcp", "jeton-expire", passe);
        let hote = ScriptedHost::with(vec![
            Ok(McpDecision::Granted(expiré.clone())),
            Ok(McpDecision::Granted(expiré)),
        ]);

        let refus = resolve_role(Some(&*hote), &db, &alice, "G_prod", "sandbox", Some("sandbox"))
            .await
            .expect_err("un grant expiré est refusé à l'assemblage");
        assert!(
            matches!(&refus, LymailleError::Forbidden(message)
                if message == SERVER_UNAVAILABLE),
            "expiration = échec fermé : {refus:?}"
        );

        let auth = SeaHostMcpAuth::new(
            hote.clone(),
            Identity::new(db.clone()),
            "alice".to_string(),
            "G_prod".to_string(),
            "sandbox".to_string(),
        );
        let erreur = auth
            .headers()
            .await
            .expect_err("un grant expiré refuse aussi à l'appel");
        assert!(
            format!("{erreur}").contains(HOST_DENIED)
                && !format!("{erreur}").contains("jeton-expire")
                && !format!("{erreur}").contains("sbx"),
            "McpError fixe, sans jeton ni URL : {erreur}"
        );
    }

    /// Scenario « le jeton est redemandé à chaque appel » : deux `headers()` déclenchent deux
    /// `resolve_mcp` (compteur), chacun avec le jeton courant de l'hôte — rien n'est gardé.
    #[tokio::test]
    async fn le_jeton_est_redemande_a_chaque_appel() {
        let db = db_migre().await;
        let _alice = compte(&db, "alice").await; // ligne miryad_users : précondition du resolve d’identité
        let hote = ScriptedHost::with(vec![
            Ok(McpDecision::Granted(grant("https://sbx/mcp", "jeton-1"))),
            Ok(McpDecision::Granted(grant("https://sbx/mcp", "jeton-2"))),
        ]);
        let auth = SeaHostMcpAuth::new(
            hote.clone(),
            Identity::new(db.clone()),
            "alice".to_string(),
            "G_prod".to_string(),
            "sandbox".to_string(),
        );

        let premier = auth.headers().await.expect("premiers en-têtes de l'hôte");
        let second = auth.headers().await.expect("deuxièmes en-têtes de l'hôte");

        assert_eq!(
            hote.appels(),
            2,
            "le jeton est redemandé à chaque headers(), jamais gardé"
        );
        for entete in [&premier, &second] {
            assert!(
                format!("{entete:?}").contains("bearer: true"),
                "l'en-tête courant porte le jeton rendu par l'hôte : {entete:?}"
            );
        }
    }

    /// Scenario « l'autorité de l'hôte est relue à chaque appel » : `Granted` puis `Denied` →
    /// `Ok` puis `McpError` fixe.
    #[tokio::test]
    async fn lautorite_de_lhote_est_relu_a_chaque_appel() {
        let db = db_migre().await;
        let _alice = compte(&db, "alice").await; // ligne miryad_users : précondition du resolve d’identité
        let hote = ScriptedHost::with(vec![
            Ok(McpDecision::Granted(grant("https://sbx/mcp", "jeton"))),
            Ok(McpDecision::Denied),
        ]);
        let auth = SeaHostMcpAuth::new(
            hote.clone(),
            Identity::new(db.clone()),
            "alice".to_string(),
            "G_prod".to_string(),
            "sandbox".to_string(),
        );

        let accord = auth.headers().await.expect("l'hôte accorde au premier appel");
        assert!(format!("{accord:?}").contains("bearer: true"));

        let refus = auth
            .headers()
            .await
            .expect_err("l'autorité est relue : Denied au second appel");
        assert!(
            format!("{refus}").contains(HOST_DENIED),
            "le refus de l'hôte rend la phrase fixe : {refus}"
        );
    }

    /// Scenario « Debug de `SeaHostMcpAuth` est opaque » : la sortie ne contient ni jeton ni URL.
    #[tokio::test]
    async fn debug_de_seahostmcpauth_est_opaque() {
        let db = db_migre().await;
        let _alice = compte(&db, "alice").await; // ligne miryad_users : précondition du resolve d’identité
        let hote = ScriptedHost::with(vec![Ok(McpDecision::Granted(grant(
            "https://sbx.example/mcp",
            "s3cret",
        )))]);
        let auth = SeaHostMcpAuth::new(
            hote,
            Identity::new(db.clone()),
            "alice".to_string(),
            "G_prod".to_string(),
            "sandbox".to_string(),
        );
        auth.headers().await.expect("en-têtes de l'hôte");

        let rendu = format!("{auth:?}");
        assert!(
            !rendu.contains("s3cret") && !rendu.contains("sbx.example"),
            "le Debug de SeaHostMcpAuth ne contient ni jeton ni URL : {rendu}"
        );
    }
}
