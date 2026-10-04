use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::rejection::{JsonRejection, MissingJsonContentType};
use axum::extract::{FromRef, FromRequest, Request, State};
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::header::CONTENT_TYPE;
use axum::response::IntoResponse;
use axum::{Extension, Json, Router};
use serde_json::{Value, json};
use vynil_core::hbs::HandleBars;

use crate::auth::{AuthPrincipal, MiryadAuthState};
use crate::mcp::error::McpError;
use crate::mcp::format::{RenderShape, render};
use crate::mcp::protocol::{
    InitializeResult, JsonRpcResponse, PROTOCOL_VERSION, ParsedRequest, ProtocolRejection, ToolDeclaration,
    parse_request,
};
use crate::mcp::registry::{McpOp, McpToolRegistry};

const OPERATIONS: &[(&str, McpOp)] = &[
    ("_list", McpOp::List),
    ("_get", McpOp::Get),
    ("_create", McpOp::Create),
    ("_update", McpOp::Update),
    ("_delete", McpOp::Delete),
];

/// Monte `POST /mcp` — dispatch JSON-RPC 2.0 (`initialize`, `tools/list`, `tools/call`).
/// Réutilise `MiryadAuthState` (dual-auth, db) comme REST/GraphQL.
pub fn mcp_router<S>(registry: McpToolRegistry) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
    MiryadAuthState: FromRef<S>,
{
    Router::new()
        .route("/mcp", axum::routing::post(mcp_handler))
        .layer(Extension(Arc::new(registry)))
}

/// Corps de la requête sous la porte de contenu de `axum::Json` : les octets bruts sont
/// livrés à `parse_request` sans désérialisation préalable (`handler.sdd` — « corps en octets
/// sous Content-Type application/json »). Les rejets restent ceux d'axum, corps inchangés :
/// `Content-Type` absent ou hors `application/json` (suffixe `+json` accepté) → `415`
/// `MissingJsonContentType`, corps au-delà de la `DefaultBodyLimit` (`2MB`) → `413`
/// `BytesRejection`.
struct JsonBytes(Bytes);

impl<S: Send + Sync> FromRequest<S> for JsonBytes {
    type Rejection = JsonRejection;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        if !json_content_type(req.headers()) {
            // Le type de rejet est `#[non_exhaustive]` hors de sa crate : seul `Default` le pose.
            return Err(MissingJsonContentType::default().into());
        }
        let bytes = Bytes::from_request(req, state).await?;
        Ok(Self(bytes))
    }
}

/// Porte de contenu reprise de la règle `handler.sdd` : `application/json` (paramètres
/// acceptés) ou tout type `application/*+json` ; absent ou illisible → non JSON.
fn json_content_type(headers: &HeaderMap) -> bool {
    let Some(raw) = headers.get(CONTENT_TYPE).and_then(|value| value.to_str().ok()) else {
        return false;
    };
    let primary = raw
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    primary == "application/json"
        || primary
            .strip_prefix("application/")
            .is_some_and(|subtype| subtype.ends_with("+json"))
}

async fn mcp_handler(
    State(auth): State<MiryadAuthState>,
    principal: AuthPrincipal,
    Extension(registry): Extension<Arc<McpToolRegistry>>,
    JsonBytes(body): JsonBytes,
) -> impl IntoResponse {
    let parsed = match parse_request(&body) {
        Ok(parsed) => parsed,
        Err(rejection) => return Json(rejection_response(&rejection)).into_response(),
    };

    // Notifications (pas d'id) : pas de réponse, HTTP 202 — un `id` nul explicite n'en
    // est plus une, `parse_request` la rend `-32600` (arbitré 2026-09-29).
    let request = match parsed {
        ParsedRequest::Notification => return StatusCode::ACCEPTED.into_response(),
        ParsedRequest::Call(request) => request,
    };

    let response = match request.method.as_str() {
        "initialize" => handle_initialize(request.id),
        "tools/list" => handle_tools_list(request.id, &registry),
        "tools/call" => handle_tools_call(request.id, request.params, &registry, &auth, &principal).await,
        other => error_response(request.id, &McpError::MethodNotFound(other.to_owned())),
    };

    Json(response).into_response()
}

/// Rejet d'enveloppe traduit par `./error.rs` seul : `-32700` (`ParseError`) et `-32600`
/// (`InvalidRequest`), réponse `200` à `id` `null` (règle « deux niveaux », arbitré
/// 2026-09-29) — les rejets de transport (`401`/`405`/`413`/`415`) restent HTTP.
fn rejection_response(rejection: &ProtocolRejection) -> JsonRpcResponse {
    let err = match rejection {
        ProtocolRejection::Parse => McpError::ParseError,
        ProtocolRejection::InvalidRequest => McpError::InvalidRequest,
    };
    error_response(Value::Null, &err)
}

/// Assemblage de toute réponse d'erreur par `./error.rs` seul : `rpc_code`, `wire_message`
/// (générique pour les `-32603`, le détail ne sort jamais sur le fil) et `data` — aucun
/// message, aucun code réécrit ici. Traces (arbitré 2026-09-29) : `error!` portant la
/// `Display` complète de tout `-32603` (base, rendu, interne), `warn!` sur `-32001`
/// (interdiction), aucune trace pour les autres codes — jamais les `arguments`, un corps
/// ou un secret.
fn error_response(id: Value, err: &McpError) -> JsonRpcResponse {
    match err.rpc_code() {
        // Codes de `McpError::rpc_code` — `-32603` base/rendu/interne et `-32001`
        // interdiction (`handler.sdd`, règle de trace).
        -32603 => tracing::error!("{err}"),
        -32001 => tracing::warn!("{err}"),
        _ => {}
    }
    match err.data() {
        Some(data) => JsonRpcResponse::err_with_data(id, err.rpc_code(), err.wire_message(), Some(data)),
        None => JsonRpcResponse::err(id, err.rpc_code(), err.wire_message()),
    }
}

/// `initialize` sans lire les `params` du client : le serveur annonce `PROTOCOL_VERSION`,
/// le client décide (arbitré 2026-09-29). La sérialisation de `InitializeResult` est la
/// seule source des clés wire (`protocolVersion`, `capabilities`, `serverInfo`).
fn handle_initialize(id: Value) -> JsonRpcResponse {
    let result = InitializeResult {
        protocol_version: PROTOCOL_VERSION,
        capabilities: json!({ "tools": {} }),
        server_info: json!({
            "name": env!("CARGO_PKG_NAME"),
            "version": env!("CARGO_PKG_VERSION"),
        }),
    };
    match serde_json::to_value(result) {
        Ok(value) => JsonRpcResponse::ok(id, value),
        Err(err) => error_response(id, &McpError::Render(err.to_string())),
    }
}

/// Cinq déclarations par entité, ordre déterministe (ressources par nom croissant puis
/// l'ordre fixe de `OPERATIONS`, arbitré 2026-09-29) ; `format` n'entre dans aucune
/// déclaration et les descriptions restent les littéraux de la spec.
fn handle_tools_list(id: Value, registry: &McpToolRegistry) -> JsonRpcResponse {
    let id_schema = json!({
        "type": "object",
        "properties": { "id": { "type": "integer" } },
        "required": ["id"],
    });

    let mut names: Vec<&'static str> = registry
        .entities
        .values()
        .map(|entity| entity.resource_name())
        .collect();
    names.sort_unstable();
    let mut tools = Vec::with_capacity(names.len().saturating_mul(OPERATIONS.len()));
    for name in names {
        tools.push(ToolDeclaration {
            name: format!("{name}_list"),
            description: format!("List {name}, paginated"),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "page": { "type": "integer" },
                    "per_page": { "type": "integer" },
                    "filter": { "type": "string" },
                },
            }),
        });
        tools.push(ToolDeclaration {
            name: format!("{name}_get"),
            description: format!("Get a single {name} by id"),
            input_schema: id_schema.clone(),
        });
        tools.push(ToolDeclaration {
            name: format!("{name}_create"),
            description: format!("Create a new {name}"),
            input_schema: json!({ "type": "object" }),
        });
        tools.push(ToolDeclaration {
            name: format!("{name}_update"),
            description: format!("Update an existing {name} by id"),
            // Copie littérale identique du schéma de `_get`/`_delete` (arbitré 2026-09-29).
            input_schema: json!({
                "type": "object",
                "properties": { "id": { "type": "integer" } },
                "required": ["id"],
            }),
        });
        tools.push(ToolDeclaration {
            name: format!("{name}_delete"),
            description: format!("Delete a {name} by id"),
            input_schema: id_schema.clone(),
        });
    }

    match serde_json::to_value(tools) {
        Ok(tools) => JsonRpcResponse::ok(id, json!({ "tools": tools })),
        Err(err) => error_response(id, &McpError::Render(err.to_string())),
    }
}

/// Absent ou `null`, `arguments` vaut l'objet vide `{}` (arbitré 2026-09-29) : les outils
/// sans clé requise (`_list`) fonctionnent sans arguments, les autres échouent
/// normalement en `-32602` (`MRD-MCP-005`) faute de la clé requise.
fn empty_arguments() -> Value {
    json!({})
}

#[derive(serde::Deserialize)]
struct ToolCallParams {
    name: String,
    #[serde(default = "empty_arguments")]
    arguments: Value,
}

fn parse_tool_name(name: &str) -> Option<(&str, McpOp)> {
    OPERATIONS
        .iter()
        .find_map(|(suffix, op)| name.strip_suffix(suffix).map(|resource| (resource, *op)))
}

async fn handle_tools_call(
    id: Value,
    params: Value,
    registry: &McpToolRegistry,
    auth: &MiryadAuthState,
    principal: &AuthPrincipal,
) -> JsonRpcResponse {
    let call: ToolCallParams = match serde_json::from_value(params) {
        Ok(call) => call,
        Err(err) => return error_response(id, &McpError::InvalidParams(err.to_string())),
    };

    let Some((resource_name, op)) = parse_tool_name(&call.name) else {
        return error_response(id, &McpError::UnknownTool(call.name));
    };
    let Some(entity) = registry.entities.get(resource_name) else {
        return error_response(id, &McpError::UnknownTool(call.name.clone()));
    };

    let arguments = match call.arguments {
        Value::Null => empty_arguments(),
        arguments => arguments,
    };

    match entity.call(op, &auth.db, principal, arguments).await {
        Ok(data) => {
            // Suppression commise : confirmation fixe `deleted`, sans gabarit — le rendu
            // d'un `Value::Null` ne doit pas pouvoir échouer après coup (arbitré 2026-09-29).
            if op == McpOp::Delete {
                return JsonRpcResponse::ok(
                    id,
                    json!({ "content": [{ "type": "text", "text": "deleted" }] }),
                );
            }
            let shape = if op == McpOp::List {
                RenderShape::List
            } else {
                RenderShape::Record
            };
            let mut engine = HandleBars::new();
            match render(&mut engine, &registry.format, shape, &data) {
                Ok(text) => JsonRpcResponse::ok(id, json!({ "content": [{ "type": "text", "text": text }] })),
                Err(err) => error_response(id, &err),
            }
        }
        Err(err) => error_response(id, &err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::PrincipalSource;
    use crate::auth::cookie::build_set_cookie;
    use crate::auth::oidc::{MockOidcClient, OidcIdentity};
    use crate::auth::token::issue_token;
    use crate::mcp::OutputFormat;
    use crate::migration::Migrator;
    use crate::resource::{AccessPolicy, HookError, MiryadResource};
    use crate::users::resolve_user;
    use axum::Router;
    use axum::body::Body;
    use axum::http::{HeaderMap, HeaderValue, Request, StatusCode};
    use sea_orm::entity::prelude::*;
    use sea_orm::{ActiveModelTrait, ConnectionTrait, Database, Schema, Set};
    use sea_orm_migration::MigratorTrait;
    use serde::{Deserialize, Serialize};
    use serde_json::{Value, json};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use tower::ServiceExt;

    /// Entité fixture `recipes` — `OwnerOnly` en lecture et écriture, colonne de
    /// possession explicite, et `before_create` hostile (rejet `HOOK-42` / « titre vide »)
    /// pour le scenario de hook applicatif : la création MCP échoue toujours sur cette
    /// entité, les quatre autres operations sont intactes.
    mod recipe {
        use super::*;

        #[derive(Clone, Debug, PartialEq, Serialize, Deserialize, DeriveEntityModel)]
        #[sea_orm(table_name = "recipes")]
        pub struct Model {
            #[sea_orm(primary_key)]
            pub id: i32,
            pub title: String,
            pub owner_id: i32,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {}

        impl ActiveModelBehavior for ActiveModel {}

        impl MiryadResource for Entity {
            fn resource_name() -> &'static str {
                "recipes"
            }
            fn read_policy() -> AccessPolicy {
                AccessPolicy::OwnerOnly
            }
            fn write_policy() -> AccessPolicy {
                AccessPolicy::OwnerOnly
            }
            fn owner_column() -> Option<Column> {
                Some(Column::OwnerId)
            }
            fn before_create(
                _active: ActiveModel,
                _principal: &AuthPrincipal,
            ) -> Result<ActiveModel, HookError> {
                Err(HookError::with_code("HOOK-42", "titre vide"))
            }
        }
    }

    /// Seconde entité fixture (lecture `Public`) — sert au scenario d'ordre déterministe
    /// de `tools/list` (`articles_*` avant `recipes_*`).
    mod article {
        use super::*;

        #[derive(Clone, Debug, PartialEq, Serialize, Deserialize, DeriveEntityModel)]
        #[sea_orm(table_name = "articles")]
        pub struct Model {
            #[sea_orm(primary_key)]
            pub id: i32,
            pub title: String,
            pub owner_id: i32,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {}

        impl ActiveModelBehavior for ActiveModel {}

        impl MiryadResource for Entity {
            fn resource_name() -> &'static str {
                "articles"
            }
            fn read_policy() -> AccessPolicy {
                AccessPolicy::Public
            }
            fn write_policy() -> AccessPolicy {
                AccessPolicy::Public
            }
            fn owner_column() -> Option<Column> {
                None
            }
        }
    }

    fn state_around(db: DatabaseConnection) -> MiryadAuthState {
        MiryadAuthState {
            oidc_client: Arc::new(MockOidcClient::default()),
            cookie_key: cookie::Key::from(&[0u8; 64]),
            post_login_redirect: "/".to_string(),
            post_logout_redirect: "/".to_string(),
            db,
            secure_cookies: false,
            token_pepper: String::new(),
        }
    }

    /// Base `SQLite` en mémoire migrée (table des tokens présente, `issue_token` usable)
    /// avec les deux tables de fixtures créées.
    async fn migrated_state() -> MiryadAuthState {
        let db = Database::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite connects");
        Migrator::up(&db, None).await.expect("migrations apply cleanly");
        let schema = Schema::new(db.get_database_backend());
        db.execute(&schema.create_table_from_entity(recipe::Entity))
            .await
            .expect("recipes table creates");
        db.execute(&schema.create_table_from_entity(article::Entity))
            .await
            .expect("articles table creates");
        state_around(db)
    }

    /// Base migrée mais sans aucune table d'entité — la table `recipes` manque, toute
    /// operation de l'entité echoue en `DbErr` detaille (scenario de panne de base).
    async fn migrated_state_without_entities() -> MiryadAuthState {
        let db = Database::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite connects");
        Migrator::up(&db, None).await.expect("migrations apply cleanly");
        state_around(db)
    }

    fn app(state: MiryadAuthState, format: OutputFormat) -> Router {
        let mut registry = McpToolRegistry::new(format);
        registry.register::<recipe::Entity>();
        mcp_router(registry).with_state(state)
    }

    fn app_with_articles(state: MiryadAuthState, format: OutputFormat) -> Router {
        let mut registry = McpToolRegistry::new(format);
        registry.register::<recipe::Entity>();
        registry.register::<article::Entity>();
        mcp_router(registry).with_state(state)
    }

    fn principal(subject: &str) -> AuthPrincipal {
        AuthPrincipal {
            subject: subject.to_string(),
            email: None,
            preferred_username: None,
            source: PrincipalSource::ApiToken { token_id: 0 },
        }
    }

    async fn bearer_for(db: &DatabaseConnection, subject: &str) -> String {
        issue_token(db, subject, "handler-test", None, "")
            .await
            .expect("issuing succeeds")
            .token
    }

    async fn insert_recipe(db: &DatabaseConnection, id: i32, title: &str, owner_id: i32) {
        recipe::ActiveModel {
            id: Set(id),
            title: Set(title.to_string()),
            owner_id: Set(owner_id),
        }
        .insert(db)
        .await
        .expect("fixture row inserts");
    }

    // ——— JWT et cookie de session (gabarit duplique de ../auth/dual.rs) ———

    fn make_jwt(exp: u64) -> String {
        use base64::Engine as _;
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(format!(r#"{{"exp":{exp}}}"#));
        format!("header.{payload}.sig")
    }

    fn future_exp() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after epoch")
            .as_secs()
            + 3600
    }

    fn valid_session_cookie(state: &MiryadAuthState) -> String {
        let set_cookie = build_set_cookie(
            &OidcIdentity {
                id_token: make_jwt(future_exp()),
                subject: "session-user".to_string(),
                email: None,
                preferred_username: None,
            },
            &state.cookie_key,
            state.secure_cookies,
        );
        set_cookie
            .split(';')
            .next()
            .expect("cookie pair present")
            .to_string()
    }

    // ——— Requêtes et réponses servies ———

    struct Served {
        status: StatusCode,
        headers: HeaderMap,
        body: Vec<u8>,
    }

    impl Served {
        fn json(&self) -> Value {
            serde_json::from_slice(&self.body).expect("corps JSON-RPC interpretable")
        }
        fn text(&self) -> String {
            String::from_utf8_lossy(&self.body).into_owned()
        }
        fn content_type(&self) -> Option<&str> {
            self.headers
                .get(axum::http::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
        }
    }

    async fn serve(app: Router, req: Request<Body>) -> Served {
        let resp = app.oneshot(req).await.expect("router does not fail");
        let status = resp.status();
        let headers = resp.headers().clone();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("readable body");
        Served {
            status,
            headers,
            body: body.to_vec(),
        }
    }

    fn request(method: &str, uri: &str, extra: &[(&'static str, String)], body: Body) -> Request<Body> {
        let mut builder = Request::builder().method(method).uri(uri);
        for (name, value) in extra {
            builder = builder.header(*name, value);
        }
        builder.body(body).expect("valid request")
    }

    fn bearer_header(token: &str) -> (&'static str, String) {
        ("Authorization", format!("Bearer {token}"))
    }

    fn json_body(text: &str) -> Body {
        Body::from(text.to_string())
    }

    /// `POST /mcp` authentifie par bearer, `Content-Type: application/json`.
    async fn post_mcp(app: Router, token: &str, body: Body) -> Served {
        let bearer = bearer_header(token);
        let req = request(
            "POST",
            "/mcp",
            &[("Content-Type", "application/json".to_string()), bearer],
            body,
        );
        serve(app, req).await
    }

    async fn post_json(app: Router, token: &str, body: &str) -> Served {
        post_mcp(app, token, json_body(body)).await
    }

    // ——— Capture des traces (gabarit de ../auth/dual.rs, niveau en tete de ligne) ———

    #[derive(Clone, Default)]
    struct CapturedTraces(Arc<Mutex<Vec<String>>>);

    impl CapturedTraces {
        fn lines(&self) -> Vec<String> {
            self.0.lock().expect("test mutex is not poisoned").clone()
        }
        fn at_level(&self, level: &str) -> Vec<String> {
            self.lines()
                .into_iter()
                .filter(|line| line.starts_with(level))
                .collect()
        }
    }

    struct CaptureLayer(Arc<Mutex<Vec<String>>>);

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for CaptureLayer {
        fn on_event(&self, event: &tracing::Event<'_>, _ctx: tracing_subscriber::layer::Context<'_, S>) {
            if !event.metadata().target().starts_with("miryad_core") {
                return;
            }
            let mut visitor = MessageVisitor::default();
            event.record(&mut visitor);
            let line = format!("{} {}", event.metadata().level(), visitor.finish());
            self.0.lock().expect("test mutex is not poisoned").push(line);
        }
    }

    #[derive(Default)]
    struct MessageVisitor {
        rendered: String,
    }

    impl MessageVisitor {
        fn finish(&self) -> String {
            self.rendered.clone()
        }
    }

    impl tracing::field::Visit for MessageVisitor {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            use std::fmt::Write as _;
            if !self.rendered.is_empty() {
                self.rendered.push(' ');
            }
            let _ = write!(&mut self.rendered, "{}={value:?}", field.name());
        }
    }

    fn capture_traces() -> (tracing::subscriber::DefaultGuard, CapturedTraces) {
        use tracing_subscriber::layer::SubscriberExt as _;
        let captured = CapturedTraces::default();
        let subscriber =
            tracing_subscriber::registry::Registry::default().with(CaptureLayer(Arc::clone(&captured.0)));
        let guard = tracing::subscriber::set_default(subscriber);
        (guard, captured)
    }

    // ——— Sérialisation des tests du module ———
    //
    // Le cache d'interet de `tracing` est global et par callsite. Un test voisin sans
    // capture qui emet le meme callsite `warn!` (par exemple
    // `appel_refuse_par_la_politique…`, bisecte 2026-10-02) pendant la fenetre de capture
    // d'un test voisin peut fixer l'entree de ce callsite a « jamais » pour la generation
    // en cours : l'evenement ne remonte alors plus jusqu'a la `CaptureLayer` et
    // l'assertion de comptage devient un tirage. Seuls les tests de handler.rs emettent
    // les callsites `warn!`/`error!` captures ici — ils sont donc serialises deux a deux,
    // ce qui supprime la course sans cacher le comportement observe.

    /// Nombre de tours de test deja distribues.
    static TEST_TAKEN: AtomicUsize = AtomicUsize::new(0);
    /// Numéro de tour autorise a passer.
    static TEST_TURN: AtomicUsize = AtomicUsize::new(0);

    /// Garde de tour : rend la main au tour suivant a la fin du test, y compris sur panic.
    struct SerialTurn(usize);

    impl Drop for SerialTurn {
        fn drop(&mut self) {
            TEST_TURN.store(self.0.wrapping_add(1), Ordering::SeqCst);
        }
    }

    /// Prend le prochain tour de test. A appeler en tete de chaque test du module :
    /// le tour est rendu a la fin du corps (Drop), y compris en cas d'echec d'assertion.
    /// L'attente bloque le fil de test (chaque `#[tokio::test]` a son propre runtime
    /// mono-fil) jusqu'a son tour — les autres tests tournent sur d'autres fils libtest.
    fn serial_turn() -> SerialTurn {
        let mine = TEST_TAKEN.fetch_add(1, Ordering::SeqCst);
        while TEST_TURN.load(Ordering::SeqCst) != mine {
            std::thread::yield_now();
        }
        SerialTurn(mine)
    }

    // ——— Les 23 Scenario de `handler.sdd` + la parite de `Done when` ———

    /// `Scenario` : « tools/call valide avec token API ».
    #[tokio::test]
    async fn tools_call_valide_avec_token_api() {
        let _tour = serial_turn();
        let state = migrated_state().await;
        let alice_id = resolve_user(&state.db, "alice", None).await.expect("resolve").id;
        insert_recipe(&state.db, 1, "Tarte aux pommes", alice_id).await;
        let token = bearer_for(&state.db, "alice").await;
        let served = post_json(
            app(state, OutputFormat::Json),
            &token,
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"recipes_get","arguments":{"id":1}}}"#,
        )
        .await;
        assert_eq!(served.status, StatusCode::OK);
        assert_eq!(
            served.content_type(),
            Some("application/json"),
            "CONTENT_TYPE exact, sans charset : {:?}",
            served.headers.get(axum::http::header::CONTENT_TYPE)
        );
        let body = served.json();
        assert_eq!(body["jsonrpc"], "2.0");
        assert_eq!(body["id"], 1);
        assert!(
            !body.as_object().expect("objet").contains_key("error"),
            "`error` absent : {body}"
        );
        assert_eq!(body["result"]["content"][0]["type"], "text");
        let text = body["result"]["content"][0]["text"].as_str().expect("bloc text");
        let record: Value = serde_json::from_str(text).expect("le rendu est du JSON parsable");
        assert_eq!(record["title"], "Tarte aux pommes");
    }

    /// `Scenario` : « session cookie authentifie un tools/call comme le bearer ».
    #[tokio::test]
    async fn session_cookie_authentifie_un_tools_call_comme_le_bearer() {
        let _tour = serial_turn();
        let state = migrated_state().await;
        let session_id = resolve_user(&state.db, "session-user", None)
            .await
            .expect("resolve")
            .id;
        insert_recipe(&state.db, 1, "Tarte aux pommes", session_id).await;
        let cookie = valid_session_cookie(&state);
        let token = bearer_for(&state.db, "session-user").await;
        let call = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"recipes_get","arguments":{"id":1}}}"#;

        let served = serve(
            app(state.clone(), OutputFormat::Json),
            request(
                "POST",
                "/mcp",
                &[
                    ("Content-Type", "application/json".to_string()),
                    ("Cookie", cookie),
                ],
                json_body(call),
            ),
        )
        .await;
        assert_eq!(served.status, StatusCode::OK);
        let body = served.json();
        assert!(!body.as_object().expect("objet").contains_key("error"));
        let session_text = body["result"]["content"][0]["text"].as_str().expect("bloc text");

        // Aucune distinction de `PrincipalSource` sur le fil : le meme appel porte un
        // enregistrement identique par bearer que par session.
        let served_token = post_json(app(state, OutputFormat::Json), &token, call).await;
        assert_eq!(served_token.status, StatusCode::OK);
        let token_body = served_token.json();
        let token_text = token_body["result"]["content"][0]["text"]
            .as_str()
            .expect("bloc text");
        assert_eq!(
            session_text, token_text,
            "le fil ne trahit pas la source du principal"
        );
    }

    /// `Scenario` : « requete sans credentials est rejetee 401 avant corps » — corps
    /// non parsable livré, aucune erreur JSON-RPC n'apparait (regle « deux niveaux »).
    #[tokio::test]
    async fn requete_sans_credentials_est_rejetee_401_avant_corps() {
        let _tour = serial_turn();
        let state = migrated_state().await;
        let served = serve(
            app(state, OutputFormat::Json),
            request(
                "POST",
                "/mcp",
                &[("Content-Type", "application/json".to_string())],
                json_body("not-a-json"),
            ),
        )
        .await;
        assert_eq!(served.status, StatusCode::UNAUTHORIZED);
        assert_eq!(
            served.text(),
            "MRD-AUTH-001: not authenticated (no session cookie)"
        );
        assert!(
            !served.text().contains("\"error\"") && !served.text().contains("-32700"),
            "le refus est HTTP, pas une enveloppe JSON-RPC : {}",
            served.text()
        );
    }

    /// `Scenario` : « bearer invalide ne retombe pas sur le cookie » — la session valide
    /// presente n'est jamais consultee (sinon ce serait un `200`).
    #[tokio::test]
    async fn bearer_invalide_ne_retombe_pas_sur_le_cookie() {
        let _tour = serial_turn();
        let state = migrated_state().await;
        let cookie = valid_session_cookie(&state);
        let served = serve(
            app(state, OutputFormat::Json),
            request(
                "POST",
                "/mcp",
                &[
                    ("Content-Type", "application/json".to_string()),
                    ("Authorization", "Bearer mrd_pas_un_token".to_string()),
                    ("Cookie", cookie),
                ],
                json_body(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#),
            ),
        )
        .await;
        assert_eq!(served.status, StatusCode::UNAUTHORIZED);
        assert_eq!(served.text(), "MRD-AUTH-014: invalid or unknown API token");
        assert!(
            !served.text().contains("result"),
            "aucun `result` rendu : {}",
            served.text()
        );
    }

    /// `Scenario` : « seule la méthode POST est admise » — `405` corps vide et en-tete
    /// `Allow: POST` sur GET, PUT, OPTIONS et DELETE.
    #[tokio::test]
    async fn seule_la_methode_post_est_admise_sur_mcp() {
        let _tour = serial_turn();
        let state = migrated_state().await;
        for method in ["GET", "PUT", "OPTIONS", "DELETE"] {
            let served = serve(
                app(state.clone(), OutputFormat::Json),
                request(
                    method,
                    "/mcp",
                    &[("Content-Type", "application/json".to_string())],
                    Body::empty(),
                ),
            )
            .await;
            assert_eq!(served.status, StatusCode::METHOD_NOT_ALLOWED, "{method}");
            assert!(served.body.is_empty(), "corps vide attendu : {:?}", served.body);
            assert_eq!(
                served.headers.get("allow"),
                Some(&HeaderValue::from_static("POST")),
                "Allow: POST sur {method}"
            );
        }
    }

    /// `Scenario` : « le chemin /mcp ne se sert pas avec un slash final ».
    #[tokio::test]
    async fn le_chemin_mcp_ne_se_set_pas_avec_un_slash_final() {
        let _tour = serial_turn();
        let state = migrated_state().await;
        let token = bearer_for(&state.db, "alice").await;
        let bearer = bearer_header(&token);
        let served = serve(
            app(state, OutputFormat::Json),
            request(
                "POST",
                "/mcp/",
                &[("Content-Type", "application/json".to_string()), bearer],
                json_body(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#),
            ),
        )
        .await;
        assert_eq!(served.status, StatusCode::NOT_FOUND);
        assert!(
            served.body.is_empty(),
            "aucun dispatch ne tourne : {:?}",
            served.body
        );
    }

    /// `Scenario` : « corps JSON non syntaxique rend -32700 sous 200 » — corps exact
    /// affirme, octets non UTF-8 inclus (arbitre 2026-09-29).
    #[tokio::test]
    async fn corps_json_non_syntaxique_rend_moins_32700_sous_200() {
        let _tour = serial_turn();
        let state = migrated_state().await;
        let token = bearer_for(&state.db, "alice").await;
        let expected = json!({
            "jsonrpc": "2.0",
            "id": null,
            "error": { "code": -32700, "message": "MRD-MCP-009: parse error" }
        });
        let served = post_json(app(state.clone(), OutputFormat::Json), &token, "{oops").await;
        assert_eq!(served.status, StatusCode::OK);
        assert_eq!(served.json(), expected);

        let served = post_mcp(
            app(state, OutputFormat::Json),
            &token,
            Body::from(vec![0xFFu8, 0xFE]),
        )
        .await;
        assert_eq!(served.status, StatusCode::OK);
        assert_eq!(
            served.json(),
            expected,
            "les octets non UTF-8 rendent la meme enveloppe"
        );
    }

    /// `Scenario` : « Content-Type non JSON est rejeté 415 après l'auth » — corps exact
    /// d'axum, l'`AuthPrincipal` a ete extrait d'abord.
    #[tokio::test]
    async fn content_type_non_json_est_rejete_415_apres_l_auth() {
        let _tour = serial_turn();
        let state = migrated_state().await;
        let token = bearer_for(&state.db, "alice").await;
        let served = serve(
            app(state, OutputFormat::Json),
            request(
                "POST",
                "/mcp",
                &[("Content-Type", "text/plain".to_string()), bearer_header(&token)],
                json_body(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#),
            ),
        )
        .await;
        assert_eq!(served.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
        assert_eq!(
            served.text(),
            "Expected request with `Content-Type: application/json`"
        );
    }

    /// `Scenario` : « enveloppe invalide rend -32600 sous 200 » — `method` absent,
    /// `jsonrpc` 1.0, `id` booleen : message exact a chaque fois (arbitre 2026-09-29).
    #[tokio::test]
    async fn enveloppe_invalide_rend_moins_32600_sous_200() {
        let _tour = serial_turn();
        let state = migrated_state().await;
        let token = bearer_for(&state.db, "alice").await;
        let app = app(state, OutputFormat::Json);
        for body in [
            r#"{"jsonrpc":"2.0","params":{}}"#,
            r#"{"jsonrpc":"1.0","id":1,"method":"ping"}"#,
            r#"{"jsonrpc":"2.0","id":true,"method":"ping"}"#,
        ] {
            let served = post_json(app.clone(), &token, body).await;
            assert_eq!(served.status, StatusCode::OK, "{body}");
            let value = served.json();
            assert!(
                value.as_object().expect("objet").contains_key("id") && value["id"].is_null(),
                "`id` present valant null : {value}"
            );
            assert_eq!(value["error"]["code"], -32600, "{body}");
            assert_eq!(
                value["error"]["message"].as_str().expect("message"),
                "MRD-MCP-010: invalid request"
            );
        }
    }

    /// `Scenario` : « batch refuse en -32600 » — une seule reponse, aucun dispatch.
    #[tokio::test]
    async fn batch_refuse_en_moins_32600() {
        let _tour = serial_turn();
        let state = migrated_state().await;
        let token = bearer_for(&state.db, "alice").await;
        let served = post_json(
            app(state, OutputFormat::Json),
            &token,
            r#"[{"jsonrpc":"2.0","id":1,"method":"tools/list"}]"#,
        )
        .await;
        assert_eq!(served.status, StatusCode::OK);
        let value = served.json();
        assert!(value.is_object(), "une seule reponse objet, pas un lot : {value}");
        assert!(value["id"].is_null());
        assert_eq!(value["error"]["code"], -32600);
    }

    /// `Scenario` : « notification sans id répond 202 quelle que soit la méthode » —
    /// methode connue ou inconnue, corps vide, avant tout dispatch.
    #[tokio::test]
    async fn notification_sans_id_repond_202_quelle_que_soit_la_methode() {
        let _tour = serial_turn();
        let state = migrated_state().await;
        let token = bearer_for(&state.db, "alice").await;
        let app = app(state, OutputFormat::Json);
        for body in [
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            r#"{"jsonrpc":"2.0","method":"aucune/methode"}"#,
        ] {
            let served = post_json(app.clone(), &token, body).await;
            assert_eq!(served.status, StatusCode::ACCEPTED, "{body}");
            assert!(served.body.is_empty(), "corps vide attendu : {:?}", served.body);
        }
    }

    /// `Scenario` : « id null explicite rend -32600, pas un 202 » (arbitre 2026-09-29).
    #[tokio::test]
    async fn id_null_explicite_rend_moins_32600_pas_un_202() {
        let _tour = serial_turn();
        let state = migrated_state().await;
        let token = bearer_for(&state.db, "alice").await;
        let served = post_json(
            app(state, OutputFormat::Json),
            &token,
            r#"{"jsonrpc":"2.0","method":"tools/list","id":null}"#,
        )
        .await;
        assert_eq!(served.status, StatusCode::OK);
        let value = served.json();
        assert!(
            value.as_object().expect("objet").contains_key("id") && value["id"].is_null(),
            "`id` present valant null : {value}"
        );
        assert_eq!(value["error"]["code"], -32600);
    }

    /// `Scenario` : « initialize annonce la version du serveur, quelle que soit celle
    /// du client » — params du client non lus, id rendu verbatim.
    #[tokio::test]
    async fn initialize_annonce_la_version_du_serveur() {
        let _tour = serial_turn();
        let state = migrated_state().await;
        let token = bearer_for(&state.db, "alice").await;
        let served = post_json(
            app(state, OutputFormat::Json),
            &token,
            r#"{"jsonrpc":"2.0","id":"abc-123","method":"initialize","params":{"protocolVersion":"1999-01-01","capabilities":{}}}"#,
        )
        .await;
        assert_eq!(served.status, StatusCode::OK);
        let body = served.json();
        assert_eq!(body["result"]["protocolVersion"], "2024-11-05");
        assert_eq!(body["result"]["capabilities"], json!({ "tools": {} }));
        assert_eq!(body["result"]["serverInfo"]["name"], "miryad-core");
        assert_eq!(
            body["result"]["serverInfo"]["version"],
            env!("CARGO_PKG_VERSION"),
            "version comparee a CARGO_PKG_VERSION, pas a un litteral"
        );
        assert_eq!(body["id"], "abc-123", "l'id est rendu verbatim");
    }

    /// `Scenario` : « tools/list déclare cinq tools par entité » — ordre deterministe
    /// (`articles_*` avant `recipes_*`, puis l'ordre des operations), schemas exacts,
    /// descriptions litterales inchangees par le format `Custom`.
    #[tokio::test]
    async fn tools_list_declare_cinq_tools_par_entite() {
        let _tour = serial_turn();
        let state = migrated_state().await;
        let token = bearer_for(&state.db, "alice").await;
        let served = post_json(
            app_with_articles(state, OutputFormat::Custom("Recette : {{title}}".to_string())),
            &token,
            r#"{"jsonrpc":"2.0","id":5,"method":"tools/list"}"#,
        )
        .await;
        assert_eq!(served.status, StatusCode::OK);
        let body = served.json();
        let tools = body["result"]["tools"].as_array().expect("tableau de tools");
        let names: Vec<&str> = tools
            .iter()
            .map(|tool| tool["name"].as_str().expect("nom"))
            .collect();
        assert_eq!(
            names,
            [
                "articles_list",
                "articles_get",
                "articles_create",
                "articles_update",
                "articles_delete",
                "recipes_list",
                "recipes_get",
                "recipes_create",
                "recipes_update",
                "recipes_delete",
            ],
            "ordre deterministe : noms croissants puis l'ordre des operations"
        );

        let by_name = |name: &str| -> Value {
            tools
                .iter()
                .find(|tool| tool["name"] == name)
                .cloned()
                .unwrap_or_else(|| panic!("tool {name} absent"))
        };
        let id_schema = json!({
            "type": "object",
            "properties": { "id": { "type": "integer" } },
            "required": ["id"],
        });
        for name in ["recipes_get", "recipes_update", "recipes_delete"] {
            assert_eq!(by_name(name)["inputSchema"], id_schema, "{name}");
        }
        let list = by_name("recipes_list");
        let list_properties = list["inputSchema"]["properties"]
            .as_object()
            .expect("proprietes de recipes_list");
        // L'ordre de declaration (page, per_page, filter) est porte par registry.sdd :
        // `serde_json::Map` est un BTreeMap sans `preserve_order`, les cles lues sont
        // donc triees alphabetairement sur tout le chemin handler -> fil.
        assert_eq!(
            list_properties.keys().map(String::as_str).collect::<Vec<_>>(),
            ["filter", "page", "per_page"]
        );
        assert_eq!(list_properties["page"]["type"], "integer");
        assert_eq!(list_properties["per_page"]["type"], "integer");
        assert_eq!(list_properties["filter"]["type"], "string");
        assert!(
            !list["inputSchema"]
                .as_object()
                .expect("schema")
                .contains_key("required"),
            "aucun requis pour recipes_list : {list}"
        );
        assert_eq!(
            by_name("recipes_create")["inputSchema"],
            json!({ "type": "object" })
        );

        // Les descriptions sont les litteraux de la spec — l'`OutputFormat::Custom`
        // n'y introduit aucun template ni texte.
        assert_eq!(by_name("recipes_list")["description"], "List recipes, paginated");
        assert_eq!(
            by_name("recipes_get")["description"],
            "Get a single recipes by id"
        );
        assert_eq!(by_name("recipes_create")["description"], "Create a new recipes");
        assert_eq!(
            by_name("recipes_update")["description"],
            "Update an existing recipes by id"
        );
        assert_eq!(by_name("recipes_delete")["description"], "Delete a recipes by id");
        for tool in tools {
            let description = tool["description"].as_str().expect("description");
            assert!(
                !description.contains("{{"),
                "aucun gabarit dans une description : {description}"
            );
        }
    }

    /// `Scenario` : « méthode JSON-RPC sans dispatch répond -32601 préfixé sous 200 ».
    #[tokio::test]
    async fn methode_sans_dispatch_rend_moins_32601_prefixe_sous_200() {
        let _tour = serial_turn();
        let state = migrated_state().await;
        let token = bearer_for(&state.db, "alice").await;
        let served = post_json(
            app(state, OutputFormat::Json),
            &token,
            r#"{"jsonrpc":"2.0","id":7,"method":"ping"}"#,
        )
        .await;
        assert_eq!(served.status, StatusCode::OK);
        let body = served.json();
        assert_eq!(body["error"]["code"], -32601);
        assert_eq!(
            body["error"]["message"].as_str().expect("message"),
            "MRD-MCP-008: method not found: ping"
        );
        assert!(
            !body.as_object().expect("objet").contains_key("result"),
            "`result` absent : {body}"
        );
        assert_eq!(body["id"], 7);
    }

    /// `Scenario` : « outil inconnu au dispatch répond -32601 sous 200 » — suffixe hors
    /// table et ressource absente rendent la variante `UnknownTool` avec le nom recu.
    #[tokio::test]
    async fn outil_inconnu_au_dispatch_rend_moins_32601_sous_200() {
        let _tour = serial_turn();
        let state = migrated_state().await;
        let token = bearer_for(&state.db, "alice").await;
        let app = app(state, OutputFormat::Json);
        for (id, name) in [(11, "recipes_ping"), (12, "ghost_list")] {
            let params = json!({ "name": name, "arguments": {} });
            let served = post_json(
                app.clone(),
                &token,
                &format!(r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{params}}}"#),
            )
            .await;
            assert_eq!(served.status, StatusCode::OK);
            let body = served.json();
            assert_eq!(body["error"]["code"], -32601, "{name}");
            assert_eq!(
                body["error"]["message"].as_str().expect("message"),
                format!("MRD-MCP-006: unknown tool: {name}")
            );
            assert_eq!(body["id"], id);
        }
    }

    /// `Scenario` : « params invalides pour tools/call répond -32602 sous 200 » —
    /// `name` non textuel, `arguments` absents faute de cle requise, et `recipes_list`
    /// sans arguments qui reussit (arbitre 2026-09-29).
    #[tokio::test]
    async fn params_invalides_pour_tools_call_rendent_moins_32602_sous_200() {
        let _tour = serial_turn();
        let state = migrated_state().await;
        let token = bearer_for(&state.db, "alice").await;
        let app = app(state, OutputFormat::Json);

        let served = post_json(
            app.clone(),
            &token,
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":123}}"#,
        )
        .await;
        let body = served.json();
        assert_eq!(served.status, StatusCode::OK);
        assert_eq!(body["error"]["code"], -32602);
        assert!(
            body["error"]["message"]
                .as_str()
                .expect("message")
                .starts_with("MRD-MCP-005: invalid params"),
            "message préfixé MRD-MCP-005 : {:?}",
            body["error"]["message"]
        );

        let served = post_json(
            app.clone(),
            &token,
            r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"recipes_get"}}"#,
        )
        .await;
        let body = served.json();
        assert_eq!(
            body["error"]["code"], -32602,
            "arguments vides, faute de la cle id"
        );
        assert!(
            body["error"]["message"]
                .as_str()
                .expect("message")
                .starts_with("MRD-MCP-005: invalid params"),
            "{body}"
        );

        let served = post_json(
            app,
            &token,
            r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"recipes_list"}}"#,
        )
        .await;
        let body = served.json();
        assert!(
            !body.as_object().expect("objet").contains_key("error"),
            "recipes_list sans arguments reussit : {body}"
        );
        assert_eq!(body["result"]["content"][0]["type"], "text");
    }

    /// `Scenario` : « appel refusé par la politique rend -32001 sous 200 » — l'echec de
    /// politique ne se dit pas en `403` comme en REST ; `data` absent.
    #[tokio::test]
    async fn appel_refuse_par_la_politique_rend_moins_32001_sous_200() {
        let _tour = serial_turn();
        let state = migrated_state().await;
        let other_id = resolve_user(&state.db, "autre-possesseur", None)
            .await
            .expect("resolve")
            .id;
        insert_recipe(&state.db, 1, "Tarte d'autrui", other_id).await;
        let token = bearer_for(&state.db, "alice").await;
        let served = post_json(
            app(state, OutputFormat::Json),
            &token,
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"recipes_delete","arguments":{"id":1}}}"#,
        )
        .await;
        assert_eq!(
            served.status,
            StatusCode::OK,
            "l'echec de politique n'est pas un 403"
        );
        let body = served.json();
        assert_eq!(body["error"]["code"], -32001);
        assert_eq!(
            body["error"]["message"].as_str().expect("message"),
            "MRD-MCP-001: forbidden"
        );
        assert!(
            !body["error"]
                .as_object()
                .expect("objet error")
                .contains_key("data"),
            "`data` absent : {body}"
        );
    }

    /// `Scenario` : « hook applicatif rend -32000 avec son code dans data » — message
    /// nu du hook, aucun prefixe `MRD-`, `data` exactement `{"code":"HOOK-42"}`.
    #[tokio::test]
    async fn hook_applicatif_rend_moins_32000_avec_son_code_dans_data() {
        let _tour = serial_turn();
        let state = migrated_state().await;
        let token = bearer_for(&state.db, "alice").await;
        let served = post_json(
            app(state, OutputFormat::Json),
            &token,
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"recipes_create","arguments":{"id":0,"title":"Gaspacho","owner_id":0}}}"#,
        )
        .await;
        assert_eq!(served.status, StatusCode::OK);
        let body = served.json();
        assert_eq!(body["error"]["code"], -32000);
        assert_eq!(body["error"]["message"].as_str().expect("message"), "titre vide");
        assert_eq!(body["error"]["data"], json!({ "code": "HOOK-42" }));
    }

    /// `Scenario` : « delete valide rend la confirmation fixe, sous tout format » —
    /// texte exactement `deleted` en `Json` comme en `Markdown`, et la relire ensuite
    /// echoue bien en `-32002` (l'effet en base a eu lieu).
    #[tokio::test]
    async fn delete_valide_rend_la_confirmation_fixe_sous_tout_format() {
        let _tour = serial_turn();
        let state = migrated_state().await;
        let alice_id = resolve_user(&state.db, "alice", None).await.expect("resolve").id;
        let token = bearer_for(&state.db, "alice").await;

        for (format, row) in [(OutputFormat::Json, 1), (OutputFormat::Markdown, 2)] {
            insert_recipe(&state.db, row, "A supprimer", alice_id).await;
            let params = json!({ "name": "recipes_delete", "arguments": { "id": row } });
            let served = post_json(
                app(state.clone(), format),
                &token,
                &format!(r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{params}}}"#),
            )
            .await;
            assert_eq!(served.status, StatusCode::OK);
            let body = served.json();
            assert!(
                !body.as_object().expect("objet").contains_key("error"),
                "aucune erreur apres suppression commise : {body}"
            );
            let content = body["result"]["content"].as_array().expect("content");
            assert_eq!(content.len(), 1, "bloc unique");
            assert_eq!(content[0]["type"], "text");
            assert_eq!(
                content[0]["text"].as_str().expect("texte"),
                "deleted",
                "la confirmation fixe ne depend pas du gabarit de rendu"
            );
        }

        let served = post_json(
            app(state, OutputFormat::Json),
            &token,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"recipes_get","arguments":{"id":1}}}"#,
        )
        .await;
        assert_eq!(
            served.json()["error"]["code"],
            -32002,
            "l'enregistrement est bien supprime"
        );
    }

    /// `Scenario` : « corps au-dessus de la limite par défaut répond 413 » — corps
    /// d'axum prefixe `Failed to buffer the request body: `, aucune enveloppe emise.
    #[tokio::test]
    async fn corps_au_dessus_de_la_limite_par_defaut_repond_413() {
        let _tour = serial_turn();
        let state = migrated_state().await;
        let token = bearer_for(&state.db, "alice").await;
        let padding = "a".repeat(2 * 1024 * 1024 + 8);
        let body =
            format!(r#"{{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{{"pad":"{padding}"}}}}"#);
        let served = post_json(app(state, OutputFormat::Json), &token, &body).await;
        assert_eq!(served.status, StatusCode::PAYLOAD_TOO_LARGE);
        assert!(
            served.text().starts_with("Failed to buffer the request body: "),
            "corps attendu prefixe : {}",
            &served.text()[..served.text().len().min(80)]
        );
        assert!(
            !served.text().contains("jsonrpc"),
            "aucune enveloppe JSON-RPC emise"
        );
    }

    /// `Scenario` : « panne de base rend un -32603 générique et trace le détail » —
    /// message fil generique sans texte de driver, exactement un evenement `error`
    /// portant le detail de la `DbErr` (arbitre 2026-09-29).
    #[tokio::test]
    async fn panne_de_base_rend_un_moins_32603_generique_et_trace_le_detail() {
        let _tour = serial_turn();
        let state = migrated_state_without_entities().await;
        let token = bearer_for(&state.db, "alice").await;
        let (_guard, captured) = capture_traces();
        let served = post_json(
            app(state, OutputFormat::Json),
            &token,
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"recipes_list","arguments":{}}}"#,
        )
        .await;
        assert_eq!(served.status, StatusCode::OK);
        let body = served.json();
        assert_eq!(body["error"]["code"], -32603);
        assert_eq!(
            body["error"]["message"].as_str().expect("message"),
            "MRD-MCP-003: database error"
        );
        assert!(
            !served.text().contains("no such table"),
            "le detail de la DbErr ne sort jamais sur le fil : {}",
            served.text()
        );

        let errors = captured.at_level("ERROR");
        assert_eq!(errors.len(), 1, "exactement un evenement error : {errors:?}");
        assert!(
            errors[0].contains("no such table"),
            "la trace porte le detail de la DbErr : {}",
            errors[0]
        );
    }

    /// `Scenario` : « appel interdit trace un warn sans arguments » — un evenement
    /// `warn` pour le `-32001`, jamais les arguments ni un secret.
    #[tokio::test]
    async fn appel_interdit_trace_un_warn_sans_arguments() {
        let _tour = serial_turn();
        let state = migrated_state().await;
        let other_id = resolve_user(&state.db, "autre-possesseur", None)
            .await
            .expect("resolve")
            .id;
        insert_recipe(&state.db, 1, "Tarte d'autrui", other_id).await;
        let token = bearer_for(&state.db, "alice").await;
        let (_guard, captured) = capture_traces();
        let served = post_json(
            app(state, OutputFormat::Json),
            &token,
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"recipes_delete","arguments":{"id":1}}}"#,
        )
        .await;
        assert_eq!(served.json()["error"]["code"], -32001);
        let warns = captured.at_level("WARN");
        assert_eq!(
            warns.len(),
            1,
            "un evenement warn est capture : lignes={:?} status={:?} body={}",
            captured.lines(),
            served.status,
            served.text()
        );
        for line in captured.lines() {
            assert!(!line.contains(&token), "aucune trace ne porte le token : {line}");
            assert!(
                !line.contains("arguments"),
                "aucune trace ne porte les arguments : {line}"
            );
        }
    }

    /// `Done when` : « la parite de surface est tenue : les memes refus que REST
    /// produisent la meme decision `rest::core` rendue `-32001`/`-32002` par error.rs
    /// (test de comparaison lu, pas suppose) » — la decision vient de `rest::core` lui-meme.
    #[tokio::test]
    async fn parite_refus_rest_rendus_moins_32001_et_moins_32002() {
        use crate::mcp::error::McpError;
        use crate::rest::error::RestError;

        let _tour = serial_turn();
        let state = migrated_state().await;
        let alice = principal("alice");
        let other_id = resolve_user(&state.db, "autre-possesseur", None)
            .await
            .expect("resolve")
            .id;
        insert_recipe(&state.db, 1, "Tarte d'autrui", other_id).await;

        // La decision REST nue : supprimer la ligne d'autrui est un `RestError::Forbidden`.
        let forbidden = crate::rest::core::delete::<recipe::Entity>(&state.db, &alice, 1)
            .await
            .expect_err("le refus de politique est attendu");
        assert!(matches!(forbidden, RestError::Forbidden), "{forbidden:?}");
        let mcp: McpError = forbidden.into();
        assert_eq!(mcp.rpc_code(), -32001);
        assert_eq!(mcp.wire_message(), "MRD-MCP-001: forbidden");
        assert_eq!(mcp.data(), None);

        // La relire par le noyau REST : `RestError::NotFound` ; en MCP, `-32002`.
        let not_found = crate::rest::core::get::<recipe::Entity>(&state.db, &alice, 999)
            .await
            .expect_err("la cle absente est attendue");
        assert!(matches!(not_found, RestError::NotFound), "{not_found:?}");
        let mcp: McpError = not_found.into();
        assert_eq!(mcp.rpc_code(), -32002);
        assert_eq!(mcp.wire_message(), "MRD-MCP-002: resource not found");
        assert_eq!(mcp.data(), None);
    }
}
