//! `PgTurnLock` — la porte de tour **partagée entre processus** (`src/lymaille/turn_lock.sdd`,
//! DSD `FR-11`, `DEC-31`, issue #81 lot 1). `lymaille` 0.3.0 rend la porte de tour remplaçable
//! derrière le trait `TurnLock` et ne livre que `TurnGate`, par processus ; ce fichier est
//! l'implémentation sur verrou consultatif **transactionnel** PostgreSQL (`pg_try_advisory_xact_lock`),
//! tenu pendant tout le tour sur une connexion de la base de l'application.
//!
//! La perte de cette connexion (réseau, redémarrage de la base, session tuée côté serveur) est
//! la perte du bail : le jeton `lost` du permis est annulé et `lymaille` abandonne le tour sans
//! sauvegarder, en émettant `TurnLeaseLost`. Aucune ligne de table n'est écrite, aucune
//! migration : le verrou est lié à la transaction, donc tout arrêt du processus, de la connexion
//! ou de la transaction le libère côté PostgreSQL.
//!
//! **Taille du pool** : un tour retient **une** connexion du pool pendant toute sa durée (la
//! transaction du verrou, tenue par une tâche de battement). Le pool de l'application doit
//! compter, en plus de ses besoins habituels, le nombre de tours simultanés attendus par
//! réplique. Sans PostgreSQL, `pg_turn_lock` rend `None` et le gestionnaire garde `TurnGate`
//! seul (comportement antérieur, inchangé).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use sea_orm::ConnectionTrait;
use sea_orm::DatabaseBackend;
use sea_orm::DatabaseConnection;
use sea_orm::DbErr;
use sea_orm::Statement;
use sea_orm::TransactionTrait;
use sea_orm::Value;
use sha2::Digest;
use sha2::Sha256;

use ::lymaille::CancelHandle;
use ::lymaille::SessionError;
use ::lymaille::SessionId;
use ::lymaille::SessionResult;
use ::lymaille::TurnLock;
use ::lymaille::TurnPermit;

/// Borne inférieure du battement : sous 1 s, le `SELECT 1` dominerait le trafic utile.
const HEARTBEAT_MIN: Duration = Duration::from_secs(1);
/// Borne supérieure du battement : au-delà de 60 s, la perte de bail serait détectée trop tard.
const HEARTBEAT_MAX: Duration = Duration::from_secs(60);
/// Préfixe domain-separated de la clé : aucune autre famille de verrous consultatifs de
/// l'application ne peut entrer en collision avec les tours.
const KEY_PREFIX: &[u8] = b"miryad_lymaille_turn:";
/// Requête de prise : verrou consultatif **transactionnel** — libéré par le commit, le rollback,
/// la coupure de connexion ou l'arrêt du processus, jamais par une simple fin de fonction.
const TRY_LOCK_SQL: &str = "SELECT pg_try_advisory_xact_lock($1) AS verrou";
/// Requête de battement : maintient la transaction vivante et révèle la perte de connexion.
const HEARTBEAT_SQL: &str = "SELECT 1";

/// Clé du verrou : les huit premiers octets (big-endian) du SHA-256 du préfixe suivi de
/// l'identifiant de session. Déterministe par construction (`Scenario` « la clé de verrou est
/// déterministe et propre à la session »). Deux sessions peuvent en théorie partager une clé
/// (collision 64 bits) : le seul effet est un `TurnActive` à tort, jamais deux tours
/// simultanés — la spec l'assume (`DEC-31`).
fn turn_key(session: &SessionId) -> i64 {
    let mut digest = Sha256::new();
    digest.update(KEY_PREFIX);
    digest.update(session.as_str());
    let hash = digest.finalize();
    // Le condensé SHA-256 fait 32 octets : la conversion des 8 premiers est infaillible ;
    // `unwrap_or` évite tout panic dans le chemin non testable (harnais : pas d'`unwrap`).
    let octets: [u8; 32] = hash.into();
    let (head, _tail) = octets.split_at(8);
    let cle: [u8; 8] = head.try_into().unwrap_or([0; 8]);
    i64::from_be_bytes(cle)
}

/// Erreur de porte : le message de la base, **amputé** de l'identifiant de session et de la
/// valeur de clé s'il les portait (`Scenario` « une panne de la base est une erreur de porte »).
fn turn_lock_error(error: &DbErr, session: &str, key: i64) -> SessionError {
    let texte = error.to_string();
    SessionError::TurnLock(texte.replace(session, "").replace(&key.to_string(), ""))
}

/// Porte de tour partagée sur verrou consultatif PostgreSQL (`turn_lock.sdd`) — `Clone` : tous
/// les clones partagent le pool de la même base. Construire par [`pg_turn_lock`] quand le
/// backend est PostgreSQL ; la fonction est la seule porte de sélection des backends (un tour
/// retient une connexion du pool pendant toute sa durée).
#[derive(Clone)]
pub struct PgTurnLock {
    db: DatabaseConnection,
    heartbeat: Duration,
}

impl PgTurnLock {
    /// Porte sur la base `db`, battement `heartbeat` **ramené** dans
    /// [`HEARTBEAT_MIN`]..=[`HEARTBEAT_MAX`] (hors bornes, la borne la plus proche — jamais
    /// d'erreur).
    #[must_use]
    pub fn new(db: DatabaseConnection, heartbeat: Duration) -> Self {
        Self {
            db,
            heartbeat: heartbeat.clamp(HEARTBEAT_MIN, HEARTBEAT_MAX),
        }
    }
}

impl TurnLock for PgTurnLock {
    /// Verrou consultatif transactionnel : prise immédiate (`pg_try_advisory_xact_lock`),
    /// battement borné détenu par une tâche, relâchement par commit. Ne bloque jamais sur un
    /// tour actif.
    fn try_begin(
        &self,
        session: SessionId,
    ) -> Pin<Box<dyn Future<Output = SessionResult<TurnPermit>> + Send>> {
        let db = self.db.clone();
        let heartbeat = self.heartbeat;
        let key = turn_key(&session);
        let sujet = session.as_str().to_owned();
        Box::pin(async move {
            let txn = match db.begin().await {
                Ok(txn) => txn,
                Err(error) => {
                    tracing::debug!("lymaille turn lock: open failed");
                    return Err(turn_lock_error(&error, &sujet, key));
                }
            };
            let backend = txn.get_database_backend();
            let prise = Statement::from_sql_and_values(backend, TRY_LOCK_SQL, [Value::from(key)]);
            let acquired = match txn.query_one_raw(prise).await {
                Ok(Some(row)) => match row.try_get::<bool>("", "verrou") {
                    Ok(acquired) => acquired,
                    Err(error) => {
                        let _repli = txn.rollback().await;
                        tracing::debug!("lymaille turn lock: read failed");
                        return Err(SessionError::TurnLock(error.to_string()));
                    }
                },
                Ok(None) => {
                    let _repli = txn.rollback().await;
                    tracing::debug!("lymaille turn lock: read failed");
                    return Err(SessionError::TurnLock(
                        "the lock query returned no row".to_owned(),
                    ));
                }
                Err(error) => {
                    let _repli = txn.rollback().await;
                    tracing::debug!("lymaille turn lock: query failed");
                    return Err(turn_lock_error(&error, &sujet, key));
                }
            };
            if !acquired {
                // Refus immédiat : la transaction qui n'a rien verrouillé est annulée.
                let _repli = txn.rollback().await;
                tracing::debug!("lymaille turn lock: busy");
                return Err(SessionError::TurnActive(session));
            }
            tracing::debug!("lymaille turn lock: acquired");
            // La transaction est **détenue par une tâche** (jamais par le permis) : battement
            // toutes les `heartbeat`, la première erreur annule le bail et replie ; le signal
            // de relâchement (ou un canal fermé sans signal) valide la transaction.
            let lost = CancelHandle::new();
            let (tx, mut rx) = tokio::sync::oneshot::channel::<()>();
            let battement = Statement::from_string(backend, HEARTBEAT_SQL);
            let perdant = lost.clone();
            tokio::spawn(async move {
                let mut txn = Some(txn);
                loop {
                    tokio::select! {
                        biased;
                        _ = &mut rx => {
                            if let Some(txn) = txn.take() {
                                match txn.commit().await {
                                    Ok(()) => tracing::debug!("lymaille turn lock: released"),
                                    Err(_error) => tracing::debug!("lymaille turn lock: commit failed"),
                                }
                            }
                            break;
                        }
                        () = tokio::time::sleep(heartbeat) => {
                            let Some(t) = txn.as_mut() else { break };
                            if t.execute_raw(battement.clone()).await.is_err() {
                                // Perte du bail : le jeton est annulé avant que la transaction
                                // ne soit repliée (le verrou meurt avec elle).
                                perdant.cancel();
                                tracing::debug!("lymaille turn lock: lost");
                                if let Some(txn) = txn.take() {
                                    let _repli = txn.rollback().await;
                                }
                                break;
                            }
                        }
                    }
                }
            });
            let release = move || {
                // Le `Drop` du permis ne fait qu'avertir la tâche : jamais de blocage, jamais
                // d'échec visible ; un canal fermé sans signal vaut relâchement côté tâche.
                let _signal = tx.send(());
            };
            Ok(TurnPermit::new(session, release, lost))
        })
    }
}

/// Porte de sélection : `Some` **seulement** si le backend de `db` est PostgreSQL ; `None`
/// sinon (SQLite, MySQL : l'appelant garde la porte locale `TurnGate`).
#[must_use]
pub fn pg_turn_lock(db: &DatabaseConnection, heartbeat: Duration) -> Option<Arc<dyn TurnLock>> {
    (db.get_database_backend() == DatabaseBackend::Postgres)
        .then(|| Arc::new(PgTurnLock::new(db.clone(), heartbeat)) as Arc<dyn TurnLock>)
}

#[cfg(test)]
mod tests {
    use super::*;

    use sea_orm::ConnectOptions;
    use sea_orm::Database;

    /// Scenario « sans PostgreSQL, aucune porte partagée » : sur SQLite mémoire, `pg_turn_lock`
    /// rend `None` (SQLite forcé ici, même quand `MIRYAD_TEST_PG_URL` est posée par ailleurs).
    #[tokio::test]
    async fn sans_postgres_aucune_porte_partagee() {
        let db = Database::connect("sqlite::memory:")
            .await
            .expect("base SQLite mémoire de test");
        assert!(
            pg_turn_lock(&db, Duration::from_secs(5)).is_none(),
            "aucune porte partagée sur SQLite"
        );
    }

    /// Scenario « la clé de verrou est déterministe et propre à la session » : deux calculs
    /// pour `a` égaux, clé de `b` différente.
    #[test]
    fn la_cle_de_verrou_est_deterministe_et_distincte() {
        let a = SessionId::new("session-a").expect("identifiant de test");
        let b = SessionId::new("session-b").expect("identifiant de test");
        assert_eq!(turn_key(&a), turn_key(&a), "deux calculs pour a sont égaux");
        assert_ne!(turn_key(&a), turn_key(&b), "la clé de b est différente");
    }

    /// Scenario « le heartbeat est borné » : 0 s → 1 s, 5 s → 5 s, 10 min → 60 s.
    #[tokio::test]
    async fn le_heartbeat_est_borne() {
        let db = Database::connect("sqlite::memory:")
            .await
            .expect("base SQLite mémoire de test");
        let cases: [(Duration, Duration); 3] = [
            (Duration::from_secs(0), HEARTBEAT_MIN),
            (Duration::from_secs(5), Duration::from_secs(5)),
            (Duration::from_secs(600), HEARTBEAT_MAX),
        ];
        for (donne, attendue) in cases {
            let porte = PgTurnLock::new(db.clone(), donne);
            assert_eq!(
                porte.heartbeat, attendue,
                "heartbeat {donne:?} doit être ramené à {attendue:?}"
            );
        }
    }

    // ── Scénarios contre PostgreSQL réel (`#[ignore]`, convention du banc Matrix : variable
    // `MIRYAD_TEST_PG_URL`) ──────────────────────────────────────────────────

    /// Variable d'environnement portant l'URL PostgreSQL de test (jamais affichée).
    const PG_URL_VAR: &str = "MIRYAD_TEST_PG_URL";

    fn pg_url() -> Option<String> {
        std::env::var(PG_URL_VAR).ok().filter(|v| !v.is_empty())
    }

    /// Connexion dédiée (pool borné) : deux `PgTurnLock` bâtis ainsi portent bien sur deux
    /// connexions distinctes. L'URL n'est jamais affichée : le message d'échec la remplace.
    async fn pg_db(url: &str) -> DatabaseConnection {
        let mut opts = ConnectOptions::new(url);
        opts.max_connections(2).min_connections(1);
        Database::connect(opts).await.unwrap_or_else(|err| {
            panic!(
                "connexion PostgreSQL impossible : {}",
                scrub_url(&err.to_string(), url)
            )
        })
    }

    fn scrub_url(text: &str, url: &str) -> String {
        text.replace(url, "<url>")
    }

    /// Scenario « un second tour sur la même session est refusé, même depuis une autre
    /// connexion » : `#[ignore]` faute de base ; rejoué contre PostgreSQL réel.
    #[tokio::test]
    #[ignore = "exige PostgreSQL (MIRYAD_TEST_PG_URL)"]
    async fn un_second_tour_est_refuse_depuis_une_autre_connexion() {
        let Some(url) = pg_url() else { return };
        let un = PgTurnLock::new(pg_db(&url).await, Duration::from_secs(5));
        let deux = PgTurnLock::new(pg_db(&url).await, Duration::from_secs(5));
        let s1 = session_unique("s1");
        let s2 = session_unique("s2");
        let _permit = un.try_begin(s1.clone()).await.expect("premier permis");
        let refus = deux
            .try_begin(s1.clone())
            .await
            .expect_err("la même session est prise depuis lautre connexion");
        assert!(
            matches!(refus, SessionError::TurnActive(_)),
            "le refus est un conflit de tour actif : {refus:?}"
        );
        drop(un.try_begin(s2.clone()).await.expect("s2 libre sur le premier"));
        drop(deux.try_begin(s2).await.expect("s2 libre sur le second"));
    }

    /// Scenario « relâcher le permis libère le verrou » : après `drop`, le second verrou
    /// réussit en ≤ 1 s et `lost` du premier na pas été annulé.
    #[tokio::test]
    #[ignore = "exige PostgreSQL (MIRYAD_TEST_PG_URL)"]
    async fn relacher_le_permis_libere_le_verrou() {
        let Some(url) = pg_url() else { return };
        let un = PgTurnLock::new(pg_db(&url).await, Duration::from_secs(5));
        let deux = PgTurnLock::new(pg_db(&url).await, Duration::from_secs(5));
        let s1 = session_unique("s1");
        let premier = un.try_begin(s1.clone()).await.expect("premier permis");
        let perdu = premier.lost();
        assert!(deux.try_begin(s1.clone()).await.is_err(), "verrou tenu");
        drop(premier);
        let delai = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                match deux.try_begin(s1.clone()).await {
                    Ok(permit) => return permit,
                    Err(SessionError::TurnActive(_)) => tokio::time::sleep(Duration::from_millis(20)).await,
                    Err(other) => panic!("le verrou a echoue : {other:?}"),
                }
            }
        })
        .await
        .expect("le verrou sest libere en une seconde");
        drop(delai);
        assert!(!perdu.is_cancelled(), "un permis relâché nannule jamais le bail");
    }

    /// Scenario « la perte de la connexion annule le bail » : la session PostgreSQL du tour est
    /// tuée côté serveur (`pg_terminate_backend` sur le backend titulaire du verrou) ; le
    /// battement suivant échoue, `lost` est annulé dans `heartbeat` + 1 s et un autre
    /// `PgTurnLock` peut prendre la session.
    #[tokio::test]
    #[ignore = "exige PostgreSQL (MIRYAD_TEST_PG_URL)"]
    async fn la_perte_de_la_connexion_annule_le_bail() {
        let Some(url) = pg_url() else { return };
        let tour = PgTurnLock::new(pg_db(&url).await, Duration::from_secs(1));
        let autre = PgTurnLock::new(pg_db(&url).await, Duration::from_secs(1));
        let s1 = session_unique("s1");
        let permit = tour.try_begin(s1.clone()).await.expect("permis du tour");
        let perdu = permit.lost();
        let cle = turn_key(&s1);
        // Le verrou bigint se splitpe dans `pg_locks` en (classid = poids fort 32 bits,
        // objid = poids faible 32 bits) ; le filtre ne tue donc que le backend titulaire.
        let bits = cle.cast_unsigned();
        let classid = bits >> 32;
        let objid = bits & 0xFFFF_FFFF;
        let tue = pg_db(&url).await;
        tue.execute_unprepared(&format!(
            "SELECT pg_terminate_backend(pid) FROM pg_locks WHERE locktype = 'advisory' AND classid::oid::bigint = {classid} AND objid::bigint = {objid}"
        ))
            .await
            .expect("tuer le backend titulaire du verrou");
        tokio::time::timeout(Duration::from_secs(3), async {
            while !perdu.is_cancelled() {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("lost annule dans heartbeat + 1 s");
        drop(permit);
        let _nouveau = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match autre.try_begin(s1.clone()).await {
                    Ok(permit) => return permit,
                    Err(SessionError::TurnActive(_)) => tokio::time::sleep(Duration::from_millis(50)).await,
                    Err(other) => panic!("porte en panne : {other:?}"),
                }
            }
        })
        .await
        .expect("un autre PgTurnLock peut reprendre la session");
    }

    /// Scenario « une panne de la base est une erreur de porte, pas un conflit » : base
    /// injoignable à l'ouverture (connexion fermée), `try_begin` rend `TurnLock` dont le texte
    /// ne contient pas l'identifiant de session.
    #[tokio::test]
    #[ignore = "exige PostgreSQL (MIRYAD_TEST_PG_URL)"]
    async fn une_panne_de_la_base_est_une_erreur_de_porte() {
        let Some(url) = pg_url() else { return };
        let db = pg_db(&url).await;
        let porte = PgTurnLock::new(db.clone(), Duration::from_secs(5));
        db.close().await.expect("connexion fermee (base injoignable)");
        let s1 = session_unique("s1");
        let erreur = porte
            .try_begin(s1.clone())
            .await
            .expect_err("la porte est en panne");
        match erreur {
            SessionError::TurnLock(texte) => {
                assert!(
                    !texte.contains(s1.as_str()),
                    "le texte ne doit pas porter lidentifiant de session"
                );
            }
            autre => panic!("une erreur de porte est attendue, obtenu : {autre:?}"),
        }
    }

    /// Identifiant de session unique par exécution : les scénarios `#[ignore]` tournent en
    /// parallèle sur la même base et ne doivent jamais partager une clé.
    fn session_unique(nom: &str) -> SessionId {
        SessionId::new(format!("{nom}-{:016x}", rand::random::<u64>())).expect("identifiant de test")
    }
}
