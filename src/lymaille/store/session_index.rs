//! `SeaSessionIndex` — index des sessions et erreurs de script pour la session de configuration
//! et les routes de conversation (`src/lymaille/store/session_index.sdd`).
//!
//! `list(filter)` énumère les identifiants de sessions de `miryad_lymaille_sessions` selon agent,
//! nature, drapeau de revue et limite. `script_errors(id)` relit les erreurs de script d'une
//! session depuis la table d'audit `miryad_lymaille_script_errors` (alimentée par
//! `../interactive/hub.sdd`), car l'état d'une session ne les garde pas. Le store est lié à un
//! `subject` : il ne voit que **ses** sessions ; un admin voit celles de tous (le droit admin est
//! relu en base à chaque appel, jamais mis en cache — ./mod.sdd règle 2, DSD `FR-9`).

use sea_orm::ConnectionTrait;
use sea_orm::DatabaseConnection;
use sea_orm::sea_query::Alias;
use sea_orm::sea_query::Expr;
use sea_orm::sea_query::ExprTrait;
use sea_orm::sea_query::Order;
use sea_orm::sea_query::Query;

use ::lymaille::ReviewState;
use ::lymaille::ScriptError;
use ::lymaille::ScriptErrorClass;
use ::lymaille::SessionError;
use ::lymaille::SessionFilter;
use ::lymaille::SessionId;
use ::lymaille::SessionIndex;
use ::lymaille::SessionKind;
use ::lymaille::SessionResult;
use ::lymaille::SessionState;
use ::lymaille::TurnId;
use ::lymaille::review_state;

use super::db_text;
use crate::lymaille::identity::Identity;

/// Table des sessions, nom complet (../migration.sdd) — la même que ./session.rs.
const SESSIONS_TABLE: &str = "miryad_lymaille_sessions";

/// Table d'audit des erreurs de script, posée par `m20261104_000001_lymaille_lot2`
/// (../migration.sdd, ./audit.sdd).
const SCRIPT_ERRORS_TABLE: &str = "miryad_lymaille_script_errors";

/// Limite par défaut d'une liste (`./session_index.sdd` : 50 par défaut, plafonnée à 200).
const DEFAULT_LIMIT: usize = 50;

/// Plafond de la limite d'une liste.
const MAX_LIMIT: usize = 200;

/// Index des sessions, lié à un `subject` (`./session_index.sdd`).
#[derive(Clone)]
pub struct SeaSessionIndex {
    db: DatabaseConnection,
    identity: Identity,
    subject: String,
}

impl SeaSessionIndex {
    // Constructeur appelé par `LymailleStores::session_index()` (amendement lot 2 de `./mod.sdd`).
    pub(crate) fn new(db: DatabaseConnection, identity: Identity, subject: String) -> Self {
        Self {
            db,
            identity,
            subject,
        }
    }

    /// Principal de l'appel, relu en base à chaque appel — le droit admin n'est jamais mis en
    /// cache (./mod.sdd règle 2, `Must not` de `./session_index.sdd`). Un refus de l'hôte traverse
    /// en `Store(<Display>, code MRD compris)` (règle 5).
    async fn principal(&self) -> Result<crate::lymaille::identity::CallerPrincipal, SessionError> {
        self.identity
            .resolve(&self.subject)
            .await
            .map_err(|err| SessionError::Store(err.to_string()))
    }
}

/// Nature d'un filtre en texte de colonne — même dérivation que `kind_text` de ./session.rs
/// (propriétaire de la colonne, que ce store ne fait que lire).
fn kind_text(kind: SessionKind) -> SessionResult<String> {
    match serde_json::to_value(kind) {
        Ok(serde_json::Value::String(text)) => Ok(text),
        _ => Err(SessionError::Store(format!(
            "session kind {kind:?} has no stored text form"
        ))),
    }
}

/// Texte de la colonne `class` de `miryad_lymaille_script_errors` en classe typée. L'audit
/// (./audit.sdd) stocke ces textes ; `internal` est la classe de repli : une classe inconnue ou
/// absente est rendue `Internal` — échec fermé, sans inventer de classe.
fn error_class(text: Option<&str>) -> ScriptErrorClass {
    match text {
        Some("syntax") => ScriptErrorClass::Syntax,
        Some("runtime") => ScriptErrorClass::Runtime,
        Some("not_found") => ScriptErrorClass::NotFound,
        Some("limit") => ScriptErrorClass::Limit,
        Some("cancelled") => ScriptErrorClass::Cancelled,
        Some("engine") => ScriptErrorClass::Engine,
        _ => ScriptErrorClass::Internal,
    }
}

impl SessionIndex for SeaSessionIndex {
    async fn list(&self, filter: &SessionFilter) -> SessionResult<Vec<SessionId>> {
        let caller = self.principal().await?;
        let mut select = Query::select();
        select
            .columns([Alias::new("session_id"), Alias::new("state")])
            .from(Alias::new(SESSIONS_TABLE));
        // Cloisonnement : `WHERE subject = ?` sauf pour un admin (relu en base à cet appel).
        if !caller.is_admin {
            select.and_where(Expr::col(Alias::new("subject")).eq(self.subject.as_str()));
        }
        // La nature est une colonne dérivée (./session.rs) ; l'agent et le drapeau de revue ne le
        // sont pas — leur filtre s'applique après lecture de l'état, la limite s'appliquant au
        // résultat filtré (`./session_index.sdd`).
        if let Some(kind) = filter.kind() {
            select.and_where(Expr::col(Alias::new("kind")).eq(kind_text(kind)?));
        }
        select.order_by(Alias::new("updated_at"), Order::Desc);
        let rows = self
            .db
            .query_all(&select)
            .await
            .map_err(|err| SessionError::Store(db_text(&err)))?;
        let limit = filter.limit().unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT);
        let mut ids: Vec<SessionId> = Vec::new();
        for row in rows {
            if ids.len() >= limit {
                break;
            }
            let text: String = row
                .try_get("", "state")
                .map_err(|err| SessionError::Store(db_text(&err)))?;
            let Ok(state) = SessionState::from_json(&text) else {
                // Une ligne d'état illisible est ignorée, jamais l'échec de la liste : la trace
                // ne cite ni la session ni le contenu (../error.sdd).
                tracing::warn!("lymaille session index: unreadable session state, row skipped");
                continue;
            };
            if filter.agent().is_some_and(|agent| state.agent().name() != agent) {
                continue;
            }
            if filter.review_requested()
                && !matches!(review_state(state.labels()), Some(ReviewState::Requested(_)))
            {
                continue;
            }
            let session_text: String = row
                .try_get("", "session_id")
                .map_err(|err| SessionError::Store(db_text(&err)))?;
            let Some(id) = SessionId::new(session_text) else {
                tracing::warn!("lymaille session index: empty session identifier, row skipped");
                continue;
            };
            ids.push(id);
        }
        Ok(ids)
    }

    async fn script_errors(&self, id: &SessionId) -> SessionResult<Vec<ScriptError>> {
        let caller = self.principal().await?;
        let mut select = Query::select();
        select
            .columns([Alias::new("turn"), Alias::new("class"), Alias::new("text")])
            .from(Alias::new(SCRIPT_ERRORS_TABLE))
            .and_where(Expr::col(Alias::new("session_id")).eq(id.as_str()));
        if !caller.is_admin {
            select.and_where(Expr::col(Alias::new("subject")).eq(self.subject.as_str()));
        }
        // Triées par tour (déterminisme : à tour égal, par identifiant de ligne).
        select
            .order_by(Alias::new("turn"), Order::Asc)
            .order_by(Alias::new("id"), Order::Asc);
        let rows = self
            .db
            .query_all(&select)
            .await
            .map_err(|err| SessionError::Store(db_text(&err)))?;
        rows.iter()
            .map(|row| {
                let turn: Option<i64> = row
                    .try_get("", "turn")
                    .map_err(|err| SessionError::Store(db_text(&err)))?;
                let class: Option<String> = row
                    .try_get("", "class")
                    .map_err(|err| SessionError::Store(db_text(&err)))?;
                let text: Option<String> = row
                    .try_get("", "text")
                    .map_err(|err| SessionError::Store(db_text(&err)))?;
                Ok(ScriptError::new(
                    TurnId::new(turn.and_then(|turn| u64::try_from(turn).ok()).unwrap_or(0)),
                    error_class(class.as_deref()),
                    text.unwrap_or_default(),
                ))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::SeaSessionIndex;
    use crate::lymaille::store::test_support::test_db;
    use crate::users::membership::sync_group_memberships;
    use crate::users::resolve_user;
    use ::lymaille::ScriptError;
    use ::lymaille::ScriptErrorClass;
    use ::lymaille::SessionFilter;
    use ::lymaille::SessionId;
    use ::lymaille::SessionIndex as _;
    use ::lymaille::SessionKind;
    use ::lymaille::TurnId;
    use sea_orm::ConnectionTrait as _;
    use sea_orm::DatabaseConnection;
    use serde_json::json;

    /// Compte local de test, admin ou non.
    async fn user(db: &DatabaseConnection, subject: &str, admin: bool) {
        let account = resolve_user(db, subject, None)
            .await
            .expect("compte local de test créé");
        if admin {
            sync_group_memberships(db, account.id, &["admin".to_string()])
                .await
                .expect("appartenance au groupe admin posée");
        }
    }

    fn index(db: &DatabaseConnection, subject: &str) -> SeaSessionIndex {
        SeaSessionIndex::new(
            db.clone(),
            crate::lymaille::identity::Identity::new(db.clone()),
            subject.to_string(),
        )
    }

    fn session(text: &str) -> SessionId {
        SessionId::new(text).expect("un texte non vide fait un identifiant de session")
    }

    /// État JSON minimal chargeable par `SessionState::from_json`, agent `agent`.
    fn state_json(id: &str, agent: &str) -> String {
        json!({
            "id": id,
            "agent": { "name": agent, "fingerprint": "sha-1" },
            "format": { "major": 1, "minor": 0 },
            "history": [{ "role": "user", "content": "hello" }],
        })
        .to_string()
    }

    /// Ligne de session posée brutalement, horodatage figé pour rendre le tri déterministe.
    async fn insert_session(db: &DatabaseConnection, subject: &str, id: &str, kind: &str, at: &str) {
        db.execute_unprepared(&format!(
            "INSERT INTO miryad_lymaille_sessions (subject, session_id, kind, state, updated_at) \
             VALUES ('{subject}', '{id}', '{kind}', '{}', '{at}')",
            state_json(id, if kind == "config" { "cfg" } else { "main" }).replace('\'', "''")
        ))
        .await
        .expect("ligne de session de test insérée");
    }

    /// `Scenario` « un utilisateur ne liste que ses sessions » : des sessions d'`alice` et de
    /// `bob` ; la liste d'`alice` sans filtre ne rend que ses identifiants.
    #[tokio::test]
    async fn a_user_lists_only_their_sessions() {
        let db = test_db().await;
        user(&db, "alice", false).await;
        user(&db, "bob", false).await;
        insert_session(&db, "alice", "s-a1", "work", "2026-11-01T00:00:01Z").await;
        insert_session(&db, "alice", "s-a2", "work", "2026-11-01T00:00:02Z").await;
        insert_session(&db, "bob", "s-b1", "work", "2026-11-01T00:00:03Z").await;

        let listed = index(&db, "alice")
            .list(&SessionFilter::new())
            .await
            .expect("la liste d'alice réussit");
        assert_eq!(
            listed,
            vec![session("s-a2"), session("s-a1")],
            "uniquement ses identifiants, triés du plus récent"
        );
    }

    /// `Scenario` « un admin les voit toutes » : l'admin voit les identifiants d'`alice` et de
    /// `bob`, triés du plus récent.
    #[tokio::test]
    async fn an_admin_sees_them_all() {
        let db = test_db().await;
        user(&db, "alice", false).await;
        user(&db, "bob", false).await;
        user(&db, "root", true).await;
        insert_session(&db, "alice", "s-a1", "work", "2026-11-01T00:00:01Z").await;
        insert_session(&db, "bob", "s-b1", "work", "2026-11-01T00:00:02Z").await;

        let listed = index(&db, "root")
            .list(&SessionFilter::new())
            .await
            .expect("la liste de l'admin réussit");
        assert_eq!(
            listed,
            vec![session("s-b1"), session("s-a1")],
            "les identifiants d'alice et de bob"
        );
    }

    /// `Scenario` « filtres agent, nature, limite » : cinq sessions de deux agents et deux
    /// natures ; `with_agent`, `with_kind` puis `with_limit(2)` — les résultats respectent le
    /// filtre et la limite, triés du plus récent.
    #[tokio::test]
    async fn agent_kind_and_limit_filters_apply() {
        let db = test_db().await;
        user(&db, "alice", false).await;
        insert_session(&db, "alice", "s-1", "work", "2026-11-01T00:00:01Z").await;
        insert_session(&db, "alice", "s-2", "config", "2026-11-01T00:00:02Z").await;
        insert_session(&db, "alice", "s-3", "work", "2026-11-01T00:00:03Z").await;
        // Deux sessions de l'agent `cfg` (nature config) : `s-2` ci-dessus et `s-4`.
        insert_session(&db, "alice", "s-4", "config", "2026-11-01T00:00:04Z").await;
        insert_session(&db, "alice", "s-5", "work", "2026-11-01T00:00:05Z").await;

        let by_agent = index(&db, "alice")
            .list(&SessionFilter::new().with_agent("main"))
            .await
            .expect("le filtre agent réussit");
        assert_eq!(
            by_agent,
            vec![session("s-5"), session("s-3"), session("s-1")],
            "les sessions de l'agent `main`, du plus récent"
        );

        let by_kind = index(&db, "alice")
            .list(&SessionFilter::new().with_kind(SessionKind::Config))
            .await
            .expect("le filtre nature réussit");
        assert_eq!(
            by_kind,
            vec![session("s-4"), session("s-2")],
            "les sessions de nature config"
        );

        let limited = index(&db, "alice")
            .list(&SessionFilter::new().with_agent("cfg").with_limit(2))
            .await
            .expect("le filtre agent et limite réussit");
        assert_eq!(
            limited,
            vec![session("s-4"), session("s-2")],
            "la limite s'applique au résultat filtré"
        );
    }

    /// `Scenario` « un état illisible ne casse pas la liste » : une ligne dont `state` n'est pas
    /// du JSON valide est ignorée, les autres sessions sont rendues.
    #[tokio::test]
    async fn an_unreadable_state_does_not_break_the_list() {
        let db = test_db().await;
        user(&db, "alice", false).await;
        db.execute_unprepared(
            "INSERT INTO miryad_lymaille_sessions (subject, session_id, kind, state, updated_at) \
             VALUES ('alice', 's-casse', 'work', 'pas-du-json{{{', '2026-11-01T00:00:09Z')",
        )
        .await
        .expect("la ligne d'état illisible est insérée");
        insert_session(&db, "alice", "s-ok", "work", "2026-11-01T00:00:01Z").await;

        let listed = index(&db, "alice")
            .list(&SessionFilter::new())
            .await
            .expect("la liste réussit malgré l'état illisible");
        assert_eq!(listed, vec![session("s-ok")], "les autres sessions sont rendues");
    }

    /// Ligne d'audit d'erreur de script (colonne `text` : le texte **déjà masqué**).
    async fn insert_script_error(
        db: &DatabaseConnection,
        subject: &str,
        session_id: &str,
        turn: i64,
        class: &str,
        text: &str,
    ) {
        db.execute_unprepared(&format!(
            "INSERT INTO miryad_lymaille_script_errors (subject, session_id, turn, class, text, at) \
             VALUES ('{subject}', '{session_id}', {turn}, '{class}', '{text}', '2026-11-01T00:00:00Z')"
        ))
        .await
        .expect("ligne d'audit de test insérée");
    }

    /// `Scenario` « `script_errors` est cloisonné » : les erreurs d'une session d'`alice` sont
    /// vides pour `bob` (non-admin), triées par tour pour `alice`, avec le texte stocké.
    #[tokio::test]
    async fn script_errors_are_gated_by_owner() {
        let db = test_db().await;
        user(&db, "alice", false).await;
        user(&db, "bob", false).await;
        user(&db, "root", true).await;
        insert_session(&db, "alice", "s-1", "work", "2026-11-01T00:00:01Z").await;
        insert_script_error(&db, "alice", "s-1", 2, "runtime", "erreur masque e2").await;
        insert_script_error(&db, "alice", "s-1", 1, "syntax", "erreur masque e1").await;

        let empty = index(&db, "bob")
            .script_errors(&session("s-1"))
            .await
            .expect("la lecture de bob réussit");
        assert!(
            empty.is_empty(),
            "liste vide pour un autre utilisateur : {empty:?}"
        );

        let mine = index(&db, "alice")
            .script_errors(&session("s-1"))
            .await
            .expect("la lecture d'alice réussit");
        assert_eq!(
            mine,
            vec![
                ScriptError::new(TurnId::new(1), ScriptErrorClass::Syntax, "erreur masque e1"),
                ScriptError::new(TurnId::new(2), ScriptErrorClass::Runtime, "erreur masque e2"),
            ],
            "triées par tour, le texte est celui déjà masqué de l'audit"
        );

        let all = index(&db, "root")
            .script_errors(&session("s-1"))
            .await
            .expect("la lecture de l'admin réussit");
        assert_eq!(all.len(), 2, "l'admin voit les erreurs de la session");
    }
}
