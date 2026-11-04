//! `purge_sessions` — rétention des sessions et de leurs traces
//! (`src/lymaille/store/purge.sdd`, DSD `DEC-22`, `AC-13`).
//!
//! Le trait `SessionStore` de `lymaille` n'a pas de suppression : une session `Sub` par
//! (clé de workflow, step) s'accumulerait sans fin dans `miryad_lymaille_sessions`.
//! `purge_sessions` supprime les sessions dont `updated_at` précède `now - retention`, quelle
//! que soit leur nature, ainsi que les traces qui les accompagnent : erreurs de script
//! (`./audit.sdd`) et journal d'écritures de ressources (`./write_journal.sdd`) plus anciens que
//! le même seuil. L'application **planifie** l'appel ; la crate ne démarre rien. `now` est un
//! paramètre — jamais l'horloge — pour des tests déterministes.

use sea_orm::ConnectionTrait;
use sea_orm::DatabaseConnection;
use sea_orm::sea_query::Alias;
use sea_orm::sea_query::Expr;
use sea_orm::sea_query::ExprTrait as _;
use sea_orm::sea_query::Query;

use chrono::DateTime;
use chrono::Duration;
use chrono::Utc;

use crate::lymaille::error::LymailleError;

use super::audit;
use super::db_text;
use super::write_journal;

/// Table des sessions, nom complet (`../migration.sdd`, `./session.sdd`).
const SESSIONS_TABLE: &str = "miryad_lymaille_sessions";

/// Rétention par défaut proposée à l'application (`./purge.sdd`) : 30 jours.
pub const DEFAULT_RETENTION: Duration = Duration::days(30);

/// Nombre de lignes supprimées par table (`./purge.sdd`).
#[derive(Debug, Clone, PartialEq)]
pub struct PurgeReport {
    /// Sessions de `miryad_lymaille_sessions` supprimées.
    pub sessions: u64,
    /// Erreurs de script de `miryad_lymaille_script_errors` supprimées.
    pub script_errors: u64,
    /// Entrées de `miryad_lymaille_write_journal` supprimées.
    pub write_journal: u64,
}

/// Supprime les sessions `updated_at < now - retention`, puis les erreurs de script et les
/// entrées de journal d'écritures `< cutoff` (`./purge.sdd`). Chaque suppression dans sa propre
/// requête, sans transaction englobante : une purge partielle reste valide. Idempotente — un
/// second appel immédiat rend trois zéros.
///
/// # Errors
///
/// - [`LymailleError::InvalidDefinition`] (`MRD-LYMAILLE-004`) : `retention` non positive, nulle
///   ou négative (une rétention négative mettrait le `cutoff` dans le futur et viderait tout), ou
///   hors de portée du calendrier ;
/// - [`LymailleError::Store`] (`MRD-LYMAILLE-009`) : panne de la base, texte expurgé (règle 5 de
///   `./mod.sdd`).
pub async fn purge_sessions(
    db: &DatabaseConnection,
    now: DateTime<Utc>,
    retention: Duration,
) -> Result<PurgeReport, LymailleError> {
    if retention <= Duration::zero() {
        return Err(LymailleError::InvalidDefinition(
            "retention must be positive".to_owned(),
        ));
    }
    // Arithmétique vérifiée : un dépassement de calendrier est une erreur, jamais une panique.
    let cutoff = now
        .checked_sub_signed(retention)
        .ok_or_else(|| LymailleError::InvalidDefinition("retention out of calendar range".to_owned()))?;
    let statement = Query::delete()
        .from_table(Alias::new(SESSIONS_TABLE))
        .and_where(Expr::col(Alias::new("updated_at")).lt(cutoff))
        .to_owned();
    let sessions = db
        .execute(&statement)
        .await
        .map_err(|error| LymailleError::Store(db_text(&error)))?
        .rows_affected();
    // Les traces suivent, chacune dans sa requête (`./purge.sdd`).
    let script_errors = audit::purge_before(db, cutoff).await?;
    let journal = write_journal::purge_before(db, cutoff).await?;
    Ok(PurgeReport {
        sessions,
        script_errors,
        write_journal: journal,
    })
}

#[cfg(test)]
mod tests {
    use super::DEFAULT_RETENTION;
    use super::PurgeReport;
    use super::purge_sessions;
    use crate::lymaille::store::test_support::test_db;
    use sea_orm::ConnectionTrait as _;
    use sea_orm::DatabaseConnection;
    use sea_orm::sea_query::Alias;
    use sea_orm::sea_query::Expr;
    use sea_orm::sea_query::Query;

    /// `now` de fixture : instant figé, la fonction ne lit jamais l'horloge (`./purge.sdd`).
    fn now() -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339("2026-11-01T00:00:00Z")
            .expect("now de test")
            .with_timezone(&chrono::Utc)
    }

    /// Horodatage `days` jours avant `now()`, en littéral SQL.
    fn ago(days: i64) -> String {
        (now() - chrono::Duration::days(days)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    }

    /// Ligne de session posée brutalement avec son `updated_at` (la purge ne lit pas l'état).
    async fn insert_session(db: &DatabaseConnection, subject: &str, id: &str, updated_at: &str) {
        db.execute_unprepared(&format!(
            "INSERT INTO miryad_lymaille_sessions (subject, session_id, kind, state, updated_at) \
             VALUES ('{subject}', '{id}', 'work', '{{}}', '{updated_at}')"
        ))
        .await
        .expect("ligne de session de test insérée");
    }

    async fn insert_script_error(db: &DatabaseConnection, session_id: &str, at: &str) {
        db.execute_unprepared(&format!(
            "INSERT INTO miryad_lymaille_script_errors (subject, session_id, turn, class, text, at) \
             VALUES ('alice', '{session_id}', NULL, 'runtime', 'erreur masquée', '{at}')"
        ))
        .await
        .expect("ligne d'audit de test insérée");
    }

    async fn insert_journal_row(db: &DatabaseConnection, run: &str, step: &str, op: &str, at: &str) {
        db.execute_unprepared(&format!(
            "INSERT INTO miryad_lymaille_write_journal (run_key, step_key, op_key, result, created_at) \
             VALUES ('{run}', '{step}', '{op}', '{{\"v\": 1}}', '{at}')"
        ))
        .await
        .expect("ligne de journal de test insérée");
    }

    /// Nombre total de lignes d'une table.
    async fn count_all(db: &DatabaseConnection, table: &str) -> usize {
        db.query_all(
            &Query::select()
                .expr_as(Expr::val(1), Alias::new("row_probe"))
                .from(Alias::new(table))
                .to_owned(),
        )
        .await
        .unwrap_or_else(|error| panic!("comptage sur {table} : {error}"))
        .len()
    }

    /// `Scenario` « seules les sessions anciennes disparaissent » : trois sessions de 1, 20 et 40
    /// jours, rétention de 30 jours — seule celle de 40 jours disparaît, `PurgeReport.sessions = 1`.
    #[tokio::test]
    async fn only_the_old_sessions_disappear() {
        let db = test_db().await;
        insert_session(&db, "alice", "s-1j", &ago(1)).await;
        insert_session(&db, "alice", "s-20j", &ago(20)).await;
        insert_session(&db, "alice", "s-40j", &ago(40)).await;

        let report = purge_sessions(&db, now(), chrono::Duration::days(30))
            .await
            .expect("la purge réussit");
        assert_eq!(report.sessions, 1, "`PurgeReport.sessions = 1` : {report:?}");
        let kept: Vec<String> = {
            let rows = db
                .query_all(
                    &Query::select()
                        .column(Alias::new("session_id"))
                        .from(Alias::new("miryad_lymaille_sessions"))
                        .to_owned(),
                )
                .await
                .expect("relecture des sessions");
            rows.iter()
                .map(|row| {
                    let id: String = row.try_get("", "session_id").expect("colonne session_id");
                    id
                })
                .collect()
        };
        let mut kept_sorted = kept.clone();
        kept_sorted.sort();
        assert_eq!(
            kept_sorted,
            vec!["s-1j".to_string(), "s-20j".to_string()],
            "la session de 40 jours a disparu, les deux d'après le seuil restent : {kept:?}"
        );
    }

    /// `Scenario` « les traces suivent » : erreurs de script et entrées de journal de 40 jours et
    /// de 1 jour — seules celles de 40 jours disparaissent.
    #[tokio::test]
    async fn the_traces_follow() {
        let db = test_db().await;
        insert_script_error(&db, "s-vieux", &ago(40)).await;
        insert_script_error(&db, "s-neuf", &ago(1)).await;
        insert_journal_row(&db, "run-vieux", "step", "op", &ago(40)).await;
        insert_journal_row(&db, "run-neuf", "step", "op", &ago(1)).await;

        let report = purge_sessions(&db, now(), DEFAULT_RETENTION)
            .await
            .expect("la purge avec la rétention par défaut réussit");
        assert_eq!(
            report,
            PurgeReport {
                sessions: 0,
                script_errors: 1,
                write_journal: 1,
            },
            "une trace de chaque table disparaît"
        );
        assert_eq!(
            count_all(&db, "miryad_lymaille_script_errors").await,
            1,
            "seule l'erreur récente reste"
        );
        assert_eq!(
            count_all(&db, "miryad_lymaille_write_journal").await,
            1,
            "seule l'entrée de journal récente reste"
        );
    }

    /// `Scenario` `idempotence` : un second appel immédiat rend trois zéros.
    #[tokio::test]
    async fn a_second_call_is_three_zeros() {
        let db = test_db().await;
        insert_session(&db, "alice", "s-40j", &ago(40)).await;
        insert_script_error(&db, "s-40j", &ago(40)).await;
        insert_journal_row(&db, "run", "step", "op", &ago(40)).await;

        let first = purge_sessions(&db, now(), chrono::Duration::days(30))
            .await
            .expect("la première purge réussit");
        assert_eq!(first.sessions, 1, "la première purge fait son travail");
        let second = purge_sessions(&db, now(), chrono::Duration::days(30))
            .await
            .expect("la seconde purge réussit");
        assert_eq!(
            second,
            PurgeReport {
                sessions: 0,
                script_errors: 0,
                write_journal: 0,
            },
            "un second appel immédiat rend trois zéros"
        );
    }

    /// `Scenario` « rétention nulle refusée » : `InvalidDefinition` (code `MRD-LYMAILLE-004`) et
    /// rien n'est supprimé.
    #[tokio::test]
    async fn a_zero_retention_is_refused() {
        let db = test_db().await;
        insert_session(&db, "alice", "s-40j", &ago(40)).await;

        let refused = purge_sessions(&db, now(), chrono::Duration::zero())
            .await
            .expect_err("une rétention nulle viderait tout : elle est refusée");
        assert!(
            refused.to_string().contains("MRD-LYMAILLE-004"),
            "`InvalidDefinition` avec son code : {refused}"
        );
        assert_eq!(
            count_all(&db, "miryad_lymaille_sessions").await,
            1,
            "la session ancienne est encore là : rien n'est supprimé"
        );
    }

    /// `Scenario` « une rétention non positive est refusée » : une rétention **négative** rendrait
    /// un `cutoff` dans le futur et viderait toute la table — `InvalidDefinition` (code
    /// `MRD-LYMAILLE-004`) et aucune ligne supprimée (renforcement acté 2026-11-04 ; le cas nul
    /// est verrouillé par `a_zero_retention_is_refused`).
    #[tokio::test]
    async fn a_negative_retention_is_refused() {
        let db = test_db().await;
        insert_session(&db, "alice", "s-1j", &ago(1)).await;
        insert_session(&db, "alice", "s-40j", &ago(40)).await;

        let refused = purge_sessions(&db, now(), chrono::Duration::days(-1))
            .await
            .expect_err(
                "une rétention négative met le cutoff dans le futur et viderait tout : elle est refusée",
            );
        assert!(
            refused.to_string().contains("MRD-LYMAILLE-004"),
            "`InvalidDefinition` avec son code : {refused}"
        );
        assert_eq!(
            count_all(&db, "miryad_lymaille_sessions").await,
            2,
            "aucune ligne supprimée, y compris la session ancienne : {refused}"
        );
    }

    /// `Scenario` « les autres tables sont intactes » : des agents, des toolsets et des mémoires
    /// anciens — la purge ne touche que les sessions et leurs deux traces (`Must not`).
    #[tokio::test]
    async fn the_other_tables_are_intact() {
        let db = test_db().await;
        let old = ago(40);
        db.execute_unprepared(&format!(
            "INSERT INTO miryad_lymaille_agents (owner_id, agent_id, kind, profile, definition, created_at) \
             VALUES (1, 'vieux', 'work', 'qwen', '{{}}', '{old}')"
        ))
        .await
        .expect("agent privé ancien inséré");
        db.execute_unprepared(&format!(
            "INSERT INTO miryad_lymaille_toolsets (owner_id, toolset_id, version, active, draft, created_at) \
             VALUES (1, 'vieux-ts', '1.0.0', NULL, NULL, '{old}')"
        ))
        .await
        .expect("toolset privé ancien inséré");
        db.execute_unprepared(
            "INSERT INTO miryad_lymaille_memories (subject, level, reference, memory_id, memory) \
             VALUES ('alice', 'user', 'r', 'm1', '{}')",
        )
        .await
        .expect("mémoire inséré");

        purge_sessions(&db, now(), chrono::Duration::days(30))
            .await
            .expect("la purge réussit");
        assert_eq!(
            count_all(&db, "miryad_lymaille_agents").await,
            1,
            "l'agent ancien existe encore"
        );
        assert_eq!(
            count_all(&db, "miryad_lymaille_toolsets").await,
            1,
            "le toolset ancien existe encore"
        );
        assert_eq!(
            count_all(&db, "miryad_lymaille_memories").await,
            1,
            "le mémoire existe encore"
        );
    }
}
