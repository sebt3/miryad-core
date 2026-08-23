//! Logique métier des 5 opérations CRUD génériques, indépendante d'axum — utilisée par les
//! handlers REST (`rest/mod.rs`) et par les tools MCP (feature 6), pour ne jamais dupliquer les
//! règles RBAC/pagination/injection de propriétaire entre les deux surfaces d'API.

use sea_orm::{
    ActiveModelTrait, ColumnTrait, Condition, DatabaseConnection, IntoActiveModel, Iterable, PaginatorTrait,
    PrimaryKeyToColumn, QueryFilter,
};

use crate::auth::AuthPrincipal;
use crate::query::{PagedResult, Pagination};
use crate::rbac::{ListAccess, can_create, can_read, can_write, list_access};
use crate::resource::AccessPolicy;
use crate::rest::RestEntity;
use crate::rest::error::RestError;
use crate::users::resolve_user;

pub(crate) fn primary_key_column<E: RestEntity>() -> E::Column {
    E::PrimaryKey::iter()
        .next()
        .expect("RestEntity assumes exactly one primary key column")
        .into_column()
}

/// `Model::into_active_model()` (généré par `DeriveEntityModel`) marque tous les champs
/// `Unchanged`, pas `Set` — pertinent pour un modèle relu depuis la base, pas pour un corps de
/// requête PUT/POST qu'on veut écrire tel quel. `ActiveModelTrait::update()` n'inclut que les
/// champs `Set` dans la clause `SET` ; sans ce passage, un `PUT` n'écrirait aucune colonne
/// (`insert()` fonctionne quand même car il traite `Unchanged` comme une valeur à insérer, mais
/// `update()` ne le fait pas).
pub(crate) fn mark_all_set<E: RestEntity>(mut active: E::ActiveModel) -> E::ActiveModel {
    for col in E::Column::iter() {
        if let Some(value) = active.get(col).into_value() {
            active.set(col, value);
        }
    }
    active
}

pub(crate) async fn list<E: RestEntity>(
    db: &DatabaseConnection,
    principal: &AuthPrincipal,
    page: Option<u64>,
    per_page: Option<u64>,
    filter: Option<&str>,
) -> Result<PagedResult<E::Model>, RestError> {
    let user = resolve_user(db, &principal.subject, principal.email.as_deref()).await?;

    let condition = match list_access::<E>(db, &user).await? {
        ListAccess::Unrestricted => Condition::all(),
        ListAccess::FilterByOwner(condition) => condition,
        ListAccess::Forbidden => return Err(RestError::Forbidden),
    };
    let condition = match (E::filter_column(), filter) {
        (Some(col), Some(value)) => condition.add(col.eq(value)),
        _ => condition,
    };

    let pagination = Pagination::from_raw(page, per_page);
    let paginator = E::find().filter(condition).paginate(db, pagination.per_page);
    let totals = paginator.num_items_and_pages().await?;
    let items = paginator.fetch_page(pagination.page - 1).await?;

    Ok(PagedResult {
        items,
        page: pagination.page,
        per_page: pagination.per_page,
        total_items: totals.number_of_items,
        total_pages: totals.number_of_pages,
    })
}

pub(crate) async fn get<E: RestEntity>(
    db: &DatabaseConnection,
    principal: &AuthPrincipal,
    id: i32,
) -> Result<E::Model, RestError> {
    let user = resolve_user(db, &principal.subject, principal.email.as_deref()).await?;
    let record = E::find_by_id(id).one(db).await?.ok_or(RestError::NotFound)?;

    if !can_read::<E>(db, &user, &record).await? {
        return Err(RestError::Forbidden);
    }
    Ok(record)
}

pub(crate) async fn create<E: RestEntity>(
    db: &DatabaseConnection,
    principal: &AuthPrincipal,
    body: E::Model,
) -> Result<E::Model, RestError> {
    let user = resolve_user(db, &principal.subject, principal.email.as_deref()).await?;

    if !can_create::<E>(db, &user).await? {
        return Err(RestError::Forbidden);
    }

    let mut active = mark_all_set::<E>(body.into_active_model());
    // Le hook métier s'exécute avant le PK-stripping/l'injection du propriétaire ci-dessous, pour
    // que ces deux invariants de sécurité restent les derniers mots — un hook buggé ne peut pas
    // les contourner en mutant l'ActiveModel.
    active = E::before_create(active, principal).map_err(RestError::Application)?;
    // La BD attribue l'id — jamais une PK choisie par le client.
    active.not_set(primary_key_column::<E>());
    // Un utilisateur ne peut jamais créer une ressource au nom de quelqu'un d'autre, même en le
    // demandant explicitement dans le corps de la requête.
    if E::write_policy() == AccessPolicy::OwnerOnly
        && let Some(owner_col) = E::owner_column()
    {
        active.set(owner_col, sea_orm::Value::from(user.id));
    }

    Ok(active.insert(db).await?)
}

pub(crate) async fn update<E: RestEntity>(
    db: &DatabaseConnection,
    principal: &AuthPrincipal,
    id: i32,
    body: E::Model,
) -> Result<E::Model, RestError> {
    let user = resolve_user(db, &principal.subject, principal.email.as_deref()).await?;
    let existing = E::find_by_id(id).one(db).await?.ok_or(RestError::NotFound)?;

    if !can_write::<E>(db, &user, &existing).await? {
        return Err(RestError::Forbidden);
    }

    let mut active = mark_all_set::<E>(body.into_active_model());
    // Force la PK depuis le chemin — ignore toute divergence dans le corps de la requête.
    active.set(primary_key_column::<E>(), sea_orm::Value::from(id));

    Ok(active.update(db).await?)
}

pub(crate) async fn delete<E: RestEntity>(
    db: &DatabaseConnection,
    principal: &AuthPrincipal,
    id: i32,
) -> Result<(), RestError> {
    let user = resolve_user(db, &principal.subject, principal.email.as_deref()).await?;
    let existing = E::find_by_id(id).one(db).await?.ok_or(RestError::NotFound)?;

    if !can_write::<E>(db, &user, &existing).await? {
        return Err(RestError::Forbidden);
    }

    E::delete_by_id(id).exec(db).await?;
    Ok(())
}
