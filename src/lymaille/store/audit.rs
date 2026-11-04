//! `AuditWriter` — journal des erreurs de script de `lymaille`
//! (`src/lymaille/store/audit.sdd`).
//!
//! Les événements `EventKind::Script` dont le record est en erreur sont persistés dans
//! `miryad_lymaille_script_errors`, masquées en amont par le @Redactor de la session : ce
//! fichier ne masque rien, il tronque seulement le texte à 2000 caractères. `record` est
//! synchrone et ne bloque jamais : les enregistrements transitent par un canal `tokio` borné
//! (1024) et une tâche dédiée écrit en base. Un débordement **perd** l'enregistrement
//! (compteur `dropped`, journal `warn`) plutôt que de bloquer un tour.

use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use sea_orm::ConnectionTrait;
use sea_orm::DatabaseConnection;
use sea_orm::sea_query::Alias;
use sea_orm::sea_query::Expr;
use sea_orm::sea_query::ExprTrait as _;
use sea_orm::sea_query::Query;
use tokio::sync::mpsc;

use ::lymaille::Event;
use ::lymaille::EventKind;
use ::lymaille::Timestamp;

use super::db_text;
use crate::lymaille::error::LymailleError;

/// Table d'audit des erreurs de script, posée par `m20261104_000001_lymaille_lot2`
/// (`src/lymaille/migration.sdd`) : `id` PK auto, `subject`, `session_id`, `turn`, `class`,
/// `text`, `at` ; index sur `session_id`.
const TABLE: &str = "miryad_lymaille_script_errors";

/// Capacité du canal borné entre `record` et la tâche d'écriture (`./audit.sdd`).
const CHANNEL_CAPACITY: usize = 1024;

/// Longueur maximale du texte persisté, en caractères (`./audit.sdd` : le texte est tronqué à
/// 2000 caractères — l'appelant masque, ici on ne fait que borner).
const MAX_TEXT_CHARS: usize = 2000;

/// Enregistrement prêt à écrire, construit par `record` et consommé par la tâche.
struct PendingRow {
    subject: String,
    session_id: String,
    class: &'static str,
    text: String,
    at: chrono::DateTime<chrono::Utc>,
}

/// Classe texte persistée sous `class`, dérivée du texte d'erreur masqué — harmonisée sur le
/// lecteur `error_class` de `./session_index.rs` : `syntax` | `runtime` | `not_found` | `limit`
/// | `cancelled` | `engine`, repli `internal`. `ScriptRecord.error` ne porte que le texte (masqué)
/// de l'erreur amont, dont les préfixes sont fixes (`lymaille::ScratchpadError`, Display) : la
/// classification se fait sur ces préfixes, dans l'ordre des Display les plus reconnaissables.
fn error_class(text: &str) -> &'static str {
    if text.starts_with("syntax error") {
        "syntax"
    } else if text.starts_with("runtime error") {
        "runtime"
    } else if text.starts_with("limit exceeded") {
        "limit"
    } else if text.starts_with("script cancelled") {
        "cancelled"
    } else if text.starts_with("scratchpad engine failed") {
        "engine"
    } else if text.contains("not found") {
        "not_found"
    } else {
        "internal"
    }
}

/// `Instant` amont (millisecondes depuis l'époque) en horodatage UTC de colonne. Un instant hors
/// de portée de `chrono` est ramené à l'heure courante : la colonne est NOT NULL, une trace
/// approximative vaut mieux qu'une ligne perdue.
fn column_time(at: Timestamp) -> chrono::DateTime<chrono::Utc> {
    i64::try_from(at.as_millis())
        .ok()
        .and_then(chrono::DateTime::from_timestamp_millis)
        .unwrap_or_else(chrono::Utc::now)
}

/// Écrivain du journal d'erreurs de script (`./audit.sdd`) — `Clone` : tous les clones partagent
/// le même canal et le même compteur `dropped`.
#[derive(Clone)]
pub struct AuditWriter {
    sender: mpsc::Sender<PendingRow>,
    dropped: Arc<AtomicU64>,
}

impl AuditWriter {
    /// Démarre la tâche d'écriture sur `db` et rend l'écrivain. La tâche s'arrête quand tous les
    /// `AuditWriter` (et donc toutes les extrémités émettrices) sont détruits.
    #[must_use]
    pub fn spawn(db: DatabaseConnection) -> Self {
        let (sender, mut receiver) = mpsc::channel::<PendingRow>(CHANNEL_CAPACITY);
        let dropped = Arc::new(AtomicU64::new(0));
        tokio::spawn(async move {
            // L'extrémité émettrice du canal est détenue par les `AuditWriter` seuls : quand le
            // dernier est détruit, `recv` rend `None` et la tâche se termine.
            while let Some(row) = receiver.recv().await {
                // La colonne `turn` n'est pas alimentée : `ScriptRecord` (et l'événement amont)
                // ne porte aucun numéro de tour (`./audit.sdd` est muette sur sa source) — elle
                // reste `NULL`, et le lecteur de `./session_index.rs` rend alors le tour 0.
                let mut insert = Query::insert();
                insert.into_table(Alias::new(TABLE)).columns([
                    "subject",
                    "session_id",
                    "class",
                    "text",
                    "at",
                ]);
                let Ok(statement) = insert.values(vec![
                    Expr::val(row.subject),
                    Expr::val(row.session_id),
                    Expr::val(row.class),
                    Expr::val(row.text),
                    Expr::val(row.at),
                ]) else {
                    // Erreur de construction de la requête (types connus, inatteignable en
                    // pratique) : l'enregistrement est perdu, la tâche continue.
                    tracing::warn!("lymaille audit: script error statement rejected");
                    continue;
                };
                let statement = statement.to_owned();
                if let Err(error) = db.execute(&statement).await {
                    // Sans le texte ni l'identifiant de session : la trace ne cite que
                    // la panne (`Must not` de `./audit.sdd`, contrat `../error.sdd`).
                    tracing::warn!(
                        error = %db_text(&error),
                        "lymaille audit: script error row not stored"
                    );
                }
            }
        });
        Self { sender, dropped }
    }

    /// Reçoit un événement : `EventKind::Script` dont le record est en erreur uniquement, converti
    /// en enregistrement et poussé par `try_send` — synchrone, jamais bloquant. Un canal plein
    /// **perd** l'enregistrement (compteur `dropped`, journal `warn` sans contenu).
    pub fn record(&self, subject: &str, event: &Event) {
        let EventKind::Script(record) = &event.kind else {
            return;
        };
        let Some(error) = &record.error else {
            return;
        };
        let row = PendingRow {
            subject: subject.to_owned(),
            session_id: event.session.as_str().to_owned(),
            class: error_class(error),
            text: error.chars().take(MAX_TEXT_CHARS).collect(),
            at: column_time(event.at),
        };
        match self.sender.try_send(row) {
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                tracing::warn!("lymaille audit: script error dropped, audit channel full");
            }
            // `Ok` : en file. `Closed` : la tâche s'est arrêtée (écrivains en cours de
            // destruction) — la perte est silencieuse par conception.
            Ok(()) | Err(mpsc::error::TrySendError::Closed(_)) => {}
        }
    }

    /// Nombre d'enregistrements perdus par débordement du canal depuis la création (Scenario
    /// « `record` ne bloque jamais » de `./audit.sdd`).
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

/// Supprime les lignes plus anciennes que `cutoff` (appelée par `./purge.sdd`) ; rend le nombre
/// de lignes supprimées.
///
/// # Errors
///
/// [`LymailleError::Store`] (`MRD-LYMAILLE-009`) : panne de la base, texte expurgé (règle 5 de
/// `./mod.sdd`).
pub async fn purge_before(
    db: &DatabaseConnection,
    cutoff: chrono::DateTime<chrono::Utc>,
) -> Result<u64, LymailleError> {
    let statement = Query::delete()
        .from_table(Alias::new(TABLE))
        .and_where(Expr::col(Alias::new("at")).lt(cutoff))
        .to_owned();
    let result = db
        .execute(&statement)
        .await
        .map_err(|error| LymailleError::Store(db_text(&error)))?;
    Ok(result.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::AuditWriter;
    use super::purge_before;
    use crate::lymaille::store::test_support::test_db;
    use ::lymaille::Event;
    use ::lymaille::EventKind;
    use ::lymaille::Redactor;
    use ::lymaille::ScriptInput;
    use ::lymaille::ScriptRecord;
    use ::lymaille::SessionId;
    use ::lymaille::Timestamp;
    use sea_orm::ConnectionTrait as _;
    use sea_orm::DatabaseConnection;
    use sea_orm::sea_query::Alias;
    use sea_orm::sea_query::Expr;
    use sea_orm::sea_query::ExprTrait as _;
    use sea_orm::sea_query::Query;

    /// Table d'audit, posée par `m20261104_000001_lymaille_lot2`.
    const TABLE: &str = "miryad_lymaille_script_errors";

    fn session(text: &str) -> SessionId {
        SessionId::new(text).expect("un texte non vide fait un identifiant de session")
    }

    /// Événement `Script` dont le record porte l'erreur `error` (`None` : script réussi). Le
    /// texte est masqué par le @Redactor amont avant d'arriver ici — la fixture utilise un
    /// @Redactor sans secret, le texte traverse tel quel.
    fn script_event(session_id: &str, error: Option<&str>) -> Event {
        let mut input = ScriptInput::new("agent", "let x = 1;", "42", std::time::Duration::from_millis(5));
        if let Some(error) = error {
            input = input.with_error(error);
        }
        let record = ScriptRecord::new(&Redactor::default(), 2000, input);
        Event::new(
            session(session_id),
            Timestamp::from_millis(1_730_000_000_000),
            EventKind::Script(record),
        )
    }

    /// Lignes de la table, sous forme `(subject, session_id, class, longueur du texte)`.
    async fn rows(db: &DatabaseConnection) -> Vec<(Option<String>, Option<String>, Option<String>, usize)> {
        let mut select = Query::select();
        select
            .columns([
                Alias::new("subject"),
                Alias::new("session_id"),
                Alias::new("class"),
                Alias::new("text"),
            ])
            .from(Alias::new(TABLE));
        db.query_all(&select)
            .await
            .expect("relecture de la table d'audit")
            .iter()
            .map(|row| {
                let subject: Option<String> = row.try_get("", "subject").expect("colonne subject");
                let session_id: Option<String> = row.try_get("", "session_id").expect("colonne session_id");
                let class: Option<String> = row.try_get("", "class").expect("colonne class");
                let text: Option<String> = row.try_get("", "text").expect("colonne text");
                (
                    subject,
                    session_id,
                    class,
                    text.map_or(0, |text| text.chars().count()),
                )
            })
            .collect()
    }

    /// La tâche d'écriture a consommé tout ce qui avait été poussé avant le marqueur : on attend
    /// une ligne d'audit dont le texte est le marqueur, la file étant d'ordre FIFO.
    async fn drained(db: &DatabaseConnection, marker: &str) {
        for _ in 0..400 {
            let found = {
                let mut select = Query::select();
                select
                    .column(Alias::new("id"))
                    .from(Alias::new(TABLE))
                    .and_where(Expr::col(Alias::new("text")).eq(marker));
                db.query_one(&select).await.expect("sonde de drainage").is_some()
            };
            if found {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!("la tâche d'audit n'a pas drainé le marqueur dans le délai");
    }

    /// Fixture : une ligne posée brutalement avec un `at` figé (le `record` pose l'heure de
    /// l'événement amont, pas un `now`).
    async fn insert_row(db: &DatabaseConnection, subject: &str, session_id: &str, class: &str, at: &str) {
        db.execute_unprepared(&format!(
            "INSERT INTO {TABLE} (subject, session_id, turn, class, text, at) \
             VALUES ('{subject}', '{session_id}', NULL, '{class}', 'erreur masquée', '{at}')"
        ))
        .await
        .expect("ligne d'audit de test insérée");
    }

    /// `Scenario` « une erreur de script est persistée » : après `record` et drainage par la
    /// tâche, une ligne existe — sujet, session, classe harmonisée avec `./session_index.rs`,
    /// texte tronqué à 2000 caractères.
    #[tokio::test]
    async fn a_script_error_is_persisted() {
        let db = test_db().await;
        let writer = AuditWriter::spawn(db.clone());
        let long_error = format!("runtime error: {}", "é".repeat(2500));
        writer.record("alice", &script_event("s-1", Some(&long_error)));
        // Marqueur de drainage : second enregistrement, court, qui arrive après le premier.
        writer.record("alice", &script_event("s-marqueur", Some("script cancelled")));
        drained(&db, "script cancelled").await;

        let stored = rows(&db).await;
        assert!(
            stored.iter().any(
                |(subject, session, class, length)| subject.as_deref() == Some("alice")
                    && session.as_deref() == Some("s-1")
                    && class.as_deref() == Some("runtime")
                    && *length == 2000
            ),
            "une ligne existe, texte tronqué à 2000 caractères, classe `runtime` (harmonisée \
             avec le lecteur de `./session_index.rs`) : {stored:?}"
        );
    }

    /// `Scenario` « un événement qui n'est pas une erreur est ignoré » : `TokenUsage` et `Script`
    /// réussi — aucune ligne. La preuve de drainage passe par un marqueur écrit **après** les
    /// deux événements ignorés (file FIFO) : le marqueur présent et unique prouve qu'ils ont été
    /// consommés et jetés.
    #[tokio::test]
    async fn events_that_are_not_errors_are_ignored() {
        let db = test_db().await;
        let writer = AuditWriter::spawn(db.clone());
        let usage = Event::new(
            session("s-1"),
            Timestamp::from_millis(1_730_000_000_000),
            EventKind::TokenUsage {
                prompt: 10,
                completion: 2,
            },
        );
        writer.record("alice", &usage);
        writer.record("alice", &script_event("s-1", None));
        writer.record(
            "alice",
            &script_event("s-marqueur", Some("scratchpad internal failure: x")),
        );
        drained(&db, "scratchpad internal failure: x").await;

        let stored = rows(&db).await;
        assert_eq!(
            stored.len(),
            1,
            "seul le marqueur est écrit : `TokenUsage` et le `Script` réussi sont ignorés : {stored:?}"
        );
        assert_eq!(
            stored.first().map(|(_, _, class, _)| class.as_deref()),
            Some(Some("internal")),
            "le texte inclassable porte le repli `internal` (repli du lecteur de `./session_index.rs`)"
        );
    }

    /// `Scenario` « `record` ne bloque jamais » : canal de 1024, tâche jamais mise à jour (le
    /// test ne cède pas le courant pendant la boucle) ; 2000 appels synchrones reviennent
    /// immédiatement et le compteur `dropped` croît.
    #[tokio::test]
    async fn record_never_blocks_and_dropped_grows() {
        let db = test_db().await;
        let writer = AuditWriter::spawn(db);
        let event = script_event("s-1", Some("runtime error: x"));
        let started = std::time::Instant::now();
        for _ in 0..2000 {
            writer.record("alice", &event);
        }
        let elapsed = started.elapsed();
        let dropped = writer.dropped();
        assert!(
            dropped > 0,
            "au-delà de la capacité de 1024, `dropped` croît : {dropped}"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "les 2000 appels `record` reviennent immédiatement : {elapsed:?}"
        );
    }

    /// `Scenario` `purge_before` : trois lignes d'âges différents, seules celles d'avant le
    /// `cutoff` disparaissent et leur nombre est rendu.
    #[tokio::test]
    async fn purge_before_removes_only_the_older_rows() {
        let db = test_db().await;
        insert_row(&db, "alice", "s-old", "runtime", "2026-01-01T00:00:00Z").await;
        insert_row(&db, "alice", "s-old2", "syntax", "2026-01-02T00:00:00Z").await;
        insert_row(&db, "alice", "s-recent", "runtime", "2026-10-01T00:00:00Z").await;
        let cutoff = chrono::DateTime::parse_from_rfc3339("2026-06-01T00:00:00Z")
            .expect("cutoff de test")
            .with_timezone(&chrono::Utc);

        let deleted = purge_before(&db, cutoff).await.expect("purge_before réussit");
        assert_eq!(deleted, 2, "les deux lignes anciennes sont supprimées");
        let stored = rows(&db).await;
        assert_eq!(
            stored
                .iter()
                .filter_map(|(_, session, _, _)| session.clone())
                .count(),
            1,
            "seule la ligne récente reste : {stored:?}"
        );
        assert!(
            stored
                .iter()
                .any(|(_, session, _, _)| session.as_deref() == Some("s-recent")),
            "c'est la ligne récente qui reste : {stored:?}"
        );
    }
}
