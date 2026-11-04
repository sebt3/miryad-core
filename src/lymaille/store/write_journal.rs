//! Journal d'idempotence des écritures de ressources faites par un step batch
//! (`src/lymaille/store/write_journal.sdd`).
//!
//! Le step batch est rejoué par Restate avec la session restaurée (DSD `DEC-21`) : une clé
//! d'ordre ne désignerait pas la même opération d'un rejeu à l'autre, la clé est donc **dérivée
//! du contenu** — `op_key` est pure, sans horodatage ni compteur. Le journal est rangé par
//! `(run_key, step_key)` (les clés de la session `Sub`, `../../workflow/subagent_step.sdd`).
//! Ordre imposé aux appelants (`../resource_module.sdd`) : `replay` ; sinon exécuter l'écriture ;
//! puis `record`. Les sessions interactives (non rejouées) n'utilisent pas le journal.

use sea_orm::ConnectionTrait;
use sea_orm::DatabaseConnection;
use sea_orm::DbErr;
use sea_orm::sea_query::Alias;
use sea_orm::sea_query::Expr;
use sea_orm::sea_query::ExprTrait;
use sea_orm::sea_query::OnConflict;
use sea_orm::sea_query::Query;
use sha2::Digest;
use sha2::Sha256;

use super::db_text;
use crate::lymaille::error::LymailleError;

/// Table du journal, nom complet posé par `m20261104_000001_lymaille_lot2`
/// (`src/lymaille/migration.sdd`) : PK(`run_key`, `step_key`, `op_key`), `result` JSON NOT NULL,
/// `created_at`.
const TABLE: &str = "miryad_lymaille_write_journal";

/// Borne du résultat journalisé (`Must not` de `./write_journal.sdd` : le contenu des lignes ne
/// se journalise pas, seul le résultat expurgé est stocké, borné à 64 Ko).
const MAX_RESULT_BYTES: usize = 64 * 1024;

/// Clé de rangée du journal : le run et le step de la session `Sub` qui rejoue ses écritures
/// (`../../workflow/subagent_step.sdd` les fournit).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WriteScope {
    /// Clé du run de workflow (`run_key` de la session).
    pub run_key: String,
    /// Clé du step dans le run (`step_key` de la session).
    pub step_key: String,
}

/// Écriture journalisée, discriminant de `op_key` (le `Exposes` de `./write_journal.sdd`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    /// Création de ressource.
    Create,
    /// Mise à jour de ressource.
    Update,
    /// Suppression de ressource.
    Delete,
}

impl Op {
    /// Texte de l'opération dans l'enveloppe hachée (jamais l'heure, jamais un compteur).
    fn text(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Update => "update",
            Self::Delete => "delete",
        }
    }
}

/// JSON canonique : compact, clés d'objet triées récursivement en ordre binaire de leurs
/// clés **brutes**, chacune étant échappée à l'émission (le tri porte sur les clés non
/// échappées) ; tableaux ordonnés (l'ordre des éléments fait partie du contenu). Le texte
/// des scalaires est celui de `Display` de `serde_json::Value` (sérialisation compacte).
fn canonical(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let mut out = String::from("{");
            for (index, key) in keys.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::Value::String((*key).clone()).to_string());
                out.push(':');
                if let Some(item) = map.get(key.as_str()) {
                    out.push_str(&canonical(item));
                }
            }
            out.push('}');
            out
        }
        serde_json::Value::Array(items) => {
            let mut out = String::from("[");
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(&canonical(item));
            }
            out.push(']');
            out
        }
        scalar => scalar.to_string(),
    }
}

/// Clé d'idempotence : SHA-256 hexadécimal (minuscules) du JSON canonique de l'enveloppe
/// `{op, resource, id, body}` — pure, deux corps égaux à l'ordre des clés près donnent la même
/// clé (`./write_journal.sdd`).
#[must_use]
pub fn op_key(op: Op, resource: &str, id: Option<i32>, body: &serde_json::Value) -> String {
    let envelope = serde_json::json!({
        "op": op.text(),
        "resource": resource,
        "id": id,
        "body": body,
    });
    hex::encode(Sha256::digest(canonical(&envelope).as_bytes()))
}

/// Le résultat déjà journalisé sous `(scope, op_key)`, ou `None` — premier appel d'un step, ou
/// step différent.
///
/// # Errors
///
/// [`LymailleError::Store`] (`MRD-LYMAILLE-009`) : panne de la base, texte expurgé (règle 5 de
/// `./mod.sdd`).
pub async fn replay(
    db: &DatabaseConnection,
    scope: &WriteScope,
    op_key: &str,
) -> Result<Option<serde_json::Value>, LymailleError> {
    let mut select = Query::select();
    select
        .column(Alias::new("result"))
        .from(Alias::new(TABLE))
        .and_where(Expr::col(Alias::new("run_key")).eq(scope.run_key.as_str()))
        .and_where(Expr::col(Alias::new("step_key")).eq(scope.step_key.as_str()))
        .and_where(Expr::col(Alias::new("op_key")).eq(op_key));
    db.query_one(&select)
        .await
        .map_err(|err| LymailleError::Store(db_text(&err)))?
        .map(|row| row.try_get::<serde_json::Value>("", "result"))
        .transpose()
        .map_err(|err| LymailleError::Store(db_text(&err)))
}

/// Journalise le `result` sous `(scope, op_key)` (`created_at` posé à l'heure UTC courante,
/// règle 6 de `./mod.sdd`). Une ligne existe déjà — concurrence ou rejeu après `replay` :
/// **succès**, le premier gagne (`./write_journal.sdd`) ; l'`INSERT` passe par
/// `ON CONFLICT DO NOTHING`, la violation n'atteint jamais le chemin d'erreur.
///
/// # Errors
///
/// - [`LymailleError::Store`] (`MRD-LYMAILLE-009`) : panne de la base, texte expurgé (règle 5 de
///   `./mod.sdd`), ou `result` dont la forme JSON dépasse 64 Ko (`Must not` de `./write_journal.sdd`).
pub async fn record(
    db: &DatabaseConnection,
    scope: &WriteScope,
    op_key: &str,
    result: &serde_json::Value,
) -> Result<(), LymailleError> {
    let text = serde_json::to_string(result)
        .map_err(|err| LymailleError::Store(db_text(&DbErr::Custom(err.to_string()))))?;
    if text.len() > MAX_RESULT_BYTES {
        return Err(LymailleError::Store(
            "journalized result exceeds 64 KiB".to_owned(),
        ));
    }
    let statement = Query::insert()
        .into_table(Alias::new(TABLE))
        .columns(["run_key", "step_key", "op_key", "result", "created_at"])
        .values(vec![
            Expr::val(scope.run_key.as_str()),
            Expr::val(scope.step_key.as_str()),
            Expr::val(op_key),
            Expr::val(result.clone()),
            Expr::val(chrono::Utc::now()),
        ])
        .map_err(|err| LymailleError::Store(db_text(&DbErr::Custom(err.to_string()))))?
        .on_conflict(
            OnConflict::columns([
                Alias::new("run_key"),
                Alias::new("step_key"),
                Alias::new("op_key"),
            ])
            .do_nothing()
            .to_owned(),
        )
        .to_owned();
    db.execute(&statement)
        .await
        .map_err(|err| LymailleError::Store(db_text(&err)))?;
    Ok(())
}

/// Supprime les entrées créées avant `cutoff` (appelée par `../purge.sdd`) ; rend le nombre de
/// lignes supprimées.
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
        .and_where(Expr::col(Alias::new("created_at")).lt(cutoff))
        .to_owned();
    let result = db
        .execute(&statement)
        .await
        .map_err(|err| LymailleError::Store(db_text(&err)))?;
    Ok(result.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::Op;
    use super::WriteScope;
    use super::op_key;
    use super::purge_before;
    use super::record;
    use super::replay;
    use crate::lymaille::store::test_support::test_db;
    use sea_orm::ConnectionTrait as _;
    use sea_orm::DatabaseConnection;
    use sea_orm::sea_query::Alias;
    use sea_orm::sea_query::Expr;
    use sea_orm::sea_query::ExprTrait as _;
    use sea_orm::sea_query::Query;
    use serde_json::json;

    /// Table du journal, nom complet posé par `m20261104_000001_lymaille_lot2`.
    const TABLE: &str = "miryad_lymaille_write_journal";

    fn scope(run: &str, step: &str) -> WriteScope {
        WriteScope {
            run_key: run.to_string(),
            step_key: step.to_string(),
        }
    }

    /// Nombre de lignes du journal pour une paire `(run_key, step_key)`.
    async fn row_count(db: &DatabaseConnection, run: &str, step: &str) -> usize {
        let mut select = Query::select();
        select
            .column(Alias::new("op_key"))
            .from(Alias::new(TABLE))
            .and_where(Expr::col(Alias::new("run_key")).eq(run))
            .and_where(Expr::col(Alias::new("step_key")).eq(step));
        db.query_all(&select)
            .await
            .expect("comptage des lignes du journal")
            .len()
    }

    /// `Scenario` « la clé est dérivée du contenu, pas de l'ordre » : deux corps égaux à
    /// l'ordre des clés près donnent la même clé, un champ différent en donne une autre.
    /// Pur — aucune base, aucun horodatage, aucun compteur.
    #[test]
    fn the_key_is_derived_from_content_not_order() {
        let first = json!({"b": 2, "a": {"y": 1, "x": "texte"}, "n": [3, {"q": true, "p": null}]});
        let same_other_order = json!({"a": {"x": "texte", "y": 1}, "n": [3, {"p": null, "q": true}], "b": 2});
        assert_eq!(
            op_key(Op::Create, "widget", Some(7), &first),
            op_key(Op::Create, "widget", Some(7), &same_other_order),
            "deux corps égaux à l'ordre des clés près ont la même clé"
        );

        let changed_field = json!({"a": {"x": "texte", "y": 2}, "b": 2});
        assert_ne!(
            op_key(Op::Create, "widget", Some(7), &first),
            op_key(Op::Create, "widget", Some(7), &changed_field),
            "un corps avec un champ différent a une clé différente"
        );
    }

    /// `Scenario` « un rejeu rend le résultat journalisé » : `replay` avec la même clé rend le
    /// résultat, avec un autre step rend `None`.
    #[tokio::test]
    async fn a_replay_returns_the_journaled_result() {
        let db = test_db().await;
        let first = scope("run-1", "step-1");
        let result = json!({"id": 3, "label": "x"});
        let key = op_key(Op::Update, "widget", Some(3), &json!({"label": "x"}));
        record(&db, &first, &key, &result)
            .await
            .expect("le premier record réussit");
        assert_eq!(
            replay(&db, &first, &key)
                .await
                .expect("le replay de la même clé réussit"),
            Some(result),
            "le résultat journalisé est rendu tel quel"
        );

        let other_step = scope("run-1", "step-2");
        assert!(
            matches!(replay(&db, &other_step, &key).await, Ok(None)),
            "un autre step ne voit pas la ligne : {:?}",
            replay(&db, &other_step, &key).await
        );
    }

    /// `Scenario` « l'enregistrement concurrent est un succès » : deux `record` simultanés de la
    /// même clé rendent tous deux `Ok` et une seule ligne existe (le premier gagne).
    #[tokio::test]
    async fn concurrent_records_are_both_a_success_with_one_row() {
        let db = test_db().await;
        let scope = scope("run-c", "step-c");
        let key = op_key(Op::Delete, "widget", Some(9), &json!(null));
        let result = json!({"deleted": true});
        let left = record(&db, &scope, &key, &result);
        let right = record(&db, &scope, &key, &result);
        let (first, second) = tokio::join!(left, right);
        first.expect("le premier record rend Ok");
        second.expect("le second record rend Ok malgré la violation d'unicité");
        assert_eq!(
            row_count(&db, "run-c", "step-c").await,
            1,
            "une seule ligne existe, celle du premier"
        );
        assert_eq!(
            replay(&db, &scope, &key).await.expect("le replay réussit"),
            Some(result),
            "le résultat rendu est celui journalisé par le premier"
        );
    }

    /// `Raises` de `./write_journal.sdd` (sémantique du « borné à 64 Ko », actée 2026-11-04) : un
    /// résultat sérialisé dépassant 64 Ko est **refusé, jamais tronqué** — `Store`
    /// (`MRD-LYMAILLE-009`) et aucune ligne écrite, car du JSON tronqué ne se relirait pas.
    #[tokio::test]
    async fn an_oversized_result_is_refused_never_truncated() {
        let db = test_db().await;
        let scope = scope("run-big", "step-big");
        let key = op_key(Op::Create, "widget", Some(4), &json!({"v": "gros"}));
        let oversized = json!({"blob": "x".repeat(super::MAX_RESULT_BYTES + 1)});

        let refused = record(&db, &scope, &key, &oversized)
            .await
            .expect_err("un résultat au-dessus de 64 Ko est refusé, jamais tronqué");
        assert!(
            refused.to_string().contains("MRD-LYMAILLE-009"),
            "`Store` avec son code : {refused}"
        );
        assert_eq!(
            row_count(&db, "run-big", "step-big").await,
            0,
            "aucune ligne n'est écrite"
        );
        assert!(
            matches!(replay(&db, &scope, &key).await, Ok(None)),
            "le replay ne voit rien : rien n'a été journalisé"
        );
    }

    /// Fixture : une ligne ancienne (2020) posée par `INSERT` brut, `record` posant
    /// `created_at` à l'heure courante (règle 6 de `./mod.sdd`).
    async fn insert_old_row(db: &DatabaseConnection, run: &str, step: &str, key: &str) {
        db.execute_unprepared(&format!(
            "INSERT INTO {TABLE} (run_key, step_key, op_key, result, created_at) \
             VALUES ('{run}', '{step}', '{key}', '{{\"v\": 1}}', '2020-01-01T00:00:00Z')"
        ))
        .await
        .expect("la fixture ancienne est insérée");
    }

    /// `Scenario` « `purge_before` » : seules les entrées créées avant le `cutoff` disparaissent ;
    /// le nombre rendu est celui des lignes supprimées.
    #[tokio::test]
    async fn purge_before_removes_only_the_old_entries() {
        let db = test_db().await;
        let scope = scope("run-purge", "step-purge");
        let old_key = op_key(Op::Create, "widget", Some(1), &json!({"v": "ancienne"}));
        insert_old_row(&db, "run-purge", "step-purge", &old_key).await;
        let cutoff = chrono::Utc::now();
        let recent_key = op_key(Op::Create, "widget", Some(2), &json!({"v": "récente"}));
        record(&db, &scope, &recent_key, &json!({"id": 2}))
            .await
            .expect("la ligne récente est posée par record");

        let deleted = purge_before(&db, cutoff).await.expect("purge_before réussit");
        assert_eq!(deleted, 1, "une seule ligne ancienne est supprimée");
        assert!(
            matches!(replay(&db, &scope, &old_key).await, Ok(None)),
            "l'ancienne ligne a disparu"
        );
        assert_eq!(
            row_count(&db, "run-purge", "step-purge").await,
            1,
            "la ligne récente est conservée"
        );

        let again = purge_before(&db, cutoff)
            .await
            .expect("purge_before est rappelable");
        assert_eq!(again, 0, "sans ligne ancienne, purge_before rend 0");
    }
}
