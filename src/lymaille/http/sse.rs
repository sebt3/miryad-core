//! Flux d'événements `text/event-stream` d'une conversation
//! (`src/lymaille/http/sse.sdd`).

use std::convert::Infallible;
use std::time::Duration;

use axum::extract::Extension;
use axum::extract::Path;
use axum::response::sse::Event;
use axum::response::sse::Sse;
use futures::Stream;
use futures::stream;
use serde_json::json;

use ::lymaille::SessionId;

use crate::auth::AuthPrincipal;
use crate::lymaille::error::LymailleError;
use crate::lymaille::interactive::event::SessionEvent;
use crate::lymaille::interactive::hub::Subscription;
use crate::lymaille::interactive::hub::SubscriptionError;
use crate::lymaille::interactive::manager::SessionManager;
use crate::rest::error::RestError;

/// Intervalle du battement SSE (`Must` de `./sse.sdd`) : un commentaire `: keepalive` quand
/// aucun événement n'arrive.
const KEEPALIVE_PERIOD: Duration = Duration::from_secs(15);

/// Corps de l'événement `error` rendu quand la sérialisation d'un `SessionEvent` échoue
/// (`Must` de `./sse.sdd`) — un code `MRD-*` nu, aucun contenu de session. Le chemin est
/// quasi-inatteignable (`SessionEvent` est un type serde ordinaire). Le code est **dérivé** de la
/// variante `Serialization` de @`LymailleError` (`../error.sdd`, code `MRD-LYMAILLE-012`, arbitrage
/// du 2026-11-06) : préfixe de la `Display` avant le premier `:`, verrouillé par le `Scenario`
/// « les codes sont stables » de `../error.sdd` — plus aucune constante nue dans ce fichier.
fn serialization_error_body() -> String {
    let error = LymailleError::Serialization("session event serialization failed".to_owned());
    let display = error.to_string();
    let code = display
        .split_once(':')
        .map_or(display.as_str(), |(code, _cause)| code);
    json!({ "code": code }).to_string()
}

/// Traduction de la rupture d'abonnement en erreur REST (même table que `to_rest` de
/// `./routes.rs`, que `./sse.sdd` ne cite pas en dépendance — duplication assumée de dix lignes).
fn to_rest(error: LymailleError) -> RestError {
    match error {
        LymailleError::UnknownSubject | LymailleError::Forbidden(_) => RestError::Forbidden,
        LymailleError::NotFound(_) => RestError::NotFound,
        LymailleError::Conflict(_) => RestError::Conflict,
        LymailleError::InvalidDefinition(_) => {
            RestError::InvalidInput("invalid conversation definition".to_owned())
        }
        other => RestError::Internal(other.to_string()),
    }
}

/// `GET /api/v1/lymaille/conversations/{id}/events` (`Must` de `./sse.sdd`) : cloisonnement et
/// abonnement par `SessionManager` **avant** tout en-tête SSE (session inconnue ou d'un autre →
/// `404` ordinaire), puis flux `text/event-stream` alimenté par la `Subscription` du hub. Le flux
/// ne touche jamais la base et n'ajoute aucune donnée aux événements.
pub(super) async fn events(
    principal: AuthPrincipal,
    Extension(manager): Extension<SessionManager>,
    Path(id): Path<String>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, RestError> {
    let session = SessionId::new(id.as_str()).ok_or(RestError::NotFound)?;
    let subscription = manager
        .subscribe(&principal.subject, &session)
        .await
        .map_err(to_rest)?;
    Ok(Sse::new(event_stream(subscription, encode_event)))
}

/// Le flux lui-même : `event:` = le discriminant `type` du `SessionEvent` (miroir typé, DSD
/// `DEC-30`), `data:` = son JSON complet, `id:` = compteur croissant propre à la connexion ;
/// `Lagged(n)` devient `event: lagged` `{"missed":n}` et le flux continue ; `Closed` le termine ;
/// 15 s sans événement émettent le commentaire `: keepalive`. `encode` est le seam d'encodage :
/// `events` passe `encode_event` ; une erreur d'encodage devient `event: error` et le flux continue.
fn event_stream<E>(subscription: Subscription, encode: E) -> impl Stream<Item = Result<Event, Infallible>>
where
    E: Fn(&SessionEvent, u64) -> Result<Event, LymailleError> + Send + 'static,
{
    stream::unfold(
        (subscription, 0_u64, encode),
        move |(mut subscription, counter, encode)| async move {
            let (event, counter) = match tokio::time::timeout(KEEPALIVE_PERIOD, subscription.recv()).await {
                Ok(Ok(session_event)) => {
                    let next = counter.checked_add(1).unwrap_or(counter);
                    (
                        encode(&session_event, next).unwrap_or_else(|_cause| serialization_error_event(next)),
                        next,
                    )
                }
                Ok(Err(SubscriptionError::Lagged(missed))) => {
                    let next = counter.checked_add(1).unwrap_or(counter);
                    (lagged_event(missed, next), next)
                }
                Ok(Err(SubscriptionError::Closed)) => return None,
                Err(_elapsed) => (Event::default().comment("keepalive"), counter),
            };
            Some((Ok(event), (subscription, counter, encode)))
        },
    )
}

/// Un `SessionEvent` en événement SSE : le nom vient du discriminant `kind.type` du miroir (les
/// variantes sont déjà en `snake_case` — `../interactive/event.sdd`), le contenu est le JSON
/// complet, recopié sans lecture d'aucun champ (un `Host` traverse tel quel). Une panne de
/// sérialisation est rendue en `Err` ; `event_stream` la remplace par `event: error`.
fn encode_event(session_event: &SessionEvent, id: u64) -> Result<Event, LymailleError> {
    let value = serde_json::to_value(session_event)
        .map_err(|cause| LymailleError::Serialization(cause.to_string()))?;
    let name = value
        .get("kind")
        .and_then(|kind| kind.get("type"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("error");
    Event::default()
        .id(id.to_string())
        .event(name)
        .json_data(&value)
        .map_err(|cause| LymailleError::Serialization(cause.to_string()))
}

/// L'événement de rupture `Lagged(n)` (`Must` de `./sse.sdd`) : `event: lagged`, données
/// `{"missed": n}`, puis le flux continue.
fn lagged_event(missed: u64, id: u64) -> Event {
    Event::default()
        .id(id.to_string())
        .event("lagged")
        .json_data(json!({ "missed": missed }))
        .unwrap_or_else(|_cause| serialization_error_event(id))
}

/// L'événement de panne de sérialisation : un code `MRD-*` nu, jamais un contenu de session.
fn serialization_error_event(id: u64) -> Event {
    Event::default()
        .id(id.to_string())
        .event("error")
        .data(serialization_error_body())
}

#[cfg(test)]
mod tests {
    //! Les 7 `Scenario` de `./sse.sdd` : le flux est lu en mémoire (`oneshot`), le battement
    //! s'exerce horloge tokio en pause. Fixtures compactes : celles de `./routes.rs` ne sont
    //! citées nulle part dans `sse.sdd` (la spec est écrite avant `routes.rs`, ordre R11→R12).

    use futures::StreamExt as _;
    use std::future::Future;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    use sea_orm::ConnectionTrait;
    use sea_orm::DatabaseConnection;
    use sea_orm::sea_query::Alias;
    use sea_orm::sea_query::Expr;
    use sea_orm::sea_query::Query;
    use serde_json::json;
    use tower::ServiceExt;
    use vynil_core::Semver;
    use zeroize::Zeroizing;

    use super::events;
    use crate::auth::MiryadAuthState;
    use crate::auth::MockOidcClient;
    use crate::auth::issue_token;
    use crate::lymaille::interactive::assemble::AssembleDeps;
    use crate::lymaille::interactive::assemble::DynInference;
    use crate::lymaille::interactive::assemble::InferenceFactory;
    use crate::lymaille::interactive::hub::EventHub;
    use crate::lymaille::interactive::manager::Limits;
    use crate::lymaille::interactive::manager::ManagerDeps;
    use crate::lymaille::interactive::manager::SessionManager;
    use crate::lymaille::mcp_mount::McpConnectorFactory;
    use crate::lymaille::resource_module::ResourceRegistry;
    use crate::lymaille::secret::SecretResolver;
    use crate::lymaille::store::LymailleStores;
    use crate::lymaille::store::test_support::test_db;
    use crate::users::resolve_user;

    use ::lymaille::AgentDefinition;
    use ::lymaille::AgentKind;
    use ::lymaille::AgentStore as _;
    use ::lymaille::ContextLevel;
    use ::lymaille::ContextType;
    use ::lymaille::InferenceResponse;
    use ::lymaille::Message;
    use ::lymaille::ModelProfile;
    use ::lymaille::ProfileStore as _;
    use ::lymaille::Provider;
    use ::lymaille::ProviderLevel;
    use ::lymaille::ProviderStore as _;
    use ::lymaille::ScriptedProvider;
    use ::lymaille::SessionId;
    use ::lymaille::SessionStore as _;
    use ::lymaille::SystemClock;
    use ::lymaille::ToolsetContext;
    use ::lymaille::TurnGate;

    /// Poivre des tokens API de test (valeur fixe, sans portée hors des tests).
    const PEPPER: &str = "sse-test-pepper";

    // ── Fixtures (motif de ../interactive/manager.rs) ────────────────────────

    /// Compte local (éventuellement admin par le groupe), principal relu par `Identity`.
    async fn compte(db: &DatabaseConnection, subject: &str, admin: bool) {
        let account = resolve_user(db, subject, None)
            .await
            .expect("compte local de test cree");
        if admin {
            crate::users::membership::sync_group_memberships(
                db,
                account.id,
                &[crate::users::group::ADMIN_GROUP_NAME.to_owned()],
            )
            .await
            .expect("appartenance au groupe admin");
        }
    }

    /// Catalogue câblé par `root` (admin) : profil `qwen`, provider `local` d'auth `FIXED_KEY`.
    async fn wire_catalog(db: &DatabaseConnection) {
        compte(db, "root", true).await;
        let stores = LymailleStores::new(db.clone(), "root".to_owned());
        stores
            .profiles()
            .put_profile(
                &ModelProfile::new("qwen".to_owned(), "local".to_owned(), "qwen-test".to_owned())
                    .with_context_size(262_144),
            )
            .await
            .expect("l'admin ecrit le profil");
        stores
            .providers()
            .put_provider(
                &Provider::new(
                    "local".to_owned(),
                    "openai".to_owned(),
                    "http://inference.invalid/v1".to_owned(),
                    Some("FIXED_KEY".to_owned()),
                    ProviderLevel::User,
                )
                .expect("des references simples font un provider"),
            )
            .await
            .expect("l'admin ecrit le provider");
    }

    fn agent(id: &str, kind: AgentKind) -> AgentDefinition {
        AgentDefinition::new(id.to_owned(), kind, "le prompt".to_owned(), "qwen".to_owned())
            .with_modules(vec!["resources".to_owned()])
    }

    async fn put_agent(db: &DatabaseConnection, owner: &str, definition: &AgentDefinition) {
        LymailleStores::new(db.clone(), owner.to_owned())
            .agents()
            .put_agent(definition)
            .await
            .expect("le proprietaire ecrit son agent");
    }

    async fn put_type(
        db: &DatabaseConnection,
        name: &str,
        session_kind: Option<::lymaille::SessionKind>,
        default_agent: Option<&str>,
        allowed: &[&str],
    ) {
        let context_type = ContextType::new(
            name.to_owned(),
            Semver::parse("1.0.0").expect("la version de fixture se parse"),
            json!({}),
        )
        .expect("le type de fixture se construit");
        let context_type = match session_kind {
            Some(kind) => context_type
                .with_session_kind(kind)
                .expect("la nature de fixture se pose"),
            None => context_type,
        };
        let context_type = match default_agent {
            Some(agent) => context_type
                .with_default_agent(agent.to_owned())
                .expect("l'agent par defaut de fixture se pose"),
            None => context_type,
        };
        let active = serde_json::to_value(context_type).expect("le type se serialise");
        let mut insert = Query::insert();
        insert
            .into_table(Alias::new("miryad_lymaille_catalog_context_types"))
            .columns([
                "name",
                "version",
                "allowed_agents",
                "active",
                "draft",
                "created_at",
            ])
            .values(vec![
                Expr::val(name),
                Expr::val("1.0.0"),
                Expr::val(serde_json::Value::Array(
                    allowed.iter().map(|agent| json!(*agent)).collect(),
                )),
                Expr::val(active),
                Expr::null(),
                Expr::val(chrono::Utc::now()),
            ])
            .expect("les colonnes et valeurs de fixture correspondent");
        db.execute(&insert).await.expect("ligne de type inseree");
    }

    async fn inserer_contexte_prive(
        db: &DatabaseConnection,
        owner_id: i32,
        name: &str,
        context_type: Option<&str>,
    ) {
        let context = ToolsetContext::new(name.to_owned(), ContextLevel::User)
            .expect("le contexte de fixture se construit");
        let value = serde_json::to_value(context).expect("le contexte se serialise");
        let mut insert = Query::insert();
        insert
            .into_table(Alias::new("miryad_lymaille_user_contexts"))
            .columns(["owner_id", "name", "context", "context_type", "created_at"])
            .values(vec![
                Expr::val(owner_id),
                Expr::val(name),
                Expr::val(value),
                context_type.map_or_else(Expr::null, |name| Expr::val(name.to_owned())),
                Expr::val(chrono::Utc::now()),
            ])
            .expect("les colonnes et valeurs de fixture correspondent");
        db.execute(&insert).await.expect("ligne de contexte inseree");
    }

    /// Dépendances : fabrique d'inférence et hub donnés, capacité de file 256 (le `Scenario`
    /// « retard » l'attend), MCP sans usage, horloge système.
    fn deps_with(
        db: DatabaseConnection,
        inference: Arc<dyn InferenceFactory>,
        hub: Arc<EventHub>,
    ) -> ManagerDeps {
        ManagerDeps {
            assemble: AssembleDeps {
                db,
                readers: Arc::new(ResourceRegistry::new()),
                secrets: Arc::new(FixedSecret("cle-fixe")),
                hub,
                clock: Arc::new(SystemClock::new()),
                host: None,
                inference,
                mcp: Arc::new(NoMcp),
            },
            gate: TurnGate::new(),
            limits: Limits::default(),
        }
    }

    struct ScriptFactory(Arc<ScriptedProvider>);

    impl InferenceFactory for ScriptFactory {
        fn make(&self, _address: String, _api_key: Option<String>) -> Arc<dyn DynInference> {
            let provider: Arc<dyn DynInference> = self.0.clone();
            provider
        }
    }

    fn final_text(text: &str) -> InferenceResponse {
        InferenceResponse::new(Message::assistant(text.to_owned(), None, Vec::new()), None)
    }

    struct NoMcp;

    impl McpConnectorFactory for NoMcp {
        fn make(
            &self,
            _url: &str,
            _request_timeout: Duration,
            _connect_timeout: Duration,
            _auth: Option<Arc<dyn crate::lymaille::mcp_mount::DynMcpAuth>>,
        ) -> Arc<dyn crate::lymaille::mcp_mount::DynMcpConnector> {
            Arc::new(NoConnector)
        }
    }

    struct NoConnector;

    impl ::lymaille::McpLister for NoConnector {
        fn list_tools(
            &self,
        ) -> impl Future<Output = Result<Vec<::lymaille::McpTool>, ::lymaille::McpError>> + Send {
            std::future::ready(Ok(Vec::new()))
        }
    }

    impl ::lymaille::McpConnector for NoConnector {
        fn call_tool(
            &self,
            _name: &str,
            _arguments: &serde_json::Value,
        ) -> impl Future<Output = Result<serde_json::Value, ::lymaille::McpError>> + Send {
            std::future::ready(Ok(json!({})))
        }

        fn reconnect(&self) -> impl Future<Output = ()> + Send {
            std::future::ready(())
        }
    }

    struct FixedSecret(&'static str);

    impl SecretResolver for FixedSecret {
        fn resolve(&self, reference: &str) -> Result<Zeroizing<String>, crate::lymaille::LymailleError> {
            if reference == "FIXED_KEY" {
                Ok(Zeroizing::new(self.0.to_owned()))
            } else {
                Err(crate::lymaille::LymailleError::SecretUnresolved(
                    reference.to_owned(),
                ))
            }
        }
    }

    // ── Montage et lecture du flux ───────────────────────────────────────────

    fn test_state(db: DatabaseConnection) -> MiryadAuthState {
        MiryadAuthState {
            oidc_client: Arc::new(MockOidcClient::new()),
            cookie_key: ::cookie::Key::from(&[3u8; 64]),
            post_login_redirect: "/".to_owned(),
            post_logout_redirect: "/".to_owned(),
            db,
            secure_cookies: false,
            token_pepper: PEPPER.to_owned(),
        }
    }

    /// Compte local + token API (le Bearer `mrd_…` n'est jamais JWT-shaped : chemin token du
    /// dual-auth, `verify_bearer` n'est jamais appelé).
    async fn token_for(db: &DatabaseConnection, subject: &str) -> String {
        compte(db, subject, false).await;
        issue_token(db, subject, "test", None, PEPPER)
            .await
            .expect("token api emis")
            .token
    }

    fn events_url(id: &str) -> String {
        format!("/api/v1/lymaille/conversations/{id}/events")
    }

    fn app(manager: SessionManager, db: DatabaseConnection) -> axum::Router {
        axum::Router::new()
            .route(
                "/api/v1/lymaille/conversations/{id}/events",
                axum::routing::get(events),
            )
            .layer(axum::Extension(manager))
            .with_state(test_state(db))
    }

    async fn get_events(
        manager: &SessionManager,
        db: &DatabaseConnection,
        token: &str,
        id: &str,
    ) -> axum::response::Response {
        let request = axum::http::Request::builder()
            .method("GET")
            .uri(events_url(id))
            .header("Authorization", format!("Bearer {token}"))
            .body(axum::body::Body::empty())
            .expect("requete valide");
        app(manager.clone(), db.clone())
            .oneshot(request)
            .await
            .expect("le routeur repond")
    }

    /// Le corps SSE en fragments de texte (`into_data_stream` : `Body` seul n'est pas `Stream`).
    fn body_stream(response: axum::response::Response) -> impl futures::Stream<Item = String> + Unpin {
        response.into_body().into_data_stream().map(|piece| match piece {
            Ok(bytes) => String::from_utf8(bytes.to_vec()).expect("le SSE est en UTF-8"),
            Err(cause) => panic!("le corps SSE est illisible : {cause}"),
        })
    }

    /// Un fragment SSE, borné à 5 s — l'échec rouge est un timeout.
    async fn next_chunk<S>(body: &mut S) -> Option<String>
    where
        S: futures::Stream<Item = String> + Unpin,
    {
        tokio::time::timeout(Duration::from_secs(5), body.next())
            .await
            .expect("un fragment SSE dans le delai")
    }

    /// Lit des fragments jusqu'à rendre un tampon vérifiant `done` (ou échoue après 40 tours).
    async fn read_until<S>(body: &mut S, done: impl Fn(&str) -> bool) -> String
    where
        S: futures::Stream<Item = String> + Unpin,
    {
        let mut buffer = String::new();
        for _ in 0..40 {
            if done(&buffer) {
                return buffer;
            }
            let Some(chunk) = next_chunk(body).await else {
                panic!("le flux s'est termine avant la fin attendue : {buffer}");
            };
            buffer.push_str(&chunk);
        }
        panic!("la fin attendue n'est jamais apparue : {buffer}");
    }

    /// Attend (5 s max) que le prédicat sur l'état persisté de la session soit vrai.
    async fn wait_state(db: &DatabaseConnection, subject: &str, id: &SessionId, want: impl Fn(&str) -> bool) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let loaded = LymailleStores::new(db.clone(), subject.to_owned())
                .sessions()
                .load(id)
                .await
                .expect("lecture de l'etat de session");
            if let Some(state) = loaded
                && want(&state.to_json().expect("l'etat se serialise"))
            {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "l'etat attendu n'est jamais apparu"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// L'installation d'un scénario type : catalogue, agent `dev` (`work`) d'alice, type `t`,
    /// contexte privé `c1`. Rend `(db, hub, manager)`.
    async fn setup() -> (DatabaseConnection, Arc<EventHub>, SessionManager) {
        let db = test_db().await;
        wire_catalog(&db).await;
        compte(&db, "alice", false).await;
        compte(&db, "bob", false).await;
        put_agent(&db, "alice", &agent("dev", AgentKind::Work)).await;
        put_type(
            &db,
            "t",
            Some(::lymaille::SessionKind::Work),
            Some("dev"),
            &["dev"],
        )
        .await;
        let alice = crate::lymaille::identity::Identity::new(db.clone())
            .resolve("alice")
            .await
            .expect("alice est un compte local");
        inserer_contexte_prive(&db, alice.user_id, "c1", Some("t")).await;
        let hub = Arc::new(EventHub::new(None, 256));
        let provider = Arc::new(ScriptedProvider::new(vec![Ok(final_text("reponse de tour"))]));
        let manager = SessionManager::new(deps_with(
            db.clone(),
            Arc::new(ScriptFactory(provider)),
            Arc::clone(&hub),
        ));
        (db, hub, manager)
    }

    // ── Scénarios ────────────────────────────────────────────────────────────

    /// Scenario « les événements arrivent dans l'ordre avec leur nom » : `turn_started`, des
    /// `text_delta`, `turn_ended`, ids croissants.
    #[tokio::test(flavor = "multi_thread")]
    async fn les_evenements_arrivent_dans_l_ordre_avec_leur_nom() {
        let (db, _hub, manager) = setup().await;
        let alice = token_for(&db, "alice").await;
        let handle = manager.open("alice", "c1", None).await.expect("ouverture");
        let response = get_events(&manager, &db, &alice, handle.id.as_str()).await;
        assert_eq!(response.status(), axum::http::StatusCode::OK, "le flux s'ouvre");
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("text/event-stream"),
            "le type du flux est `text/event-stream`"
        );
        assert_eq!(
            response
                .headers()
                .get("cache-control")
                .and_then(|value| value.to_str().ok()),
            Some("no-cache"),
            "`Cache-Control: no-cache`"
        );
        manager
            .send("alice", &handle.id, "bonjour".to_owned(), None)
            .await
            .expect("le tour est lance");
        let mut body = body_stream(response);
        let buffer = read_until(&mut body, |text| text.contains("event: turn_ended")).await;
        let started = buffer.find("event: turn_started").expect("turn_started emis");
        let delta = buffer.find("event: text_delta").expect("des text_delta emis");
        let ended = buffer.find("event: turn_ended").expect("turn_ended emis");
        assert!(
            started < delta && delta < ended,
            "l'ordre est respecte : {buffer}"
        );
        let ids: Vec<u64> = buffer
            .lines()
            .filter_map(|line| line.strip_prefix("id: "))
            .filter_map(|id| id.parse().ok())
            .collect();
        assert!(
            ids.windows(2).all(|pair| pair[0] < pair[1]),
            "les ids sont croissants : {ids:?}"
        );
        assert!(ids.len() >= 3, "un id par evenement : {ids:?}");
    }

    /// Scenario « un événement hôte traverse sans interprétation » : `event: host`, `data` portant
    /// `type` = `host`, le `kind` publié et le `payload` tel que publié.
    #[tokio::test(flavor = "multi_thread")]
    async fn un_evenement_hote_traverse_sans_interpretation() {
        let (db, hub, manager) = setup().await;
        let alice = token_for(&db, "alice").await;
        let handle = manager.open("alice", "c1", None).await.expect("ouverture");
        let response = get_events(&manager, &db, &alice, handle.id.as_str()).await;
        hub.publish_host(&handle.id, "dashboard.show", json!({ "trace_id": "abc" }))
            .expect("la publication hote est valide");
        let mut body = body_stream(response);
        let buffer = read_until(&mut body, |text| text.contains("event: host")).await;
        let payload: serde_json::Value = serde_json::from_str(
            buffer
                .lines()
                .find_map(|line| line.strip_prefix("data: "))
                .expect("une ligne data"),
        )
        .expect("le data est du JSON");
        assert_eq!(
            payload["kind"]["type"],
            json!("host"),
            "le discriminant est `host` : {payload}"
        );
        assert_eq!(
            payload["kind"]["kind"],
            json!("dashboard.show"),
            "le nom publie est porte"
        );
        assert_eq!(
            payload["kind"]["payload"],
            json!({ "trace_id": "abc" }),
            "le payload est rendu tel quel"
        );
    }

    /// Scenario « la déconnexion n'annule pas le tour » : le client ferme le flux en cours de
    /// tour, l'état est quand même sauvegardé (la tâche du tour est détachée, ./manager.sdd).
    #[tokio::test(flavor = "multi_thread")]
    async fn la_deconnexion_n_annule_pas_le_tour() {
        let (db, _hub, manager) = setup().await;
        let alice = token_for(&db, "alice").await;
        let handle = manager.open("alice", "c1", None).await.expect("ouverture");
        let response = get_events(&manager, &db, &alice, handle.id.as_str()).await;
        assert_eq!(response.status(), axum::http::StatusCode::OK, "le flux s'ouvre");
        manager
            .send("alice", &handle.id, "bonjour".to_owned(), None)
            .await
            .expect("le tour est lance");
        let mut body = body_stream(response);
        let first = next_chunk(&mut body)
            .await
            .expect("le flux livre le debut du tour");
        assert!(
            first.contains("event: turn_started") || first.contains("event: text_delta"),
            "le client lit un debut de tour : {first:?}"
        );
        drop(body); // le client se deconnecte : rien n'annule le tour
        wait_state(&db, "alice", &handle.id, |etat| {
            etat.contains("bonjour") && etat.contains("reponse de tour")
        })
        .await;
    }

    /// Scenario « session d'un autre » : `bob` ouvre le flux d'alice → `404` sans en-tête
    /// `text/event-stream` (la traduction d'erreur ordinaire précède tout en-tête SSE).
    #[tokio::test(flavor = "multi_thread")]
    async fn session_d_un_autre() {
        let (db, _hub, manager) = setup().await;
        let alice = token_for(&db, "alice").await;
        let bob = token_for(&db, "bob").await;
        let handle = manager.open("alice", "c1", None).await.expect("ouverture");
        let probe = get_events(&manager, &db, &alice, handle.id.as_str()).await;
        assert_eq!(
            probe.status(),
            axum::http::StatusCode::OK,
            "le flux s'ouvre pour la proprietaire"
        );
        let response = get_events(&manager, &db, &bob, handle.id.as_str()).await;
        assert_eq!(
            response.status(),
            axum::http::StatusCode::NOT_FOUND,
            "introuvable indiscernable"
        );
        assert_ne!(
            response
                .headers()
                .get(axum::http::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default(),
            "text/event-stream",
            "aucun en-tete SSE n'est pose sur le 404"
        );
    }

    /// Scenario « retard » : 300 événements publiés pendant que le flux n'est pas lu (capacité
    /// 256) → `event: lagged` avec `missed > 0`, puis le flux continue.
    #[tokio::test(flavor = "multi_thread")]
    async fn retard() {
        let (db, hub, manager) = setup().await;
        let alice = token_for(&db, "alice").await;
        let handle = manager.open("alice", "c1", None).await.expect("ouverture");
        let response = get_events(&manager, &db, &alice, handle.id.as_str()).await;
        for index in 0..300 {
            hub.publish_host(&handle.id, "h", json!({ "i": index }))
                .expect("la publication hote est valide");
        }
        let mut body = body_stream(response);
        let buffer = read_until(&mut body, |text| text.contains("event: lagged")).await;
        let payload: serde_json::Value = serde_json::from_str(
            buffer
                .lines()
                .find_map(|line| line.strip_prefix("data: "))
                .expect("une ligne data"),
        )
        .expect("le data du lagged est du JSON");
        let missed = payload["missed"].as_u64().expect("missed est un nombre");
        assert!(missed > 0, "des evenement perdus sont declares : {payload}");
        let buffer = read_until(&mut body, |text| text.contains("event: host")).await;
        assert!(
            buffer.contains("event: host"),
            "le flux continue apres le lagged : {buffer}"
        );
    }

    /// Scenario « battement » : aucun événement pendant 20 s (horloge tokio en pause, avance
    /// automatique) → le commentaire `: keepalive` est émis.
    #[tokio::test]
    async fn battement() {
        let (db, _hub, manager) = setup().await;
        let alice = token_for(&db, "alice").await;
        let handle = manager.open("alice", "c1", None).await.expect("ouverture");
        let response = get_events(&manager, &db, &alice, handle.id.as_str()).await;
        let mut body = body_stream(response);
        tokio::time::pause();
        // 30 s > 15 s : sous horloge en pause, l'avance automatique saute au prochain minuteur
        // réel (le battement) et non au délai de lecture.
        let piece = tokio::time::timeout(Duration::from_secs(30), body.next())
            .await
            .expect("le battement est emis dans les 30 s");
        let chunk = piece.expect("le flux n'est pas termine");
        assert!(
            chunk.contains(": keepalive"),
            "le commentaire SSE est emis : {chunk:?}"
        );
    }

    /// Scenario « fin de session » : `close` ferme le canal du hub → le flux se termine.
    #[tokio::test(flavor = "multi_thread")]
    async fn fin_de_session() {
        let (db, _hub, manager) = setup().await;
        let alice = token_for(&db, "alice").await;
        let handle = manager.open("alice", "c1", None).await.expect("ouverture");
        let response = get_events(&manager, &db, &alice, handle.id.as_str()).await;
        assert_eq!(response.status(), axum::http::StatusCode::OK, "le flux s'ouvre");
        let mut body = body_stream(response);
        manager.close("alice", &handle.id).await;
        let end = tokio::time::timeout(Duration::from_secs(5), async move {
            while body.next().await.is_some() {}
        })
        .await;
        assert!(end.is_ok(), "le flux s'est arrete de lui-meme");
    }

    /// Verrou du code rendu sur panne de sérialisation (`Must` de `./sse.sdd` : `event: error`
    /// `data: {"code":"MRD-LYMAILLE-…"}`) : le code est `MRD-LYMAILLE-012`, variante
    /// `Serialization` de `../error.sdd` (arbitrage du 2026-11-05) — plus `-009`, doublon de
    /// `Store`. Le corps ne porte qu'un code nu, aucun contenu de session.
    #[test]
    fn panne_de_serialisation_rend_le_code_012() {
        assert_eq!(
            super::serialization_error_body(),
            r#"{"code":"MRD-LYMAILLE-012"}"#,
            "le corps rendu par `serialization_error_event` porte le code unique -012"
        );
    }

    /// Émission de la variante (arbitrage du 2026-11-06) : le corps rendu est celui que produit
    /// le chemin d'erreur pour @`LymailleError::Serialization` — le préfixe de sa `Display` (le
    /// code, verrouillé par `../error.sdd`), rendu en JSON `{"code": …}`. Plus aucune constante
    /// nue dans `./sse.rs` : si le code de la variante bouge, ce test et le verrou `-012` ne
    /// peuvent plus être verts ensemble.
    #[test]
    fn corps_derive_de_la_variante_serialization() {
        let erreur = crate::lymaille::error::LymailleError::Serialization("cause expurgée".to_owned());
        let display = erreur.to_string();
        let code = display
            .split_once(':')
            .map_or(display.as_str(), |(code, _cause)| code);
        assert_eq!(
            super::serialization_error_body(),
            json!({ "code": code }).to_string(),
            "le corps rendu egale le corps que produit le chemin d'erreur pour la variante Serialization"
        );
    }

    /// Scenario « une panne de sérialisation n'interrompt pas le flux » : l'encodeur injecté
    /// échoue sur le deuxième événement ; le flux rend `event: error` à sa place, le premier et
    /// le troisième sont normaux, et le flux reste ouvert jusqu'à `Closed`.
    #[tokio::test(flavor = "multi_thread")]
    async fn une_panne_de_serialisation_n_interrompt_pas_le_flux() {
        let (_db, hub, manager) = setup().await;
        let handle = manager.open("alice", "c1", None).await.expect("ouverture");
        let subscription = manager
            .subscribe("alice", &handle.id)
            .await
            .expect("l'abonnement de la proprietaire");
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let encoder = move |session_event: &crate::lymaille::interactive::event::SessionEvent, id: u64| {
            if counter.fetch_add(1, Ordering::SeqCst) == 1 {
                Err(crate::lymaille::error::LymailleError::Serialization(
                    "panne injectee".to_owned(),
                ))
            } else {
                super::encode_event(session_event, id)
            }
        };
        let response = axum::response::IntoResponse::into_response(axum::response::sse::Sse::new(
            super::event_stream(subscription, encoder),
        ));
        let mut body = body_stream(response);
        for index in 0..3 {
            hub.publish_host(&handle.id, "h", json!({ "i": index }))
                .expect("la publication hote est valide");
        }
        manager.close("alice", &handle.id).await;
        let buffer = tokio::time::timeout(Duration::from_secs(5), async {
            let mut buffer = String::new();
            while let Some(chunk) = body.next().await {
                buffer.push_str(&chunk);
            }
            buffer
        })
        .await
        .expect("le flux se termine a la fermeture de la session");
        let names: Vec<&str> = buffer
            .lines()
            .filter_map(|line| line.strip_prefix("event: "))
            .collect();
        assert_eq!(
            names,
            ["host", "error", "host"],
            "le deuxieme evenement est remplace par `error` : {buffer}"
        );
        assert!(
            buffer.contains(r#"data: {"code":"MRD-LYMAILLE-012"}"#),
            "le corps de l'erreur porte le code -012 : {buffer}"
        );
        assert!(
            buffer.contains(r#""i":0"#) && buffer.contains(r#""i":2"#),
            "le premier et le troisieme evenements sont emis normalement : {buffer}"
        );
    }
}
