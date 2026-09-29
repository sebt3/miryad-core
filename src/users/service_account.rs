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
/// `pepper` : poivre HMAC des empreintes (cf. `auth::token`) — en pratique
/// `MiryadAuthState::token_pepper`, transmis à `ensure_token` (arbitrage 2026-09-27).
///
/// Pensée pour être appelée par l'app cible à son démarrage, après ses migrations, uniquement si
/// elle le décide. Idempotent : rejouable à chaque démarrage sans dupliquer ni le compte, ni ses
/// appartenances de groupe, ni le token.
///
/// # Errors
///
/// Aucun code `MRD-*` inventé ici — le fichier ne fait que propager et aplatir :
/// - `sea_orm::DbErr` nu de `resolve_user` : panne de connexion, table absente, ou le `DbErr`
///   d'origine de l'`INSERT` propagé verbatim quand la relecture est vide (arbitré 2026-09-29 —
///   plus de `DbErr::RecordNotFound` « vanished » fabriqué, cf. `user.sdd`) ;
/// - `sea_orm::DbErr` de `sync_group_memberships` : toute panne de lecture/insertion/suppression
///   des appartenances ou de `group::ensure_group` ;
/// - `sea_orm::DbErr` d'`ensure_token` via `AuthError::Database` (déballé nu) : panne des
///   opérations de base, collision UNIQUE de `token_hash` sous concurrence comprise ;
/// - `DbErr::Custom` : seulement si `ensure_token` rendait une variante d'`AuthError`
///   non-`Database` — `ensure_token` n'en produit aucune aujourd'hui, branche inexercée.
pub async fn ensure_service_account(
    db: &DatabaseConnection,
    subject: &str,
    token: &str,
    token_name: &str,
    groups: &[String],
    expires_at: Option<DateTimeUtc>,
    pepper: &str,
) -> Result<(), DbErr> {
    let user = resolve_user(db, subject, None).await?;
    sync_group_memberships(db, user.id, groups).await?;
    ensure_token(db, subject, token_name, token, expires_at, pepper)
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
            "test-pepper",
        )
        .await
        .expect("provisioning succeeds");

        let principal = validate_token(&db, "mrd_bootstrap-secret", "test-pepper")
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
                "test-pepper",
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
        ensure_service_account(
            &db,
            "system:kuberest",
            "mrd_first-secret",
            "bootstrap",
            &[],
            None,
            "test-pepper",
        )
        .await
        .expect("first provisioning succeeds");
        ensure_service_account(
            &db,
            "system:kuberest",
            "mrd_second-secret",
            "bootstrap",
            &[],
            None,
            "test-pepper",
        )
        .await
        .expect("second provisioning succeeds");

        assert!(
            validate_token(&db, "mrd_first-secret", "test-pepper")
                .await
                .is_ok()
        );
        assert!(
            validate_token(&db, "mrd_second-secret", "test-pepper")
                .await
                .is_ok()
        );
    }
}
