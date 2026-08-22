use sea_orm::prelude::DateTimeUtc;
use sea_orm::{DatabaseConnection, DbErr};

use crate::auth::ensure_token;
use crate::users::membership::sync_group_memberships;
use crate::users::user::resolve_user;

/// Garantit l'existence d'un compte de service (jamais de login OIDC — pensé pour
/// l'automatisation de déploiement, ex. kuberest) et d'un token le référençant, avec une valeur
/// de token **fournie par l'appelant** (pas générée aléatoirement comme `issue_token`) —
/// typiquement lue d'une variable d'environnement, pour que l'automatisation de déploiement
/// connaisse le secret à l'avance sans devoir le récupérer après coup.
///
/// Pensée pour être appelée par l'app cible à son démarrage, après ses migrations, uniquement si
/// elle le décide. Idempotent : rejouable à chaque démarrage sans dupliquer ni le compte, ni ses
/// appartenances de groupe, ni le token.
pub async fn ensure_service_account(
    db: &DatabaseConnection,
    subject: &str,
    token: &str,
    token_name: &str,
    groups: &[String],
    expires_at: Option<DateTimeUtc>,
) -> Result<(), DbErr> {
    let user = resolve_user(db, subject, None).await?;
    sync_group_memberships(db, user.id, groups).await?;
    ensure_token(db, subject, token_name, token, expires_at)
        .await
        .map_err(|e| match e {
            crate::auth::AuthError::Database(db_err) => db_err,
            other => DbErr::Custom(other.to_string()),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::validate_token;
    use crate::migration::Migrator;
    use crate::users::group::is_admin;
    use sea_orm::entity::prelude::*;
    use sea_orm_migration::MigratorTrait;

    async fn test_db() -> DatabaseConnection {
        let db = sea_orm::Database::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite connects");
        Migrator::up(&db, None).await.expect("migrations apply cleanly");
        db
    }

    #[tokio::test]
    async fn creates_account_groups_and_token() {
        let db = test_db().await;
        ensure_service_account(
            &db,
            "system:kuberest",
            "mrd_bootstrap-secret",
            "bootstrap",
            &["admin".to_string()],
            None,
        )
        .await
        .expect("provisioning succeeds");

        let principal = validate_token(&db, "mrd_bootstrap-secret")
            .await
            .expect("token authenticates");
        assert_eq!(principal.subject, "system:kuberest");

        let user = resolve_user(&db, "system:kuberest", None)
            .await
            .expect("resolve succeeds");
        assert!(is_admin(&db, user.id).await.expect("query succeeds"));
    }

    #[tokio::test]
    async fn is_idempotent_across_restarts() {
        let db = test_db().await;
        for _ in 0..2 {
            ensure_service_account(
                &db,
                "system:kuberest",
                "mrd_bootstrap-secret",
                "bootstrap",
                &["admin".to_string()],
                None,
            )
            .await
            .expect("provisioning succeeds");
        }

        let user = resolve_user(&db, "system:kuberest", None)
            .await
            .expect("resolve succeeds");
        let memberships = crate::users::membership::Entity::find()
            .filter(crate::users::membership::Column::UserId.eq(user.id))
            .all(&db)
            .await
            .expect("query succeeds");
        assert_eq!(memberships.len(), 1);
    }

    #[tokio::test]
    async fn rotating_the_secret_adds_a_new_token_without_removing_the_old_one() {
        let db = test_db().await;
        ensure_service_account(&db, "system:kuberest", "mrd_first-secret", "bootstrap", &[], None)
            .await
            .expect("first provisioning succeeds");
        ensure_service_account(
            &db,
            "system:kuberest",
            "mrd_second-secret",
            "bootstrap",
            &[],
            None,
        )
        .await
        .expect("second provisioning succeeds");

        assert!(validate_token(&db, "mrd_first-secret").await.is_ok());
        assert!(validate_token(&db, "mrd_second-secret").await.is_ok());
    }
}
