use async_graphql::dynamic::Schema;
use async_graphql_axum::{GraphQLRequest, GraphQLResponse};
use axum::extract::{FromRef, State};

use crate::auth::{AuthError, AuthPrincipal, MiryadAuthState};
use crate::graphql::principal::load_principal;

/// Monte `POST /api/graphql` (et `GET /api/graphiql` sous la feature `graphiql`) à partir d'un
/// `Schema` déjà construit par l'app (via `seaography::Builder`, hooks compris — cf.
/// `MiryadHooks`). Réutilise `MiryadAuthState` comme les autres routeurs (REST, auth) — rien de
/// nouveau à composer côté app au-delà du `Schema` lui-même. Préfixe `/api` figé dans le crate
/// (feature 6) — chemins absolus plutôt que `.nest("/api", ...)` : `graphiql_handler` embarque
/// `/api/graphql` comme URL de fetch dans le HTML servi, un nest externe désynchroniserait la
/// route réellement montée de celle que l'UI interroge (même piège que `swagger_ui_router`).
pub fn graphql_router<S>(schema: Schema) -> axum::Router<S>
where
    S: Clone + Send + Sync + 'static,
    MiryadAuthState: FromRef<S>,
{
    let router = axum::Router::new()
        .route("/api/graphql", axum::routing::post(graphql_handler))
        .layer(axum::Extension(schema));

    #[cfg(feature = "graphiql")]
    let router = router.route("/api/graphiql", axum::routing::get(graphiql_handler));

    router
}

async fn graphql_handler(
    State(auth): State<MiryadAuthState>,
    principal: AuthPrincipal,
    axum::Extension(schema): axum::Extension<Schema>,
    req: GraphQLRequest,
) -> Result<GraphQLResponse, AuthError> {
    let snapshot = load_principal(&auth.db, &principal).await?;
    // `snapshot` (GraphQlPrincipal) sert au RBAC (entity_guard/entity_filter, déjà en place).
    // `principal` (AuthPrincipal) est aussi injecté pour before_active_model_save (feature 7b) :
    // le hook métier doit recevoir le même type de principal que REST/MCP, pas un dérivé GraphQL.
    // `auth.db` : attendu par tout resolver généré par `seaography::register_entity!`
    // (`ctx.data::<DatabaseConnection>()`, pattern standard Seaography/SeaORM).
    let request = req
        .into_inner()
        .data(auth.db.clone())
        .data(snapshot)
        .data(principal);
    Ok(schema.execute(request).await.into())
}

#[cfg(feature = "graphiql")]
async fn graphiql_handler() -> axum::response::Html<String> {
    axum::response::Html(
        async_graphql::http::GraphiQLSource::build()
            .endpoint("/api/graphql")
            .finish(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_graphql::Value;
    use async_graphql::dynamic::{Field, FieldFuture, Object, TypeRef};
    use axum::Router;
    use axum::body::Body;
    use axum::http::Request;
    use sea_orm::{Database, DatabaseConnection};
    use sea_orm_migration::MigratorTrait;
    use tower::ServiceExt;

    use crate::auth::MockOidcClient;
    use crate::auth::issue_token;
    use crate::migration::Migrator;

    async fn test_db() -> DatabaseConnection {
        let db = Database::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite connects");
        Migrator::up(&db, None).await.expect("migrations apply cleanly");
        db
    }

    fn test_state(db: DatabaseConnection) -> MiryadAuthState {
        MiryadAuthState {
            oidc_client: std::sync::Arc::new(MockOidcClient),
            cookie_key: ::cookie::Key::from(&[0u8; 64]),
            post_login_redirect: "/".to_string(),
            post_logout_redirect: "/".to_string(),
            db,
        }
    }

    /// Schéma minimal, indépendant de `seaography::register_entity!` (dont la génération réelle
    /// exige un `RelatedEntity` codegen hors du périmètre de ce test) — reproduit exactement le
    /// même appel que les resolvers Seaography (`ctx.data::<DatabaseConnection>()`), donc exerce
    /// le même défaut d'intégration que l'issue #7.
    fn db_check_schema() -> Schema {
        let query =
            Object::new("Query").field(Field::new("dbCheck", TypeRef::named_nn(TypeRef::STRING), |ctx| {
                FieldFuture::new(async move {
                    ctx.data::<DatabaseConnection>()?;
                    Ok(Some(Value::from("ok")))
                })
            }));
        Schema::build("Query", None, None)
            .register(query)
            .finish()
            .expect("schema builds")
    }

    #[tokio::test]
    async fn graphql_handler_injects_database_connection_into_request_context() {
        let db = test_db().await;
        let token = issue_token(&db, "alice", "test", None)
            .await
            .expect("issuing succeeds")
            .token;
        let state = test_state(db);
        let app: Router = graphql_router::<MiryadAuthState>(db_check_schema()).with_state(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/graphql")
                    .header("Authorization", format!("Bearer {token}"))
                    .header("Content-Type", "application/json")
                    .body(Body::from(
                        serde_json::json!({ "query": "{ dbCheck }" }).to_string(),
                    ))
                    .expect("valid request"),
            )
            .await
            .expect("request succeeds");

        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("readable body");
        let body: serde_json::Value = serde_json::from_slice(&bytes).expect("valid JSON body");

        assert_eq!(body["data"]["dbCheck"], "ok", "graphql response: {body}");
    }
}
