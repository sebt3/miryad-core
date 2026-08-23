use async_graphql::dynamic::Schema;
use async_graphql_axum::{GraphQLRequest, GraphQLResponse};
use axum::extract::{FromRef, State};

use crate::auth::{AuthError, AuthPrincipal, MiryadAuthState};
use crate::graphql::principal::load_principal;

/// Monte `POST /graphql` (et `GET /graphiql` sous la feature `graphiql`) à partir d'un `Schema`
/// déjà construit par l'app (via `seaography::Builder`, hooks compris — cf. `MiryadHooks`).
/// Réutilise `MiryadAuthState` comme les autres routeurs (REST, auth) — rien de nouveau à
/// composer côté app au-delà du `Schema` lui-même.
pub fn graphql_router<S>(schema: Schema) -> axum::Router<S>
where
    S: Clone + Send + Sync + 'static,
    MiryadAuthState: FromRef<S>,
{
    let router = axum::Router::new()
        .route("/graphql", axum::routing::post(graphql_handler))
        .layer(axum::Extension(schema));

    #[cfg(feature = "graphiql")]
    let router = router.route("/graphiql", axum::routing::get(graphiql_handler));

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
    let request = req.into_inner().data(snapshot).data(principal);
    Ok(schema.execute(request).await.into())
}

#[cfg(feature = "graphiql")]
async fn graphiql_handler() -> axum::response::Html<String> {
    axum::response::Html(
        async_graphql::http::GraphiQLSource::build()
            .endpoint("/graphql")
            .finish(),
    )
}
