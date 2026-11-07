//! Montage d'un client MCP par rôle, commun à la session interactive et au step batch
//! (`src/lymaille/mcp_mount.sdd`, DSD `DEC-27`, `DEC-20`, `FR-9`, `FR-14`). Ce module porte la
//! couture des connecteurs MCP (fabrique de production et adaptateurs objets), déplacée de
//! `interactive/assemble.rs`, et `mount_role` : résolution de la liaison du rôle, authentification,
//! connecteur, module Rhai accordé. La politique d'un rôle refusé reste chez l'appelant.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use sea_orm::DatabaseConnection;
use zeroize::Zeroizing;

use ::lymaille::GrantedModule;
use ::lymaille::McpAuth;
use ::lymaille::McpClientConfig;
use ::lymaille::McpConnector;
use ::lymaille::McpEnv;
use ::lymaille::McpError;
use ::lymaille::McpHeaders;
use ::lymaille::McpLister;
use ::lymaille::McpTool;
use ::lymaille::RmcpConnector;
use ::lymaille::mcp_module;

use super::error::LymailleError;
use super::host::SharedHost;
use super::identity::{CallerPrincipal, Identity};
use super::mcp_access::{RoleBinding, SeaHostMcpAuth, build_auth, resolve_role};
use super::secret::SecretResolver;

/// Auth MCP sous forme objet : [`McpAuth`] de `lymaille` n'est pas object-safe (méthode
/// `impl Future`), le pas d'indirection est posé ici.
pub(crate) trait DynMcpAuth: Send + Sync {
    fn headers_dyn(&self) -> Pin<Box<dyn Future<Output = Result<McpHeaders, McpError>> + Send + '_>>;
}

impl<A> DynMcpAuth for A
where
    A: McpAuth + Sync + 'static,
{
    fn headers_dyn(&self) -> Pin<Box<dyn Future<Output = Result<McpHeaders, McpError>> + Send + '_>> {
        Box::pin(self.headers())
    }
}

/// Auth en forme objet pour le connecteur production : `RmcpConnector` est générique sur son
/// auth, la liste des rôles ne peut pas monomorphiser deux auth différentes par appel — le pas
/// d'indirection inverse (objet → `impl McpAuth`) est posé ici.
struct BoxAuth(Option<Arc<dyn DynMcpAuth>>);

impl std::fmt::Debug for BoxAuth {
    /// Opaque : l'auth sous-jacente peut porter un jeton.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BoxAuth").finish_non_exhaustive()
    }
}

impl McpAuth for BoxAuth {
    fn headers(&self) -> impl Future<Output = Result<McpHeaders, McpError>> + Send {
        // Le futur retourné par `McpAuth::headers` ne peut pas emprunter `self` (RPIT sans
        // `use<'_>`) : l'`Arc` est déplacé dans le futur, qui le lâche à sa fin.
        let auth = self.0.clone();
        Box::pin(async move {
            match auth {
                Some(auth) => auth.headers_dyn().await,
                None => Ok(McpHeaders::new()),
            }
        })
    }
}

/// Connecteur MCP sous forme objet : [`McpConnector`] de `lymaille` n'est pas object-safe ; le
/// pas d'indirection est posé ici (même raison que `DynInference`).
pub(crate) trait DynMcpConnector: Send + Sync {
    fn list_dyn(&self) -> Pin<Box<dyn Future<Output = Result<Vec<McpTool>, McpError>> + Send + '_>>;
    fn call_dyn(
        &self,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value, McpError>> + Send + '_>>;
    fn reconnect_dyn(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;
}

impl<C> DynMcpConnector for C
where
    C: McpConnector + 'static,
{
    fn list_dyn(&self) -> Pin<Box<dyn Future<Output = Result<Vec<McpTool>, McpError>> + Send + '_>> {
        Box::pin(self.list_tools())
    }

    fn call_dyn(
        &self,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value, McpError>> + Send + '_>> {
        // Le futur de `call_tool` ne capture pas les références d'appel (RPITIT sans
        // `use<'_, '_>`) : nom et arguments sont copiés dans le futur, qui ne garde que l'emprunt
        // de `self`.
        let name = name.to_owned();
        let arguments = arguments.clone();
        Box::pin(async move { self.call_tool(&name, &arguments).await })
    }

    fn reconnect_dyn(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(self.reconnect())
    }
}

/// Connecteur objet rendu compatible avec le générique [`McpConnector`] de `lymaille` — la voie
/// d'injection des clients MCP par rôle dans `mcp_module` (la tâche `[?]` de la spec, tranchée
/// dans le source de `lymaille` 0.1.0 : `mcp_module` est générique sur le connecteur, un seul
/// type concret peut donc passer, les autres étant derrière cet adaptateur).
pub(crate) struct DynConnector(pub(crate) Arc<dyn DynMcpConnector>);

impl McpLister for DynConnector {
    fn list_tools(&self) -> impl Future<Output = Result<Vec<McpTool>, McpError>> + Send {
        self.0.list_dyn()
    }
}

impl McpConnector for DynConnector {
    fn call_tool(
        &self,
        name: &str,
        arguments: &serde_json::Value,
    ) -> impl Future<Output = Result<serde_json::Value, McpError>> + Send {
        self.0.call_dyn(name, arguments)
    }

    fn reconnect(&self) -> impl Future<Output = ()> + Send {
        self.0.reconnect_dyn()
    }
}

/// Fabrique des connecteurs MCP (couture de test) : production → [`RmcpConnector`] ; les tests
/// injectent un connecteur espion — la voie par défaut n'a pas de serveur MCP factice publié par
/// `lymaille` dans le graphe de la crate (`Scenario` « le jeton forward n'atteint que son
/// serveur », tâche `[?]` de la spec tranchée par le connecteur injecté).
pub(crate) trait McpConnectorFactory: Send + Sync {
    /// Connecteur vers `url`, avec les timeouts du serveur et l'auth du rôle (`None` = aucun
    /// en-tête). La clé et le jeton ne figurent dans aucune trace.
    fn make(
        &self,
        url: &str,
        request_timeout: Duration,
        connect_timeout: Duration,
        auth: Option<Arc<dyn DynMcpAuth>>,
    ) -> Arc<dyn DynMcpConnector>;
}

/// Fabrique de production : un [`RmcpConnector`] par rôle (streamable HTTP, TLS par
/// `install_crypto_provider`).
pub(crate) struct RmcpConnectorFactory;

impl McpConnectorFactory for RmcpConnectorFactory {
    fn make(
        &self,
        url: &str,
        request_timeout: Duration,
        connect_timeout: Duration,
        auth: Option<Arc<dyn DynMcpAuth>>,
    ) -> Arc<dyn DynMcpConnector> {
        let config = McpClientConfig::new(url)
            .with_request_timeout(request_timeout)
            .with_connect_timeout(connect_timeout);
        Arc::new(RmcpConnector::new(config, Arc::new(BoxAuth(auth))))
    }
}

/// Timeouts de connexion MCP par défaut quand la ligne ne les fixe pas (relus du modèle sinon).
pub(crate) const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
pub(crate) const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// secondes de la colonne → `Duration`. Les hooks de `../definitions/mcp_server.rs` garantissent
/// 1..=600 et 1..=60 ; une ligne hors contrat retombe sur le défaut, jamais un `unwrap`.
pub(crate) fn timeout(seconds: i32) -> Duration {
    u64::try_from(seconds).map_or(DEFAULT_REQUEST_TIMEOUT, Duration::from_secs)
}

/// Dépendances du montage (`Must` de `./mcp_mount.sdd`) : emprunts seulement, le constructeur de
/// l'appelant garde la propriété.
pub(crate) struct MountDeps<'a> {
    /// Base de l'application (résolution des liaisons, relecture du droit d'usage).
    pub(crate) db: &'a DatabaseConnection,
    /// Résolveur des références d'authentification des serveurs de la table.
    pub(crate) secrets: &'a Arc<dyn SecretResolver>,
    /// Hôte applicatif optionnel : sans lui, seule la liaison classique est possible.
    pub(crate) host: Option<&'a SharedHost>,
    /// Couture des connecteurs : [`RmcpConnectorFactory`] en production, espion en test.
    pub(crate) mcp: &'a dyn McpConnectorFactory,
}

/// Connecteur du rôle selon sa liaison (étapes 2 et 3 de `mount_role`). Une liaison `Host` sans
/// hôte dans les dépendances est un échec fermé (`Config`) : aucun connecteur n'est créé.
fn connector_for(
    deps: &MountDeps<'_>,
    caller: &CallerPrincipal,
    context: &str,
    role: &str,
    binding: RoleBinding,
    interactive_token: Option<&Zeroizing<String>>,
) -> Result<Arc<dyn DynMcpConnector>, LymailleError> {
    match binding {
        RoleBinding::Host(grant) => {
            let host = deps
                .host
                .ok_or_else(|| LymailleError::Config("host binding without a host".to_string()))?;
            // Le jeton du `Grant` n'est pas lu ici : l'auth le redemande à chaque appel.
            let auth: Arc<dyn DynMcpAuth> = Arc::new(SeaHostMcpAuth::new(
                Arc::clone(host),
                Identity::new(deps.db.clone()),
                caller.subject.clone(),
                context.to_owned(),
                role.to_owned(),
            ));
            Ok(deps.mcp.make(
                &grant.url,
                DEFAULT_REQUEST_TIMEOUT,
                DEFAULT_CONNECT_TIMEOUT,
                Some(auth),
            ))
        }
        RoleBinding::Table(model) => {
            let auth = build_auth(&model, Arc::clone(deps.secrets), interactive_token.cloned())?
                .map(|sea_auth| Arc::new(sea_auth) as Arc<dyn DynMcpAuth>);
            Ok(deps.mcp.make(
                &model.url,
                timeout(model.request_timeout_secs),
                timeout(model.connect_timeout_secs),
                auth,
            ))
        }
    }
}

/// Monte le rôle `role` du contexte `context` : résolution de la liaison, connecteur, module
/// Rhai accordé. Un refus, une panne ou une liaison incohérente remonte en erreur ; la politique
/// (fatal ou avis) reste chez l'appelant.
pub(crate) async fn mount_role(
    deps: &MountDeps<'_>,
    caller: &CallerPrincipal,
    context: &str,
    role: &str,
    bound_server: Option<&str>,
    interactive_token: Option<&Zeroizing<String>>,
    env: McpEnv,
) -> Result<GrantedModule, LymailleError> {
    let binding = resolve_role(
        deps.host.map(|host| &**host),
        deps.db,
        caller,
        context,
        role,
        bound_server,
    )
    .await?;
    let connector = connector_for(deps, caller, context, role, binding, interactive_token)?;
    let module = mcp_module(role, Arc::new(DynConnector(connector)), env)
        .await
        .map_err(|error| LymailleError::Store(error.to_string()))?;
    GrantedModule::new(role, module.into())
        .ok_or_else(|| LymailleError::InvalidDefinition(format!("invalid module name: {role}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lymaille::EventHub;
    use crate::lymaille::definitions::mcp_server;
    use crate::lymaille::host::{HostError, LymailleHost, McpDecision, McpGrant, McpRequest};
    use crate::lymaille::store::test_support::test_db;
    use crate::users::resolve_user;
    use ::lymaille::{EventSink, McpRetryPolicy, Redactor, SessionId, SystemClock};
    use sea_orm::ActiveModelTrait;
    use sea_orm::ActiveValue::{NotSet, Set};
    use serde_json::json;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // ── Fixtures ────────────────────────────────────────────────────────────

    /// Base neuve migrée (SQLite mémoire, ou PostgreSQL si `MIRYAD_TEST_PG_URL`).
    async fn db_migre() -> DatabaseConnection {
        test_db().await
    }

    /// Compte local de test et principal non-admin relu par `Identity`.
    async fn compte(db: &DatabaseConnection, subject: &str) -> CallerPrincipal {
        let _account = resolve_user(db, subject, None)
            .await
            .expect("compte local de test cree");
        Identity::new(db.clone())
            .resolve(subject)
            .await
            .expect("principal relu")
    }

    /// Ligne de `mcp_servers` ouverte à `all`, d'URL `https://<nom>.example/mcp`.
    async fn inserer_serveur(db: &DatabaseConnection, nom: &str, auth_mode: &str, auth_ref: Option<&str>) {
        let actif = mcp_server::ActiveModel {
            id: NotSet,
            name: Set(nom.to_string()),
            url: Set(format!("https://{nom}.example/mcp")),
            request_timeout_secs: Set(30),
            connect_timeout_secs: Set(5),
            auth_mode: Set(auth_mode.to_string()),
            auth_ref: Set(auth_ref.map(str::to_owned)),
            allowed_groups: Set(json!(["all"])),
            created_at: Set(chrono::Utc::now()),
        };
        actif.insert(db).await.expect("ligne mcp_servers de test");
    }

    /// Résolveur de secrets de test : toute référence rend la même valeur.
    struct FixedSecret;

    impl SecretResolver for FixedSecret {
        fn resolve(&self, _reference: &str) -> Result<Zeroizing<String>, LymailleError> {
            Ok(Zeroizing::new("cle-test".to_owned()))
        }
    }

    /// Hôte factice : la décision du n-ième appel (1-based) vient de `rendre` ; le compteur
    /// prouve la relecture de l'autorité à chaque appel.
    struct ScriptedHost {
        appels: AtomicUsize,
        rendre: Box<dyn Fn(usize) -> Result<McpDecision, HostError> + Send + Sync>,
    }

    impl ScriptedHost {
        fn new(
            rendre: impl Fn(usize) -> Result<McpDecision, HostError> + Send + Sync + 'static,
        ) -> Arc<Self> {
            Arc::new(Self {
                appels: AtomicUsize::new(0),
                rendre: Box::new(rendre),
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
            let rang = self.appels.fetch_add(1, Ordering::SeqCst).saturating_add(1);
            (self.rendre)(rang)
        }
    }

    fn grant(url: &str, token: &str, expires_at: Option<chrono::DateTime<chrono::Utc>>) -> McpGrant {
        McpGrant {
            url: url.to_string(),
            token: Zeroizing::new(token.to_string()),
            expires_at,
        }
    }

    /// Fabrique espionne : chaque connecteur rendu est noté (URL) ; à chaque relevé d'en-têtes
    /// fait par le connecteur (listage au montage, appel d'outil), la présence d'un porteur est
    /// notée. `McpHeaders` ne montre que la présence du jeton, pas sa valeur.
    #[derive(Default)]
    struct SpyFactory {
        montes: Mutex<Vec<String>>,
        releves: Arc<Mutex<Vec<(String, bool)>>>,
    }

    impl SpyFactory {
        fn montes(&self) -> Vec<String> {
            self.montes.lock().expect("verrou de lespion").clone()
        }

        fn releves(&self) -> Vec<(String, bool)> {
            self.releves.lock().expect("verrou de lespion").clone()
        }
    }

    impl McpConnectorFactory for SpyFactory {
        fn make(
            &self,
            url: &str,
            _request_timeout: Duration,
            _connect_timeout: Duration,
            auth: Option<Arc<dyn DynMcpAuth>>,
        ) -> Arc<dyn DynMcpConnector> {
            self.montes
                .lock()
                .expect("verrou de lespion")
                .push(url.to_owned());
            Arc::new(SpyConnector {
                url: url.to_owned(),
                auth,
                releves: Arc::clone(&self.releves),
            })
        }
    }

    /// Connecteur espion : sans réseau, relève les en-têtes de son auth à chaque connexion.
    struct SpyConnector {
        url: String,
        auth: Option<Arc<dyn DynMcpAuth>>,
        releves: Arc<Mutex<Vec<(String, bool)>>>,
    }

    impl SpyConnector {
        async fn relever(&self) -> Result<(), McpError> {
            let porteur = match &self.auth {
                Some(auth) => format!("{:?}", auth.headers_dyn().await?).contains("bearer: true"),
                None => false,
            };
            self.releves
                .lock()
                .expect("verrou de lespion")
                .push((self.url.clone(), porteur));
            Ok(())
        }
    }

    impl McpLister for SpyConnector {
        async fn list_tools(&self) -> Result<Vec<McpTool>, McpError> {
            self.relever().await?;
            Ok(vec![McpTool::new("ping", "le ping", json!({"type": "object"}))])
        }
    }

    impl McpConnector for SpyConnector {
        async fn call_tool(
            &self,
            _name: &str,
            _arguments: &serde_json::Value,
        ) -> Result<serde_json::Value, McpError> {
            self.relever().await?;
            Ok(json!({"pong": true}))
        }

        fn reconnect(&self) -> impl Future<Output = ()> + Send {
            std::future::ready(())
        }
    }

    fn env() -> McpEnv {
        McpEnv::new(
            Arc::new(Redactor::new()),
            Arc::new(EventHub::new(None, 16)) as Arc<dyn EventSink>,
            Arc::new(SystemClock::new()),
            SessionId::new("s-montage").expect("identifiant de session de test"),
            McpRetryPolicy::default(),
        )
    }

    /// Erreur attendue d'un montage (le `Result` ne porte pas `Debug` pour le module).
    fn echec_de<T>(resultat: Result<T, LymailleError>) -> LymailleError {
        match resultat {
            Ok(_) => panic!("le montage devait echouer"),
            Err(erreur) => erreur,
        }
    }

    // ── Scenarios ───────────────────────────────────────────────────────────

    /// Scenario « un rôle servi par l'hôte monte un client dont le jeton est redemandé à chaque
    /// appel » : le client vise l'URL du `Grant`, l'espion voit un porteur à chaque relevé
    /// (listage puis deux appels d'outil), et l'hôte est relu à chaque relevé.
    #[tokio::test(flavor = "multi_thread")]
    async fn un_role_servi_par_lhote_monte_un_client_dont_le_jeton_est_redemande_a_chaque_appel() {
        let db = db_migre().await;
        let alice = compte(&db, "alice").await;
        let host = ScriptedHost::new(|rang| {
            Ok(McpDecision::Granted(grant(
                "https://sbx/mcp",
                &format!("jeton-{rang}"),
                None,
            )))
        });
        let shared: SharedHost = host.clone();
        let spy = SpyFactory::default();
        let secrets: Arc<dyn SecretResolver> = Arc::new(FixedSecret);
        let deps = MountDeps {
            db: &db,
            secrets: &secrets,
            host: Some(&shared),
            mcp: &spy,
        };

        let module = mount_role(&deps, &alice, "G_prod", "sandbox", None, None, env())
            .await
            .expect("le rôle pourvu par l'hôte est monté");
        let mut moteur = rhai::Engine::new();
        let _enregistre = moteur.register_static_module("sandbox", module.module().clone());
        let _premier = moteur
            .eval::<rhai::Dynamic>("sandbox::ping(#{})")
            .expect("premier appel d'outil");
        let _second = moteur
            .eval::<rhai::Dynamic>("sandbox::ping(#{})")
            .expect("second appel d'outil");

        assert_eq!(spy.montes(), vec!["https://sbx/mcp".to_string()]);
        assert_eq!(
            spy.releves(),
            vec![
                ("https://sbx/mcp".to_string(), true),
                ("https://sbx/mcp".to_string(), true),
                ("https://sbx/mcp".to_string(), true),
            ]
        );
        // Une décision au montage, puis une par relevé : aucun jeton gardé entre deux relevés.
        assert_eq!(host.appels(), 4);
    }

    /// Scenario « un rôle refusé par l'hôte n'est pas monté » : `Forbidden` fixe, aucun
    /// connecteur créé.
    #[tokio::test(flavor = "multi_thread")]
    async fn un_role_refuse_par_lhote_nest_pas_monte() {
        let db = db_migre().await;
        let bob = compte(&db, "bob").await;
        let shared: SharedHost = ScriptedHost::new(|_| Ok(McpDecision::Denied));
        let spy = SpyFactory::default();
        let secrets: Arc<dyn SecretResolver> = Arc::new(FixedSecret);
        let deps = MountDeps {
            db: &db,
            secrets: &secrets,
            host: Some(&shared),
            mcp: &spy,
        };

        let erreur = echec_de(mount_role(&deps, &bob, "G_prod", "sandbox", None, None, env()).await);

        assert!(
            matches!(&erreur, LymailleError::Forbidden(message) if message == "mcp server unavailable"),
            "refus attendu : {erreur:?}"
        );
        assert!(spy.montes().is_empty(), "aucun connecteur ne doit être créé");
    }

    /// Scenario « un serveur static est monté avec son en-tête » : sans hôte, la table porte le
    /// serveur `static` ; l'espion voit un porteur. La valeur du secret n'est pas observable
    /// par `McpHeaders` (présence seulement) : voir le rapport.
    #[tokio::test(flavor = "multi_thread")]
    async fn un_serveur_static_est_monte_avec_son_en_tete() {
        let db = db_migre().await;
        let alice = compte(&db, "alice").await;
        inserer_serveur(&db, "stat", "static", Some("REF_STAT")).await;
        let spy = SpyFactory::default();
        let secrets: Arc<dyn SecretResolver> = Arc::new(FixedSecret);
        let deps = MountDeps {
            db: &db,
            secrets: &secrets,
            host: None,
            mcp: &spy,
        };

        let _module = mount_role(&deps, &alice, "G_prod", "stat", Some("stat"), None, env())
            .await
            .expect("le serveur static est monté");

        assert_eq!(
            spy.releves(),
            vec![("https://stat.example/mcp".to_string(), true)],
            "le listage au montage porte le secret résolu"
        );
    }

    /// Scenario « un serveur sans authentification est monté sans en-tête ».
    #[tokio::test(flavor = "multi_thread")]
    async fn un_serveur_sans_authentification_est_monte_sans_en_tete() {
        let db = db_migre().await;
        let alice = compte(&db, "alice").await;
        inserer_serveur(&db, "nu", "none", None).await;
        let spy = SpyFactory::default();
        let secrets: Arc<dyn SecretResolver> = Arc::new(FixedSecret);
        let deps = MountDeps {
            db: &db,
            secrets: &secrets,
            host: None,
            mcp: &spy,
        };

        let _module = mount_role(&deps, &alice, "G_prod", "nu", Some("nu"), None, env())
            .await
            .expect("le serveur sans auth est monté");

        assert_eq!(spy.releves(), vec![("https://nu.example/mcp".to_string(), false)]);
    }

    /// Scenario « un serveur forward sans jeton interactif est refusé, puis un module accordé dont
    /// le jeton n'atteint que ce serveur » : `forward` (rôle `fwd`) et `none` (rôle `plain`)
    /// montés avec le même jeton ; seul `fwd` porte le porteur.
    #[tokio::test(flavor = "multi_thread")]
    async fn un_serveur_forward_sans_jeton_interactif_est_refuse() {
        let db = db_migre().await;
        let alice = compte(&db, "alice").await;
        inserer_serveur(&db, "fwd", "forward", None).await;
        inserer_serveur(&db, "plain", "none", None).await;
        let spy = SpyFactory::default();
        let secrets: Arc<dyn SecretResolver> = Arc::new(FixedSecret);
        let deps = MountDeps {
            db: &db,
            secrets: &secrets,
            host: None,
            mcp: &spy,
        };

        let refus = echec_de(mount_role(&deps, &alice, "G_prod", "fwd", Some("fwd"), None, env()).await);
        assert!(
            matches!(&refus, LymailleError::Forbidden(message) if message == "forward auth needs an interactive session"),
            "refus attendu : {refus:?}"
        );
        assert!(spy.montes().is_empty(), "aucun connecteur sans jeton interactif");

        let jeton = Zeroizing::new("jeton-session".to_owned());
        let _fwd = mount_role(&deps, &alice, "G_prod", "fwd", Some("fwd"), Some(&jeton), env())
            .await
            .expect("le serveur forward est monté avec le jeton du tour");
        let _plain = mount_role(
            &deps,
            &alice,
            "G_prod",
            "plain",
            Some("plain"),
            Some(&jeton),
            env(),
        )
        .await
        .expect("le serveur sans auth est monté");

        assert_eq!(
            spy.releves(),
            vec![
                ("https://fwd.example/mcp".to_string(), true),
                ("https://plain.example/mcp".to_string(), false),
            ]
        );
    }

    /// Scenario « un jeton hôte expiré fait échouer le montage » : `Forbidden` fixe, aucun texte
    /// de l'hôte, aucun connecteur.
    #[tokio::test(flavor = "multi_thread")]
    async fn un_jeton_hote_expire_fait_echouer_le_montage() {
        let db = db_migre().await;
        let alice = compte(&db, "alice").await;
        let passe = chrono::Utc::now() - chrono::Duration::seconds(60);
        let shared: SharedHost = ScriptedHost::new(move |_| {
            Ok(McpDecision::Granted(grant(
                "https://sbx/mcp",
                "perime",
                Some(passe),
            )))
        });
        let spy = SpyFactory::default();
        let secrets: Arc<dyn SecretResolver> = Arc::new(FixedSecret);
        let deps = MountDeps {
            db: &db,
            secrets: &secrets,
            host: Some(&shared),
            mcp: &spy,
        };

        let erreur = echec_de(mount_role(&deps, &alice, "G_prod", "sandbox", None, None, env()).await);

        assert!(
            matches!(&erreur, LymailleError::Forbidden(message) if message == "mcp server unavailable"),
            "refus fixe attendu : {erreur:?}"
        );
        assert!(spy.montes().is_empty(), "aucun connecteur pour un jeton expiré");
    }

    /// Scenario « une liaison hôte sans hôte est un échec fermé » : le choix du connecteur pour
    /// une liaison `Host` sans hôte dans les `MountDeps` est `Config`, sans connecteur.
    #[tokio::test(flavor = "multi_thread")]
    async fn une_liaison_hote_sans_hote_est_un_echec_ferme() {
        let db = db_migre().await;
        let alice = compte(&db, "alice").await;
        let spy = SpyFactory::default();
        let secrets: Arc<dyn SecretResolver> = Arc::new(FixedSecret);
        let deps = MountDeps {
            db: &db,
            secrets: &secrets,
            host: None,
            mcp: &spy,
        };
        let liaison = RoleBinding::Host(grant("https://sbx/mcp", "jeton", None));

        let erreur = echec_de(connector_for(&deps, &alice, "G_prod", "sandbox", liaison, None));

        assert!(
            matches!(&erreur, LymailleError::Config(message) if message == "host binding without a host"),
            "échec fermé attendu : {erreur:?}"
        );
        assert_eq!(spy.montes(), Vec::<String>::new());
    }
}
