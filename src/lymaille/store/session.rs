//! `SeaSessionStore` — persistance des sessions de `lymaille` sur
//! `miryad_lymaille_sessions` (`src/lymaille/store/session.sdd`).
//!
//! L'état est stocké comme **texte JSON** produit par `SessionState::to_json` et relu par
//! `SessionState::from_json` : un état rechargé passe par le même format qu'un vrai stockage
//! (DSD `FR-3`). Clé `(subject, session_id)` : un `subject` ne lit ni n'écrase la session d'un
//! autre (DSD `DEC-7`, `AC-7`). Le dernier `save` l'emporte, sans jeton de version
//! (`session#DEC-1`).

use sea_orm::ConnectionTrait;
use sea_orm::DatabaseConnection;
use sea_orm::DbErr;
use sea_orm::sea_query::Alias;
use sea_orm::sea_query::Expr;
use sea_orm::sea_query::ExprTrait;
use sea_orm::sea_query::OnConflict;
use sea_orm::sea_query::Query;

use ::lymaille::SessionError;
use ::lymaille::SessionId;
use ::lymaille::SessionKind;
use ::lymaille::SessionResult;
use ::lymaille::SessionState;
use ::lymaille::SessionStore;

use super::db_text;

/// Table des sessions, nom complet (../migration.sdd).
const SESSIONS_TABLE: &str = "miryad_lymaille_sessions";

/// Nature d'un état en texte de colonne : `work`, `config` ou `sub` — la forme sérialisée en
/// minuscules de `SessionKind`, même dérivation que la colonne `kind` du JSON de l'état. Une
/// nature que serde ne rend pas en texte (natura amont inconnue d'ici) est un **refus explicite**
/// de l'écriture, jamais un repli silencieux sur `work` (`./session.sdd`).
fn kind_text(kind: SessionKind) -> SessionResult<String> {
    match serde_json::to_value(kind) {
        Ok(serde_json::Value::String(text)) => Ok(text),
        _ => Err(SessionError::Store(format!(
            "session kind {kind:?} has no stored text form"
        ))),
    }
}

/// Store des sessions, clé par `(subject, session_id)` — créé par
/// `LymailleStores::sessions()` (`./session.sdd`).
#[derive(Clone)]
pub struct SeaSessionStore {
    db: DatabaseConnection,
    subject: String,
}

impl SeaSessionStore {
    pub(crate) fn new(db: DatabaseConnection, subject: String) -> Self {
        Self { db, subject }
    }
}

impl SessionStore for SeaSessionStore {
    async fn load(&self, id: &SessionId) -> SessionResult<Option<SessionState>> {
        let mut select = Query::select();
        select
            .column(Alias::new("state"))
            .from(Alias::new(SESSIONS_TABLE))
            .and_where(Expr::col(Alias::new("subject")).eq(self.subject.as_str()))
            .and_where(Expr::col(Alias::new("session_id")).eq(id.as_str()));
        let text = self
            .db
            .query_one(&select)
            .await
            .map_err(|err| SessionError::Store(db_text(&err)))?
            .map(|row| row.try_get::<String>("", "state"))
            .transpose()
            .map_err(|err| SessionError::Store(db_text(&err)))?;
        // Un état présent est relu par le même format qu'un vrai stockage — jamais un état par
        // défaut (`./session.sdd`).
        text.map(|text| SessionState::from_json(&text)).transpose()
    }

    async fn save(&self, state: &SessionState) -> SessionResult<()> {
        let text = state.to_json()?;
        let kind = kind_text(state.kind())?;
        let statement = Query::insert()
            .into_table(Alias::new(SESSIONS_TABLE))
            .columns(["subject", "session_id", "kind", "state", "updated_at"])
            .values(vec![
                Expr::val(self.subject.as_str()),
                Expr::val(state.id().as_str()),
                Expr::val(kind),
                Expr::val(text),
                Expr::val(chrono::Utc::now()),
            ])
            .map_err(|err| SessionError::Store(db_text(&DbErr::Custom(err.to_string()))))?
            .on_conflict(
                OnConflict::columns([Alias::new("subject"), Alias::new("session_id")])
                    .update_columns([Alias::new("kind"), Alias::new("state"), Alias::new("updated_at")])
                    .to_owned(),
            )
            .to_owned();
        // Le dernier `save` l'emporte : aucun jeton de version (`session#DEC-1`).
        self.db
            .execute(&statement)
            .await
            .map_err(|err| SessionError::Store(db_text(&err)))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::SeaSessionStore;
    use crate::lymaille::store::test_support::test_db;
    use ::lymaille::SessionId;
    use ::lymaille::SessionKind;
    use ::lymaille::SessionState;
    use ::lymaille::SessionStore as _;
    use sea_orm::ConnectionTrait as _;
    use sea_orm::DatabaseConnection;
    use sea_orm::sea_query::Alias;
    use sea_orm::sea_query::Expr;
    use sea_orm::sea_query::ExprTrait;
    use sea_orm::sea_query::Query;
    use serde_json::json;

    fn session(text: &str) -> SessionId {
        SessionId::new(text).expect("un texte non vide fait un identifiant de session")
    }

    /// État avec historique, variable et fonction (`Scenario` « save puis load ») — relu depuis
    /// sa forme JSON, la variable `count` le distingue de son jumeau.
    fn fixture(id: &str, count: i64) -> SessionState {
        let document = json!({
            "id": id,
            "agent": { "name": "main-agent", "fingerprint": "sha-1" },
            "format": { "major": 1, "minor": 0 },
            "history": [{ "role": "user", "content": "hello" }],
            "variables": { "count": { "type": "int", "value": count } },
            "functions": { "scale": "fn scale(n) { n }" },
        });
        SessionState::from_json(&document.to_string()).expect("la fixture JSON se charge")
    }

    /// Nombre de lignes `miryad_lymaille_sessions` d'un sujet pour une session.
    async fn row_count(db: &DatabaseConnection, subject: &str, session_id: &str) -> usize {
        let mut select = Query::select();
        select
            .column(Alias::new("session_id"))
            .from(Alias::new("miryad_lymaille_sessions"))
            .and_where(Expr::col(Alias::new("subject")).eq(subject))
            .and_where(Expr::col(Alias::new("session_id")).eq(session_id));
        db.query_all(&select)
            .await
            .expect("comptage des lignes de session")
            .len()
    }

    /// `Scenario` « save puis load restitue l'état » — SQLite, et PostgreSQL si
    /// `MIRYAD_TEST_PG_URL` (`test_db` bascule). La colonne `kind` porte la nature de l'état.
    #[tokio::test]
    async fn save_then_load_restores_the_state() {
        let db = test_db().await;
        let store = SeaSessionStore::new(db.clone(), "alice".to_string());
        let original = fixture("s-1", 7).with_kind(SessionKind::Config);
        store.save(&original).await.expect("le save réussit");
        let loaded = store
            .load(&session("s-1"))
            .await
            .expect("le load de l'identifiant enregistré réussit")
            .expect("l'état enregistré se retrouve par son identifiant");
        assert_eq!(loaded, original, "l'état relu doit égaler l'état enregistré");

        let mut select = Query::select();
        select
            .column(Alias::new("kind"))
            .from(Alias::new("miryad_lymaille_sessions"))
            .and_where(Expr::col(Alias::new("subject")).eq("alice"))
            .and_where(Expr::col(Alias::new("session_id")).eq("s-1"));
        let row = db
            .query_one(&select)
            .await
            .expect("lecture de la colonne kind")
            .expect("la ligne existe");
        let kind: String = row.try_get("", "kind").expect("kind est un texte");
        assert_eq!(kind, "config", "`kind` est dérivé de la nature de l'état");
    }

    /// `Scenario` « le dernier save l'emporte » — deux save du même identifiant, une seule
    /// ligne, c'est le second état qui est relu.
    #[tokio::test]
    async fn the_last_save_wins() {
        let db = test_db().await;
        let store = SeaSessionStore::new(db.clone(), "alice".to_string());
        store.save(&fixture("s-1", 1)).await.expect("premier save");
        store.save(&fixture("s-1", 2)).await.expect("second save");
        let loaded = store
            .load(&session("s-1"))
            .await
            .expect("le load réussit")
            .expect("la session existe");
        assert_eq!(loaded, fixture("s-1", 2), "le dernier état enregistré est relu");
        assert_eq!(
            row_count(&db, "alice", "s-1").await,
            1,
            "le second save remplace la ligne, il n'en ajoute pas"
        );
    }

    /// `Scenario` « deux sujets ne partagent pas un identifiant » — `s-1` est la session de
    /// chacun (DSD `AC-7`).
    #[tokio::test]
    async fn two_subjects_do_not_share_a_session_identifier() {
        let db = test_db().await;
        let alice = SeaSessionStore::new(db.clone(), "alice".to_string());
        let bob = SeaSessionStore::new(db.clone(), "bob".to_string());
        alice.save(&fixture("s-1", 1)).await.expect("save d'alice");
        bob.save(&fixture("s-1", 2)).await.expect("save de bob");
        assert_eq!(
            alice
                .load(&session("s-1"))
                .await
                .expect("load d'alice")
                .expect("la session d'alice existe"),
            fixture("s-1", 1),
            "alice relit son état"
        );
        assert_eq!(
            bob.load(&session("s-1"))
                .await
                .expect("load de bob")
                .expect("la session de bob existe"),
            fixture("s-1", 2),
            "bob relit son état"
        );
    }

    /// `Scenario` « une session absente est None ».
    #[tokio::test]
    async fn an_absent_session_loads_as_none() {
        let store = SeaSessionStore::new(test_db().await, "alice".to_string());
        assert!(
            matches!(store.load(&session("inconnue")).await, Ok(None)),
            "un identifiant inconnu rend `Ok(None)` : {:?}",
            store.load(&session("inconnue")).await
        );
    }
}
