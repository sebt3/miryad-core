//! Snapshot d'autorisation précalculé par requête GraphQL — `load_principal` résout
//! une seule fois, avant l'exécution du schéma, l'identité interne du principal
//! authentifié (`resolve_user` get-or-create, puis relecture des appartenances et des
//! groupes), et `GraphQlPrincipal` est ce que les hooks `seaography`, synchrones et sans
//! accès base, lisent comme donnée de requête. Un même `subject` produit le même
//! snapshot depuis une session ou un token API : la source d'authentification s'efface.
//! Le fichier ne décide aucune permission et ne porte aucun code `MRD-*` ; toute erreur
//! de base est un `DbErr` propagé verbatim. Ne compile que sous la feature `graphql`.

use std::collections::HashSet;

use sea_orm::DatabaseConnection;
use sea_orm::entity::prelude::*;

use crate::auth::AuthPrincipal;
use crate::users::group::ADMIN_GROUP_NAME;
use crate::users::{group, membership, resolve_user};

/// Snapshot précalculé du principal courant — une seule fois par requête HTTP, avant
/// `schema.execute(...)`. Les hooks Seaography (`entity_guard`/`entity_filter`) sont synchrones :
/// ils ne font aucun accès DB, ils ne font que lire ce snapshot déjà injecté comme donnée de
/// requête.
#[derive(Debug, Clone)]
pub struct GraphQlPrincipal {
    /// Identifiant interne de la ligne `miryad_users` résolue depuis le `subject` du
    /// principal.
    pub user_id: i32,
    /// Appartenance au groupe admin, calculée depuis les appartenances lues en base.
    pub is_admin: bool,
    /// Noms des groupes de l'utilisateur — source de vérité en lecture des hooks.
    pub groups: HashSet<String>,
}

/// Calcule le snapshot d'autorisation d'un principal authentifié : résolution de
/// l'utilisateur (get-or-create sur `miryad_users`), relecture de ses appartenances et
/// des groupes pointés, calcul du drapeau admin. Échec possible uniquement sur un
/// `DbErr` de `sea-orm`, propagé verbatim par l'opérateur `?`.
pub async fn load_principal(
    db: &DatabaseConnection,
    principal: &AuthPrincipal,
) -> Result<GraphQlPrincipal, DbErr> {
    let user = resolve_user(db, &principal.subject, principal.email.as_deref()).await?;

    let group_ids: Vec<i32> = membership::Entity::find()
        .filter(membership::Column::UserId.eq(user.id))
        .all(db)
        .await?
        .into_iter()
        .map(|m| m.group_id)
        .collect();

    let groups: HashSet<String> = if group_ids.is_empty() {
        HashSet::new()
    } else {
        group::Entity::find()
            .filter(group::Column::Id.is_in(group_ids))
            .all(db)
            .await?
            .into_iter()
            .map(|g| g.name)
            .collect()
    };

    let is_admin = groups.contains(ADMIN_GROUP_NAME);

    Ok(GraphQlPrincipal {
        user_id: user.id,
        is_admin,
        groups,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::PrincipalSource;
    use crate::migration::Migrator;
    use crate::users::sync_group_memberships;
    use sea_orm_migration::MigratorTrait;

    async fn test_db() -> DatabaseConnection {
        let db = sea_orm::Database::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite connects");
        Migrator::up(&db, None).await.expect("migrations apply cleanly");
        db
    }

    #[tokio::test]
    async fn reflects_admin_status_and_groups() {
        let db = test_db().await;
        let user = resolve_user(&db, "alice", None).await.expect("resolve");
        sync_group_memberships(&db, user.id, &["admin".to_string(), "editors".to_string()])
            .await
            .expect("sync");

        let principal = AuthPrincipal {
            subject: "alice".to_string(),
            email: None,
            preferred_username: None,
            source: PrincipalSource::Session {
                id_token: String::new(),
            },
        };
        let snapshot = load_principal(&db, &principal).await.expect("loads");

        assert_eq!(snapshot.user_id, user.id);
        assert!(snapshot.is_admin);
        assert!(snapshot.groups.contains("editors"));
    }

    #[tokio::test]
    async fn non_admin_user_has_empty_or_partial_groups() {
        let db = test_db().await;
        let principal = AuthPrincipal {
            subject: "stranger".to_string(),
            email: None,
            preferred_username: None,
            source: PrincipalSource::Session {
                id_token: String::new(),
            },
        };
        let snapshot = load_principal(&db, &principal).await.expect("loads");

        assert!(!snapshot.is_admin);
        assert!(snapshot.groups.is_empty());
    }
}
