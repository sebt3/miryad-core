use std::collections::HashMap;

use sea_orm::DatabaseConnection;
use serde_json::Value;

use crate::auth::AuthPrincipal;
use crate::mcp::error::McpError;
use crate::mcp::format::OutputFormat;
use crate::rest::RestEntity;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum McpOp {
    List,
    Get,
    Create,
    Update,
    Delete,
}

/// Dispatch par nom d'entité (runtime, comme en GraphQL — cf. `graphql::PolicyRegistry`) plutôt
/// que par type : `tools/call` arrive avec un nom de méthode string, pas un type Rust.
#[async_trait::async_trait]
pub(crate) trait McpEntity: Send + Sync {
    fn resource_name(&self) -> &'static str;
    async fn call(
        &self,
        op: McpOp,
        db: &DatabaseConnection,
        principal: &AuthPrincipal,
        params: Value,
    ) -> Result<Value, McpError>;
}

struct McpEntityImpl<E>(std::marker::PhantomData<E>);

#[derive(serde::Deserialize)]
struct ListParams {
    #[serde(default)]
    page: Option<u64>,
    #[serde(default)]
    per_page: Option<u64>,
    #[serde(default)]
    filter: Option<String>,
}

#[derive(serde::Deserialize)]
struct IdParams {
    id: i32,
}

#[async_trait::async_trait]
impl<E: RestEntity> McpEntity for McpEntityImpl<E> {
    fn resource_name(&self) -> &'static str {
        E::resource_name()
    }

    async fn call(
        &self,
        op: McpOp,
        db: &DatabaseConnection,
        principal: &AuthPrincipal,
        params: Value,
    ) -> Result<Value, McpError> {
        match op {
            McpOp::List => {
                let params: ListParams =
                    serde_json::from_value(params).map_err(|e| McpError::InvalidParams(e.to_string()))?;
                let page = crate::rest::core::list::<E>(
                    db,
                    principal,
                    params.page,
                    params.per_page,
                    params.filter.as_deref(),
                )
                .await?;
                serde_json::to_value(page).map_err(|e| McpError::Render(e.to_string()))
            }
            McpOp::Get => {
                let params: IdParams =
                    serde_json::from_value(params).map_err(|e| McpError::InvalidParams(e.to_string()))?;
                let record = crate::rest::core::get::<E>(db, principal, params.id).await?;
                serde_json::to_value(record).map_err(|e| McpError::Render(e.to_string()))
            }
            McpOp::Create => {
                let body: E::Model =
                    serde_json::from_value(params).map_err(|e| McpError::InvalidParams(e.to_string()))?;
                let created = crate::rest::core::create::<E>(db, principal, body).await?;
                serde_json::to_value(created).map_err(|e| McpError::Render(e.to_string()))
            }
            McpOp::Update => {
                // `id` est à la fois une clé de dispatch et un champ de `E::Model` (PK SeaORM,
                // non optionnel) : les extraire via un `#[serde(flatten)]` ferait perdre `id` au
                // profit du champ nommé, laissant `E::Model` sans PK à désérialiser. On lit donc
                // `params` deux fois — une fois pour `id` seul, une fois pour le modèle complet
                // (même convention que REST : la PK du corps est de toute façon écrasée par
                // `core::update`, cf. `rest/core.rs`).
                let IdParams { id } = serde_json::from_value(params.clone())
                    .map_err(|e| McpError::InvalidParams(e.to_string()))?;
                let body: E::Model =
                    serde_json::from_value(params).map_err(|e| McpError::InvalidParams(e.to_string()))?;
                let updated = crate::rest::core::update::<E>(db, principal, id, body).await?;
                serde_json::to_value(updated).map_err(|e| McpError::Render(e.to_string()))
            }
            McpOp::Delete => {
                let params: IdParams =
                    serde_json::from_value(params).map_err(|e| McpError::InvalidParams(e.to_string()))?;
                crate::rest::core::delete::<E>(db, principal, params.id).await?;
                Ok(Value::Null)
            }
        }
    }
}

/// Registre des entités montées sur le serveur MCP, avec le format de sortie choisi une fois
/// pour toute l'app (cf. `OutputFormat`).
pub struct McpToolRegistry {
    pub(crate) format: OutputFormat,
    pub(crate) entities: HashMap<&'static str, Box<dyn McpEntity>>,
}

impl McpToolRegistry {
    pub fn new(format: OutputFormat) -> Self {
        Self {
            format,
            entities: HashMap::new(),
        }
    }

    /// Enregistre les 5 tools (`{resource_name}_list`, `_get`, `_create`, `_update`, `_delete`)
    /// pour `E`.
    pub fn register<E: RestEntity>(&mut self) -> &mut Self {
        self.entities.insert(
            E::resource_name(),
            Box::new(McpEntityImpl::<E>(std::marker::PhantomData)),
        );
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::PrincipalSource;
    use crate::migration::Migrator;
    use crate::resource::{AccessPolicy, MiryadResource};
    use sea_orm::entity::prelude::*;
    use sea_orm::{ConnectionTrait, Database, Schema};
    use sea_orm_migration::MigratorTrait;
    use serde::{Deserialize, Serialize};
    use serde_json::json;

    mod recipe {
        use super::*;

        #[derive(Clone, Debug, PartialEq, Serialize, Deserialize, DeriveEntityModel)]
        #[sea_orm(table_name = "recipes")]
        pub struct Model {
            #[sea_orm(primary_key)]
            pub id: i32,
            pub title: String,
            pub owner_id: i32,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {}

        impl ActiveModelBehavior for ActiveModel {}

        impl MiryadResource for Entity {
            fn resource_name() -> &'static str {
                "recipes"
            }
            fn read_policy() -> AccessPolicy {
                AccessPolicy::OwnerOnly
            }
            fn write_policy() -> AccessPolicy {
                AccessPolicy::OwnerOnly
            }
            fn owner_column() -> Option<Column> {
                Some(Column::OwnerId)
            }
        }
    }

    #[test]
    fn register_makes_entity_dispatchable_by_name() {
        let mut registry = McpToolRegistry::new(OutputFormat::Json);
        registry.register::<recipe::Entity>();

        assert!(registry.entities.contains_key("recipes"));
        assert_eq!(
            registry.entities.get("recipes").unwrap().resource_name(),
            "recipes"
        );
    }

    async fn test_db() -> DatabaseConnection {
        let db = Database::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite connects");
        Migrator::up(&db, None).await.expect("migrations apply cleanly");
        let schema = Schema::new(db.get_database_backend());
        db.execute(&schema.create_table_from_entity(recipe::Entity))
            .await
            .expect("recipes table creates");
        db
    }

    fn principal(subject: &str) -> AuthPrincipal {
        AuthPrincipal {
            subject: subject.to_string(),
            email: None,
            source: PrincipalSource::ApiToken { token_id: 0 },
        }
    }

    // Régression : `UpdateParams<E::Model>` avec `#[serde(flatten)]` faisait perdre le champ
    // `id` (consommé par le champ nommé de l'enveloppe, jamais transmis à `E::Model` qui le
    // requiert comme PK non optionnelle) — `_update` échouait systématiquement avec "missing
    // field `id`" dès qu'un vrai modèle était utilisé.
    #[tokio::test]
    async fn update_dispatch_accepts_id_alongside_full_model_body() {
        let db = test_db().await;
        let alice = principal("alice");
        let entity = McpEntityImpl::<recipe::Entity>(std::marker::PhantomData);

        let created = entity
            .call(
                McpOp::Create,
                &db,
                &alice,
                json!({"id": 0, "title": "Tarte", "owner_id": 0}),
            )
            .await
            .expect("create dispatch succeeds");
        let id = created["id"].as_i64().expect("id present");

        let updated = entity
            .call(
                McpOp::Update,
                &db,
                &alice,
                json!({"id": id, "title": "Tarte modifiee", "owner_id": 0}),
            )
            .await
            .expect("update dispatch succeeds");

        assert_eq!(updated["title"], "Tarte modifiee");
    }
}
