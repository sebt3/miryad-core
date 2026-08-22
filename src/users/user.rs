use chrono::Utc;
use sea_orm::entity::prelude::*;
use sea_orm::{ConnectionTrait, Set};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "miryad_users")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    /// Claim `sub` OIDC — lien avec `AuthPrincipal.subject` (feature 2b).
    #[sea_orm(unique)]
    pub subject: String,
    pub email: Option<String>,
    pub display_name: Option<String>,
    pub created_at: DateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

pub type User = Entity;

/// Get-or-create par `subject`. Pas de vraie contrainte `ON CONFLICT` portable entre SQLite/
/// Postgres exploitée ici — en cas de course (deux premiers logins concurrents du même
/// `subject`), l'`insert` échoue sur la contrainte unique et on retombe sur un `find` pour
/// récupérer la ligne posée par l'autre requête, plutôt que de propager l'erreur.
pub async fn resolve_user<C: ConnectionTrait>(
    db: &C,
    subject: &str,
    email: Option<&str>,
) -> Result<Model, DbErr> {
    if let Some(existing) = Entity::find().filter(Column::Subject.eq(subject)).one(db).await? {
        return Ok(existing);
    }

    let active = ActiveModel {
        subject: Set(subject.to_string()),
        email: Set(email.map(str::to_string)),
        display_name: Set(None),
        created_at: Set(Utc::now()),
        ..Default::default()
    };

    match active.insert(db).await {
        Ok(model) => Ok(model),
        Err(_) => Entity::find()
            .filter(Column::Subject.eq(subject))
            .one(db)
            .await?
            .ok_or_else(|| DbErr::RecordNotFound(format!("user with subject {subject} vanished"))),
    }
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
    async fn resolve_user_creates_then_reuses_same_row() {
        let db = test_db().await;
        let first = resolve_user(&db, "sub-1", Some("a@example.com"))
            .await
            .expect("first resolve succeeds");
        let second = resolve_user(&db, "sub-1", Some("a@example.com"))
            .await
            .expect("second resolve succeeds");
        assert_eq!(first.id, second.id);
    }
}
