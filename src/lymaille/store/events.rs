//! `SeaEventLog` — journal des événements de session et son curseur
//! (`src/lymaille/store/events.sdd`, DSD `DEC-31`, issue #81 lot 3).
//!
//! Persistance écrite par la tâche d'écriture du hub (`../interactive/hub.sdd`), lue par le
//! hub pour les rejeux (`Last-Event-ID`) et purgée par `purge_sessions` (./purge.sdd). Aucune
//! entité `SeaORM` : requêtes `sea_query` brutes, comme ./audit.sdd. `now` est toujours un
//! paramètre — jamais l'horloge.

use sea_orm::ConnectionTrait;
use sea_orm::DatabaseConnection;
use sea_orm::DbErr;
use sea_orm::SqlErr;
use sea_orm::TransactionTrait as _;
use sea_orm::sea_query::Alias;
use sea_orm::sea_query::Expr;
use sea_orm::sea_query::ExprTrait as _;
use sea_orm::sea_query::Query;

use ::lymaille::SessionId;

use super::db_text;
use crate::lymaille::error::LymailleError;
use crate::lymaille::interactive::event::SessionEvent;

/// Table du journal, nom complet (`../migration.sdd`, amendement du 2026-11-14).
const TABLE: &str = "miryad_lymaille_session_events";

/// Borne de la limite de relecture (`./events.sdd` `Must` : bornée `[1, 10 000]`).
const MAX_LIMIT: usize = 10_000;

/// Reprises maximales de la transaction d'écriture sur conflit d'unicité (`Must`).
const MAX_RETRIES: u32 = 5;

/// Journal persistant des événements diffusés par le hub.
#[derive(Clone)]
pub struct SeaEventLog {
    db: DatabaseConnection,
}

/// Une ligne du journal : son curseur et l'événement relu.
#[derive(Clone, Debug, PartialEq)]
pub struct LoggedEvent {
    /// Curseur monotone propre à la session.
    pub seq: u64,
    /// L'événement tel que le flux le sert.
    pub event: SessionEvent,
}

/// Page rendue par [`SeaEventLog::read_after`].
#[derive(Clone, Debug)]
pub struct ReplayPage {
    /// Les lignes conservées, en ordre croissant de curseur.
    pub events: Vec<LoggedEvent>,
    /// Événements disparus entre le curseur demandé et la première ligne conservée.
    pub missed: u64,
    /// Il restait des lignes au-delà de la limite demandée.
    pub truncated: bool,
}

/// Le nom `snake_case` de la variante, lu du discriminant `type` de la sérialisation serde
/// (`./../interactive/event.rs`) — la colonne `kind` n'est qu'une étiquette, le contenu est
/// intégralement dans la colonne `event`.
fn kind_name(event: &SessionEvent) -> String {
    serde_json::to_value(&event.kind)
        .ok()
        .and_then(|value| {
            value
                .get("type")
                .and_then(|kind| kind.as_str())
                .map(str::to_owned)
        })
        .unwrap_or_default()
}

/// Une ligne lue : curseur et JSON brut tels que stockés.
struct RawRow {
    seq: u64,
    event: serde_json::Value,
}

/// Les lignes d'une session après un curseur, en ordre croissant, `limit + 1` au plus (la
/// sonde de `truncated`). Aucun verrou, aucune transaction : une simple lecture.
async fn fetch<C: ConnectionTrait>(
    conn: &C,
    session: &SessionId,
    after: Option<u64>,
    limit: usize,
) -> Result<Vec<RawRow>, LymailleError> {
    let mut select = Query::select();
    select
        .columns([Alias::new("seq"), Alias::new("event")])
        .from(Alias::new(TABLE))
        .and_where(Expr::col(Alias::new("session_id")).eq(session.as_str()))
        .order_by(Alias::new("seq"), sea_orm::sea_query::Order::Asc)
        .limit((limit.saturating_add(1)) as u64);
    if let Some(cursor) = after {
        select.and_where(Expr::col(Alias::new("seq")).gt(i64::try_from(cursor).unwrap_or(i64::MAX)));
    }
    let rows = conn
        .query_all(&select)
        .await
        .map_err(|error| LymailleError::Store(db_text(&error)))?;
    rows.iter()
        .map(|row| {
            let seq = row
                .try_get::<i64>("", "seq")
                .map_err(|error| LymailleError::Store(db_text(&error)))?;
            let event = row
                .try_get::<serde_json::Value>("", "event")
                .map_err(|error| LymailleError::Store(db_text(&error)))?;
            Ok(RawRow {
                seq: u64::try_from(seq)
                    .map_err(|_| LymailleError::Store("negative event cursor in journal".to_owned()))?,
                event,
            })
        })
        .collect()
}

/// Issue d'une tentative d'écriture sur conflit : conflit à reprendre, ou panne définitive.
enum WriteOutcome {
    Conflict,
    Failed(LymailleError),
}

impl SeaEventLog {
    /// Une tentative d'écriture du lot : relit le maximum dans la transaction, pose les
    /// curseurs `max + 1 ..= max + n`, commit. Un conflit d'unicité `(session_id, seq)` remonte
    /// en [`WriteOutcome::Conflict`], toute autre panne en `Failed`.
    async fn append_once(
        &self,
        session: &SessionId,
        events: &[SessionEvent],
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<Vec<u64>, WriteOutcome> {
        let transaction = self
            .db
            .begin()
            .await
            .map_err(|error| WriteOutcome::Failed(LymailleError::Store(db_text(&error))))?;
        let max = transaction
            .query_one(
                &Query::select()
                    .expr_as(Expr::col(Alias::new("seq")).max(), Alias::new("max_seq"))
                    .from(Alias::new(TABLE))
                    .and_where(Expr::col(Alias::new("session_id")).eq(session.as_str()))
                    .to_owned(),
            )
            .await
            .map_err(|error| WriteOutcome::Failed(LymailleError::Store(db_text(&error))))?
            .map(|row| {
                row.try_get::<Option<i64>>("", "max_seq")
                    .map_err(|error| WriteOutcome::Failed(LymailleError::Store(db_text(&error))))
            })
            .transpose()?
            .flatten()
            .unwrap_or(0);
        let mut statement = Query::insert();
        statement.into_table(Alias::new(TABLE));
        statement.columns(["session_id", "seq", "created_at", "kind", "event"]);
        let mut cursors = Vec::with_capacity(events.len());
        for (index, event) in events.iter().enumerate() {
            let offset = u64::try_from(index).unwrap_or(u64::MAX);
            let Some(next) = u64::try_from(max)
                .ok()
                .and_then(|base| base.checked_add(1).and_then(|first| first.checked_add(offset)))
            else {
                return Err(WriteOutcome::Failed(LymailleError::Store(
                    "event cursor out of range".to_owned(),
                )));
            };
            let payload = serde_json::to_value(event).map_err(|error| {
                WriteOutcome::Failed(LymailleError::Store(db_text(&DbErr::Custom(error.to_string()))))
            })?;
            statement
                .values(vec![
                    Expr::val(session.as_str()),
                    Expr::val(i64::try_from(next).unwrap_or(i64::MAX)),
                    Expr::val(now),
                    Expr::val(kind_name(event)),
                    Expr::val(payload),
                ])
                .map_err(|error| {
                    WriteOutcome::Failed(LymailleError::Store(db_text(&DbErr::Custom(error.to_string()))))
                })?;
            cursors.push(next);
        }
        transaction.execute(&statement).await.map_err(|error| {
            if matches!(error.sql_err(), Some(SqlErr::UniqueConstraintViolation(_))) {
                WriteOutcome::Conflict
            } else {
                WriteOutcome::Failed(LymailleError::Store(db_text(&error)))
            }
        })?;
        transaction
            .commit()
            .await
            .map_err(|error| WriteOutcome::Failed(LymailleError::Store(db_text(&error))))?;
        Ok(cursors)
    }
}

impl SeaEventLog {
    /// Journal sur la base `db`.
    #[must_use]
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }

    /// Ajoute un lot d'événements à la session, **une transaction** pour tout le lot : les
    /// curseurs `COALESCE(MAX(seq), 0) + 1` sont calculés dans la transaction et rendus dans
    /// l'ordre d'entrée. Un lot vide n'ouvre aucune transaction. Un conflit d'unicité
    /// `(session_id, seq)` (répliques concurrentes) reprend la transaction au plus
    /// `MAX_RETRIES` fois, la panne de la base est une `Store`.
    ///
    /// # Errors
    ///
    /// [`LymailleError::Store`] (`MRD-LYMAILLE-009`) : panne de la base, texte expurgé (règle 5
    /// de `./mod.sdd`), ou épuisement des reprises après conflit d'unicité.
    pub async fn append(
        &self,
        session: &SessionId,
        events: &[SessionEvent],
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<Vec<u64>, LymailleError> {
        if events.is_empty() {
            return Ok(Vec::new());
        }
        for attempt in 0..=MAX_RETRIES {
            match self.append_once(session, events, now).await {
                Ok(cursors) => return Ok(cursors),
                Err(WriteOutcome::Conflict) => {
                    if attempt == MAX_RETRIES {
                        return Err(LymailleError::Store(
                            "event journal cursor contention exceeded".to_owned(),
                        ));
                    }
                }
                Err(WriteOutcome::Failed(error)) => return Err(error),
            }
        }
        Err(LymailleError::Store(
            "event journal cursor contention exceeded".to_owned(),
        ))
    }

    /// Relit les lignes de la session d'après `after` (toutes si `None`), en ordre croissant,
    /// `limit` au plus (ramené dans `[1, 10 000]`). `truncated` dit qu'il en restait au-delà ;
    /// `missed` compte les curseurs disparus entre `after` et la première ligne rendue (jamais
    /// pour `after = None`) et les lignes dont le JSON ne se relit plus. Une relecture
    /// n'ouvre pas de transaction.
    ///
    /// # Errors
    ///
    /// [`LymailleError::Store`] (`MRD-LYMAILLE-009`) : panne de la base, texte expurgé.
    pub async fn read_after(
        &self,
        session: &SessionId,
        after: Option<u64>,
        limit: usize,
    ) -> Result<ReplayPage, LymailleError> {
        let limit = limit.clamp(1, MAX_LIMIT);
        let rows = fetch(&self.db, session, after, limit).await?;
        let truncated = rows.len() > limit;
        let window: &[RawRow] = if truncated {
            rows.get(..limit).unwrap_or(&rows)
        } else {
            &rows
        };
        let mut events = Vec::with_capacity(window.len());
        let mut missed = 0_u64;
        for row in window {
            match serde_json::from_value::<SessionEvent>(row.event.clone()) {
                Ok(event) => events.push(LoggedEvent { seq: row.seq, event }),
                Err(_) => missed = missed.saturating_add(1),
            }
        }
        if let (Some(cursor), Some(first)) = (after, window.first()) {
            // Écarts entre le curseur demandé et la première ligne conservée (lignes purgées).
            if first.seq > cursor.saturating_add(1) {
                missed = missed.saturating_add(first.seq.saturating_sub(cursor).saturating_sub(1));
            }
        }
        Ok(ReplayPage {
            events,
            missed,
            truncated,
        })
    }

    /// Le plus grand curseur de la session, `None` si elle n'a aucune ligne.
    ///
    /// # Errors
    ///
    /// [`LymailleError::Store`] (`MRD-LYMAILLE-009`) : panne de la base, texte expurgé.
    pub async fn last_seq(&self, session: &SessionId) -> Result<Option<u64>, LymailleError> {
        let row = self
            .db
            .query_one(
                &Query::select()
                    .expr_as(Expr::col(Alias::new("seq")).max(), Alias::new("max_seq"))
                    .from(Alias::new(TABLE))
                    .and_where(Expr::col(Alias::new("session_id")).eq(session.as_str()))
                    .to_owned(),
            )
            .await
            .map_err(|error| LymailleError::Store(db_text(&error)))?;
        Ok(row
            .map(|row| {
                row.try_get::<Option<i64>>("", "max_seq")
                    .map_err(|error| LymailleError::Store(db_text(&error)))
                    .map(|value| value.and_then(|max| u64::try_from(max).ok()))
            })
            .transpose()?
            .flatten())
    }

    /// Supprime les lignes d'une session, rend le nombre de lignes parties. Utilisée quand une
    /// session est supprimée ; ne touche jamais une autre session.
    ///
    /// # Errors
    ///
    /// [`LymailleError::Store`] (`MRD-LYMAILLE-009`) : panne de la base, texte expurgé.
    pub async fn delete_session(&self, session: &SessionId) -> Result<u64, LymailleError> {
        let statement = Query::delete()
            .from_table(Alias::new(TABLE))
            .and_where(Expr::col(Alias::new("session_id")).eq(session.as_str()))
            .to_owned();
        let result = self
            .db
            .execute(&statement)
            .await
            .map_err(|error| LymailleError::Store(db_text(&error)))?;
        Ok(result.rows_affected())
    }

    /// Supprime les lignes créées avant `cutoff` (appelée par `purge_sessions`, ./purge.sdd,
    /// avec le même seuil que les autres traces) ; rend le nombre de lignes parties, idempotente.
    ///
    /// # Errors
    ///
    /// [`LymailleError::Store`] (`MRD-LYMAILLE-009`) : panne de la base, texte expurgé.
    pub async fn purge_before(&self, cutoff: chrono::DateTime<chrono::Utc>) -> Result<u64, LymailleError> {
        let statement = Query::delete()
            .from_table(Alias::new(TABLE))
            .and_where(Expr::col(Alias::new("created_at")).lt(cutoff))
            .to_owned();
        let result = self
            .db
            .execute(&statement)
            .await
            .map_err(|error| LymailleError::Store(db_text(&error)))?;
        Ok(result.rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use super::SeaEventLog;
    use crate::lymaille::interactive::event::SessionEvent;
    use crate::lymaille::interactive::event::SessionEventKind;
    use crate::lymaille::store::test_support::test_db;
    use sea_orm::ConnectionTrait as _;
    use sea_orm::DatabaseConnection;
    use sea_orm::sea_query::Alias;
    use sea_orm::sea_query::Expr;
    use sea_orm::sea_query::ExprTrait as _;
    use sea_orm::sea_query::Query;

    /// `now` de fixture : instant figé, les méthodes ne lisent jamais l'horloge (`./events.sdd`).
    fn now() -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339("2026-11-14T00:00:00Z")
            .expect("now de test")
            .with_timezone(&chrono::Utc)
    }

    /// Identifiant de session de fixture.
    fn session(id: &str) -> ::lymaille::SessionId {
        ::lymaille::SessionId::new(id).expect("un texte non vide fait un identifiant de session")
    }

    /// Un `TurnStarted` typique de la session `id`, tour `turn`.
    fn event(id: &str, turn: u64) -> SessionEvent {
        SessionEvent {
            session: id.to_owned(),
            at: now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            kind: SessionEventKind::TurnStarted { turn },
        }
    }

    /// Un lot de `count` événements pour la session `id`, tours 0 à `count-1`.
    fn batch(id: &str, count: u64) -> Vec<SessionEvent> {
        (0..count).map(|turn| event(id, turn)).collect()
    }

    /// Le journal sur la base de test migrée.
    async fn log() -> SeaEventLog {
        SeaEventLog::new(test_db().await)
    }

    /// Les curseurs lus après un curseur, vides en page vide.
    fn seqs(page: &super::ReplayPage) -> Vec<u64> {
        page.events.iter().map(|logged| logged.seq).collect()
    }

    /// Nombre total de lignes de la table du journal.
    async fn count_all(db: &DatabaseConnection) -> usize {
        db.query_all(
            &Query::select()
                .expr_as(Expr::val(1), Alias::new("row_probe"))
                .from(Alias::new("miryad_lymaille_session_events"))
                .to_owned(),
        )
        .await
        .unwrap_or_else(|error| panic!("comptage du journal : {error}"))
        .len()
    }

    /// `Scenario` « les curseurs d'une session sont consécutifs à partir de 1 » : deux lots sur
    /// `s1`, un sur `s2` — `[1, 2]`, `[3]`, `[1]` ; `last_seq(s1) = Some(3)`, `last_seq(s3) = None`.
    #[tokio::test]
    async fn les_curseurs_d_une_session_sont_consecutifs_a_partir_de_1() {
        let log = log().await;
        let first = log
            .append(&session("s1"), &[event("s1", 0), event("s1", 1)], now())
            .await
            .expect("le premier lot est accepté");
        assert_eq!(first, vec![1, 2], "les curseurs rendus dans l'ordre d'entrée");
        let second = log
            .append(&session("s1"), &[event("s1", 2)], now())
            .await
            .expect("le second lot est accepté");
        assert_eq!(second, vec![3], "le curseur reprend où il s'est arrêté");
        let other = log
            .append(&session("s2"), &[event("s2", 0)], now())
            .await
            .expect("une autre session a son propre compteur");
        assert_eq!(other, vec![1], "les curseurs sont propres à une session");
        assert_eq!(
            log.last_seq(&session("s1")).await.expect("last_seq"),
            Some(3),
            "`last_seq(s1)` vaut `Some(3)`"
        );
        assert_eq!(
            log.last_seq(&session("s3")).await.expect("last_seq"),
            None,
            "`last_seq(s3)` vaut `None` : aucune ligne"
        );
    }

    /// `Scenario` « un lot vide n'écrit rien » : `Ok([])` et aucune ligne, sans transaction.
    #[tokio::test]
    async fn un_lot_vide_n_ecrit_rien() {
        let log = log().await;
        let cursors = log
            .append(&session("s1"), &[], now())
            .await
            .expect("un lot vide est un succès");
        assert!(cursors.is_empty(), "`Ok([])` pour un lot vide");
        assert_eq!(count_all(log_db(&log)).await, 0, "aucune ligne n'a été écrite");
    }

    /// Accès à la base du journal pour les sondes de test.
    fn log_db(log: &SeaEventLog) -> &DatabaseConnection {
        &log.db
    }

    /// `Scenario` « la relecture rend l'ordre et respecte le curseur » : après le curseur 3,
    /// `[4, 5]` sans signal ; sans curseur avec limite 2, `[1, 2]` avec `truncated`.
    #[tokio::test]
    async fn la_relecture_rend_l_ordre_et_respecte_le_curseur() {
        let log = log().await;
        log.append(&session("s1"), &batch("s1", 5), now())
            .await
            .expect("cinq événements posés");
        let page = log
            .read_after(&session("s1"), Some(3), 100)
            .await
            .expect("la relecture après le curseur 3 réussit");
        assert_eq!(seqs(&page), vec![4, 5], "les lignes d'après le curseur, en ordre");
        assert!(
            !page.truncated && page.missed == 0,
            "rien ne manque, rien ne déborde"
        );
        let page = log
            .read_after(&session("s1"), None, 2)
            .await
            .expect("la relecture sans curseur réussit");
        assert_eq!(seqs(&page), vec![1, 2], "les deux premières lignes");
        assert!(page.truncated, "il en restait au-delà de la limite");
    }

    /// `Scenario` « la relecture après une purge signale ce qui a disparu » : 1 à 40 purgés,
    /// relecture après 10 → `[41..50]`, `missed = 30`, pas de `truncated`.
    #[tokio::test]
    async fn la_relecture_apres_une_purge_signale_ce_qui_a_disparu() {
        let log = log().await;
        log.append(&session("s1"), &batch("s1", 40), now())
            .await
            .expect("les curseurs 1 à 40 sont posés à t0");
        let later = now()
            .checked_add_signed(chrono::Duration::hours(2))
            .expect("t0 + 2 h");
        log.append(&session("s1"), &batch("s1", 10), later)
            .await
            .expect("les curseurs 41 à 50 sont posés à t0 + 2 h");
        let purged = log
            .purge_before(
                now()
                    .checked_add_signed(chrono::Duration::hours(1))
                    .expect("cutoff entre les deux tranches"),
            )
            .await
            .expect("la purge par âge réussit");
        assert_eq!(purged, 40, "les quarante lignes de t0 sont parties");
        let page = log
            .read_after(&session("s1"), Some(10), 100)
            .await
            .expect("la relecture après le curseur 10 réussit");
        assert_eq!(
            seqs(&page),
            (41..=50).collect::<Vec<u64>>(),
            "les curseurs 41 à 50 sont rendus en ordre"
        );
        assert_eq!(page.missed, 30, "les curseurs 11 à 40 ont disparu");
        assert!(!page.truncated, "il en restait moins que la limite");
    }

    /// `Scenario` « une ligne illisible est écartée, pas fatale » : le JSON du curseur 2 n'est
    /// plus un `SessionEvent` → les curseurs 1 et 3, `missed = 1`, sans erreur.
    #[tokio::test]
    async fn une_ligne_illisible_est_ecartee_pas_fatale() {
        let log = log().await;
        log.append(&session("s1"), &batch("s1", 3), now())
            .await
            .expect("trois événements posés");
        log_db(&log)
            .execute_unprepared(
                "UPDATE miryad_lymaille_session_events SET event = '{\"bogus\": true}' \
                 WHERE session_id = 's1' AND seq = 2",
            )
            .await
            .expect("le curseur 2 porte un JSON qui n'est pas un SessionEvent");
        let page = log
            .read_after(&session("s1"), None, 10)
            .await
            .expect("une ligne illisible n'est jamais une erreur");
        assert_eq!(seqs(&page), vec![1, 3], "la ligne illisible est écartée");
        assert_eq!(page.missed, 1, "elle est comptée dans `missed`");
    }

    /// `Scenario` « le journal n'a pas de plafond par session » : 120 événements en deux lots
    /// sont tous relus, rien n'est supprimé à l'écriture.
    #[tokio::test]
    async fn le_journal_n_a_pas_de_plafond_par_session() {
        let log = log().await;
        log.append(&session("s1"), &batch("s1", 60), now())
            .await
            .expect("premier lot de 60");
        log.append(&session("s1"), &batch("s1", 60), now())
            .await
            .expect("second lot de 60");
        let page = log
            .read_after(&session("s1"), None, 1000)
            .await
            .expect("la relecture des 120 lignes réussit");
        assert_eq!(
            seqs(&page),
            (1..=120).collect::<Vec<u64>>(),
            "les curseurs 1 à 120, aucune ligne supprimée à l'écriture"
        );
        assert!(!page.truncated, "120 < 1000, rien ne déborde");
    }

    /// `Scenario` « des écrivains concurrents n'obtiennent jamais le même curseur » : deux
    /// journaux, deux tâches de 20 événements — les curseurs sont exactement 1 à 40.
    ///
    /// Limite assumée (validation du 2026-11-16) : `test_db` sans `MIRYAD_TEST_PG_URL` est un pool
    /// SQLite mémoire à **une** connexion (deux connexions verraient deux bases distinctes) ; le
    /// `tokio::join!` est donc de fait séquentiel — chaque transaction détient la connexion
    /// unique jusqu'au commit. L'invariant (aucun doublon, 1 à 40) est prouvé, mais le chemin
    /// conflit/reprise `MAX + 1` n'est pas exercé ici : sur SQLite une seconde écriture concurrente
    /// rend `SQLITE_BUSY`, jamais une violation d'unicité. Ce chemin ne peut être réellement joué
    /// que sous PostgreSQL (`MIRYAD_TEST_PG_URL`) — voir le `[?]` de la tâche de validation de
    /// `./events.sdd`.
    #[tokio::test]
    async fn des_ecrivains_concurrents_n_obtiennent_jamais_le_meme_curseur() {
        let db = test_db().await;
        let a = SeaEventLog::new(db.clone());
        let b = SeaEventLog::new(db.clone());
        let task = |log: SeaEventLog, tag: &'static str| async move {
            let mut cursors = Vec::new();
            for chunk in 0..4 {
                let events: Vec<SessionEvent> = (0..5).map(|i| event(tag, chunk * 5 + i)).collect();
                let mut appended = log
                    .append(&session("s1"), &events, now())
                    .await
                    .expect("un lot d'une tâche concurrente est accepté");
                cursors.append(&mut appended);
            }
            cursors
        };
        let (first, second) = tokio::join!(task(a, "a"), task(b, "b"));
        let mut cursors = first;
        cursors.extend(second);
        cursors.sort_unstable();
        cursors.dedup();
        assert_eq!(
            cursors,
            (1..=40).collect::<Vec<u64>>(),
            "les 40 curseurs de s1 sont tous distincts, de 1 à 40"
        );
    }

    /// `Scenario` « `purge_before` ne supprime que l'ancien » : les lignes de `t0` partent, celles
    /// de `t0 + 2 h` restent ; le second appel rend 0.
    #[tokio::test]
    async fn purge_before_ne_supprime_que_l_ancien() {
        let log = log().await;
        log.append(&session("s1"), &batch("s1", 3), now())
            .await
            .expect("trois lignes à t0");
        let later = now()
            .checked_add_signed(chrono::Duration::hours(2))
            .expect("t0 + 2 h");
        log.append(&session("s1"), &batch("s1", 2), later)
            .await
            .expect("deux lignes à t0 + 2 h");
        let cutoff = now()
            .checked_add_signed(chrono::Duration::hours(1))
            .expect("t0 + 1 h");
        assert_eq!(
            log.purge_before(cutoff).await.expect("la purge réussit"),
            3,
            "seules les trois lignes de t0 partent"
        );
        assert_eq!(
            log.purge_before(cutoff).await.expect("la purge est idempotente"),
            0,
            "un second appel rend 0"
        );
        assert_eq!(
            count_all(log_db(&log)).await,
            2,
            "les deux lignes récentes restent"
        );
    }

    /// `Scenario` « `delete_session` ne touche que sa session » : le nombre rendu est celui de
    /// `s1`, `last_seq(s1)` devient `None`, `s2` est intacte.
    #[tokio::test]
    async fn delete_session_ne_touche_que_sa_session() {
        let log = log().await;
        log.append(&session("s1"), &batch("s1", 3), now())
            .await
            .expect("trois lignes pour s1");
        log.append(&session("s2"), &batch("s2", 2), now())
            .await
            .expect("deux lignes pour s2");
        assert_eq!(
            log.delete_session(&session("s1"))
                .await
                .expect("la suppression réussit"),
            3,
            "le nombre rendu est celui des lignes de s1"
        );
        assert_eq!(
            log.last_seq(&session("s1")).await.expect("last_seq"),
            None,
            "s1 n'a plus de ligne"
        );
        assert_eq!(
            log.last_seq(&session("s2")).await.expect("last_seq"),
            Some(2),
            "s2 est intacte"
        );
    }

    /// `Scenario` « le journal ne porte ni secret ni contenu journalisé » : la preuve se fait sur
    /// la **colonne brute** lue en SQL (pas sur le `SessionEvent` re-sérialisé après relecture —
    /// serde ignore les champs inconnus, une colonne enrichie hors contrat passerait inaperçue).
    /// Un `Script` (résumé, sans `text`) et un `McpCall` (sans `args`) : chaque colonne `event`
    /// est exactement le JSON que le flux sert (aucune clé `text` ni `args`, aucun texte de
    /// script) ; la colonne `kind` ne porte que le nom `snake_case` de la variante ; et aucune
    /// trace `tracing` capturée pendant l'écriture et la relecture ne contient l'identifiant de
    /// session. Runtime `current_thread` : la capture de traces est thread-local (`hub.rs`).
    #[tokio::test(flavor = "current_thread")]
    async fn le_journal_ne_porte_ni_secret_ni_contenu_journalise() {
        use crate::lymaille::interactive::hub::tests::capture_traces;

        let log = log().await;
        // Identifiant distinctif : c'est lui que les traces ne doivent jamais porter (`Must not`).
        let identifiant = "sess-journal-trace-7c9e";
        let events = vec![
            SessionEvent {
                session: identifiant.to_owned(),
                at: now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                kind: SessionEventKind::Script {
                    agent: "agent".to_owned(),
                    duration_ms: 12,
                    result: "résultat masqué".to_owned(),
                    truncated: false,
                    error: None,
                },
            },
            SessionEvent {
                session: identifiant.to_owned(),
                at: now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                kind: SessionEventKind::McpCall {
                    role: "serveur".to_owned(),
                    function: "outil".to_owned(),
                    status: "ok".to_owned(),
                    duration_ms: 3,
                },
            },
        ];
        let (_guard, captured) = capture_traces();
        log.append(&session(identifiant), &events, now())
            .await
            .expect("les deux résumés sont ajoutés");
        let page = log
            .read_after(&session(identifiant), None, 10)
            .await
            .expect("la relecture réussit");
        assert_eq!(page.events.len(), 2, "les deux lignes sont relues");
        assert_eq!(page.events[0].event, events[0], "le résumé est restitué tel quel");
        assert_eq!(page.events[1].event, events[1], "le résumé est restitué tel quel");

        // La colonne brute, lue en SQL sans repasser par `SessionEvent` : le `SELECT` rend le
        // JSON tel qu'il est stocké, champs inconnus de serde compris.
        let rows = log_db(&log)
            .query_all(
                &Query::select()
                    .columns([Alias::new("seq"), Alias::new("kind"), Alias::new("event")])
                    .from(Alias::new(super::TABLE))
                    .and_where(Expr::col(Alias::new("session_id")).eq(identifiant))
                    .order_by(Alias::new("seq"), sea_orm::sea_query::Order::Asc)
                    .to_owned(),
            )
            .await
            .expect("la colonne brute se lit en SQL");
        assert_eq!(rows.len(), 2, "deux lignes pour la session");
        let attendu = events
            .iter()
            .map(|e| serde_json::to_value(e).expect("un `SessionEvent` se sérialise"))
            .collect::<Vec<_>>();
        let kinds_attendus = ["script", "mcp_call"];
        for (index, row) in rows.iter().enumerate() {
            let brut = row
                .try_get::<serde_json::Value>("", "event")
                .expect("la colonne `event` est du JSON");
            assert_eq!(
                brut, attendu[index],
                "la colonne brute est exactement ce que le flux sert, rien en plus : {brut}"
            );
            let rendu = brut.to_string();
            for interdit in ["\"text\"", "\"args\"", "secret_looking_script"] {
                assert!(
                    !rendu.contains(interdit),
                    "la colonne brute du {index}e événement ne porte {interdit} : {rendu}"
                );
            }
            let kind = row
                .try_get::<String>("", "kind")
                .expect("la colonne `kind` est un texte");
            assert_eq!(
                kind,
                attendu[index]["kind"]["type"].as_str().unwrap_or_default(),
                "la colonne `kind` porte le nom snake_case de la variante, pas un contenu"
            );
            assert_eq!(kind, kinds_attendus[index], "le nom de variante attendu");
        }

        // Clause « aucune trace `tracing` ne contient l'identifiant de session » (`Must not`) :
        // toute trace du crate émise pendant l'`append` et la relecture est capturée ici.
        for ligne in captured.lines() {
            assert!(
                !ligne.contains(identifiant),
                "une trace `tracing` porte l'identifiant de session : {ligne}"
            );
        }
    }
}
