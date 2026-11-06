//! Handlers JSON des conversations interactives (`src/lymaille/http/routes.sdd`).

use axum::Json;
use axum::extract::Extension;
use axum::extract::Path;
use axum::extract::Query;
use axum::extract::State;
use axum::extract::rejection::JsonRejection;
use axum::http::StatusCode;
use serde::Deserialize;
use serde::Serialize;
use zeroize::Zeroizing;

use ::lymaille::DraftKind;
use ::lymaille::Message;
use ::lymaille::Redactor;
use ::lymaille::ReviewState;
use ::lymaille::SessionFilter;
use ::lymaille::SessionId;
use ::lymaille::SessionKind;
use ::lymaille::SessionState;
use ::lymaille::SessionStore as _;
use ::lymaille::SessionSummary;
use ::lymaille::Timestamp;

use crate::auth::AuthPrincipal;
use crate::auth::MiryadAuthState;
use crate::auth::PrincipalSource;
use crate::lymaille::error::LymailleError;
use crate::lymaille::identity::Identity;
use crate::lymaille::interactive::activation::activate_toolset;
use crate::lymaille::interactive::activation::discard_draft;
use crate::lymaille::interactive::event::SessionEvent;
use crate::lymaille::interactive::event::SessionEventKind;
use crate::lymaille::interactive::event::SessionOutcome;
use crate::lymaille::interactive::manager::SessionHandle;
use crate::lymaille::interactive::manager::SessionManager;
use crate::lymaille::store::LymailleStores;
use crate::rest::error::RestError;

/// Plafond du texte d'un message (`Must` de `./routes.sdd`) : 32 Ko. Le corps total est borné
/// plus haut par `DefaultBodyLimit` (64 Ko, ./mod.sdd).
const MAX_TEXT_BYTES: usize = 32 * 1024;

/// Traduction des erreurs `LymailleError` (`Must` de `./routes.sdd`) : textes fixes, jamais le
/// contenu d'un message, d'un jeton, d'un prompt ni d'une URL. `UnknownSubject`/`Forbidden` →
/// `403`, `NotFound` → `404` indiscernable, `Conflict` → `409`, `InvalidDefinition` → `422` ;
/// `Store` et le reste → `500` — le code `MRD-LYMAILLE-*` voyage dans la `Display` portée par la
/// trace `error!` de `RestError::Internal`, jamais sur le fil.
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

/// Corps JSON accepté : toute syntaxe ou forme invalide est le même `422` à texte fixe ; les
/// corps à l'ancienne (`kind`/`agent_id`, tout champ inconnu, `context` manquant) tombent là
/// (`Must` de `./routes.sdd`, `Scenario` « le corps d'ouverture à l'ancienne est refusé »).
fn json_body<T>(body: Result<Json<T>, JsonRejection>) -> Result<T, RestError> {
    match body {
        Ok(Json(value)) => Ok(value),
        Err(_rejection) => Err(RestError::InvalidInput("invalid JSON body".to_owned())),
    }
}

/// Identifiant de session du chemin : toute forme invalide est un `404` indiscernable.
fn parse_session(text: &str) -> Result<SessionId, RestError> {
    SessionId::new(text).ok_or(RestError::NotFound)
}

/// Jeton `forward` transmis à `SessionManager::send` (`Must` de `./routes.sdd`) : seul le JWT
/// Bearer d'une session OIDC validée est relayé ; le cookie de session et les tokens API n'en
/// transmettent aucun. Le jeton n'est jamais journalisé.
fn forward_token(source: &PrincipalSource) -> Option<Zeroizing<String>> {
    match source {
        PrincipalSource::Bearer { jwt } => Some(Zeroizing::new(jwt.clone())),
        PrincipalSource::Session { .. } | PrincipalSource::ApiToken { .. } | PrincipalSource::Agent => None,
    }
}

/// Anse JSON d'une session ouverte ou reprise (`Must` de `./routes.sdd`) : la nature rendue est
/// celle que le type du contexte a résolue (`DEC-28`), jamais une nature demandée à l'appelant.
#[derive(Serialize)]
pub(super) struct SessionBody {
    /// Identifiant opaque de la session.
    id: String,
    /// Nature résolue (`work`/`config`, lowercase comme l'amont).
    kind: SessionKind,
    /// Agent retenu (explicite ou défaut du type).
    agent_id: String,
    /// Contexte persisté de la session.
    context: String,
}

/// Anse de `resume` : la même session, plus les avis de provenance de la reprise (`OI-8`).
#[derive(Serialize)]
pub(super) struct ResumeBody {
    /// Identifiant opaque de la session.
    id: String,
    /// Nature rechargée.
    kind: SessionKind,
    /// Agent rechargé.
    agent_id: String,
    /// Contexte relu de la ligne (colonne immuable).
    context: String,
    /// Avis de provenance (empreinte ou version changée) ; vide quand rien n'a bougé.
    notices: Vec<String>,
}

/// Le corps `{ "context": "…", "agent": "…" }` de l'ouverture (`DEC-28`) : `deny_unknown_fields`
/// est ce qui refuse le contrat d'avant R12 (`kind`, `agent_id`) comme tout champ inconnu.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CreateBody {
    /// Nom du contexte (`G_…`, privé, ou `H_…`) sur lequel ouvrir.
    context: String,
    /// Agent explicite, sinon le défaut du type du contexte.
    agent: Option<String>,
}

/// Le corps `{ "text": "…" }` d'un message.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct TextBody {
    /// Texte du message (non vide, ≤ 32 Ko — vérifié par le handler).
    text: String,
}

/// Filtre de liste `?agent=&kind=&limit=` — tout est optionnel, le vide garde tout.
#[derive(Deserialize)]
pub(super) struct ListQuery {
    agent: Option<String>,
    kind: Option<String>,
    limit: Option<String>,
}

/// Résumés expurgés rendus par `GET /conversations` : six clés par ligne, le détail (fonctions,
/// erreurs rhai) de `SessionSummary` reste derrière.
#[derive(Serialize)]
pub(super) struct ListBody {
    /// Les sessions de l'appelante selon le filtre.
    items: Vec<SessionItem>,
}

#[derive(Serialize)]
struct SessionItem {
    id: String,
    title: String,
    agent: String,
    kind: SessionKind,
    turns: u64,
    review: Option<&'static str>,
}

#[derive(Serialize)]
pub(super) struct CancelledBody {
    /// `true` si un tour actif venait d'être signalé.
    cancelled: bool,
}

/// L'historique rendu sous les types du flux (`Must` de `./routes.sdd`, DSD `DEC-30`) : mêmes
/// `SessionEvent` que le SSE, le client lit puis s'abonne sans trou de type.
#[derive(Serialize)]
pub(super) struct HistoryBody {
    /// L'état persisté rendu en événements (chronologique).
    events: Vec<SessionEvent>,
}

/// Le rapport d'activation rendu par `POST /toolsets/{name}/activate` (miroir JSON de
/// `ActivationReport`).
#[derive(Serialize)]
pub(super) struct ActivationBody {
    name: String,
    version: String,
    previous_version: Option<String>,
}

fn session_body(handle: &SessionHandle) -> SessionBody {
    SessionBody {
        id: handle.id.as_str().to_owned(),
        kind: handle.kind,
        agent_id: handle.agent_id.clone(),
        context: handle.context.clone(),
    }
}

/// `POST /conversations` : ouverture par contexte (`DEC-28`) — la nature vient du TYPE du
/// contexte, l'agent est optionnel (défaut du type sinon).
pub(super) async fn create_conversation(
    principal: AuthPrincipal,
    Extension(manager): Extension<SessionManager>,
    body: Result<Json<CreateBody>, JsonRejection>,
) -> Result<(StatusCode, Json<SessionBody>), RestError> {
    let body = json_body(body)?;
    let handle = manager
        .open(&principal.subject, &body.context, body.agent.as_deref())
        .await
        .map_err(to_rest)?;
    Ok((StatusCode::CREATED, Json(session_body(&handle))))
}

/// `GET /conversations?agent=&kind=&limit=` : résumés expurgés des sessions de l'appelante.
pub(super) async fn list_conversations(
    principal: AuthPrincipal,
    Extension(manager): Extension<SessionManager>,
    Query(query): Query<ListQuery>,
) -> Result<Json<ListBody>, RestError> {
    let mut filter = SessionFilter::new();
    if let Some(agent) = query.agent {
        filter = filter.with_agent(agent);
    }
    filter = match query.kind.as_deref() {
        None => filter,
        Some("work") => filter.with_kind(SessionKind::Work),
        Some("config") => filter.with_kind(SessionKind::Config),
        Some(_) => return Err(RestError::InvalidInput("invalid kind filter".to_owned())),
    };
    if let Some(limit) = query.limit {
        let parsed = limit
            .parse::<usize>()
            .map_err(|_cause| RestError::InvalidInput("invalid limit filter".to_owned()))?;
        filter = filter.with_limit(parsed);
    }
    let summaries = manager.list(&principal.subject, &filter).await.map_err(to_rest)?;
    Ok(Json(ListBody {
        items: summaries.iter().map(session_item).collect(),
    }))
}

/// Ligne expurgée d'un résumé : le titre est déjà masqué par le `Redactor` côté `list` de
/// `SessionManager` ; l'état de revue ne rend que sa classe, jamais la raison.
fn session_item(summary: &SessionSummary) -> SessionItem {
    SessionItem {
        id: summary.id().as_str().to_owned(),
        title: summary.title().to_owned(),
        agent: summary.agent().to_owned(),
        kind: summary.kind(),
        turns: summary.turns(),
        review: render_review(summary.review()),
    }
}

fn render_review(review: Option<&ReviewState>) -> Option<&'static str> {
    match review {
        Some(ReviewState::Done) => Some("done"),
        Some(ReviewState::Requested(_)) => Some("requested"),
        Some(_) | None => None,
    }
}

/// `POST /conversations/{id}/messages` : lance le tour sans l'attendre (`202`) ; le jeton Bearer
/// de la requête, s'il existe, est transmis à `send` (usage `forward`, jamais journalisé).
pub(super) async fn send_message(
    principal: AuthPrincipal,
    Extension(manager): Extension<SessionManager>,
    Path(id): Path<String>,
    body: Result<Json<TextBody>, JsonRejection>,
) -> Result<StatusCode, RestError> {
    let session = parse_session(&id)?;
    let body = json_body(body)?;
    if body.text.is_empty() || body.text.len() > MAX_TEXT_BYTES {
        return Err(RestError::InvalidInput(
            "message text must be non-empty and at most 32 KiB".to_owned(),
        ));
    }
    manager
        .send(
            &principal.subject,
            &session,
            body.text,
            forward_token(&principal.source),
        )
        .await
        .map_err(to_rest)?;
    Ok(StatusCode::ACCEPTED)
}

/// `POST /conversations/{id}/cancel` : annulation coopérative du tour actif.
pub(super) async fn cancel_conversation(
    principal: AuthPrincipal,
    Extension(manager): Extension<SessionManager>,
    Path(id): Path<String>,
) -> Result<Json<CancelledBody>, RestError> {
    let session = parse_session(&id)?;
    let cancelled = manager
        .cancel(&principal.subject, &session)
        .await
        .map_err(to_rest)?;
    Ok(Json(CancelledBody { cancelled }))
}

/// `POST /conversations/{id}/resume` : reprise de l'état persisté avec les avis de provenance.
pub(super) async fn resume_conversation(
    principal: AuthPrincipal,
    Extension(manager): Extension<SessionManager>,
    Path(id): Path<String>,
) -> Result<Json<ResumeBody>, RestError> {
    let session = parse_session(&id)?;
    let handle = manager
        .resume(&principal.subject, &session)
        .await
        .map_err(to_rest)?;
    Ok(Json(ResumeBody {
        id: handle.id.as_str().to_owned(),
        kind: handle.kind,
        agent_id: handle.agent_id,
        context: handle.context,
        notices: handle.notices,
    }))
}

/// `GET /conversations/{id}/history` : l'état persisté rendu sous les types du flux SSE
/// (`DEC-30`). Session inconnue ou d'un autre : le même `404` que partout.
pub(super) async fn history(
    State(auth): State<MiryadAuthState>,
    principal: AuthPrincipal,
    Path(id): Path<String>,
) -> Result<Json<HistoryBody>, RestError> {
    let session = parse_session(&id)?;
    let state = load_state(&auth.db, &principal.subject, &session).await?;
    Ok(Json(HistoryBody {
        events: history_events(&state),
    }))
}

/// `DELETE /conversations/{id}` : fermeture — la session d'un autre est le même `404` qu'une
/// session absente (`close` du gestionnaire est silencieuse, la garde de visibilité est ici).
pub(super) async fn close_conversation(
    State(auth): State<MiryadAuthState>,
    principal: AuthPrincipal,
    Extension(manager): Extension<SessionManager>,
    Path(id): Path<String>,
) -> Result<StatusCode, RestError> {
    let session = parse_session(&id)?;
    load_state(&auth.db, &principal.subject, &session).await?;
    manager.close(&principal.subject, &session).await;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /toolsets/{name}/activate` : le propriétaire promeut son brouillon (`../interactive/
/// activation.sdd`) — aucune décision de droit propre ici.
pub(super) async fn activate_toolset_route(
    State(auth): State<MiryadAuthState>,
    principal: AuthPrincipal,
    Path(name): Path<String>,
) -> Result<Json<ActivationBody>, RestError> {
    let caller = Identity::new(auth.db.clone())
        .resolve(&principal.subject)
        .await
        .map_err(to_rest)?;
    let report = activate_toolset(&auth.db, &caller, &name)
        .await
        .map_err(to_rest)?;
    Ok(Json(ActivationBody {
        name: report.name,
        version: report.version,
        previous_version: report.previous_version,
    }))
}

/// `DELETE /toolsets/{name}/draft` : jette le brouillon — `204` s'il existait, `404` sinon.
pub(super) async fn discard_draft_route(
    State(auth): State<MiryadAuthState>,
    principal: AuthPrincipal,
    Path(name): Path<String>,
) -> Result<StatusCode, RestError> {
    let caller = Identity::new(auth.db.clone())
        .resolve(&principal.subject)
        .await
        .map_err(to_rest)?;
    if discard_draft(&auth.db, &caller, DraftKind::Toolset, &name)
        .await
        .map_err(to_rest)?
    {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(RestError::NotFound)
    }
}

/// Lecture de la ligne d'état pour les routes que `SessionManager` ne borne pas lui-même
/// (`history`, `DELETE`) : le store est indexé `(subject, id)`, absente comme tenue par un autre
/// rend le même `NotFound` (`Must not` de la spec).
async fn load_state(
    db: &sea_orm::DatabaseConnection,
    subject: &str,
    id: &SessionId,
) -> Result<SessionState, RestError> {
    LymailleStores::new(db.clone(), subject.to_owned())
        .sessions()
        .load(id)
        .await
        .map_err(|error| to_rest(LymailleError::Store(error.to_string())))?
        .ok_or(RestError::NotFound)
}

/// Rendu de l'état persisté en événements du flux (DSD `DEC-30`, arbitrage historique complet du
/// 2026-11-05) : dans l'ordre de l'état, chaque `Message::User` rend `UserMessage { turn, text }`
/// (il ouvre un tour), chaque `ToolCall` d'un message d'assistant rend `ToolCallSummary`
/// (`has_result` = un `Message::ToolResult` de même `call_id` suit), et chaque réponse d'assistant
/// non vide rend `TurnEnded { outcome : valid }`. Les teneurs (`text`, `response`) passent par le
/// `Redactor` du chemin d'affichage ; ni arguments d'outil ni contenu de `ToolResult` ne sortent.
/// `System` (prompt), `Boundary` et `Discarded` (réponse écartée, conservée pour audit seulement)
/// ne sont jamais rendus et ne comptent pas comme tour. **Limite assumée (`DEC-30`)** :
/// `lymaille` 0.2.0 n'horodate pas ses messages ; `at` vaut l'instant de création de la session
/// pour tout l'historique — le client ne doit pas l'employer comme date.
fn history_events(state: &SessionState) -> Vec<SessionEvent> {
    render_history(state, &Redactor::new())
}

/// Rendu effectif de l'état (même contrat que `history_events`) avec le `Redactor` du chemin
/// d'affichage : la couture porte l'expurgation des teneurs (`Must` : « textes passés au
/// `Redactor` »). La route la ferme sur un redacteur sans secret déclaré — la route de lecture
/// n'a pas la liste des secrets de la session ; c'est la convention déjà prise côté
/// `SessionManager::list` (`SessionSummary::from_state`).
fn render_history(state: &SessionState, redactor: &Redactor) -> Vec<SessionEvent> {
    let at = timestamp_rfc3339(state.created_at());
    let session = state.id().as_str().to_owned();
    let messages = state.history().messages();
    let mut events = Vec::new();
    let mut turn = 0_u64;
    for (index, message) in messages.iter().enumerate() {
        match message {
            Message::User { content } => {
                turn = turn.saturating_add(1);
                events.push(SessionEvent {
                    session: session.clone(),
                    at: at.clone(),
                    kind: SessionEventKind::UserMessage {
                        turn,
                        text: redactor.redact(content),
                    },
                });
            }
            Message::Assistant {
                content, tool_calls, ..
            } => {
                for call in tool_calls {
                    events.push(SessionEvent {
                        session: session.clone(),
                        at: at.clone(),
                        kind: SessionEventKind::ToolCallSummary {
                            turn: turn.max(1),
                            tool: call.name().to_owned(),
                            call_id: call.id().to_owned(),
                            has_result: messages.iter().skip(index.saturating_add(1)).any(|later| {
                                matches!(later,
                                        Message::ToolResult { call_id, .. }
                                            if call_id == call.id())
                            }),
                        },
                    });
                }
                if !content.is_empty() {
                    events.push(SessionEvent {
                        session: session.clone(),
                        at: at.clone(),
                        kind: SessionEventKind::TurnEnded {
                            turn: turn.max(1),
                            outcome: SessionOutcome::Valid,
                            response: redactor.redact(content),
                        },
                    });
                }
            }
            Message::System { .. }
            | Message::ToolResult { .. }
            | Message::Boundary { .. }
            | Message::Discarded { .. } => {}
            _unknown_amont => {}
        }
    }
    events
}

/// Instant amont en RFC 3339 UTC (même conversion que `../interactive/event.rs`, fonction privée
/// là-bas) : un instant hors de portée est ramené à l'heure courante, jamais un événement perdu.
fn timestamp_rfc3339(at: Timestamp) -> String {
    let millis = i64::try_from(at.as_millis()).unwrap_or(i64::MAX);
    let moment = chrono::DateTime::from_timestamp_millis(millis).unwrap_or_else(chrono::Utc::now);
    moment.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

#[cfg(test)]
mod tests {
    //! Les 8 `Scenario` de `./routes.sdd` exercés par `tower::ServiceExt::oneshot` sur le même
    //! montage que ./mod.rs (routeur + `Extension(manager)` + `DefaultBodyLimit`), SQLite
    //! mémoire, provider scripté.

    use super::activate_toolset_route;
    use super::cancel_conversation;
    use super::close_conversation;
    use super::create_conversation;
    use super::discard_draft_route;
    use super::forward_token;
    use super::history;
    use super::history_events;
    use super::list_conversations;
    use super::render_history;
    use super::resume_conversation;
    use super::send_message;
    use super::to_rest;
    use crate::auth::AuthPrincipal;
    use crate::auth::PrincipalSource;
    use crate::lymaille::error::LymailleError;
    use crate::lymaille::http::sse;
    use crate::lymaille::interactive::manager::SessionManager;

    use crate::auth::MiryadAuthState;
    use crate::auth::MockOidcClient;
    use crate::auth::OidcIdentity;
    use crate::lymaille::http::routes::fix;
    use ::lymaille::SessionId;
    use axum::Router;
    use axum::body::Body;
    use axum::extract::DefaultBodyLimit;
    use axum::http::{Request, StatusCode};
    use axum::response::IntoResponse as _;
    use axum::routing::delete;
    use axum::routing::get;
    use axum::routing::post;
    use fix::*;
    use sea_orm::DatabaseConnection;
    use serde_json::json;
    use std::sync::Arc;
    use tower::ServiceExt;

    /// Le montage complet des handlers — miroir de `conversations_router` (l'agrégation exacte
    /// est sous contrat de ./mod.rs, testée là-bas).
    fn app(manager: SessionManager, db: DatabaseConnection) -> Router {
        app_on(manager, test_state(db))
    }

    /// Le même montage sur un état d'authentification fourni (le `MockOidcClient` de
    /// `test_state` est configurable par appelant : branche Bearer des scénarios).
    fn app_on(manager: SessionManager, state: MiryadAuthState) -> Router {
        Router::new()
            .nest(
                "/api/v1/lymaille",
                Router::new()
                    .route(
                        "/conversations",
                        post(create_conversation).get(list_conversations),
                    )
                    .route("/conversations/{id}/messages", post(send_message))
                    .route("/conversations/{id}/events", get(sse::events))
                    .route("/conversations/{id}/history", get(history))
                    .route("/conversations/{id}/cancel", post(cancel_conversation))
                    .route("/conversations/{id}/resume", post(resume_conversation))
                    .route("/conversations/{id}", delete(close_conversation))
                    .route("/toolsets/{name}/activate", post(activate_toolset_route))
                    .route("/toolsets/{name}/draft", delete(discard_draft_route)),
            )
            .layer(axum::Extension(manager))
            .layer(DefaultBodyLimit::max(64 * 1024))
            .with_state(state)
    }

    async fn serve_json(app: Router, request: Request<Body>) -> (StatusCode, serde_json::Value) {
        let response = app.oneshot(request).await.expect("le routeur repond");
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("corps lisible");
        let value = if bytes.is_empty() {
            serde_json::Value::Null
        } else {
            match serde_json::from_slice(&bytes) {
                Ok(value) => value,
                // les rendus d'erreur de `RestError` sont du texte porteur de code, pas du JSON
                Err(_cause) => serde_json::Value::String(String::from_utf8_lossy(&bytes).into_owned()),
            }
        };
        (status, value)
    }

    async fn serve_raw(app: Router, request: Request<Body>) -> (StatusCode, Vec<u8>, Vec<String>) {
        let response = app.oneshot(request).await.expect("le routeur repond");
        let status = response.status();
        let headers = response
            .headers()
            .iter()
            .map(|(name, value)| format!("{name}: {}", value.to_str().unwrap_or("?")))
            .collect();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("corps lisible");
        (status, bytes.to_vec(), headers)
    }

    /// Réponse streaming (SSE) : statut lu, corps rendu comme flux de fragments UTF-8 — le
    /// corps ne se lit jamais entier (`to_bytes` bouclerait sur un flux vivant).
    async fn serve_stream(
        app: Router,
        request: Request<Body>,
    ) -> (StatusCode, impl futures::Stream<Item = String> + Unpin) {
        let response = app.oneshot(request).await.expect("le routeur repond");
        let status = response.status();
        (status, body_stream(response))
    }

    // ── Scénarios ────────────────────────────────────────────────────────────

    /// Scenario « ouvrir puis envoyer rend 202 sans attendre » : provider lent (bloque),
    /// l'ouverture rend la nature portée par le type, l'envoi revient `202` sans attendre la fin.
    #[tokio::test(flavor = "multi_thread")]
    async fn ouvrir_puis_envoyer_rend_202_sans_attendre() {
        let (db, _hub, hang, manager) = setup_with(hang_factory()).await;
        let alice = token_for(&db, "alice").await;
        let (status, body) = serve_json(
            app(manager.clone(), db.clone()),
            post_json(
                "/api/v1/lymaille/conversations",
                &alice,
                &json!({ "context": "c1" }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "ouverture par contexte : {body}");
        assert_eq!(body["kind"], json!("work"), "la nature vient du type : {body}");
        assert_eq!(
            body["agent_id"],
            json!("dev"),
            "l'agent est le defaut du type : {body}"
        );
        assert_eq!(body["context"], json!("c1"), "le contexte est rendu : {body}");
        let id = body["id"].as_str().expect("un id est rendu").to_owned();

        let started = std::time::Instant::now();
        let (status, _) = serve_json(
            app(manager.clone(), db.clone()),
            post_json(
                &format!("/api/v1/lymaille/conversations/{id}/messages"),
                &alice,
                &json!({ "text": "bonjour" }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED, "le tour est lance sans attente");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "le 202 n'a pas attendu le tour : {:?}",
            started.elapsed()
        );
        wait_entered(&hang).await;
        hang.release.add_permits(8);
    }

    /// Scenario « historique puis abonnement, mêmes types » (amendement historique complet, DSD
    /// `DEC-30`) : après un tour terminé, l'historique rend dans l'ordre `user_message` puis
    /// `turn_ended` (types exportés du flux SSE, teneurs passées au `Redactor`) ; l'abonnement au
    /// flux puis un nouveau tour enchaînent sans trou de type. Le `tool_call_summary` du `Then`
    /// est verrouillé au niveau du mapping (`l_historique_rend_les_trois_variantes_dans_l_ordre_persiste`) :
    /// le provider scripté de la fixture n'émet pas d'appel d'outil.
    #[tokio::test(flavor = "multi_thread")]
    async fn historique_puis_abonnement_memes_types() {
        let (db, _hub, manager) = setup().await;
        let alice = token_for(&db, "alice").await;
        let (status, body) = serve_json(
            app(manager.clone(), db.clone()),
            post_json(
                "/api/v1/lymaille/conversations",
                &alice,
                &json!({ "context": "c1" }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let id = body["id"].as_str().expect("id").to_owned();
        let session = SessionId::new(id.clone()).expect("id de session valide");
        let (status, _) = serve_json(
            app(manager.clone(), db.clone()),
            post_json(
                &format!("/api/v1/lymaille/conversations/{id}/messages"),
                &alice,
                &json!({ "text": "bonjour MARQUEUR-VISIBLE" }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED);
        wait_state(&db, "alice", &session, |etat| etat.contains("reponse de tour")).await;

        let (status, history) = serve_json(
            app(manager.clone(), db.clone()),
            get_json(&format!("/api/v1/lymaille/conversations/{id}/history"), &alice),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "l'historique se relit");
        let events = history["events"].as_array().expect("evenements en tableau");
        assert!(!events.is_empty(), "l'historique porte la conversation");
        for event in events {
            let kind_type = event["kind"]["type"].as_str().expect("kind.type present");
            assert!(
                KNOWN_EVENT_TYPES.contains(&kind_type),
                "le type rendu est celui du flux SSE : {kind_type}"
            );
        }
        let types: Vec<&str> = events
            .iter()
            .map(|event| event["kind"]["type"].as_str().expect("kind.type present"))
            .collect();
        assert_eq!(
            types,
            vec!["user_message", "turn_ended"],
            "l'historique rend le tour dans l'ordre de l'etat persiste : {events:?}"
        );
        assert_eq!(
            events[0]["kind"]["text"].as_str().expect("texte rendu"),
            "bonjour MARQUEUR-VISIBLE",
            "le message utilisateur est rendu, texte passe par le Redactor (aucun secret configure)"
        );
        assert_eq!(
            events[1]["kind"]["response"].as_str().expect("reponse rendue"),
            "reponse de tour",
            "la reponse d'assistant est rendue en turn_ended"
        );

        let (status, mut body) = serve_stream(
            app(manager.clone(), db.clone()),
            get_json(&format!("/api/v1/lymaille/conversations/{id}/events"), &alice),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "l'abonnement suit l'historique");
        serve_json(
            app(manager.clone(), db.clone()),
            post_json(
                &format!("/api/v1/lymaille/conversations/{id}/messages"),
                &alice,
                &json!({ "text": "la suite" }),
            ),
        )
        .await;
        let stream_text = read_until(&mut body, |text| text.contains("event: turn_ended")).await;
        assert!(
            stream_text.contains("event: turn_ended"),
            "le nouveau tour coule : {stream_text}"
        );
        let flux_type = extract_type(&stream_text);
        assert!(
            KNOWN_EVENT_TYPES.contains(&flux_type.as_str()),
            "les memes types traversent l'historique et le direct"
        );
    }

    /// Scenario « deux envois concurrents » : un tour actif, le second envoi est `409`.
    #[tokio::test(flavor = "multi_thread")]
    async fn deux_envois_concurrents() {
        let (db, _hub, hang, manager) = setup_with(hang_factory()).await;
        let alice = token_for(&db, "alice").await;
        let (status, body) = serve_json(
            app(manager.clone(), db.clone()),
            post_json(
                "/api/v1/lymaille/conversations",
                &alice,
                &json!({ "context": "c1" }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let id = body["id"].as_str().expect("id").to_owned();
        let (status, _) = serve_json(
            app(manager.clone(), db.clone()),
            post_json(
                &format!("/api/v1/lymaille/conversations/{id}/messages"),
                &alice,
                &json!({ "text": "premier" }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED);
        wait_entered(&hang).await;
        let (status, _) = serve_json(
            app(manager.clone(), db.clone()),
            post_json(
                &format!("/api/v1/lymaille/conversations/{id}/messages"),
                &alice,
                &json!({ "text": "second" }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "un tour est deja actif");
        hang.release.add_permits(8);
    }

    /// Scenario « la session d'un autre est introuvable » : `messages`, `cancel`, `resume`,
    /// `DELETE` par `bob` rendent le même `404` qu'un identifiant inconnu.
    #[tokio::test(flavor = "multi_thread")]
    async fn la_session_d_un_autre_est_introuvable() {
        let (db, _hub, manager) = setup().await;
        let alice = token_for(&db, "alice").await;
        let bob = token_for(&db, "bob").await;
        let (status, body) = serve_json(
            app(manager.clone(), db.clone()),
            post_json(
                "/api/v1/lymaille/conversations",
                &alice,
                &json!({ "context": "c1" }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "la conversation d'alice est ouverte");
        let id = body["id"].as_str().expect("id").to_owned();

        // Ancrage des sondes positives : les mêmes routes, par la propriétaire, rendent leurs
        // statuts réels (jamais le `404` du trou de comportement).
        let base = format!("/api/v1/lymaille/conversations/{id}");
        for (suffixe, attendu) in [
            ("/messages", StatusCode::ACCEPTED),
            ("/cancel", StatusCode::OK),
            ("/resume", StatusCode::OK),
        ] {
            let (status, _) = serve_json(
                app(manager.clone(), db.clone()),
                post_json(&format!("{base}{suffixe}"), &alice, &json!({ "text": "a moi" })),
            )
            .await;
            assert_eq!(status, attendu, "alice chez elle : {suffixe}");
        }
        let (status, _, _) = serve_raw(app(manager.clone(), db.clone()), delete_empty(&base, &alice)).await;
        assert_eq!(status, StatusCode::NO_CONTENT, "alice supprime chez elle");

        let routeur = app(manager.clone(), db.clone());
        let mut refus = Vec::new();
        for suffixe in ["/messages", "/cancel", "/resume"] {
            refus.push(
                serve_raw(
                    routeur.clone(),
                    post_json(&format!("{base}{suffixe}"), &bob, &json!({ "text": "x" })),
                )
                .await,
            );
        }
        refus.push(serve_raw(routeur, delete_empty(&base, &bob)).await);
        refus.push(
            serve_raw(
                app(manager, db.clone()),
                delete_empty("/api/v1/lymaille/conversations/s-inconnu", &bob),
            )
            .await,
        );
        let statuses: Vec<StatusCode> = refus.iter().map(|(status, _, _)| *status).collect();
        let bodies: Vec<Vec<u8>> = refus.iter().map(|(_, body, _)| body.clone()).collect();
        assert!(
            statuses.iter().all(|status| *status == StatusCode::NOT_FOUND),
            "les cinq refus sont des 404 : {statuses:?}"
        );
        assert!(
            bodies.windows(2).all(|pair| pair[0] == pair[1]),
            "les corps sont indiscernables"
        );
    }

    /// Scenario « le corps d'ouverture à l'ancienne est refusé » : `{ "kind": "work",
    /// "agent_id": "a" }` (contrat d'avant R12), comme tout corps sans `context` ou à champ
    /// inconnu, est `422`.
    #[tokio::test(flavor = "multi_thread")]
    async fn le_corps_d_ouverture_a_l_ancienne_est_refuse() {
        let (db, _hub, manager) = setup().await;
        let alice = token_for(&db, "alice").await;
        for corps in [
            json!({ "kind": "work", "agent_id": "a" }),
            json!({}),
            json!({ "context": "c1", "inconnu": true }),
        ] {
            let (status, _) = serve_json(
                app(manager.clone(), db.clone()),
                post_json("/api/v1/lymaille/conversations", &alice, &corps),
            )
            .await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "corps refuse : {corps}");
        }
    }

    /// Scenario « erreur sans fuite » : un envoi dont le texte porte un secret factice et qui
    /// échoue ne rend jamais le secret.
    #[tokio::test(flavor = "multi_thread")]
    async fn erreur_sans_fuite() {
        let (db, _hub, manager) = setup().await;
        let alice = token_for(&db, "alice").await;
        let (status, body) = serve_json(
            app(manager.clone(), db.clone()),
            post_json(
                "/api/v1/lymaille/conversations",
                &alice,
                &json!({ "context": "c1" }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let id = body["id"].as_str().expect("id").to_owned();
        let session = SessionId::new(id.clone()).expect("id valide");
        // Ancrage : la proprietaire envoie chez elle avant la fermeture (le 404 qui suit n'est
        // pas le statut par defaut du handler).
        let (status, _) = serve_json(
            app(manager.clone(), db.clone()),
            post_json(
                &format!("/api/v1/lymaille/conversations/{id}/messages"),
                &alice,
                &json!({ "text": "premier tour" }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED, "alice envoie chez elle");
        wait_state(&db, "alice", &session, |etat| etat.contains("reponse de tour")).await;
        manager.close("alice", &session).await;

        let (status, rendu, _headers) = serve_raw(
            app(manager.clone(), db.clone()),
            post_json(
                &format!("/api/v1/lymaille/conversations/{id}/messages"),
                &alice,
                &json!({ "text": "MARQUEUR-SECRET-DANS-LE-CORPS" }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "la session fermee est introuvable");
        let texte = String::from_utf8_lossy(&rendu).into_owned();
        assert!(
            !texte.contains("MARQUEUR-SECRET-DANS-LE-CORPS"),
            "la reponse d'erreur ne porte pas le texte envoye : {texte}"
        );
    }

    /// Scenario « le jeton Bearer est transmis, le cookie non » (volet mapping à la source) : le
    /// gestionnaire n'a pas de trait espionnable (`SessionManager` est un type concret,
    /// ./manager.rs est hors périmètre) ; l'extraction du matériau par source de principal est
    /// verrouillée ici — `Some` pour `Bearer`, `None` pour cookie et token API. La branche `Some`
    /// exercée de bout en bout par un vrai JWT validé par `MockOidcClient` est verrouillée par
    /// `le_jeton_bearer_est_transmis_de_bout_en_bout`.
    #[test]
    fn le_jeton_bearer_est_transmis_le_cookie_non() {
        let bearer = AuthPrincipal {
            subject: "alice".to_owned(),
            email: None,
            preferred_username: None,
            source: PrincipalSource::Bearer {
                jwt: "aaa.bbb.ccc".to_owned(),
            },
        };
        let cookie = AuthPrincipal {
            subject: "alice".to_owned(),
            email: None,
            preferred_username: None,
            source: PrincipalSource::Session {
                id_token: "jwt-de-session".to_owned(),
            },
        };
        let api = AuthPrincipal {
            subject: "alice".to_owned(),
            email: None,
            preferred_username: None,
            source: PrincipalSource::ApiToken { token_id: 1 },
        };
        assert_eq!(
            forward_token(&bearer.source).map(|token| (*token).clone()),
            Some("aaa.bbb.ccc".to_owned()),
            "le Bearer est transmis"
        );
        assert!(
            forward_token(&cookie.source).is_none(),
            "le cookie ne transmet rien"
        );
        assert!(
            forward_token(&api.source).is_none(),
            "le token API ne transmet rien"
        );
    }

    /// Scenario « le jeton Bearer est transmis, le cookie non » (volet end-to-end, clôture R12 —
    /// le `SessionManager` concret n'est pas espionnable) : un envoi porté par un JWT Bearer
    /// validé de bout en bout — extracteur dual (`src/auth/dual.rs`), `MockOidcClient` configuré,
    /// `PrincipalSource::Bearer` réel, jamais fabriqué par le test — atteint le handler et lance
    /// le tour (`202`, état persisté). La branche `Some` de `forward_token` est ainsi exercée en
    /// situation par `send_message` ; le mapping (`Some` par Bearer, `None` par cookie et token
    /// API) reste verrouillé à la source par `le_jeton_bearer_est_transmis_le_cookie_non`.
    #[tokio::test(flavor = "multi_thread")]
    async fn le_jeton_bearer_est_transmis_de_bout_en_bout() {
        let (db, _hub, manager) = setup().await;
        let alice = token_for(&db, "alice").await;
        let (status, body) = serve_json(
            app(manager.clone(), db.clone()),
            post_json(
                "/api/v1/lymaille/conversations",
                &alice,
                &json!({ "context": "c1" }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "ouverture par token API : {body}");
        let id = body["id"].as_str().expect("id").to_owned();
        let session = SessionId::new(id.clone()).expect("id de session valide");

        // Un seul appel JWT par état : le résultat de `MockOidcClient::with_bearer_result` est
        // rendu une fois (`src/auth/oidc.rs`). L'identité rendue porte l'`id_token` = le JWT reçu,
        // matériau que l'extracteur dépose dans `PrincipalSource::Bearer` et que `forward_token`
        // relaie en `Some`.
        let jwt = "aaa.bbb.ccc";
        let state = MiryadAuthState {
            oidc_client: Arc::new(MockOidcClient::with_bearer_result(Ok(OidcIdentity {
                id_token: jwt.to_owned(),
                subject: "alice".to_owned(),
                email: None,
                preferred_username: None,
            }))),
            ..test_state(db.clone())
        };
        let (status, body) = serve_json(
            app_on(manager.clone(), state),
            post_json(
                &format!("/api/v1/lymaille/conversations/{id}/messages"),
                jwt,
                &json!({ "text": "bonjour porteur" }),
            ),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::ACCEPTED,
            "l'envoi passe par un JWT verifie de bout en bout : {body}"
        );
        wait_state(&db, "alice", &session, |etat| etat.contains("bonjour porteur")).await;
    }

    /// Scenario « activer son brouillon » : le toolset privé d'alice portant un brouillon est
    /// promu par `POST /toolsets/{name}/activate`, rapport rendu.
    #[tokio::test(flavor = "multi_thread")]
    async fn activer_son_brouillon() {
        let (db, _hub, manager) = setup().await;
        let alice = token_for(&db, "alice").await;
        let alice_id = user_id(&db, "alice").await;
        put_toolset_draft(&db, alice_id, "ts_prive", "1.0.0", "2.0.0").await;
        let (status, body) = serve_json(
            app(manager.clone(), db.clone()),
            post_json("/api/v1/lymaille/toolsets/ts_prive/activate", &alice, &json!({})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "le brouillon est promu : {body}");
        assert_eq!(body["name"], json!("ts_prive"));
        assert_eq!(body["version"], json!("2.0.0"));
        assert_eq!(body["previous_version"], json!("1.0.0"));
    }

    // ── Contrôles `Must` hors scénarios ──────────────────────────────────────

    /// `Must` : liste expurgée et filtres — la liste d'alice rend ses deux résumés à six clés,
    /// un filtre inconnu est un `422`, une session fermée `DELETE` est le `404` indiscernable,
    /// `cancel` sans tour répond `false`.
    #[tokio::test(flavor = "multi_thread")]
    async fn liste_filtres_suppression_et_annulation() {
        let (db, _hub, manager) = setup().await;
        let alice = token_for(&db, "alice").await;
        let premier = manager.open("alice", "c1", None).await.expect("premiere session");
        let second = manager.open("alice", "c1", None).await.expect("seconde session");

        let (status, body) = serve_json(
            app(manager.clone(), db.clone()),
            get_json("/api/v1/lymaille/conversations", &alice),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "la liste se relit");
        let items = body["items"].as_array().expect("items en tableau");
        assert_eq!(items.len(), 2, "deux resumés : {items:?}");
        for item in items {
            let mut keys: Vec<String> = item.as_object().expect("item objet").keys().cloned().collect();
            keys.sort();
            assert_eq!(
                keys,
                vec!["agent", "id", "kind", "review", "title", "turns"],
                "six clés expurgées"
            );
        }
        let (status, _) = serve_json(
            app(manager.clone(), db.clone()),
            get_json("/api/v1/lymaille/conversations?kind=sous", &alice),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "kind inconnu refuse");

        let (status, body) = serve_json(
            app(manager.clone(), db.clone()),
            post_json(
                &format!("/api/v1/lymaille/conversations/{}/cancel", premier.id.as_str()),
                &alice,
                &json!({}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({ "cancelled": false }), "aucun tour actif");

        let (status, _) = serve_json(
            app(manager.clone(), db.clone()),
            get_json(
                &format!("/api/v1/lymaille/conversations/{}/history", second.id.as_str()),
                &alice,
            ),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "l'historique d'une session neuve se relit"
        );

        let (status, _, _) = serve_raw(
            app(manager.clone(), db.clone()),
            delete_empty(
                &format!("/api/v1/lymaille/conversations/{}", second.id.as_str()),
                &alice,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT, "suppression rend 204");
        let (status, _, _) = serve_raw(
            app(manager.clone(), db.clone()),
            delete_empty(
                &format!("/api/v1/lymaille/conversations/{}", second.id.as_str()),
                &alice,
            ),
        )
        .await;
        // `Must` de la spec (clôture R12) : le re-`DELETE` par la propriétaire est `204` idempotent —
        // la garde de visibilité (`load_state`) repasse, la fermeture du gestionnaire est silencieuse.
        assert_eq!(
            status,
            StatusCode::NO_CONTENT,
            "le re-DELETE par la propriétaire est idempotent"
        );
    }

    /// `Must` : texte du message borné — vide et > 32 Ko sont `422`, avant tout tour.
    #[tokio::test(flavor = "multi_thread")]
    async fn le_texte_d_un_message_est_borne() {
        let (db, _hub, manager) = setup().await;
        let alice = token_for(&db, "alice").await;
        let (status, body) = serve_json(
            app(manager.clone(), db.clone()),
            post_json(
                "/api/v1/lymaille/conversations",
                &alice,
                &json!({ "context": "c1" }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let id = body["id"].as_str().expect("id").to_owned();
        for texte in [String::new(), "x".repeat(32 * 1024 + 1)] {
            let (status, _) = serve_json(
                app(manager.clone(), db.clone()),
                post_json(
                    &format!("/api/v1/lymaille/conversations/{id}/messages"),
                    &alice,
                    &json!({ "text": texte }),
                ),
            )
            .await;
            assert_eq!(
                status,
                StatusCode::UNPROCESSABLE_ENTITY,
                "texte refuse : {}",
                texte.len()
            );
        }
    }

    /// `Must` : `DELETE /toolsets/{name}/draft` rend `204` si un brouillon existait, `404` sinon.
    #[tokio::test(flavor = "multi_thread")]
    async fn jeter_le_brouillon() {
        let (db, _hub, manager) = setup().await;
        let alice = token_for(&db, "alice").await;
        let alice_id = user_id(&db, "alice").await;
        put_toolset_draft(&db, alice_id, "ts_jete", "1.0.0", "2.0.0").await;
        let (status, _, _) = serve_raw(
            app(manager.clone(), db.clone()),
            delete_empty("/api/v1/lymaille/toolsets/ts_jete/draft", &alice),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT, "le brouillon existait");
        let (status, _, _) = serve_raw(
            app(manager.clone(), db.clone()),
            delete_empty("/api/v1/lymaille/toolsets/ts_jete/draft", &alice),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "plus de brouillon a jeter");
    }

    /// Scenario « historique puis abonnement, mêmes types » (volet mapping, amendement historique
    /// complet DSD `DEC-30`) : l'état persisté est rendu dans son ordre, sous les trois variantes
    /// de l'historique — `user_message`, `tool_call_summary` (les appels viennent avant la
    /// réponse du même tour), `turn_ended` — `turn` numerote a 1, `outcome` `valid`, `at` l'instant
    /// de creation de la session (limite `DEC-30`), aucune donnee brute : ni prompt systeme, ni
    /// contenu de resultat d'outil, ni frontiere de segment.
    #[test]
    fn l_historique_rend_les_trois_variantes_dans_l_ordre_persiste() {
        let state = state_with_history();
        let events = history_events(&state);
        let json = serde_json::to_value(&events).expect("le rendu serialise");
        assert_eq!(
            json,
            json!([
                {
                    "session": "s-mapping",
                    "at": state_at_rfc3339(),
                    "kind": { "type": "user_message", "turn": 1, "text": "bonjour" }
                },
                {
                    "session": "s-mapping",
                    "at": state_at_rfc3339(),
                    "kind": {
                        "type": "tool_call_summary",
                        "turn": 1,
                        "tool": "lookup",
                        "call_id": "call-1",
                        "has_result": true
                    }
                },
                {
                    "session": "s-mapping",
                    "at": state_at_rfc3339(),
                    "kind": { "type": "turn_ended", "turn": 1, "outcome": "valid", "response": "reponse" }
                }
            ]),
            "l'historique rend l'etat persiste dans son ordre, sous les trois variantes"
        );
    }

    /// Scenario « l'historique ne rend ni arguments ni résultats d'outil » : l'appel d'outil est
    /// rendu par son nom et son `has_result`, jamais par ses arguments ni par le contenu de son
    /// resultat, tous deux porteurs de secrets factices dans la fixture.
    #[test]
    fn l_historique_ne_rend_ni_arguments_ni_contenus_de_resultat() {
        let state = state_with_history();
        let rendu = serde_json::to_string(&history_events(&state)).expect("le rendu serialise");
        assert!(rendu.contains("lookup"), "le nom de l'outil est rendu : {rendu}");
        assert!(
            rendu.contains("call-1"),
            "l'identifiant d'appel est rendu : {rendu}"
        );
        assert!(rendu.contains("has_result"), "has_result est rendu : {rendu}");
        assert!(
            !rendu.contains("ARGUMENT-MCP-PRIVE"),
            "les arguments d'outil ne sont jamais rendus : {rendu}"
        );
        assert!(
            !rendu.contains("CONTENU-RESULTAT-PRIVE"),
            "le contenu du resultat d'outil n'est jamais rendu : {rendu}"
        );
    }

    /// Tâche « historique complet » : `has_result` est `false` quand aucun `Message::ToolResult`
    /// de meme `call_id` ne suit l'appel — et une reponse d'assistant vide, meme chargee d'appels,
    /// ne rend pas de `turn_ended`.
    #[test]
    fn l_historique_marque_un_appel_sans_resultat() {
        let state = state_history_fixture(
            "s-sans-resultat",
            json!([
                { "role": "user", "content": "lanca" },
                {
                    "role": "assistant",
                    "content": "",
                    "reasoning": null,
                    "tool_calls": [
                        { "id": "call-2", "name": "rhai_pad", "arguments": { "corps": "secret-factice-d-arguments" } }
                    ]
                }
            ]),
        );
        let json = serde_json::to_value(history_events(&state)).expect("le rendu serialise");
        assert_eq!(
            json,
            json!([
                {
                    "session": "s-sans-resultat",
                    "at": state_at_rfc3339(),
                    "kind": { "type": "user_message", "turn": 1, "text": "lanca" }
                },
                {
                    "session": "s-sans-resultat",
                    "at": state_at_rfc3339(),
                    "kind": {
                        "type": "tool_call_summary",
                        "turn": 1,
                        "tool": "rhai_pad",
                        "call_id": "call-2",
                        "has_result": false
                    }
                }
            ]),
            "un appel sans resultat suit, a has_result faux, sans turn_ended sur texte vide"
        );
    }

    /// Tâche « historique complet » : le rang des tours suit les seuls messages utilisateur —
    /// chaque tour acheve rend son `user_message` puis son `turn_ended`, numerotes.
    #[test]
    fn l_historique_numerote_les_tours_successifs() {
        let state = state_history_fixture(
            "s-deux-tours",
            json!([
                { "role": "user", "content": "premier" },
                { "role": "assistant", "content": "reponse un", "reasoning": null, "tool_calls": [] },
                { "role": "user", "content": "second" },
                { "role": "assistant", "content": "reponse deux", "reasoning": null, "tool_calls": [] }
            ]),
        );
        let json = serde_json::to_value(history_events(&state)).expect("le rendu serialise");
        assert_eq!(
            json,
            json!([
                {
                    "session": "s-deux-tours",
                    "at": state_at_rfc3339(),
                    "kind": { "type": "user_message", "turn": 1, "text": "premier" }
                },
                {
                    "session": "s-deux-tours",
                    "at": state_at_rfc3339(),
                    "kind": { "type": "turn_ended", "turn": 1, "outcome": "valid", "response": "reponse un" }
                },
                {
                    "session": "s-deux-tours",
                    "at": state_at_rfc3339(),
                    "kind": { "type": "user_message", "turn": 2, "text": "second" }
                },
                {
                    "session": "s-deux-tours",
                    "at": state_at_rfc3339(),
                    "kind": { "type": "turn_ended", "turn": 2, "outcome": "valid", "response": "reponse deux" }
                }
            ]),
            "les tours sont numerotes par les seuls messages utilisateur, dans l'ordre persiste"
        );
    }

    /// Tâche « historique complet » (`Must` : « textes passés au `Redactor` ») : les teneurs de
    /// `user_message` et de `turn_ended` traversent le `Redactor` — un secret declare est remplace
    /// par le masque, la donnee brute ne sort jamais de l'historique.
    #[test]
    fn l_historique_expurge_les_teneurs_par_le_redacteur() {
        let state = state_history_fixture(
            "s-redaction",
            json!([
                { "role": "user", "content": "bonjour CLE-SECRETE-DE-TEST" },
                { "role": "assistant", "content": "voila CLE-SECRETE-DE-TEST", "reasoning": null, "tool_calls": [] }
            ]),
        );
        let mut redactor = ::lymaille::Redactor::new();
        redactor
            .add_secret("CLE-SECRETE-DE-TEST")
            .expect("le secret de fixture est assez long");
        let json = serde_json::to_value(render_history(&state, &redactor)).expect("le rendu serialise");
        assert_eq!(
            json,
            json!([
                {
                    "session": "s-redaction",
                    "at": state_at_rfc3339(),
                    "kind": {
                        "type": "user_message",
                        "turn": 1,
                        "text": "bonjour *redacted secret*"
                    }
                },
                {
                    "session": "s-redaction",
                    "at": state_at_rfc3339(),
                    "kind": {
                        "type": "turn_ended",
                        "turn": 1,
                        "outcome": "valid",
                        "response": "voila *redacted secret*"
                    }
                }
            ]),
            "les teneurs de l'historique sont expurgees par le Redactor"
        );
    }

    /// Tâche « Alignement `lymaille` 0.2.0 » (`./routes.sdd`, Clôture R12) : `Message::Discarded`
    /// (réponse écartée, conservée pour audit seulement) n'est **jamais rendue** dans l'historique
    /// et n'ouvre **aucun tour** — le rang des tours suit les seuls messages utilisateur. Contrat
    /// de non-régression : le repli `_unknown_amont` du mapping la couvrait déjà par accident,
    /// le bras explicite le verrouille. L'amendement historique complet (DSD `DEC-30`) ajoute le
    /// `user_message` du tour, que le message écarté ne remplace ni ne décale.
    #[test]
    fn une_reponse_ecartee_n_est_jamais_rendue_ni_comptee_comme_tour() {
        let state = state_history_fixture(
            "s-discarded",
            json!([
                { "role": "user", "content": "bonjour" },
                { "role": "discarded", "cause": "empty", "reasoning": null },
                { "role": "assistant", "content": "reponse", "reasoning": null, "tool_calls": [] }
            ]),
        );
        let events = history_events(&state);
        let json = serde_json::to_value(&events).expect("le rendu serialise");
        assert_eq!(
            json,
            json!([
                {
                    "session": "s-discarded",
                    "at": state_at_rfc3339(),
                    "kind": { "type": "user_message", "turn": 1, "text": "bonjour" }
                },
                {
                    "session": "s-discarded",
                    "at": state_at_rfc3339(),
                    "kind": { "type": "turn_ended", "turn": 1, "outcome": "valid", "response": "reponse" }
                }
            ]),
            "la reponse ecartee n'est ni rendue, ni comptee comme tour"
        );
    }

    /// L'instant de création de la fixture d'état (`Timestamp::from_millis(1_730_000_000_000)`)
    /// en RFC 3339 UTC, même conversion que le mapping.
    fn state_at_rfc3339() -> String {
        let moment = chrono::DateTime::from_timestamp_millis(1_730_000_000_000)
            .expect("un instant d'epoque en portee chrono");
        moment.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    }

    /// `Must` (traduction) : table variante → statut, sans contenu de session dans les textes.
    #[test]
    fn la_traduction_des_erreurs_est_fixe() {
        let cases = [
            (LymailleError::UnknownSubject, StatusCode::FORBIDDEN),
            (LymailleError::Forbidden("x".into()), StatusCode::FORBIDDEN),
            (
                LymailleError::NotFound("session s-1".into()),
                StatusCode::NOT_FOUND,
            ),
            (
                LymailleError::Conflict("turn active".into()),
                StatusCode::CONFLICT,
            ),
            (
                LymailleError::InvalidDefinition("x".into()),
                StatusCode::UNPROCESSABLE_ENTITY,
            ),
            (
                LymailleError::Store("db down".into()),
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
            (
                LymailleError::Run {
                    message: "x".into(),
                    retryable: false,
                },
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
        ];
        for (error, expected) in cases {
            let status = to_rest(error).into_response().status();
            assert_eq!(status, expected, "table de traduction");
        }
    }
}

#[cfg(test)]
pub(crate) mod fix {
    //! Fixtures partagées des tests de `./http` (motif de `../store/mod.rs::test_support`) :
    //! base SQLite mémoire migrée, catalogue câblé, type de contexte, contexte privé, montage
    //! d'état d'auth et requêtes.

    use std::future::Future;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
    use std::time::Duration;

    use sea_orm::ConnectionTrait;
    use sea_orm::DatabaseConnection;
    use sea_orm::sea_query::Alias;
    use sea_orm::sea_query::Expr;
    use sea_orm::sea_query::Query;
    use serde_json::json;
    use vynil_core::Semver;
    use zeroize::Zeroizing;

    use crate::auth::MiryadAuthState;
    use crate::auth::MockOidcClient;
    use crate::auth::issue_token;
    use crate::lymaille::identity::Identity;
    use crate::lymaille::interactive::assemble::AssembleDeps;
    use crate::lymaille::interactive::assemble::DynInference;
    use crate::lymaille::interactive::assemble::InferenceFactory;
    use crate::lymaille::interactive::assemble::McpConnectorFactory;
    use crate::lymaille::interactive::hub::EventHub;
    use crate::lymaille::interactive::manager::Limits;
    use crate::lymaille::interactive::manager::ManagerDeps;
    use crate::lymaille::interactive::manager::SessionManager;
    use crate::lymaille::resource_module::ResourceRegistry;
    use crate::lymaille::secret::SecretResolver;
    use crate::lymaille::store::LymailleStores;
    use crate::lymaille::store::test_support::test_db;
    use crate::users::group;
    use crate::users::membership::sync_group_memberships;
    use crate::users::resolve_user;

    use ::lymaille::AgentDefinition;
    use ::lymaille::AgentKind;
    use ::lymaille::AgentStore as _;
    use ::lymaille::ContextLevel;
    use ::lymaille::ContextType;
    use ::lymaille::InferenceRequest;
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
    use ::lymaille::Timestamp;
    use ::lymaille::Toolset;
    use ::lymaille::ToolsetContext;
    use ::lymaille::TurnGate;

    /// Poivre des tokens API de test.
    pub const PEPPER: &str = "routes-test-pepper";

    /// Types `SessionEventKind` attendus du miroir (le même vocabulaire que le flux SSE).
    pub const KNOWN_EVENT_TYPES: &[&str] = &[
        "turn_started",
        "text_delta",
        "turn_ended",
        "run_ended",
        "token_usage",
        "todo_changed",
        "title_changed",
        "review_requested",
        "script",
        "mcp_call",
        "mcp_reconnect",
        "inference_retry",
        "overrun",
        "response_truncated",
        "empty_response",
        "user_message",
        "tool_call_summary",
        "other",
        "host",
    ];

    pub async fn compte(db: &DatabaseConnection, subject: &str, admin: bool) {
        let account = resolve_user(db, subject, None)
            .await
            .expect("compte local de test cree");
        if admin {
            sync_group_memberships(db, account.id, &[group::ADMIN_GROUP_NAME.to_owned()])
                .await
                .expect("appartenance au groupe admin");
        }
    }

    pub async fn wire_catalog(db: &DatabaseConnection) {
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

    pub fn agent(id: &str, kind: AgentKind) -> AgentDefinition {
        AgentDefinition::new(id.to_owned(), kind, "le prompt".to_owned(), "qwen".to_owned())
            .with_modules(vec!["resources".to_owned()])
    }

    pub async fn put_agent(db: &DatabaseConnection, owner: &str, definition: &AgentDefinition) {
        LymailleStores::new(db.clone(), owner.to_owned())
            .agents()
            .put_agent(definition)
            .await
            .expect("le proprietaire ecrit son agent");
    }

    pub async fn put_type(
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

    pub async fn inserer_contexte_prive(
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

    /// Ligne de toolset privé avec version active et brouillon (SQL brut, même motif que
    /// `./assemble.rs`).
    pub async fn put_toolset_draft(
        db: &DatabaseConnection,
        owner_id: i32,
        name: &str,
        active_version: &str,
        draft_version: &str,
    ) {
        let active = toolset(name, active_version);
        let draft = toolset(name, draft_version);
        let mut insert = Query::insert();
        insert
            .into_table(Alias::new("miryad_lymaille_toolsets"))
            .columns([
                "owner_id",
                "toolset_id",
                "version",
                "active",
                "draft",
                "created_at",
            ])
            .values(vec![
                Expr::val(owner_id),
                Expr::val(name),
                Expr::val(active_version),
                Expr::val(serde_json::to_value(active).expect("l'actif se serialise")),
                Expr::val(serde_json::to_value(draft).expect("le brouillon se serialise")),
                Expr::val(chrono::Utc::now()),
            ])
            .expect("les colonnes et valeurs de fixture correspondent");
        db.execute(&insert).await.expect("ligne de toolset inseree");
    }

    fn toolset(name: &str, version: &str) -> Toolset {
        Toolset::new(
            name.to_owned(),
            format!("le toolset de fixture {name}"),
            Semver::parse(version).expect("la version de fixture se parse"),
            "fn ping() { 1 }".to_owned(),
        )
        .expect("le toolset de fixture se construit")
    }

    pub struct ScriptFactory(pub Arc<ScriptedProvider>);

    impl InferenceFactory for ScriptFactory {
        fn make(&self, _address: String, _api_key: Option<String>) -> Arc<dyn DynInference> {
            let provider: Arc<dyn DynInference> = self.0.clone();
            provider
        }
    }

    pub fn final_text(text: &str) -> InferenceResponse {
        InferenceResponse::new(Message::assistant(text.to_owned(), None, Vec::new()), None)
    }

    /// Fabrique qui bloque dans `complete` (le tour reste actif — Szenarios 202 sans attendre et
    /// 409 concurrent) ; motif de `../interactive/manager.rs`.
    pub struct HangFactory(pub Arc<Hang>);

    pub struct Hang {
        pub entered: AtomicBool,
        pub release: tokio::sync::Semaphore,
    }

    impl Hang {
        pub fn new() -> Arc<Self> {
            Arc::new(Self {
                entered: AtomicBool::new(false),
                release: tokio::sync::Semaphore::new(0),
            })
        }
    }

    impl InferenceFactory for HangFactory {
        fn make(&self, _address: String, _api_key: Option<String>) -> Arc<dyn DynInference> {
            Arc::new(HangInference(Arc::clone(&self.0)))
        }
    }

    struct HangInference(Arc<Hang>);

    impl DynInference for HangInference {
        fn complete_dyn(
            &self,
            _request: InferenceRequest,
        ) -> std::pin::Pin<
            Box<dyn Future<Output = Result<InferenceResponse, ::lymaille::InferenceError>> + Send + '_>,
        > {
            let hang = Arc::clone(&self.0);
            Box::pin(async move {
                hang.entered.store(true, AtomicOrdering::SeqCst);
                match hang.release.acquire().await {
                    Ok(_permit) => Ok(final_text("jamais rendu")),
                    Err(_closed) => Ok(final_text("libere")),
                }
            })
        }
    }

    pub struct NoMcp;

    impl McpConnectorFactory for NoMcp {
        fn make(
            &self,
            _url: &str,
            _request_timeout: Duration,
            _connect_timeout: Duration,
            _auth: Option<Arc<dyn crate::lymaille::interactive::assemble::DynMcpAuth>>,
        ) -> Arc<dyn crate::lymaille::interactive::assemble::DynMcpConnector> {
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

    pub struct FixedSecret(pub &'static str);

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

    pub fn deps_with(
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

    pub fn test_state(db: DatabaseConnection) -> MiryadAuthState {
        MiryadAuthState {
            oidc_client: Arc::new(MockOidcClient::new()),
            cookie_key: ::cookie::Key::from(&[5u8; 64]),
            post_login_redirect: "/".to_owned(),
            post_logout_redirect: "/".to_owned(),
            db,
            secure_cookies: false,
            token_pepper: PEPPER.to_owned(),
        }
    }

    pub async fn token_for(db: &DatabaseConnection, subject: &str) -> String {
        compte(db, subject, false).await;
        issue_token(db, subject, "test", None, PEPPER)
            .await
            .expect("token api emis")
            .token
    }

    pub async fn user_id(db: &DatabaseConnection, subject: &str) -> i32 {
        Identity::new(db.clone())
            .resolve(subject)
            .await
            .expect("compte local relu")
            .user_id
    }

    /// L'installation type : catalogue, agent `dev` (`work`) d'alice, type `t`, contexte `c1`.
    /// Rend `(db, hub, manager)` — provider scripté d'une réponse de tour.
    pub async fn setup() -> (DatabaseConnection, Arc<EventHub>, SessionManager) {
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
        let alice = user_id(&db, "alice").await;
        inserer_contexte_prive(&db, alice, "c1", Some("t")).await;
        let hub = Arc::new(EventHub::new(None, 256));
        let provider = Arc::new(ScriptedProvider::new(vec![
            Ok(final_text("reponse de tour")),
            Ok(final_text("seconde reponse")),
        ]));
        let manager = SessionManager::new(deps_with(
            db.clone(),
            Arc::new(ScriptFactory(provider)),
            hub.clone(),
        ));
        (db, hub, manager)
    }

    /// Variante au provider bloquant : rend aussi l'anse `hang` du scénario « tour actif ».
    pub async fn setup_with(
        hang: Arc<Hang>,
    ) -> (DatabaseConnection, Arc<EventHub>, Arc<Hang>, SessionManager) {
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
        let alice = user_id(&db, "alice").await;
        inserer_contexte_prive(&db, alice, "c1", Some("t")).await;
        let hub = Arc::new(EventHub::new(None, 256));
        let manager = SessionManager::new(deps_with(
            db.clone(),
            Arc::new(HangFactory(Arc::clone(&hang))),
            hub.clone(),
        ));
        (db, hub, hang, manager)
    }

    pub fn hang_factory() -> Arc<Hang> {
        Hang::new()
    }

    pub async fn wait_entered(hang: &Hang) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !hang.entered.load(AtomicOrdering::SeqCst) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "le provider n'a jamais ete appele"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    pub async fn wait_state(
        db: &DatabaseConnection,
        subject: &str,
        id: &SessionId,
        want: impl Fn(&str) -> bool,
    ) {
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

    pub fn post_json(
        uri: &str,
        token: &str,
        body: &serde_json::Value,
    ) -> axum::http::Request<axum::body::Body> {
        axum::http::Request::builder()
            .method("POST")
            .uri(uri)
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/json")
            .body(axum::body::Body::from(body.to_string()))
            .expect("requete valide")
    }

    pub fn get_json(uri: &str, token: &str) -> axum::http::Request<axum::body::Body> {
        axum::http::Request::builder()
            .method("GET")
            .uri(uri)
            .header("Authorization", format!("Bearer {token}"))
            .body(axum::body::Body::empty())
            .expect("requete valide")
    }

    pub fn delete_empty(uri: &str, token: &str) -> axum::http::Request<axum::body::Body> {
        axum::http::Request::builder()
            .method("DELETE")
            .uri(uri)
            .header("Authorization", format!("Bearer {token}"))
            .body(axum::body::Body::empty())
            .expect("requete valide")
    }

    /// État de session portant l'historique `history` (construit à la main, hors base) : l'état
    /// amont n'a pas de pousseur de message public — le JSON rendu par `to_json` est complété de
    /// l'historique et relu par `from_json` (mêmes types que la persistance).
    pub fn state_history_fixture(id: &str, history: serde_json::Value) -> ::lymaille::SessionState {
        use ::lymaille::AgentRef;
        let id = SessionId::new(id).expect("id de fixture");
        let state = ::lymaille::SessionState::new(
            id,
            AgentRef::new("dev", "empreinte"),
            "c1",
            std::collections::BTreeMap::new(),
            Timestamp::from_millis(1_730_000_000_000),
        );
        let mut value: serde_json::Value =
            serde_json::from_str(&state.to_json().expect("l'etat se serialise")).expect("JSON d'etat");
        value["history"] = history;
        ::lymaille::SessionState::from_json(&value.to_string()).expect("l'etat historise se relit")
    }

    /// État d'un tour terminé avec appel d'outil (fixture du mapping d'historique) : frontière de
    /// segment, prompt système, message utilisateur, appel d'outil aux arguments porteurs d'un
    /// secret factice, résultat portant un autre secret, puis réponse finale.
    pub fn state_with_history() -> ::lymaille::SessionState {
        state_history_fixture(
            "s-mapping",
            json!([
                { "role": "boundary", "kind": "Start" },
                { "role": "system", "content": "PROMPT-SYSTEME" },
                { "role": "user", "content": "bonjour" },
                {
                    "role": "assistant",
                    "content": "",
                    "reasoning": null,
                    "tool_calls": [
                        { "id": "call-1", "name": "lookup", "arguments": { "cle": "ARGUMENT-MCP-PRIVE" } }
                    ]
                },
                { "role": "tool_result", "call_id": "call-1", "tool": "lookup", "content": "CONTENU-RESULTAT-PRIVE", "original_chars": null },
                { "role": "assistant", "content": "reponse", "reasoning": null, "tool_calls": [] }
            ]),
        )
    }

    /// Extrait le discriminant `kind.type` du premier événement SSE d'un tampon.
    pub fn extract_type(stream_text: &str) -> String {
        let data_line = stream_text
            .lines()
            .find_map(|line| line.strip_prefix("data: "))
            .expect("une ligne data");
        let value: serde_json::Value = serde_json::from_str(data_line).expect("data JSON");
        value["kind"]["type"].as_str().expect("kind.type").to_owned()
    }

    /// Lit le corps SSE d'une réponse streaming en fragments de texte.
    pub fn body_stream(response: axum::response::Response) -> impl futures::Stream<Item = String> + Unpin {
        use futures::StreamExt;
        response.into_body().into_data_stream().map(|piece| match piece {
            Ok(bytes) => String::from_utf8(bytes.to_vec()).expect("le SSE est en UTF-8"),
            Err(cause) => panic!("le corps SSE est illisible : {cause}"),
        })
    }

    pub async fn read_until<S>(body: &mut S, done: impl Fn(&str) -> bool) -> String
    where
        S: futures::Stream<Item = String> + Unpin,
    {
        use futures::StreamExt;
        let mut buffer = String::new();
        for _ in 0..40 {
            if done(&buffer) {
                return buffer;
            }
            let piece = tokio::time::timeout(Duration::from_secs(5), body.next())
                .await
                .expect("un fragment SSE dans le delai");
            match piece {
                Some(text) => buffer.push_str(&text),
                None => panic!("le flux s'est termine avant la fin attendue : {buffer}"),
            }
        }
        panic!("la fin attendue n'est jamais apparue : {buffer}");
    }
}
