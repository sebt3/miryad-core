use utoipa::openapi::path::{HttpMethod, OperationBuilder, ParameterBuilder, ParameterIn};
use utoipa::openapi::request_body::RequestBodyBuilder;
use utoipa::openapi::response::ResponseBuilder;
use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityRequirement, SecurityScheme};
use utoipa::openapi::{
    ArrayBuilder, ComponentsBuilder, ContentBuilder, Object, OpenApi, OpenApiBuilder, Paths, Ref, RefOr,
    Required, Schema, Type,
};
use utoipa::{PartialSchema, ToSchema};

use crate::rest::RestEntity;

/// Nom du `SecurityScheme` déclaré par `resource_openapi` — le dual-auth de miryad-core accepte
/// aussi un cookie de session OIDC, mais celui-ci est `HttpOnly`/chiffré et n'a rien d'utilisable
/// depuis le champ "Authorize" de Swagger UI ; seul le token API (`issue_token`) est actionnable
/// depuis cette interface.
const BEARER_SECURITY_SCHEME: &str = "bearer_auth";

/// Entités éligibles à la génération OpenAPI — en plus de `RestEntity`, `Model` doit dériver
/// `utoipa::ToSchema` pour que sa forme JSON soit décrite dans le document généré.
pub trait OpenApiEntity: RestEntity<Model: ToSchema> {}
impl<E> OpenApiEntity for E where E: RestEntity<Model: ToSchema> {}

/// Fragment OpenAPI pour les 5 routes CRUD d'une entité (`GET/POST /api/v1/{resource_name}`,
/// `GET/PUT/DELETE /api/v1/{resource_name}/{id}`) — à fusionner avec celui des autres entités
/// montées (`utoipa::openapi::OpenApi::merge`) avant publication. Ne fixe pas `info`
/// (titre/version) : l'app renseigne ces champs sur le document final après fusion. Les chemins
/// suivent le préfixe figé de `resource_router` (feature 6) — toujours à jour vis-à-vis des
/// routes REST réellement montées. Déclare un `SecurityScheme` Bearer (feature 2) : le bouton
/// "Authorize" de Swagger UI fonctionne sans configuration côté app — `OpenApi::merge` dédoublonne
/// le schéma et l'exigence de sécurité par nom/égalité entre fragments d'entités.
pub fn resource_openapi<E: OpenApiEntity>() -> OpenApi {
    let resource = E::resource_name();
    let schema_name = E::Model::name().into_owned();
    let model_ref = RefOr::Ref(Ref::from_schema_name(schema_name.clone()));

    let mut components = ComponentsBuilder::new().schema(schema_name.clone(), E::Model::schema());
    let mut nested_schemas = Vec::new();
    E::Model::schemas(&mut nested_schemas);
    for (name, schema) in nested_schemas {
        components = components.schema(name, schema);
    }

    let paged_schema_name = format!("Paged{schema_name}");
    let paged_schema = Schema::Object(
        Object::builder()
            .property(
                "items",
                RefOr::T(Schema::Array(
                    ArrayBuilder::new().items(model_ref.clone()).build(),
                )),
            )
            .property("page", Object::with_type(Type::Integer))
            .property("per_page", Object::with_type(Type::Integer))
            .property("total_items", Object::with_type(Type::Integer))
            .property("total_pages", Object::with_type(Type::Integer))
            .required("items")
            .required("page")
            .required("per_page")
            .required("total_items")
            .required("total_pages")
            .build(),
    );
    let components = components
        .schema(paged_schema_name.clone(), RefOr::T(paged_schema))
        .security_scheme(
            BEARER_SECURITY_SCHEME,
            SecurityScheme::Http(
                HttpBuilder::new()
                    .scheme(HttpAuthScheme::Bearer)
                    .description(Some(
                        "Coller le token seul, sans le préfixe \"Bearer\" : Swagger UI l'ajoute automatiquement.",
                    ))
                    .build(),
            ),
        )
        .build();

    let query_param = |name: &str, schema_type: Type| {
        ParameterBuilder::new()
            .name(name)
            .parameter_in(ParameterIn::Query)
            .required(Required::False)
            .schema(Some(RefOr::T(Schema::Object(Object::with_type(schema_type)))))
    };
    let id_param = ParameterBuilder::new()
        .name("id")
        .parameter_in(ParameterIn::Path)
        .required(Required::True)
        .schema(Some(RefOr::T(Schema::Object(Object::with_type(Type::Integer)))))
        .build();

    let json_content = |schema: RefOr<Schema>| ContentBuilder::new().schema(Some(schema)).build();

    let mut paths = Paths::new();

    let list_op = OperationBuilder::new()
        .parameter(query_param("page", Type::Integer))
        .parameter(query_param("per_page", Type::Integer))
        .parameter(query_param("filter", Type::String))
        .response(
            "200",
            ResponseBuilder::new()
                .description("Liste paginée")
                .content(
                    "application/json",
                    json_content(RefOr::Ref(Ref::from_schema_name(paged_schema_name))),
                )
                .build(),
        )
        .build();
    paths.add_path_operation(format!("/api/v1/{resource}"), vec![HttpMethod::Get], list_op);

    let create_op = OperationBuilder::new()
        .request_body(Some(
            RequestBodyBuilder::new()
                .content("application/json", json_content(model_ref.clone()))
                .build(),
        ))
        .response(
            "200",
            ResponseBuilder::new()
                .description("Créé")
                .content("application/json", json_content(model_ref.clone()))
                .build(),
        )
        .response("403", ResponseBuilder::new().description("Refusé").build())
        .build();
    paths.add_path_operation(format!("/api/v1/{resource}"), vec![HttpMethod::Post], create_op);

    let get_op = OperationBuilder::new()
        .parameter(id_param.clone())
        .response(
            "200",
            ResponseBuilder::new()
                .description("Trouvé")
                .content("application/json", json_content(model_ref.clone()))
                .build(),
        )
        .response("403", ResponseBuilder::new().description("Refusé").build())
        .response("404", ResponseBuilder::new().description("Non trouvé").build())
        .build();
    paths.add_path_operation(
        format!("/api/v1/{resource}/{{id}}"),
        vec![HttpMethod::Get],
        get_op,
    );

    let update_op = OperationBuilder::new()
        .parameter(id_param.clone())
        .request_body(Some(
            RequestBodyBuilder::new()
                .content("application/json", json_content(model_ref.clone()))
                .build(),
        ))
        .response(
            "200",
            ResponseBuilder::new()
                .description("Mis à jour")
                .content("application/json", json_content(model_ref.clone()))
                .build(),
        )
        .response("403", ResponseBuilder::new().description("Refusé").build())
        .response("404", ResponseBuilder::new().description("Non trouvé").build())
        .build();
    paths.add_path_operation(
        format!("/api/v1/{resource}/{{id}}"),
        vec![HttpMethod::Put],
        update_op,
    );

    let delete_op = OperationBuilder::new()
        .parameter(id_param)
        .response("204", ResponseBuilder::new().description("Supprimé").build())
        .response("403", ResponseBuilder::new().description("Refusé").build())
        .response("404", ResponseBuilder::new().description("Non trouvé").build())
        .build();
    paths.add_path_operation(
        format!("/api/v1/{resource}/{{id}}"),
        vec![HttpMethod::Delete],
        delete_op,
    );

    OpenApiBuilder::new()
        .paths(paths)
        .components(Some(components))
        .security(Some([SecurityRequirement::new(
            BEARER_SECURITY_SCHEME,
            Vec::<String>::new(),
        )]))
        .build()
}

/// Sert `GET /api/openapi.json` à partir d'un document déjà fusionné — toujours disponible, pas
/// besoin de la feature `swagger-ui`. Ne pas combiner avec `swagger_ui_router` (celui-ci sert
/// déjà `/api/openapi.json` lui-même) : utiliser l'un ou l'autre, pas les deux.
pub fn openapi_router<S>(spec: OpenApi) -> axum::Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    axum::Router::new().route(
        "/api/openapi.json",
        axum::routing::get(move || {
            let spec = spec.clone();
            async move { axum::Json(spec) }
        }),
    )
}

/// Monte Swagger UI sur `/api/swagger-ui`, qui sert aussi `/api/openapi.json` lui-même (mécanisme
/// natif d'`utoipa-swagger-ui`) — ne pas fusionner en plus avec `openapi_router`, ça
/// collisionnerait sur `/api/openapi.json`. Chemins absolus plutôt que `.nest("/api", ...)` :
/// `.url(...)` est aussi ce que le JS de Swagger UI embarque comme URL de fetch — un nest
/// externe désynchroniserait la route réellement montée de celle que l'UI interroge.
#[cfg(feature = "swagger-ui")]
pub fn swagger_ui_router<S>(spec: OpenApi) -> axum::Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    axum::Router::new()
        .merge(utoipa_swagger_ui::SwaggerUi::new("/api/swagger-ui").url("/api/openapi.json", spec))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    mod recipe {
        use crate::resource::{AccessPolicy, MiryadResource};
        use sea_orm::entity::prelude::*;
        use serde::{Deserialize, Serialize};
        use utoipa::ToSchema;

        #[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema, DeriveEntityModel)]
        #[schema(as = Recipe)]
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
                AccessPolicy::Public
            }
            fn write_policy() -> AccessPolicy {
                AccessPolicy::OwnerOnly
            }
            fn owner_column() -> Option<Column> {
                Some(Column::OwnerId)
            }
        }
    }

    mod ingredient {
        use crate::resource::{AccessPolicy, MiryadResource};
        use sea_orm::entity::prelude::*;
        use serde::{Deserialize, Serialize};
        use utoipa::ToSchema;

        #[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema, DeriveEntityModel)]
        #[schema(as = Ingredient)]
        #[sea_orm(table_name = "ingredients")]
        pub struct Model {
            #[sea_orm(primary_key)]
            pub id: i32,
            pub name: String,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {}

        impl ActiveModelBehavior for ActiveModel {}

        impl MiryadResource for Entity {
            fn resource_name() -> &'static str {
                "ingredients"
            }
            fn read_policy() -> AccessPolicy {
                AccessPolicy::AdminOnly
            }
            fn write_policy() -> AccessPolicy {
                AccessPolicy::AdminOnly
            }
            fn owner_column() -> Option<Column> {
                None
            }
        }
    }

    #[test]
    fn resource_openapi_declares_expected_paths_and_methods() {
        let spec = resource_openapi::<recipe::Entity>();

        let collection = spec
            .paths
            .get_path_item("/api/v1/recipes")
            .expect("collection path present");
        assert!(collection.get.is_some());
        assert!(collection.post.is_some());

        let item = spec
            .paths
            .get_path_item("/api/v1/recipes/{id}")
            .expect("item path present");
        assert!(item.get.is_some());
        assert!(item.put.is_some());
        assert!(item.delete.is_some());
    }

    #[test]
    fn resource_openapi_declares_model_and_paged_schemas() {
        let spec = resource_openapi::<recipe::Entity>();
        let components = spec.components.expect("components present");
        assert!(components.schemas.contains_key("Recipe"));
        assert!(components.schemas.contains_key("PagedRecipe"));
    }

    #[test]
    fn merged_fragments_expose_all_paths_without_collision() {
        let mut spec = resource_openapi::<recipe::Entity>();
        spec.merge(resource_openapi::<ingredient::Entity>());

        assert!(spec.paths.get_path_item("/api/v1/recipes").is_some());
        assert!(spec.paths.get_path_item("/api/v1/recipes/{id}").is_some());
        assert!(spec.paths.get_path_item("/api/v1/ingredients").is_some());
        assert!(spec.paths.get_path_item("/api/v1/ingredients/{id}").is_some());
    }

    #[test]
    fn resource_openapi_declares_bearer_security_scheme() {
        let spec = resource_openapi::<recipe::Entity>();

        let components = spec.components.expect("components present");
        let scheme = components
            .security_schemes
            .get(BEARER_SECURITY_SCHEME)
            .expect("bearer security scheme present");
        let SecurityScheme::Http(http) = scheme else {
            panic!("expected a Bearer HTTP security scheme under {BEARER_SECURITY_SCHEME:?}");
        };
        assert!(matches!(http.scheme, HttpAuthScheme::Bearer));
        // #21 : le champ "Authorize" de Swagger UI n'attend que le token seul (pas le préfixe
        // "Bearer" qu'il ajoute lui-même) — sans description, rien ne l'indique.
        let description = http
            .description
            .as_deref()
            .expect("bearer scheme has a description");
        assert!(
            description.to_lowercase().contains("bearer"),
            "description should warn against typing the Bearer prefix: {description:?}"
        );

        let security = spec.security.expect("global security requirement present");
        assert!(
            security
                .iter()
                .any(|req| req == &SecurityRequirement::new(BEARER_SECURITY_SCHEME, Vec::<String>::new())),
            "expected a global security requirement referencing {BEARER_SECURITY_SCHEME:?}"
        );
    }

    /// Régression : `OpenApi::merge` dédoublonne `security_schemes`/`security` par nom/égalité —
    /// deux fragments d'entités déclarant le même schéma Bearer ne doivent pas produire de
    /// doublons dans le document final.
    #[test]
    fn merging_fragments_does_not_duplicate_the_security_scheme() {
        let mut spec = resource_openapi::<recipe::Entity>();
        spec.merge(resource_openapi::<ingredient::Entity>());

        let components = spec.components.expect("components present");
        assert_eq!(
            components
                .security_schemes
                .keys()
                .filter(|name| name.as_str() == BEARER_SECURITY_SCHEME)
                .count(),
            1
        );
        assert_eq!(spec.security.expect("security present").len(), 1);
    }

    #[tokio::test]
    async fn openapi_router_serves_the_spec_as_json() {
        let spec = resource_openapi::<recipe::Entity>();
        let app: axum::Router = openapi_router(spec);

        let req = Request::builder()
            .uri("/api/openapi.json")
            .body(Body::empty())
            .expect("valid request");
        let resp = app.oneshot(req).await.expect("router does not fail");
        assert_eq!(resp.status(), StatusCode::OK);

        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("readable body");
        let parsed: OpenApi = serde_json::from_slice(&bytes).expect("valid OpenApi JSON");
        assert!(parsed.paths.get_path_item("/api/v1/recipes").is_some());
    }

    #[cfg(feature = "swagger-ui")]
    #[tokio::test]
    async fn swagger_ui_router_serves_the_ui() {
        let spec = resource_openapi::<recipe::Entity>();
        let app: axum::Router = swagger_ui_router(spec);

        let req = Request::builder()
            .uri("/api/swagger-ui")
            .body(Body::empty())
            .expect("valid request");
        let resp = app.oneshot(req).await.expect("router does not fail");
        assert!(resp.status().is_redirection() || resp.status() == StatusCode::OK);
    }
}
