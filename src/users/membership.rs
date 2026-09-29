use sea_orm::entity::prelude::*;
use sea_orm::{ConnectionTrait, Set};

use crate::users::group::ensure_group;

/// Ligne `DeriveEntityModel` de la table d'association `miryad_group_memberships` (posée par la
/// migration `m20260822_000002`) — trois colonnes, une ligne par appartenance.
#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "miryad_group_memberships")]
pub struct Model {
    /// Clé primaire `i32` auto-incrémentée — seule identité de la ligne, ciblée par la
    /// suppression de réconciliation.
    #[sea_orm(primary_key)]
    pub id: i32,
    /// `miryad_users.id` du membre — `FK` `ON DELETE CASCADE`, la paire (`user_id`, `group_id`)
    /// est `UNIQUE`.
    pub user_id: i32,
    /// `miryad_groups.id` du groupe — `FK` `ON DELETE CASCADE`, la paire (`user_id`, `group_id`)
    /// est `UNIQUE`.
    pub group_id: i32,
}

/// `DeriveRelation` déclaré avec un enum vide : entité sans relation `SeaORM`, invisible au
/// graphe d'entités malgré ses `FK` — les liens se lisent par filtres explicites.
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

/// Alias lisible de l'`Entity` généré par `DeriveEntityModel` — même type ; mis à plat sous
/// `crate::users::GroupMembership`.
pub type GroupMembership = Entity;

/// Réconciliation complète des appartenances de `user_id` depuis un claim `groups` OIDC : les
/// groupes absents sont retirés, les nouveaux sont ajoutés (créés à la volée si inconnus). Seul
/// chemin d'écriture de cette table — pas d'API d'assignation manuelle (Authentik est la source
/// de vérité, cf. `docs/architecture.md`).
///
/// # Errors
///
/// Aucune erreur à code `MRD-*` ici — toutes les pannes remontent en `DbErr` brut propagé par
/// `?` : via `ensure_group` une erreur de requête sur `miryad_groups`, ou le `DbErr` d'origine
/// de l'`INSERT` propagé verbatim quand la relance est vide (arbitré 2026-09-29 — plus de
/// `DbErr::RecordNotFound` « vanished » fabriqué, cf. `group.sdd`) ; et pour les find/insert/
/// delete une erreur de connexion, de contrainte non ciblée ou d'auto-incrément inaccessible —
/// les conflits ciblés sur (`user_id`, `group_id`) ne remontent pas (`ON CONFLICT DO NOTHING`).
/// Violation de FK à l'insertion quand `user_id` n'existe pas dans `miryad_users`.
pub async fn sync_group_memberships<C: ConnectionTrait>(
    db: &C,
    user_id: i32,
    groups: &[String],
) -> Result<(), DbErr> {
    let mut wanted_group_ids = Vec::with_capacity(groups.len());
    for name in groups {
        wanted_group_ids.push(ensure_group(db, name).await?);
    }

    let current = Entity::find().filter(Column::UserId.eq(user_id)).all(db).await?;

    for membership in &current {
        if !wanted_group_ids.contains(&membership.group_id) {
            Entity::delete_by_id(membership.id).exec(db).await?;
        }
    }

    let current_group_ids: Vec<i32> = current.iter().map(|m| m.group_id).collect();
    for group_id in wanted_group_ids {
        if !current_group_ids.contains(&group_id) {
            let active = ActiveModel {
                user_id: Set(user_id),
                group_id: Set(group_id),
                ..Default::default()
            };
            // `current_group_ids` est un instantané pris en début de fonction, jamais rafraîchi
            // pendant cette boucle : deux appels concurrents pour le même user_id (double
            // callback OIDC) peuvent tous les deux tenter d'insérer la même ligne. ON CONFLICT
            // DO NOTHING rend l'insertion idempotente sous concurrence sans retirer la
            // contrainte unique — le perdant de la course n'échoue plus, il n'a juste rien à
            // faire (la ligne existe déjà, posée par le gagnant).
            Entity::insert(active)
                .on_conflict_do_nothing_on([Column::UserId, Column::GroupId])
                .exec(db)
                .await?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migration::Migrator;
    use crate::users::user::resolve_user;
    use sea_orm_migration::MigratorTrait;

    async fn test_db() -> DatabaseConnection {
        let db = sea_orm::Database::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite connects");
        Migrator::up(&db, None).await.expect("migrations apply cleanly");
        db
    }

    #[tokio::test]
    async fn sync_adds_missing_groups_including_unknown_ones() {
        let db = test_db().await;
        let user = resolve_user(&db, "sub-1", None).await.expect("resolve succeeds");

        sync_group_memberships(&db, user.id, &["admin".to_string(), "editors".to_string()])
            .await
            .expect("sync succeeds");

        let memberships = Entity::find()
            .filter(Column::UserId.eq(user.id))
            .all(&db)
            .await
            .expect("query succeeds");
        assert_eq!(memberships.len(), 2);
    }

    #[tokio::test]
    async fn second_sync_with_fewer_groups_removes_stale_memberships() {
        let db = test_db().await;
        let user = resolve_user(&db, "sub-1", None).await.expect("resolve succeeds");

        sync_group_memberships(&db, user.id, &["admin".to_string(), "editors".to_string()])
            .await
            .expect("first sync succeeds");
        sync_group_memberships(&db, user.id, &["editors".to_string()])
            .await
            .expect("second sync succeeds");

        let memberships = Entity::find()
            .filter(Column::UserId.eq(user.id))
            .all(&db)
            .await
            .expect("query succeeds");
        assert_eq!(memberships.len(), 1);
    }

    #[tokio::test]
    async fn sync_is_idempotent() {
        let db = test_db().await;
        let user = resolve_user(&db, "sub-1", None).await.expect("resolve succeeds");

        sync_group_memberships(&db, user.id, &["editors".to_string()])
            .await
            .expect("first sync succeeds");
        sync_group_memberships(&db, user.id, &["editors".to_string()])
            .await
            .expect("second sync succeeds");

        let memberships = Entity::find()
            .filter(Column::UserId.eq(user.id))
            .all(&db)
            .await
            .expect("query succeeds");
        assert_eq!(memberships.len(), 1);
    }

    /// Reproduit le scénario de l'issue #3 : un double callback OIDC (prefetch navigateur, double
    /// requête) déclenche deux invocations concurrentes pour le même `user_id`. Les deux lisent
    /// le même instantané `current` avant que l'une ou l'autre n'ait committé ses insertions —
    /// sans ON CONFLICT DO NOTHING, la perdante de la course viole la contrainte unique
    /// `(user_id, group_id)` et l'appelante (le callback OIDC) échoue en entier alors que les
    /// données finissent par être correctes.
    #[tokio::test]
    async fn concurrent_syncs_for_the_same_user_do_not_violate_the_unique_constraint() {
        let db = test_db().await;
        let user = resolve_user(&db, "sub-1", None).await.expect("resolve succeeds");
        let groups = vec!["admin".to_string(), "editors".to_string()];

        let (first, second) = tokio::join!(
            sync_group_memberships(&db, user.id, &groups),
            sync_group_memberships(&db, user.id, &groups),
        );
        first.expect("first concurrent sync succeeds");
        second.expect("second concurrent sync succeeds");

        let memberships = Entity::find()
            .filter(Column::UserId.eq(user.id))
            .all(&db)
            .await
            .expect("query succeeds");
        assert_eq!(memberships.len(), 2);
    }
}
