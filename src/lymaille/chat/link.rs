//! Annuaire mxid → principal, liaison par code à usage unique
//! (`src/lymaille/chat/link.sdd`).
//!
//! Un identifiant Matrix (`@alice:serveur`) ne vaut rien tant qu'il n'a pas été relié au
//! `subject` d'un utilisateur miryad par la procédure du code à usage unique (DSD `DEC-23`) :
//! code demandé authentifié sur l'API, envoyé au bot par `!link <code>`, consommé en base. Le
//! code n'est jamais stocké en clair (SHA-256), vit dix minutes, trois au plus par utilisateur,
//! et un mxid non lié n'a aucun droit.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::OnceLock;

use axum::Router;
use axum::extract::FromRef;
use axum::extract::Path;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::delete;
use axum::routing::post;
use chrono::DateTime;
use chrono::Utc;
use sea_orm::ConnectionTrait as _;
use sea_orm::DatabaseConnection;
use sea_orm::TransactionTrait as _;
use sea_orm::sea_query::Alias;
use sea_orm::sea_query::Expr;
use sea_orm::sea_query::ExprTrait as _;
use sea_orm::sea_query::OnConflict;
use sea_orm::sea_query::Order;
use sea_orm::sea_query::Query;
use sha2::Digest as _;
use sha2::Sha256;
use subtle::ConstantTimeEq as _;

use crate::auth::AuthPrincipal;
use crate::auth::MiryadAuthState;
use crate::lymaille::error::LymailleError;
use crate::lymaille::identity::CallerPrincipal;
use crate::lymaille::identity::Identity;
use crate::lymaille::store::db_text;
use crate::rest::error::RestError;

/// Table des liaisons `miryad_lymaille_matrix_links` (`../migration.sdd`, table toujours
/// présente sous la feature `lymaille`).
const LINKS_TABLE: &str = "miryad_lymaille_matrix_links";

/// Table des codes en attente `miryad_lymaille_matrix_link_codes` (`../migration.sdd`).
const CODES_TABLE: &str = "miryad_lymaille_matrix_link_codes";

/// Un code émis est consommable pendant dix minutes (`Must` de `./link.sdd`).
const CODE_TTL: i64 = 600;

/// Au plus trois codes vivants par utilisateur (`Must` de `./link.sdd`).
const MAX_LIVE_CODES: usize = 3;

/// Alphabet du code : 32 caractères sans `0`, `1`, `I` ni `O` ambigus, douze tirages = 60 bits
/// (`Must` de `./link.sdd`).
const CODE_ALPHABET: [u8; 32] = *b"23456789ABCDEFGHJKLMNPQRSTUVWXYZ";

/// Erreur fixe, indistincte entre code inconnu et code expiré (`Must not` de `./link.sdd`).
const INVALID_CODE: &str = "invalid or expired link code";

/// Cinq tentatives échouées par mxid sur dix minutes, puis refus (`Must` de `./link.sdd`).
const ATTEMPT_LIMIT: u32 = 5;

/// Fenêtre de comptage du débit, en secondes.
const ATTEMPT_WINDOW: i64 = 600;

/// Compteur de tentatives échouées par mxid, en mémoire du processus (`Must` de `./link.sdd`) :
/// `(nombre, instant d'ouverture de la fenêtre)`.
type Attempts = Mutex<HashMap<String, (u32, DateTime<Utc>)>>;

static ATTEMPTS: OnceLock<Attempts> = OnceLock::new();

fn attempts() -> &'static Attempts {
    ATTEMPTS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Un code à usage unique rendu en clair par `issue_code` : le seul moment où il existe hors de
/// la base (`./link.sdd`).
#[derive(Debug, Clone)]
pub struct IssuedCode {
    /// Le code en clair, jamais persisté en clair.
    pub code: String,
    /// Instant de consommation (émission + dix minutes).
    pub expires_at: DateTime<Utc>,
}

/// Haché SHA-256 hexadécimal en minuscules du code : la seule forme persistée
/// (`Must` de `./link.sdd`).
fn hash_code(code: &str) -> String {
    let digest = Sha256::digest(code.as_bytes());
    let mut text = String::with_capacity(digest.len().saturating_mul(2));
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(text, "{byte:02x}");
    }
    text
}

/// Tire douze caractères de l'alphabet sans ambiguïté (`Must` de `./link.sdd`) : source
/// `getrandom`, 5 bits par caractère, 60 bits au total.
fn random_code() -> Result<String, LymailleError> {
    let mut bytes = [0u8; 12];
    getrandom::fill(&mut bytes).map_err(|err| LymailleError::Store(err.to_string()))?;
    let mut code = String::with_capacity(12);
    for byte in bytes {
        // 256 est multiple de 32 : le modulo borne sans biais l'index dans l'alphabet.
        let index = usize::from(byte).checked_rem(CODE_ALPHABET.len()).unwrap_or(0);
        let letter = CODE_ALPHABET.get(index).copied().unwrap_or(b'2');
        code.push(char::from(letter));
    }
    Ok(code)
}

/// Vrai si le mxid a épuisé son quota de tentatives échouées sur la fenêtre glissante ; la
/// fenêtre courante est rafraîchie au passage.
fn rate_limited(mxid: &str, now: DateTime<Utc>) -> bool {
    let guard = attempts()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    guard.get(mxid).is_some_and(|(count, window_start)| {
        let fresh = now.signed_duration_since(*window_start).num_seconds() < ATTEMPT_WINDOW;
        fresh && *count >= ATTEMPT_LIMIT
    })
}

/// Enregistre une tentative échouée pour le mxid, fenêtre de dix minutes.
fn record_failure(mxid: &str, now: DateTime<Utc>) {
    let mut guard = attempts()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let entry = guard.entry(mxid.to_owned()).or_insert((0, now));
    let fresh = now.signed_duration_since(entry.1).num_seconds() < ATTEMPT_WINDOW;
    if fresh {
        entry.0 = entry.0.saturating_add(1);
    } else {
        *entry = (1, now);
    }
}

/// Oublie le compteur d'un mxid dont la tentative a réussi.
fn clear_attempts(mxid: &str) {
    let mut guard = attempts()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    guard.remove(mxid);
}

/// Émet un code d'appairage pour l'appelant (`./link.sdd`) : aléatoire, haché en base, dix
/// minutes, trois vivants au plus par utilisateur — le plus ancien cède la place. `purge_expired`
/// est appelée opportunément.
///
/// # Errors
///
/// [`LymailleError::Store`] : panne de la base ou source d'entropie indisponible.
pub async fn issue_code(
    db: &DatabaseConnection,
    caller: &CallerPrincipal,
    now: DateTime<Utc>,
) -> Result<IssuedCode, LymailleError> {
    purge_expired(db, now).await?;
    let mut select = Query::select();
    select
        .column(Alias::new("code_hash"))
        .from(Alias::new(CODES_TABLE))
        .and_where(Expr::col(Alias::new("subject")).eq(caller.subject.as_str()))
        .and_where(Expr::col(Alias::new("expires_at")).gt(now))
        .order_by(Alias::new("expires_at"), Order::Asc);
    let rows = db
        .query_all(&select)
        .await
        .map_err(|err| LymailleError::Store(db_text(&err)))?;
    let surplus = rows.len().saturating_sub(MAX_LIVE_CODES.saturating_sub(1));
    for row in rows.iter().take(surplus) {
        let hash: String = row
            .try_get("", "code_hash")
            .map_err(|err| LymailleError::Store(db_text(&err)))?;
        let mut purge = Query::delete();
        purge
            .from_table(Alias::new(CODES_TABLE))
            .and_where(Expr::col(Alias::new("code_hash")).eq(hash));
        db.execute(&purge)
            .await
            .map_err(|err| LymailleError::Store(db_text(&err)))?;
    }
    let expires_at = now
        .checked_add_signed(chrono::Duration::seconds(CODE_TTL))
        .unwrap_or(now);
    for _ in 0..5 {
        let code = random_code()?;
        let hash = hash_code(&code);
        let insert = Query::insert()
            .into_table(Alias::new(CODES_TABLE))
            .columns(["code_hash", "subject", "expires_at"])
            .values(vec![
                Expr::val(hash),
                Expr::val(caller.subject.as_str()),
                Expr::val(expires_at),
            ])
            .map_err(|err| LymailleError::Store(err.to_string()))?
            .on_conflict(
                OnConflict::column(Alias::new("code_hash"))
                    .do_nothing()
                    .to_owned(),
            )
            .to_owned();
        let done = db
            .execute(&insert)
            .await
            .map_err(|err| LymailleError::Store(db_text(&err)))?;
        if done.rows_affected() > 0 {
            return Ok(IssuedCode { code, expires_at });
        }
    }
    Err(LymailleError::Store("link code hash keeps colliding".to_owned()))
}

/// Consomme un code pour le `mxid` qui le présente et rend le `subject` apparié
/// (`./link.sdd`) : haché comparé en temps constant, suppression du code et insertion de la
/// liaison dans une même transaction. Un mxid déjà lié au même sujet est un succès idempotent ;
/// lié à un autre sujet, `Conflict` et le code n'est pas consommé.
///
/// # Errors
///
/// [`LymailleError::Forbidden`] (`invalid or expired link code`, `too many attempts`),
/// [`LymailleError::Conflict`] (`mxid already linked`), [`LymailleError::Store`].
pub async fn consume_code(
    db: &DatabaseConnection,
    mxid: &str,
    code: &str,
    now: DateTime<Utc>,
) -> Result<String, LymailleError> {
    if rate_limited(mxid, now) {
        return Err(LymailleError::Forbidden("too many attempts".to_owned()));
    }
    let target = hash_code(code);
    let mut select = Query::select();
    select
        .columns([
            Alias::new("code_hash"),
            Alias::new("subject"),
            Alias::new("expires_at"),
        ])
        .from(Alias::new(CODES_TABLE));
    let rows = db
        .query_all(&select)
        .await
        .map_err(|err| LymailleError::Store(db_text(&err)))?;
    let mut found: Option<(String, String, DateTime<Utc>)> = None;
    for row in rows {
        let hash: String = row
            .try_get("", "code_hash")
            .map_err(|err| LymailleError::Store(db_text(&err)))?;
        let subject: String = row
            .try_get("", "subject")
            .map_err(|err| LymailleError::Store(db_text(&err)))?;
        let expires_at: DateTime<Utc> = row
            .try_get("", "expires_at")
            .map_err(|err| LymailleError::Store(db_text(&err)))?;
        // La comparaison du haché est en temps constant (`Must` de `./link.sdd`).
        if bool::from(hash.as_bytes().ct_eq(target.as_bytes())) {
            found = Some((hash, subject, expires_at));
        }
    }
    let Some((hash, subject, expires_at)) = found else {
        record_failure(mxid, now);
        return Err(LymailleError::Forbidden(INVALID_CODE.to_owned()));
    };
    if expires_at <= now {
        record_failure(mxid, now);
        return Err(LymailleError::Forbidden(INVALID_CODE.to_owned()));
    }

    let tx = db
        .begin()
        .await
        .map_err(|err| LymailleError::Store(db_text(&err)))?;
    let mut owner_select = Query::select();
    owner_select
        .column(Alias::new("subject"))
        .from(Alias::new(LINKS_TABLE))
        .and_where(Expr::col(Alias::new("mxid")).eq(mxid));
    let owner: Option<String> = tx
        .query_one(&owner_select)
        .await
        .map_err(|err| LymailleError::Store(db_text(&err)))?
        .map(|row| row.try_get::<String>("", "subject"))
        .transpose()
        .map_err(|err| LymailleError::Store(db_text(&err)))?;
    if let Some(owner) = owner.as_deref()
        && !bool::from(owner.as_bytes().ct_eq(subject.as_bytes()))
    {
        // transaction abandonnée : le code reste consommable (`Scenario` « un mxid ne se lie
        // pas à deux comptes »)
        drop(tx);
        return Err(LymailleError::Conflict("mxid already linked".to_owned()));
    }
    let mut erase = Query::delete();
    erase
        .from_table(Alias::new(CODES_TABLE))
        .and_where(Expr::col(Alias::new("code_hash")).eq(hash.as_str()));
    let consumed = tx
        .execute(&erase)
        .await
        .map_err(|err| LymailleError::Store(db_text(&err)))?;
    if consumed.rows_affected() == 0 {
        drop(tx);
        record_failure(mxid, now);
        return Err(LymailleError::Forbidden(INVALID_CODE.to_owned()));
    }
    if owner.is_none() {
        let link = Query::insert()
            .into_table(Alias::new(LINKS_TABLE))
            .columns(["mxid", "subject", "created_at"])
            .values(vec![Expr::val(mxid), Expr::val(subject.as_str()), Expr::val(now)])
            .map_err(|err| LymailleError::Store(err.to_string()))?
            .to_owned();
        tx.execute(&link)
            .await
            .map_err(|err| LymailleError::Store(db_text(&err)))?;
    }
    tx.commit()
        .await
        .map_err(|err| LymailleError::Store(db_text(&err)))?;
    clear_attempts(mxid);
    Ok(subject)
}

/// Rend le `subject` lié à un `mxid`, ou `None` (`./link.sdd`). Le principal complet est
/// reconstruit ensuite par [`Identity::resolve`] : un compte supprimé rend l'effet `None`.
///
/// # Errors
///
/// [`LymailleError::Store`] en cas de panne de la base.
pub async fn resolve(db: &DatabaseConnection, mxid: &str) -> Result<Option<String>, LymailleError> {
    let mut select = Query::select();
    select
        .column(Alias::new("subject"))
        .from(Alias::new(LINKS_TABLE))
        .and_where(Expr::col(Alias::new("mxid")).eq(mxid));
    db.query_one(&select)
        .await
        .map_err(|err| LymailleError::Store(db_text(&err)))?
        .map(|row| row.try_get::<String>("", "subject"))
        .transpose()
        .map_err(|err| LymailleError::Store(db_text(&err)))
}

/// Délie un `mxid` du sujet de l'appelant uniquement (`./link.sdd`) : jamais la liaison d'un
/// autre, dont l'existence n'est pas révélée (`false` dans les deux cas).
///
/// # Errors
///
/// [`LymailleError::Store`] en cas de panne de la base.
pub async fn unlink(
    db: &DatabaseConnection,
    caller: &CallerPrincipal,
    mxid: &str,
) -> Result<bool, LymailleError> {
    let mut erase = Query::delete();
    erase
        .from_table(Alias::new(LINKS_TABLE))
        .and_where(Expr::col(Alias::new("mxid")).eq(mxid))
        .and_where(Expr::col(Alias::new("subject")).eq(caller.subject.as_str()));
    let done = db
        .execute(&erase)
        .await
        .map_err(|err| LymailleError::Store(db_text(&err)))?;
    Ok(done.rows_affected() > 0)
}

/// Supprime les codes expirés (`expires_at` antérieur ou égal à `now`) et rend le nombre de
/// lignes retirées (`./link.sdd`) ; appelée opportunément par [`issue_code`].
///
/// # Errors
///
/// [`LymailleError::Store`] en cas de panne de la base.
pub async fn purge_expired(db: &DatabaseConnection, now: DateTime<Utc>) -> Result<u64, LymailleError> {
    let mut erase = Query::delete();
    erase
        .from_table(Alias::new(CODES_TABLE))
        .and_where(Expr::col(Alias::new("expires_at")).lte(now));
    let done = db
        .execute(&erase)
        .await
        .map_err(|err| LymailleError::Store(db_text(&err)))?;
    Ok(done.rows_affected())
}

/// Traduction des erreurs vers le REST de la crate — mêmes textes fixes que
/// `../http/routes.rs`, aucune décision d'accès propre.
fn to_rest(error: LymailleError) -> RestError {
    match error {
        LymailleError::UnknownSubject | LymailleError::Forbidden(_) => RestError::Forbidden,
        LymailleError::NotFound(_) => RestError::NotFound,
        LymailleError::Conflict(_) => RestError::Conflict,
        other => RestError::Internal(other.to_string()),
    }
}

/// `POST /api/v1/lymaille/matrix/link-codes` : émission authentifiée (`@AuthPrincipal`),
/// `201 { "code", "expires_at" }` — le seul moment où le code existe en clair (`./link.sdd`).
async fn issue_route(
    State(auth): State<MiryadAuthState>,
    principal: AuthPrincipal,
) -> Result<(StatusCode, axum::Json<serde_json::Value>), RestError> {
    let caller = Identity::new(auth.db.clone())
        .resolve(&principal.subject)
        .await
        .map_err(to_rest)?;
    let issued = issue_code(&auth.db, &caller, Utc::now()).await.map_err(to_rest)?;
    Ok((
        StatusCode::CREATED,
        axum::Json(serde_json::json!({
            "code": issued.code,
            "expires_at": issued.expires_at.to_rfc3339(),
        })),
    ))
}

/// `DELETE /api/v1/lymaille/matrix/links/{mxid}` : délie un mxid du sujet de l'appelant ;
/// `204`, ou `404` quand aucune liaison du sujet ne correspond (`./link.sdd`).
async fn unlink_route(
    State(auth): State<MiryadAuthState>,
    principal: AuthPrincipal,
    Path(mxid): Path<String>,
) -> Result<StatusCode, RestError> {
    let caller = Identity::new(auth.db.clone())
        .resolve(&principal.subject)
        .await
        .map_err(to_rest)?;
    if unlink(&auth.db, &caller, &mxid).await.map_err(to_rest)? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(RestError::NotFound)
    }
}

/// Routeur des routes d'appairage (`./link.sdd`) : monté par l'application, jamais par
/// `../http/mod.sdd`.
pub fn link_code_router<S>() -> Router<S>
where
    S: Clone + Send + Sync + 'static,
    MiryadAuthState: FromRef<S>,
{
    Router::new()
        .route("/api/v1/lymaille/matrix/link-codes", post(issue_route))
        .route("/api/v1/lymaille/matrix/links/{mxid}", delete(unlink_route))
}

#[cfg(test)]
mod tests {
    use super::{IssuedCode, consume_code, issue_code, purge_expired, resolve, unlink};
    use crate::lymaille::LymailleError;
    use crate::lymaille::identity::{CallerPrincipal, Identity};
    use crate::lymaille::store::test_support::test_db;
    use crate::users::resolve_user;

    use sea_orm::ColumnTrait as _;
    use sea_orm::ConnectionTrait as _;
    use sea_orm::DatabaseConnection;
    use sea_orm::EntityTrait as _;
    use sea_orm::QueryFilter as _;
    use sea_orm::sea_query::{Alias, Query};

    use chrono::{TimeZone, Utc};

    fn t0() -> chrono::DateTime<chrono::Utc> {
        Utc.timestamp_opt(1_800_000_000, 0).single().expect("t0 valide")
    }

    async fn compte(db: &DatabaseConnection, subject: &str) -> CallerPrincipal {
        let account = resolve_user(db, subject, None)
            .await
            .expect("compte local de test cree");
        CallerPrincipal {
            user_id: account.id,
            subject: account.subject,
            is_admin: false,
        }
    }

    /// Le code en clair n'apparaît dans aucune colonne `code_hash` de la base.
    async fn aucun_code_en_clair(db: &DatabaseConnection, codes: &[String]) {
        let mut select = Query::select();
        select
            .column(Alias::new("code_hash"))
            .from(Alias::new(super::CODES_TABLE));
        let rows = db.query_all(&select).await.expect("lecture des hashes");
        for row in rows {
            let hash: String = row.try_get("", "code_hash").expect("un hash");
            for code in codes {
                assert_ne!(hash, *code, "un code figure en clair en base");
                assert!(
                    !hash.contains(code.as_str()),
                    "un code figure en clair dans un hash"
                );
            }
        }
    }

    async fn nb_codes(db: &DatabaseConnection) -> u64 {
        let mut select = Query::select();
        select
            .column(Alias::new("code_hash"))
            .from(Alias::new(super::CODES_TABLE));
        db.query_all(&select).await.expect("comptage des codes").len() as u64
    }

    /// Scenario « liaison de bout en bout » : `alice` obtient un code, `@alice:srv` le consomme,
    /// le `subject` est rendu par la consommation puis par `resolve`.
    #[tokio::test]
    async fn liaison_de_bout_en_bout() {
        let db = test_db().await;
        let alice = compte(&db, "alice").await;

        let issued: IssuedCode = issue_code(&db, &alice, t0())
            .await
            .expect("un code est emis pour alice");
        assert_eq!(issued.code.len(), 12, "le code fait douze caracteres");
        assert_eq!(
            issued.expires_at,
            t0() + chrono::Duration::seconds(super::CODE_TTL),
            "le code expire a emission + 10 minutes"
        );

        let subject = consume_code(&db, "@alice:srv", &issued.code, t0())
            .await
            .expect("la consommation rend le subject");
        assert_eq!(subject, "alice");
        assert_eq!(
            resolve(&db, "@alice:srv").await.expect("resolve ok"),
            Some("alice".to_owned()),
            "la liaison est lisible par resolve"
        );
    }

    /// Scenario « usage unique » : un code consommé rejoué est refusé, erreur fixe.
    #[tokio::test]
    async fn usage_unique() {
        let db = test_db().await;
        let alice = compte(&db, "alice").await;
        let issued = issue_code(&db, &alice, t0()).await.expect("code emis");
        consume_code(&db, "@reuse:srv", &issued.code, t0())
            .await
            .expect("premiere consommation");

        let rejou = consume_code(&db, "@reuse:srv", &issued.code, t0()).await;
        match rejou {
            Err(LymailleError::Forbidden(message)) => {
                assert_eq!(message, "invalid or expired link code");
            }
            other => panic!("rejou refuse par Forbidden(invalid or expired link code) : {other:?}"),
        }
    }

    /// Scenario « expiration, même erreur qu'un code inconnu » : les deux refus sont
    /// indistinguables.
    #[tokio::test]
    async fn expiration_meme_erreur_qu_code_inconnu() {
        let db = test_db().await;
        let alice = compte(&db, "alice").await;
        let issued = issue_code(&db, &alice, t0()).await.expect("code emis");

        let expire = consume_code(
            &db,
            "@expir:srv",
            &issued.code,
            t0() + chrono::Duration::seconds(super::CODE_TTL + 60),
        )
        .await
        .expect_err("un code de plus de dix minutes est refuse");
        let inconnu = consume_code(&db, "@inconnu:srv", "ZZZZZZZZZZZZ", t0())
            .await
            .expect_err("un code jamais emis est refuse");

        assert_eq!(
            expire.to_string(),
            inconnu.to_string(),
            "expiré et inconnu rendent la même erreur"
        );
        assert!(
            matches!(expire, LymailleError::Forbidden(ref message) if message == "invalid or expired link code"),
            "erreur fixe contractuelle : {expire}"
        );
    }

    /// Scenario « un mxid ne se lie pas à deux comptes » : `Conflict` et le second code reste
    /// valide.
    #[tokio::test]
    async fn un_mxid_ne_se_lie_pas_a_deux_comptes() {
        let db = test_db().await;
        let alice = compte(&db, "alice").await;
        let bob = compte(&db, "bob").await;
        let code_alice = issue_code(&db, &alice, t0()).await.expect("code d'alice").code;
        let code_bob = issue_code(&db, &bob, t0()).await.expect("code de bob").code;

        consume_code(&db, "@double:srv", &code_alice, t0())
            .await
            .expect("alice lie d'abord");
        let conflit = consume_code(&db, "@double:srv", &code_bob, t0())
            .await
            .expect_err("un mxid lié ne change pas de compte");
        match conflit {
            LymailleError::Conflict(message) => assert_eq!(message, "mxid already linked"),
            other => panic!("Conflict(mxid already linked) attendu : {other:?}"),
        }
        assert_eq!(
            resolve(&db, "@double:srv")
                .await
                .expect("liaison relue")
                .as_deref(),
            Some("alice"),
            "la liaison d'alice demeure"
        );
        assert_eq!(
            consume_code(&db, "@bob:srv", &code_bob, t0())
                .await
                .expect("le code de bob reste valide"),
            "bob",
            "le conflit ne consomme pas le code de bob"
        );
    }

    /// Scenario « codes limités et hachés » : quatre émissions, trois lignes, aucun clair.
    #[tokio::test]
    async fn codes_limites_et_haches() {
        let db = test_db().await;
        let alice = compte(&db, "alice").await;
        let mut codes = Vec::new();
        for minute in 0..4 {
            let issued = issue_code(&db, &alice, t0() + chrono::Duration::minutes(minute))
                .await
                .expect("code emis");
            codes.push(issued.code);
        }
        assert_eq!(nb_codes(&db).await, 3, "trois codes vivants au plus");
        aucun_code_en_clair(&db, &codes).await;
    }

    /// Scenario « débit » : cinq tentatives fausses puis la sixième, même juste, est refusée.
    #[tokio::test]
    async fn debit() {
        let db = test_db().await;
        let alice = compte(&db, "alice").await;
        let issued = issue_code(&db, &alice, t0()).await.expect("code emis");

        for tentative in 1..=5 {
            let erreur = consume_code(&db, "@ratelimit:srv", "AAAAAAAAAAAA", t0())
                .await
                .expect_err("tentative fausse refusée");
            assert!(
                matches!(erreur, LymailleError::Forbidden(ref message) if message == "invalid or expired link code"),
                "tentative {tentative} : {erreur}"
            );
        }
        let sixieme = consume_code(&db, "@ratelimit:srv", &issued.code, t0())
            .await
            .expect_err("la sixième est refusée, même juste");
        assert!(
            matches!(sixieme, LymailleError::Forbidden(ref message) if message == "too many attempts"),
            "débit : {sixieme}"
        );
    }

    /// Scenario « un compte supprimé n'a plus de principal » : la liaison survit, le principal
    /// ne se reconstruit plus.
    #[tokio::test]
    async fn compte_supprime_sans_principal() {
        let db = test_db().await;
        let alice = compte(&db, "alice").await;
        let issued = issue_code(&db, &alice, t0()).await.expect("code emis");
        consume_code(&db, "@fantome:srv", &issued.code, t0())
            .await
            .expect("liaison etablie");

        let supprime = crate::users::user::Entity::delete_many()
            .filter(crate::users::user::Column::Subject.eq("alice"))
            .exec(&db)
            .await
            .expect("suppression du compte");
        assert_eq!(supprime.rows_affected, 1, "le compte d'alice est supprimé");

        assert_eq!(
            resolve(&db, "@fantome:srv")
                .await
                .expect("liaison relue")
                .as_deref(),
            Some("alice"),
            "la liaison, elle, demeure"
        );
        let reconstruit = Identity::new(db.clone()).resolve("alice").await;
        assert!(
            matches!(reconstruit, Err(LymailleError::UnknownSubject)),
            "le principal n'est pas reconstruit : {reconstruit:?}"
        );
    }

    /// Scenario « délier » : `bob` ne délie pas le mxid d'`alice`, `alice` oui.
    #[tokio::test]
    async fn delier() {
        let db = test_db().await;
        let alice = compte(&db, "alice").await;
        let bob = compte(&db, "bob").await;
        let issued = issue_code(&db, &alice, t0()).await.expect("code emis");
        consume_code(&db, "@unlink:srv", &issued.code, t0())
            .await
            .expect("liaison d'alice");

        assert!(
            !unlink(&db, &bob, "@unlink:srv").await.expect("unlink de bob"),
            "bob ne délie pas le mxid d'un autre"
        );
        assert!(
            unlink(&db, &alice, "@unlink:srv").await.expect("unlink d'alice"),
            "alice délie le sien"
        );
        assert_eq!(
            resolve(&db, "@unlink:srv").await.expect("resolve apres unlink"),
            None,
            "la liaison a disparu"
        );
    }

    /// `Must` : `purge_expired` retire les codes expirés et rend le compte des lignes.
    #[tokio::test]
    async fn purge_retire_les_expires() {
        let db = test_db().await;
        let alice = compte(&db, "alice").await;
        issue_code(&db, &alice, t0()).await.expect("code emis");
        assert_eq!(
            purge_expired(&db, t0() + chrono::Duration::minutes(9))
                .await
                .expect("rien n'a dix minutes"),
            0
        );
        assert_eq!(
            purge_expired(&db, t0() + chrono::Duration::minutes(11))
                .await
                .expect("le code a dix minutes et une minute"),
            1
        );
    }

    /// `Must` : le routeur monte les deux routes — `POST` authentifié rend `201 {code,
    /// expires_at}`, `DELETE` sur un mxid lié rend `204`, vierge `404`.
    #[tokio::test]
    async fn routeur_codes_et_delie() {
        use super::link_code_router;
        use crate::auth::{MiryadAuthState, MockOidcClient};
        use axum::Router;
        use axum::http::StatusCode;
        use std::sync::Arc;
        use tower::ServiceExt as _;

        let db = test_db().await;
        let compte = compte(&db, "alice").await;
        let token = crate::auth::issue_token(&db, "alice", "test", None, "link-test-pepper")
            .await
            .expect("token api emis")
            .token;
        let etat = MiryadAuthState {
            oidc_client: Arc::new(MockOidcClient::new()),
            cookie_key: ::cookie::Key::from(&[7u8; 64]),
            post_login_redirect: "/".to_owned(),
            post_logout_redirect: "/".to_owned(),
            db: db.clone(),
            secure_cookies: false,
            token_pepper: "link-test-pepper".to_owned(),
        };
        let app: Router<()> = link_code_router::<MiryadAuthState>().with_state(etat);

        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/api/v1/lymaille/matrix/link-codes")
            .header("Authorization", format!("Bearer {token}"))
            .body(axum::body::Body::empty())
            .expect("requete valide");
        let response = app.clone().oneshot(request).await.expect("le routeur repond");
        assert_eq!(response.status(), StatusCode::CREATED, "201 a l'emission");
        let corps: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .expect("corps lu"),
        )
        .expect("json attendu");
        let code = corps["code"].as_str().expect("un code est rendu").to_owned();
        assert_eq!(code.len(), 12);
        assert!(!corps["expires_at"].is_null(), "l'expiration est rendue");

        // le code est consomme par le pont, pas par la route : liaison directe pour la suite
        consume_code(&db, "@route:srv", &code, Utc::now())
            .await
            .expect("consommation du code emis par la route");

        let request = axum::http::Request::builder()
            .method("DELETE")
            .uri("/api/v1/lymaille/matrix/links/@route:srv")
            .header("Authorization", format!("Bearer {token}"))
            .body(axum::body::Body::empty())
            .expect("requete valide");
        let response = app.clone().oneshot(request).await.expect("le routeur repond");
        assert_eq!(response.status(), StatusCode::NO_CONTENT, "204 au deliage");

        let request = axum::http::Request::builder()
            .method("DELETE")
            .uri("/api/v1/lymaille/matrix/links/@inconnu:srv")
            .header("Authorization", format!("Bearer {token}"))
            .body(axum::body::Body::empty())
            .expect("requete valide");
        let response = app.oneshot(request).await.expect("le routeur repond");
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "404 sur un mxid vierge");
        assert_eq!(compte.subject, "alice");
    }
}
