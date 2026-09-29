use base64::Engine;
use chrono::Utc;
use hmac::{Hmac, Mac};
use sea_orm::DbErr;
use sea_orm::entity::prelude::*;
use sea_orm::{DatabaseConnection, Set};
use sha2::Sha256;

use crate::auth::error::AuthError;
use crate::auth::principal::{AuthPrincipal, PrincipalSource};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "miryad_api_tokens")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    /// Identifiant du titulaire (le `sub` OIDC ou tout identifiant choisi par l'app) — pas de
    /// FK vers une table `User` qui n'existe pas encore (cf. feature 3).
    pub subject: String,
    /// Label libre pour que l'utilisateur reconnaisse son token dans une liste.
    pub name: String,
    /// HMAC-SHA256 hex minuscule du token, clé par le poivre de l'app — jamais le token en
    /// clair, et jamais l'empreinte sans le poivre (cf. `src/auth/token.sdd`, 2026-09-27).
    pub token_hash: String,
    pub created_at: DateTimeUtc,
    pub expires_at: Option<DateTimeUtc>,
    pub last_used_at: Option<DateTimeUtc>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

/// Alias plus lisible que le `Entity` généré par `DeriveEntityModel`.
pub type ApiToken = Entity;

/// Un token API émis — le champ `token` porte le secret en clair, retourné une seule fois à
/// l'émission. Il n'est jamais récupérable ensuite (seul son hash est persisté).
pub struct IssuedToken {
    pub id: i32,
    pub token: String,
}

fn generate_token() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    format!(
        "mrd_{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
    )
}

fn hash_token(token: &str, pepper: &str) -> String {
    // Poivre HMAC obligatoire (`src/auth/token.sdd` `Must`, arbitré 2026-09-27) : `Mac::new_from_slice` est infaillible pour HMAC (clé de longueur libre, source hmac 0.12.1), le `expect` ne peut pas se déclencher.
    #[allow(clippy::expect_used)]
    let mut mac =
        Hmac::<Sha256>::new_from_slice(pepper.as_bytes()).expect("HMAC-SHA256 accepts any key length");
    mac.update(token.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// Émet un token API : secret généré, seule l'empreinte HMAC-SHA256 poivrée est persistée ;
/// le champ `token` de l'`IssuedToken` rendu porte le clair, retourné une seule fois et jamais
/// récupérable ensuite.
///
/// # Errors
///
/// `AuthError::Database` (`MRD-AUTH-016`) — toute panne de base sur l'`insert` de la ligne
/// (erreur remontée nue du `DbErr` via `From<sea_orm::DbErr>`).
pub async fn issue_token(
    db: &DatabaseConnection,
    subject: &str,
    name: &str,
    expires_at: Option<DateTimeUtc>,
    pepper: &str,
) -> Result<IssuedToken, AuthError> {
    let token = generate_token();
    let active = ActiveModel {
        subject: Set(subject.to_string()),
        name: Set(name.to_string()),
        token_hash: Set(hash_token(&token, pepper)),
        created_at: Set(Utc::now()),
        expires_at: Set(expires_at),
        last_used_at: Set(None),
        ..Default::default()
    };
    let inserted = active.insert(db).await?;

    Ok(IssuedToken {
        id: inserted.id,
        token,
    })
}

/// Valide un token API présenté en clair (schéma `Bearer`) : haché sous le poivre puis cherché
/// en base, expiration vérifiée, `last_used_at` horodaté en best-effort. Rend l'`AuthPrincipal`
/// du titulaire.
///
/// # Errors
///
/// `AuthError::InvalidToken` (`MRD-AUTH-014`) — aucune ligne ne correspond à l'empreinte (token
/// jamais émis, révoqué — la révocation étant physique, inexistant et révoqué sont indiscernables
/// —, chaîne vide ou hors format), ou `UPDATE` de `last_used_at` échouant par `RecordNotUpdated`
/// / relecture par `RecordNotFound` : la ligne a été révoquée entre lecture et écriture.
///
/// `AuthError::TokenExpired` (`MRD-AUTH-015`) — la ligne trouvée porte un `expires_at` antérieur
/// ou égal à l'instant de validation (borne `≤` inclusive).
///
/// `AuthError::Database` (`MRD-AUTH-016`) — panne de base sur la lecture qui décide du résultat.
/// Toute autre `DbErr` sur l'`UPDATE` de `last_used_at` est absorbée sans erreur : le secret était
/// valide, l'horodatage est best-effort.
pub async fn validate_token(
    db: &DatabaseConnection,
    token: &str,
    pepper: &str,
) -> Result<AuthPrincipal, AuthError> {
    let record = Entity::find()
        .filter(Column::TokenHash.eq(hash_token(token, pepper)))
        .one(db)
        .await?
        .ok_or(AuthError::InvalidToken)?;

    if let Some(expires_at) = record.expires_at
        && expires_at <= Utc::now()
    {
        return Err(AuthError::TokenExpired);
    }

    let id = record.id;
    let subject = record.subject.clone();
    let mut active: ActiveModel = record.into();
    active.last_used_at = Set(Some(Utc::now()));
    if let Err(err) = active.update(db).await {
        // Course avec `revoke_token` : la ligne a été supprimée physiquement entre
        // la lecture et l'écriture (ou sa relecture). Rejet légitime comme un token
        // révoqué, pas une panne de base (src/auth/token.sdd `Handles`, 2026-09-27).
        if matches!(err, DbErr::RecordNotUpdated | DbErr::RecordNotFound(_)) {
            return Err(AuthError::InvalidToken);
        }
        // Toute autre `DbErr` (panne de connexion transitoire, timeout — la ligne est
        // toujours là) : panne de bookkeeping, absorbée sans trace (mutisme imposé par
        // token.sdd `Must`). Le secret est valide, la décision de sécurité est déjà
        // prise, `last_used_at` est best-effort (src/auth/token.sdd `Handles`, 2026-09-27).
    }

    Ok(AuthPrincipal {
        subject,
        email: None,
        preferred_username: None,
        source: PrincipalSource::ApiToken { token_id: id },
    })
}

/// Révoque un token API par son `id` : suppression physique de la ligne. Un `id` inconnu ou déjà
/// révoqué réussit en silence (`0` ligne affectée est un `Ok`).
///
/// # Errors
///
/// `AuthError::Database` (`MRD-AUTH-016`) — toute panne de base sur le `DELETE`.
pub async fn revoke_token(db: &DatabaseConnection, id: i32) -> Result<(), AuthError> {
    Entity::delete_by_id(id).exec(db).await?;
    Ok(())
}

/// Garantit l'existence d'un token dont la valeur en clair est `token` — contrairement à
/// `issue_token`, la valeur n'est pas générée ici mais fournie par l'appelant (cf.
/// `users::ensure_service_account`, feature 2c). Idempotent sous le même `subject` : si un
/// token avec ce hash existe déjà pour ce `subject`, ne fait rien (`created_at` inchangé).
/// Depuis l'arbitrage 2026-09-27, une empreinte déjà détenue par un **autre** `subject` est
/// un rejet explicite (`AuthError::TokenHashConflict`) — plus le no-op silencieux.
///
/// # Errors
///
/// `AuthError::TokenHashConflict` (`MRD-AUTH-017`) — l'empreinte de la valeur fournie existe déjà
/// en base sous un `subject` différent de celui demandé (ressource déjà allouée).
///
/// `AuthError::Database` (`MRD-AUTH-016`) — panne de base sur la prélecture sur `token_hash` ou
/// sur l'`insert` (collision `UNIQUE` `token_hash` comprise).
pub async fn ensure_token(
    db: &DatabaseConnection,
    subject: &str,
    name: &str,
    token: &str,
    expires_at: Option<DateTimeUtc>,
    pepper: &str,
) -> Result<(), AuthError> {
    let hash = hash_token(token, pepper);
    let existing = Entity::find().filter(Column::TokenHash.eq(&hash)).one(db).await?;
    if let Some(existing) = existing {
        if existing.subject != subject {
            // L'empreinte est une ressource déjà allouée à un autre titulaire : rejet
            // explicite, le second demandeur doit être informé (src/auth/token.sdd
            // `Handles`, arbitré avec Sébastien le 2026-09-27).
            return Err(AuthError::TokenHashConflict);
        }
        return Ok(());
    }

    let active = ActiveModel {
        subject: Set(subject.to_string()),
        name: Set(name.to_string()),
        token_hash: Set(hash),
        created_at: Set(Utc::now()),
        expires_at: Set(expires_at),
        last_used_at: Set(None),
        ..Default::default()
    };
    active.insert(db).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migration::Migrator;
    use hmac::{Hmac, Mac};
    use sea_orm::{ConnectionTrait, DbBackend, MockDatabase, MockExecResult};
    use sea_orm_migration::MigratorTrait;

    /// Poivre des `Scenario` « poivre » de `src/auth/token.sdd` — toute émission/validation
    /// d'un même test partage ce poivre, sauf le scénario de rotation explicite.
    const PEPPER: &str = "pepper-1";
    const OTHER_PEPPER: &str = "pepper-2";

    async fn test_db() -> DatabaseConnection {
        let db = sea_orm::Database::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite connects");
        Migrator::up(&db, None).await.expect("migrations apply cleanly");
        db
    }

    /// Instant rond (seconde pleine) : l'aller-retour `SQLite` préserve la valeur à la
    /// nanoseconde près, les égalités `expires_at` sont exactes.
    fn whole_second(offset: i64) -> DateTimeUtc {
        DateTimeUtc::from_timestamp(Utc::now().timestamp() + offset, 0).expect("valid timestamp")
    }

    /// `hex::encode(Hmac::<Sha256>::new_from_slice(pepper).chain_update(secret).finalize())`
    /// — la recalculatrice du contrat d'empreinte (`Must` de token.sdd).
    fn hmac_hex(secret: &str, pepper: &str) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(pepper.as_bytes()).expect("HMAC accepts any key length");
        mac.update(secret.as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }

    // ——— Émission ———

    /// `Scenario` : « secret émis par `issue_token` ».
    #[tokio::test]
    async fn issued_secret_is_mrd_prefixed_base64url_of_43_chars() {
        let db = test_db().await;
        let issued = issue_token(&db, "user-123", "label", None, PEPPER)
            .await
            .expect("issuing succeeds");

        assert!(issued.token.starts_with("mrd_"));
        let body = issued.token.strip_prefix("mrd_").expect("mrd_ prefix present");
        assert_eq!(body.len(), 43, "43 caractères après le préfixe");
        assert!(
            body.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "alphabet base64url RFC 4648 §5 uniquement : {}",
            issued.token
        );
        // Le `id` rendu est le PK généré de la ligne insérée.
        let row = Entity::find_by_id(issued.id)
            .one(&db)
            .await
            .expect("query succeeds")
            .expect("issued id is the inserted row's PK");
        assert_eq!(row.token_hash, hmac_hex(&issued.token, PEPPER));
    }

    /// `Scenario` : « deux émissions ne partagent jamais le même secret ».
    #[tokio::test]
    async fn consecutive_issuances_never_share_a_secret() {
        let db = test_db().await;
        let first = issue_token(&db, "user-123", "same label", None, PEPPER)
            .await
            .expect("first issuance succeeds");
        let second = issue_token(&db, "user-123", "same label", None, PEPPER)
            .await
            .expect("second issuance succeeds");

        assert_ne!(first.token, second.token);
        let rows = Entity::find().all(&db).await.expect("query succeeds");
        assert_eq!(rows.len(), 2, "deux lignes distinctes");
        assert_ne!(rows[0].token_hash, rows[1].token_hash);
    }

    /// `Scenario` : « seule l'empreinte HMAC-SHA256 poivrée hex minuscule est persistée ».
    #[tokio::test]
    async fn stored_token_hash_is_lowercase_hmac_sha256_of_secret_and_pepper() {
        let db = test_db().await;
        let issued = issue_token(&db, "user-123", "pepper check", None, PEPPER)
            .await
            .expect("issuing succeeds");

        let row = Entity::find_by_id(issued.id)
            .one(&db)
            .await
            .expect("query succeeds")
            .expect("row exists");
        assert_eq!(
            row.token_hash,
            hmac_hex(&issued.token, PEPPER),
            "l'empreinte stockée est le HMAC-SHA256 du secret clair préfixe inclus, clé le poivre"
        );
        assert_eq!(row.token_hash.len(), 64);
        assert!(
            row.token_hash
                .chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
            "hex minuscule : {}",
            row.token_hash
        );
        // Aucune colonne ne porte le secret clair ni le poivre.
        for column in [&row.subject, &row.name, &row.token_hash] {
            assert!(!column.contains(&issued.token), "secret clair fui par {column:?}");
            assert!(!column.contains(PEPPER), "poivre fui par {column:?}");
        }
    }

    /// `Scenario` : « un poivre différent ne retrouve jamais le même secret » — un
    /// changement de poivre invalide silencieusement tous les tokens existants.
    #[tokio::test]
    async fn different_pepper_never_matches_same_secret() {
        let db = test_db().await;
        let issued = issue_token(&db, "user-123", "rotated pepper", None, PEPPER)
            .await
            .expect("issuing succeeds");

        let result = validate_token(&db, &issued.token, OTHER_PEPPER).await;
        assert!(
            matches!(result, Err(AuthError::InvalidToken)),
            "l'empreinte sous {OTHER_PEPPER:?} ne doit correspondre à aucune ligne : {result:?}"
        );
    }

    /// `Scenario` : « la ligne émise porte son intitulé ».
    #[tokio::test]
    async fn issued_row_carries_subject_name_and_timestamps() {
        let db = test_db().await;
        let expires = whole_second(3600);
        let issued = issue_token(&db, "user-123", "cli laptop", Some(expires), PEPPER)
            .await
            .expect("issuing succeeds");

        let row = Entity::find_by_id(issued.id)
            .one(&db)
            .await
            .expect("query succeeds")
            .expect("row exists");
        assert_eq!(row.subject, "user-123");
        assert_eq!(row.name, "cli laptop");
        assert!(
            (Utc::now() - row.created_at).num_seconds().abs() <= 5,
            "created_at porte l'instant d'émission à la seconde près"
        );
        assert_eq!(row.expires_at, Some(expires), "expires_at est la valeur fournie");
        assert_eq!(row.last_used_at, None);
    }

    // ——— Validation ———

    /// `Scenario` : « validation valide rend le principal et horodate `last_used_at` » —
    /// étendu (arbitrage 2026-09-27) : `email` `None` et colonnes voisines inchangées.
    #[tokio::test]
    async fn issued_token_validates_and_updates_last_used() {
        let db = test_db().await;
        let issued = issue_token(&db, "user-123", "test token", None, PEPPER)
            .await
            .expect("issuing succeeds");

        assert_ne!(issued.token, hash_token(&issued.token, PEPPER));

        let before = Entity::find_by_id(issued.id)
            .one(&db)
            .await
            .expect("query succeeds")
            .expect("record exists");
        assert_eq!(before.last_used_at, None);

        let principal = validate_token(&db, &issued.token, PEPPER)
            .await
            .expect("token is valid");
        assert_eq!(principal.subject, "user-123");
        assert_eq!(principal.email, None, "un token n'a jamais connu d'email");
        assert!(matches!(
            principal.source,
            PrincipalSource::ApiToken { token_id } if token_id == issued.id
        ));

        let after = Entity::find_by_id(issued.id)
            .one(&db)
            .await
            .expect("query succeeds")
            .expect("record exists");
        let stamped = after.last_used_at.expect("last_used_at is stamped");
        assert!(
            stamped >= before.created_at,
            "l'horodatage d'usage est postérieur à l'émission"
        );
        // Seule `last_used_at` bouge — le reste de l'ActiveModel est inchangé.
        assert_eq!(after.token_hash, before.token_hash);
        assert_eq!(after.subject, before.subject);
        assert_eq!(after.name, before.name);
        assert_eq!(after.created_at, before.created_at);
        assert_eq!(after.expires_at, before.expires_at);
    }

    /// `Scenario` : « validation expirée n'horodate pas » — aucun octet écrit sur
    /// l'échec d'expiration.
    #[tokio::test]
    async fn expired_validation_does_not_stamp_last_used() {
        let db = test_db().await;
        let issued = issue_token(&db, "user-123", "doomed", Some(whole_second(-60)), PEPPER)
            .await
            .expect("issuing succeeds");

        let result = validate_token(&db, &issued.token, PEPPER).await;
        assert!(matches!(result, Err(AuthError::TokenExpired)));
        let row = Entity::find_by_id(issued.id)
            .one(&db)
            .await
            .expect("query succeeds")
            .expect("row exists");
        assert_eq!(row.last_used_at, None, "l'expiration ne stamp pas last_used_at");
    }

    /// `Scenario` : « token inconnu est rejeté ».
    #[tokio::test]
    async fn unknown_token_is_rejected() {
        let db = test_db().await;
        let result = validate_token(&db, "mrd_does-not-exist", PEPPER).await;
        assert!(matches!(result, Err(AuthError::InvalidToken)));
    }

    /// `Scenario` : « chaîne vide est rejetée en inconnue » — hachée et cherchée
    /// comme une autre, sans panic ni erreur de base.
    #[tokio::test]
    async fn empty_string_is_rejected_as_unknown() {
        let db = test_db().await;
        let result = validate_token(&db, "", PEPPER).await;
        assert!(
            matches!(result, Err(AuthError::InvalidToken)),
            "la chaîne vide doit rendre MRD-AUTH-014, jamais MRD-AUTH-016 : {result:?}"
        );
    }

    /// `Scenario` : « expiration passée est acceptée à l'émission » — la valeur est
    /// stockée telle quelle, jamais redressée, et la validation la refuse.
    #[tokio::test]
    async fn past_expiry_accepted_at_issue_then_denied_at_validate() {
        let db = test_db().await;
        let past = whole_second(-86_400);
        let issued = issue_token(&db, "user-123", "backdated", Some(past), PEPPER)
            .await
            .expect("issuing with a past expires_at succeeds");

        let row = Entity::find_by_id(issued.id)
            .one(&db)
            .await
            .expect("query succeeds")
            .expect("row exists");
        assert_eq!(
            row.expires_at,
            Some(past),
            "l'expiration passée est stockée verbatim"
        );

        let result = validate_token(&db, &issued.token, PEPPER).await;
        assert!(matches!(result, Err(AuthError::TokenExpired)));
    }

    /// `Scenario` : « borne d'expiration est inclusive » — `expires_at` égal à
    /// l'instant courant expire (`<=`).
    #[tokio::test]
    async fn expired_token_is_rejected() {
        let db = test_db().await;
        let issued = issue_token(&db, "user-123", "expired token", Some(Utc::now()), PEPPER)
            .await
            .expect("issuing succeeds");

        let result = validate_token(&db, &issued.token, PEPPER).await;
        assert!(matches!(result, Err(AuthError::TokenExpired)));
    }

    // ——— Révocation ———

    /// `Scenario` : « révocation efface physiquement la ligne » — étendu
    /// (arbitrage 2026-09-27) : `find_by_id` sur `None` matérialise l'effacement.
    #[tokio::test]
    async fn revoked_token_is_rejected() {
        let db = test_db().await;
        let issued = issue_token(&db, "user-123", "to revoke", None, PEPPER)
            .await
            .expect("issuing succeeds");
        revoke_token(&db, issued.id).await.expect("revocation succeeds");

        let row = Entity::find_by_id(issued.id)
            .one(&db)
            .await
            .expect("query succeeds");
        assert_eq!(row, None, "la suppression est physique");

        let result = validate_token(&db, &issued.token, PEPPER).await;
        assert!(matches!(result, Err(AuthError::InvalidToken)));
    }

    /// `Scenario` : « révocation d'un id inconnu réussit en silence » — `Ok` sur
    /// `0` ligne affectée, rien créé ni touché.
    #[tokio::test]
    async fn revoking_unknown_id_is_silent_noop() {
        let db = test_db().await;
        revoke_token(&db, 999_999)
            .await
            .expect("revoking an unknown id succeeds silently");

        let count = Entity::find().all(&db).await.expect("query succeeds").len();
        assert_eq!(count, 0, "aucune ligne créée ni touchée");
    }

    /// `Scenario` : « double révocation réussit » — `Ok` rendu deux fois, ligne absente.
    #[tokio::test]
    async fn revoking_twice_succeeds() {
        let db = test_db().await;
        let issued = issue_token(&db, "user-123", "to revoke twice", None, PEPPER)
            .await
            .expect("issuing succeeds");
        revoke_token(&db, issued.id)
            .await
            .expect("first revocation succeeds");
        revoke_token(&db, issued.id)
            .await
            .expect("second revocation succeeds too (0 row deleted is Ok)");

        let row = Entity::find_by_id(issued.id)
            .one(&db)
            .await
            .expect("query succeeds");
        assert_eq!(row, None, "la ligne reste absente");
    }

    // ——— ensure_token ———

    /// `Scenario` : « `ensure_token` sur une valeur inconnue » — étendu
    /// (arbitrage 2026-09-27) : `name` et `expires_at` stockés vérifiés.
    #[tokio::test]
    async fn ensure_token_creates_then_authenticates() {
        let db = test_db().await;
        let expires = whole_second(3600);
        ensure_token(
            &db,
            "service-account",
            "bootstrap",
            "mrd_fixed-secret",
            Some(expires),
            PEPPER,
        )
        .await
        .expect("ensure succeeds");

        let row = Entity::find()
            .filter(Column::TokenHash.eq(hmac_hex("mrd_fixed-secret", PEPPER)))
            .one(&db)
            .await
            .expect("query succeeds")
            .expect("row inserted");
        assert_eq!(row.name, "bootstrap", "le name fourni est stocké verbatim");
        assert_eq!(row.expires_at, Some(expires), "l'expires_at fourni est stocké");
        assert_eq!(row.last_used_at, None);

        let principal = validate_token(&db, "mrd_fixed-secret", PEPPER)
            .await
            .expect("token is valid");
        assert_eq!(principal.subject, "service-account");
    }

    /// `Scenario` : « `ensure_token` est idempotent sur sa propre empreinte » — étendu
    /// (arbitrage 2026-09-27) : `created_at` inchangé au rejeu.
    #[tokio::test]
    async fn ensure_token_is_idempotent() {
        let db = test_db().await;
        ensure_token(
            &db,
            "service-account",
            "bootstrap",
            "mrd_fixed-secret",
            None,
            PEPPER,
        )
        .await
        .expect("first ensure succeeds");
        let first_created = Entity::find()
            .filter(Column::TokenHash.eq(hmac_hex("mrd_fixed-secret", PEPPER)))
            .one(&db)
            .await
            .expect("query succeeds")
            .expect("row exists")
            .created_at;

        ensure_token(
            &db,
            "service-account",
            "bootstrap",
            "mrd_fixed-secret",
            None,
            PEPPER,
        )
        .await
        .expect("second ensure succeeds (idempotent rejeu)");

        let rows = Entity::find()
            .filter(Column::TokenHash.eq(hmac_hex("mrd_fixed-secret", PEPPER)))
            .all(&db)
            .await
            .expect("query succeeds");
        assert_eq!(rows.len(), 1, "exactement une ligne porte cette empreinte");
        assert_eq!(
            rows.first().expect("row present").created_at,
            first_created,
            "created_at est inchangé au rejeu — le no-op ne réécrit rien"
        );
    }

    /// `Scenario` : « `ensure_token` sur une empreinte déjà détenue par un autre
    /// titulaire est rejeté » — remplace le no-op silencieux (arbitré 2026-09-27).
    #[tokio::test]
    async fn ensure_token_on_existing_hash_under_another_subject_is_rejected() {
        let db = test_db().await;
        ensure_token(&db, "svc-a", "shared value", "mrd_shared-X", None, PEPPER)
            .await
            .expect("first provisioning succeeds");

        let result = ensure_token(&db, "svc-b", "stolen value", "mrd_shared-X", None, PEPPER).await;
        assert!(
            matches!(result, Err(AuthError::TokenHashConflict)),
            "l'empreinte d'un autre subject doit être rejetée, plus le no-op : {result:?}"
        );

        let rows = Entity::find()
            .filter(Column::TokenHash.eq(hmac_hex("mrd_shared-X", PEPPER)))
            .all(&db)
            .await
            .expect("query succeeds");
        assert_eq!(rows.len(), 1, "aucune ligne svc-b n'existe");
        assert_eq!(
            rows.first().expect("sole row present").subject,
            "svc-a",
            "la seule ligne de l'empreinte X porte toujours svc-a"
        );

        let principal = validate_token(&db, "mrd_shared-X", PEPPER)
            .await
            .expect("svc-a keeps authenticating");
        assert_eq!(principal.subject, "svc-a");
    }

    /// `Scenario` : « `ensure_token` accepte une valeur hors format » — aucune
    /// validation de format nulle part, préfixe `mrd_` non exigé.
    #[tokio::test]
    async fn ensure_token_accepts_out_of_format_secret() {
        let db = test_db().await;
        let human = "secret humain avec espaces et accents";
        ensure_token(&db, "svc", "label", human, None, PEPPER)
            .await
            .expect("out-of-format value is accepted");

        let principal = validate_token(&db, human, PEPPER)
            .await
            .expect("same value, same pepper, authenticates");
        assert_eq!(principal.subject, "svc");
    }

    // ——— Courses et best-effort (arbitrés 2026-09-27) ———

    /// `Scenario` : « révocation en pleine validation rend `InvalidToken`, pas `Database` » —
    /// fixture qui force `DbErr::RecordNotUpdated` : l'`UPDATE` de `last_used_at` ne
    /// touche plus de ligne (supprimée entre la lecture et l'écriture par un
    /// `revoke_token` concurrent).
    #[tokio::test]
    async fn revocation_during_validation_race_yields_invalid_token_not_database() {
        let secret = "mrd_race-token";
        let record = Model {
            id: 7,
            subject: "racer".to_string(),
            name: "race".to_string(),
            token_hash: hmac_hex(secret, PEPPER),
            created_at: Utc::now(),
            expires_at: None,
            last_used_at: None,
        };
        // SELECT → la ligne existe encore ; UPDATE → 0 ligne affectée (mer sea-orm :
        // rows_affected == 0 → RecordNotUpdated). La relecture vide couvre le chemin
        // RecordNotFound si l'UPDATE est passé avant la suppression.
        let db = MockDatabase::new(DbBackend::Sqlite)
            .append_query_results([[record]])
            .append_exec_results([MockExecResult {
                last_insert_id: 0,
                rows_affected: 0,
            }])
            .append_query_results::<Model, _, _>([[]])
            .into_connection();

        let result = validate_token(&db, secret, PEPPER).await;
        assert!(
            matches!(result, Err(AuthError::InvalidToken)),
            "un token révoqué en pleine requête est un rejet légitime (MRD-AUTH-014), \
             plus jamais MRD-AUTH-016 : {result:?}"
        );
    }

    /// `Scenario` : « l'échec de l'horodatage `last_used_at` n'invalide pas une
    /// authentification par ailleurs correcte » — panne simulée de l'`UPDATE` qui
    /// n'est ni `RecordNotUpdated` ni `RecordNotFound` (déclencheur `SQLite` qui avorte
    /// toute écriture ; la ligne reste présente).
    #[tokio::test]
    async fn last_used_at_write_failure_is_absorbed_and_validation_still_succeeds() {
        let db = test_db().await;
        let issued = issue_token(&db, "user-123", "outage survivor", None, PEPPER)
            .await
            .expect("issuing succeeds");
        db.execute_unprepared(
            "CREATE TRIGGER fail_token_updates BEFORE UPDATE ON miryad_api_tokens \
             BEGIN SELECT RAISE(ABORT, 'simulated write outage'); END",
        )
        .await
        .expect("trigger creates");

        let principal = validate_token(&db, &issued.token, PEPPER)
            .await
            .expect("a bookkeeping outage must not invalidate a valid secret");
        assert_eq!(principal.subject, "user-123");
        assert!(matches!(
            principal.source,
            PrincipalSource::ApiToken { token_id } if token_id == issued.id
        ));

        let row = Entity::find_by_id(issued.id)
            .one(&db)
            .await
            .expect("query succeeds")
            .expect("row still present");
        assert_eq!(row.last_used_at, None, "l'horodatage en échec n'est pas posé");
        assert_eq!(row.token_hash, hmac_hex(&issued.token, PEPPER));
    }
}
