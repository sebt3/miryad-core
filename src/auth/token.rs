use base64::Engine;
use chrono::Utc;
use sea_orm::entity::prelude::*;
use sea_orm::{DatabaseConnection, Set};
use sha2::{Digest, Sha256};

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
    /// SHA-256 hex du token — jamais le token en clair.
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

fn hash_token(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

pub async fn issue_token(
    db: &DatabaseConnection,
    subject: &str,
    name: &str,
    expires_at: Option<DateTimeUtc>,
) -> Result<IssuedToken, AuthError> {
    let token = generate_token();
    let active = ActiveModel {
        subject: Set(subject.to_string()),
        name: Set(name.to_string()),
        token_hash: Set(hash_token(&token)),
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

pub async fn validate_token(db: &DatabaseConnection, token: &str) -> Result<AuthPrincipal, AuthError> {
    let record = Entity::find()
        .filter(Column::TokenHash.eq(hash_token(token)))
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
    active.update(db).await?;

    Ok(AuthPrincipal {
        subject,
        email: None,
        source: PrincipalSource::ApiToken { token_id: id },
    })
}

pub async fn revoke_token(db: &DatabaseConnection, id: i32) -> Result<(), AuthError> {
    Entity::delete_by_id(id).exec(db).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migration::Migrator;
    use sea_orm_migration::MigratorTrait;

    async fn test_db() -> DatabaseConnection {
        let db = sea_orm::Database::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite connects");
        Migrator::up(&db, None).await.expect("migrations apply cleanly");
        db
    }

    #[tokio::test]
    async fn issued_token_validates_and_updates_last_used() {
        let db = test_db().await;
        let issued = issue_token(&db, "user-123", "test token", None)
            .await
            .expect("issuing succeeds");

        assert_ne!(issued.token, hash_token(&issued.token));

        let principal = validate_token(&db, &issued.token).await.expect("token is valid");
        assert_eq!(principal.subject, "user-123");
        assert!(matches!(
            principal.source,
            PrincipalSource::ApiToken { token_id } if token_id == issued.id
        ));

        let record = Entity::find_by_id(issued.id)
            .one(&db)
            .await
            .expect("query succeeds")
            .expect("record exists");
        assert!(record.last_used_at.is_some());
    }

    #[tokio::test]
    async fn expired_token_is_rejected() {
        let db = test_db().await;
        let issued = issue_token(&db, "user-123", "expired token", Some(Utc::now()))
            .await
            .expect("issuing succeeds");

        let result = validate_token(&db, &issued.token).await;
        assert!(matches!(result, Err(AuthError::TokenExpired)));
    }

    #[tokio::test]
    async fn unknown_token_is_rejected() {
        let db = test_db().await;
        let result = validate_token(&db, "mrd_does-not-exist").await;
        assert!(matches!(result, Err(AuthError::InvalidToken)));
    }

    #[tokio::test]
    async fn revoked_token_is_rejected() {
        let db = test_db().await;
        let issued = issue_token(&db, "user-123", "to revoke", None)
            .await
            .expect("issuing succeeds");
        revoke_token(&db, issued.id).await.expect("revocation succeeds");

        let result = validate_token(&db, &issued.token).await;
        assert!(matches!(result, Err(AuthError::InvalidToken)));
    }
}
