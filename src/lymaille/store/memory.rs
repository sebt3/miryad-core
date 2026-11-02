//! `SeaMemoryStore` — persistance des Memories de `lymaille` sur
//! `miryad_lymaille_memories` (`src/lymaille/store/memory.sdd`).
//!
//! La clé d'une Memory est le `subject` du store, la portée (`MemoryScope` : niveau et
//! référence) puis l'identifiant : un `subject` ne lit ni n'écrase les Memory d'un autre
//! (DSD `DEC-7`, `AC-7`). Table interne, sans CRUD.

use sea_orm::ConnectionTrait;
use sea_orm::DatabaseConnection;
use sea_orm::DbErr;
use sea_orm::sea_query::Alias;
use sea_orm::sea_query::Expr;
use sea_orm::sea_query::ExprTrait;
use sea_orm::sea_query::OnConflict;
use sea_orm::sea_query::Order;
use sea_orm::sea_query::Query;
use sea_orm::sea_query::SelectStatement;

use ::lymaille::Memory;
use ::lymaille::MemoryScope;
use ::lymaille::MemoryStore;
use ::lymaille::MemorySummary;
use ::lymaille::ModulesError;
use ::lymaille::ModulesResult;

use super::db_text;

/// Table des mémoires, nom complet (../migration.sdd).
const MEMORIES_TABLE: &str = "miryad_lymaille_memories";

/// Store des mémoires, clé par (`subject`, niveau, référence, identifiant) — créé par
/// `LymailleStores::memories()` (`./memory.sdd`).
#[derive(Clone)]
pub struct SeaMemoryStore {
    db: DatabaseConnection,
    subject: String,
}

impl SeaMemoryStore {
    pub(crate) fn new(db: DatabaseConnection, subject: String) -> Self {
        Self { db, subject }
    }

    /// Filtre de la portée : la clé `(subject, niveau, référence)` — jamais une portée d'un
    /// autre `subject` (`./memory.sdd`, DSD `AC-7`).
    fn scoped(&self, scope: &MemoryScope) -> SelectStatement {
        Query::select()
            .columns([Alias::new("memory")])
            .from(Alias::new(MEMORIES_TABLE))
            .and_where(Expr::col(Alias::new("subject")).eq(self.subject.as_str()))
            .and_where(Expr::col(Alias::new("level")).eq(scope.level().as_str()))
            .and_where(Expr::col(Alias::new("reference")).eq(scope.reference()))
            .to_owned()
    }
}

impl MemoryStore for SeaMemoryStore {
    async fn list(&self, scope: &MemoryScope) -> ModulesResult<Vec<MemorySummary>> {
        let mut select = self.scoped(scope);
        select.order_by(Alias::new("memory_id"), Order::Asc);
        let rows = self
            .db
            .query_all(&select)
            .await
            .map_err(|err| ModulesError::Store(db_text(&err)))?;
        rows.iter()
            .map(|row| {
                row.try_get::<serde_json::Value>("", "memory")
                    .map_err(|err| ModulesError::Store(db_text(&err)))
                    .and_then(decode)
                    .map(|memory| MemorySummary::from(&memory))
            })
            .collect()
    }

    async fn get(&self, scope: &MemoryScope, id: &str) -> ModulesResult<Option<Memory>> {
        let mut select = self.scoped(scope);
        select.and_where(Expr::col(Alias::new("memory_id")).eq(id));
        let row = self
            .db
            .query_one(&select)
            .await
            .map_err(|err| ModulesError::Store(db_text(&err)))?;
        row.map(|row| {
            row.try_get::<serde_json::Value>("", "memory")
                .map_err(|err| ModulesError::Store(db_text(&err)))
                .and_then(decode)
        })
        .transpose()
    }

    async fn put(&self, scope: &MemoryScope, memory: &Memory) -> ModulesResult<()> {
        let value = serde_json::to_value(memory)
            .map_err(|_cause| ModulesError::Store("memory could not be serialized".to_owned()))?;
        let statement = Query::insert()
            .into_table(Alias::new(MEMORIES_TABLE))
            .columns(["subject", "level", "reference", "memory_id", "memory"])
            .values(vec![
                Expr::val(self.subject.as_str()),
                // La colonne `level` porte le texte canonique amont de `MemoryLevel::as_str`
                // (`user`, `project` ou `session`) — l'énumération amont n'a pas de serde,
                // `as_str` est sa forme canonique (`./memory.sdd`).
                Expr::val(scope.level().as_str()),
                Expr::val(scope.reference()),
                Expr::val(memory.id()),
                Expr::val(value),
            ])
            .map_err(|err| ModulesError::Store(db_text(&DbErr::Custom(err.to_string()))))?
            .on_conflict(
                OnConflict::columns([
                    Alias::new("subject"),
                    Alias::new("level"),
                    Alias::new("reference"),
                    Alias::new("memory_id"),
                ])
                .update_columns([Alias::new("memory")])
                .to_owned(),
            )
            .to_owned();
        self.db
            .execute(&statement)
            .await
            .map(|_| ())
            .map_err(|err| ModulesError::Store(db_text(&err)))
    }

    async fn delete(&self, scope: &MemoryScope, id: &str) -> ModulesResult<bool> {
        let statement = Query::delete()
            .from_table(Alias::new(MEMORIES_TABLE))
            .and_where(Expr::col(Alias::new("subject")).eq(self.subject.as_str()))
            .and_where(Expr::col(Alias::new("level")).eq(scope.level().as_str()))
            .and_where(Expr::col(Alias::new("reference")).eq(scope.reference()))
            .and_where(Expr::col(Alias::new("memory_id")).eq(id))
            .to_owned();
        let result = self
            .db
            .execute(&statement)
            .await
            .map_err(|err| ModulesError::Store(db_text(&err)))?;
        Ok(result.rows_affected() > 0)
    }
}

/// Relecture d'une Memory depuis sa colonne JSON, sans jamais citer son corps (`./memory.sdd`
/// `Must not`).
fn decode(value: serde_json::Value) -> ModulesResult<Memory> {
    serde_json::from_value::<Memory>(value)
        .map_err(|_cause| ModulesError::Store("stored memory is unreadable".to_owned()))
}

#[cfg(test)]
mod tests {
    use super::SeaMemoryStore;
    use crate::lymaille::store::test_support::test_db;
    use ::lymaille::Memory;
    use ::lymaille::MemoryScope;
    use ::lymaille::MemoryStore as _;
    use ::lymaille::MemorySummary;
    use ::lymaille::SessionId;
    use sea_orm::ConnectionTrait as _;
    use sea_orm::sea_query::Alias;
    use sea_orm::sea_query::Expr;
    use sea_orm::sea_query::ExprTrait as _;
    use sea_orm::sea_query::Query;

    fn memory(id: &str, title: &str, body: &str) -> Memory {
        Memory::new(id, title, body).expect("une Memory valide")
    }

    /// `Scenario` « put, get, list, delete » : `list` trié par identifiant croissant sans
    /// corps, `get` rend la Memory, `delete` rend `true` puis `false`.
    #[tokio::test]
    async fn put_get_list_and_delete() {
        let store = SeaMemoryStore::new(test_db().await, "alice".to_string());
        let scope = MemoryScope::project("alpha");
        for id in ["c", "a", "b"] {
            store
                .put(
                    &scope,
                    &memory(id, &format!("titre {id}"), &format!("corps {id}")),
                )
                .await
                .expect("l'écriture de la Memory réussit");
        }

        let summaries = store.list(&scope).await.expect("le listing réussit");
        let ids: Vec<&str> = summaries.iter().map(MemorySummary::id).collect();
        assert_eq!(ids, vec!["a", "b", "c"], "triés par identifiant croissant");
        let text = serde_json::to_string(&summaries).expect("les résumés se sérialisent");
        assert!(
            !text.contains("corps"),
            "un résumé ne porte jamais le corps : {text}"
        );
        assert_eq!(
            summaries.first().map(MemorySummary::title),
            Some("titre a"),
            "le résumé porte le titre"
        );

        assert_eq!(
            store
                .get(&scope, "b")
                .await
                .expect("la lecture réussit")
                .expect("la Memory existe"),
            memory("b", "titre b", "corps b"),
            "la Memory relue égale l'originale"
        );
        assert!(
            store.delete(&scope, "b").await.expect("premier delete"),
            "le premier delete rend true"
        );
        assert!(
            !store.delete(&scope, "b").await.expect("second delete"),
            "le second delete rend false"
        );
    }

    /// Texte canonique de `MemoryLevel::as_str` dans la colonne `level` : `user`, `project` ou
    /// `session`, exercé aux trois niveaux. L'énumération amont n'a pas de serde — `as_str` est
    /// sa forme canonique, pas une forme sérialisée (`./memory.sdd`).
    #[tokio::test]
    async fn le_texte_stocke_d_un_niveau_est_la_forme_canonique() {
        let db = test_db().await;
        let store = SeaMemoryStore::new(db.clone(), "alice".to_string());
        let session =
            SessionId::new("session-canonique").expect("un texte non vide fait un identifiant de session");
        let scopes = [
            (MemoryScope::user("ref-utilisateur"), "user"),
            (MemoryScope::project("ref-projet"), "project"),
            (MemoryScope::session(&session), "session"),
        ];
        for (scope, expected) in scopes {
            assert_eq!(
                scope.level().as_str(),
                expected,
                "le texte canonique amont de `MemoryLevel::as_str`"
            );
            store
                .put(&scope, &memory("m", "titre", "corps"))
                .await
                .expect("l'écriture de la Memory réussit");
            let select = Query::select()
                .column(Alias::new("level"))
                .from(Alias::new(super::MEMORIES_TABLE))
                .and_where(Expr::col(Alias::new("subject")).eq("alice"))
                .and_where(Expr::col(Alias::new("reference")).eq(scope.reference()))
                .to_owned();
            let row = db
                .query_one(&select)
                .await
                .expect("la ligne relue")
                .expect("la ligne est écrite");
            assert_eq!(
                row.try_get::<String>("", "level").expect("la colonne `level`"),
                expected,
                "la colonne `level` porte le texte canonique"
            );
        }
    }

    /// `Scenario` « deux sujets ne partagent pas une portée » (DSD `AC-7`).
    #[tokio::test]
    async fn two_subjects_do_not_share_a_scope() {
        let db = test_db().await;
        let alice = SeaMemoryStore::new(db.clone(), "alice".to_string());
        let bob = SeaMemoryStore::new(db.clone(), "bob".to_string());
        let scope = MemoryScope::project("alpha");
        alice
            .put(&scope, &memory("m", "la sienne", "a"))
            .await
            .expect("alice écrit sa Memory");
        bob.put(&scope, &memory("m", "l'autre", "b"))
            .await
            .expect("bob écrit la sienne");

        for (store, expected) in [(&alice, "la sienne"), (&bob, "l'autre")] {
            assert_eq!(
                store
                    .get(&scope, "m")
                    .await
                    .expect("lecture")
                    .expect("la Memory existe")
                    .title(),
                expected,
                "chacun ne lit que la sienne"
            );
            let summaries = store.list(&scope).await.expect("listing");
            assert_eq!(summaries.len(), 1, "chacun ne liste que la sienne");
            assert_eq!(summaries.first().map(MemorySummary::title), Some(expected));
        }
    }
}
