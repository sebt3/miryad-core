//! Contrat public `OpenAPI` de la crate — un fragment `utoipa::openapi::OpenApi` par
//! entité montée (`resource_openapi` : les 5 routes CRUD de `resource_router`, le
//! schéma du `Model`, l'enveloppe de pagination `Paged{Model}`, l'exigence de sécurité
//! `bearer_auth`), fusionnable par `OpenApi::merge`. `openapi_router` (toujours compilé)
//! sert `GET /api/openapi.json` ; `swagger_ui_router` (seule feature `swagger-ui`) sert
//! l'UI sur `/api/swagger-ui`. artefact externe de la séparation voulue par la racine :
//! l'IR frontend est l'artefact interne, aucune extension `x-miryad-*` n'entre ici. Le
//! fichier ne décide aucune autorisation et n'accède à aucune base — il décrit des routes
//! montées ailleurs et sert un document JSON déjà construit.

use utoipa::openapi::path::{HttpMethod, OperationBuilder, ParameterBuilder, ParameterIn};
use utoipa::openapi::request_body::RequestBodyBuilder;
use utoipa::openapi::response::{Response, ResponseBuilder};
use utoipa::openapi::schema::SchemaType;
use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityRequirement, SecurityScheme};
use utoipa::openapi::{
    ArrayBuilder, ComponentsBuilder, Content, ContentBuilder, Object, OpenApi, OpenApiBuilder, Paths, Ref,
    RefOr, Required, Schema, Type,
};
use utoipa::{PartialSchema, ToSchema};

use crate::rest::RestEntity;

/// Nom du `SecurityScheme` déclaré par `resource_openapi` — le dual-auth de miryad-core accepte
/// aussi un cookie de session OIDC, mais celui-ci est `HttpOnly`/chiffré et n'a rien d'utilisable
/// depuis le champ "Authorize" de Swagger UI ; seul le token API (`issue_token`) est actionnable
/// depuis cette interface.
const BEARER_SECURITY_SCHEME: &str = "bearer_auth";

/// Nom du schéma de corps du rejet métier — référencé par les `422` des opérations qui en
/// portent (`openapi.sdd` `Must`, arbitré 2026-09-29) : comme `BEARER_SECURITY_SCHEME`, son
/// littéral est une clé du JSON publié.
const HOOK_ERROR_BODY_SCHEMA: &str = "HookErrorBody";

/// Entités éligibles à la génération `OpenAPI` — en plus de `RestEntity`, `Model` doit dériver
/// `utoipa::ToSchema` pour que sa forme JSON soit décrite dans le document généré.
pub trait OpenApiEntity: RestEntity<Model: ToSchema> {}
impl<E> OpenApiEntity for E where E: RestEntity<Model: ToSchema> {}

/// Fragment `OpenAPI` pour les 5 routes CRUD d'une entité (`GET/POST /api/v1/{resource_name}`,
/// `GET/PUT/DELETE /api/v1/{resource_name}/{id}`) — à fusionner avec celui des autres entités
/// montées (`utoipa::openapi::OpenApi::merge`) avant publication. Ne fixe pas `info`
/// (titre/version) : l'app renseigne ces champs sur le document final après fusion. Les chemins
/// suivent le préfixe figé de `resource_router` (feature 6) — toujours à jour vis-à-vis des
/// routes REST réellement montées. Déclare un `SecurityScheme` Bearer (feature 2) : le bouton
/// "Authorize" de Swagger UI fonctionne sans configuration côté app — `OpenApi::merge` dédoublonne
/// le schéma et l'exigence de sécurité par nom/égalité entre fragments d'entités.
#[must_use]
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
        .schema(
            HOOK_ERROR_BODY_SCHEMA,
            Schema::Object(
                Object::builder()
                    // `code` nullable au sens OpenAPI 3.1 (`type: ["string", "null"]`) : le
                    // `HookError` peut n'avoir aucun code — la clé est émise même nulle
                    // (`rest/error.sdd` `Must`).
                    .property(
                        "code",
                        Object::builder()
                            .schema_type(SchemaType::from_iter([Type::String, Type::Null]))
                            .build(),
                    )
                    .property("message", Object::with_type(Type::String))
                    .build(),
            ),
        )
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

    let paths = crud_paths(
        resource,
        &model_ref,
        &paged_schema_name,
        // `filter` n'est déclaré que quand il agit (`openapi.sdd` `Must`, arbitré 2026-09-29) :
        // sans `filter_column`, `rest::core::list` ignore le paramètre et le document ne promet
        // rien que la surface n'honore.
        E::filter_column().is_some(),
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

/// Les cinq opérations CRUD du fragment, ajoutées à `paths` — extraction privée de
/// `resource_openapi` (harnais `too_many_lines`, `tooling.sdd`) : seule la surface publique
/// compte à `Exposes` et elle n'a pas bougé. `has_filter` vient de `E::filter_column()` ;
/// chaque opération porte les statuts et corps d'erreur de `openapi.sdd` `Must` (arbitré
/// 2026-09-29).
fn crud_paths(resource: &str, model_ref: &RefOr<Schema>, paged_schema_name: &str, has_filter: bool) -> Paths {
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
    let json_body = |schema: RefOr<Schema>| {
        RequestBodyBuilder::new()
            .required(Some(Required::True))
            .content("application/json", json_content(schema))
            .build()
    };

    let mut paths = Paths::new();

    let list_op = OperationBuilder::new()
        .parameter(query_param("page", Type::Integer))
        .parameter(query_param("per_page", Type::Integer));
    let list_op = if has_filter {
        list_op.parameter(query_param("filter", Type::String))
    } else {
        list_op
    };
    let list_op = with_common_errors(list_op)
        .response(
            "200",
            ResponseBuilder::new()
                .description("Liste paginée")
                .content(
                    "application/json",
                    json_content(RefOr::Ref(Ref::from_schema_name(paged_schema_name.to_string()))),
                )
                .build(),
        )
        .build();
    paths.add_path_operation(format!("/api/v1/{resource}"), vec![HttpMethod::Get], list_op);

    let create_op = with_unprocessable(with_unsupported_media(with_common_errors(
        OperationBuilder::new().request_body(Some(json_body(model_ref.clone()))),
    )))
    .response(
        "201",
        ResponseBuilder::new()
            .description("Créé")
            .content("application/json", json_content(model_ref.clone()))
            .build(),
    )
    .build();
    paths.add_path_operation(format!("/api/v1/{resource}"), vec![HttpMethod::Post], create_op);

    let get_op = with_not_found(with_common_errors(
        OperationBuilder::new().parameter(id_param.clone()),
    ))
    .response(
        "200",
        ResponseBuilder::new()
            .description("Trouvé")
            .content("application/json", json_content(model_ref.clone()))
            .build(),
    )
    .build();
    paths.add_path_operation(
        format!("/api/v1/{resource}/{{id}}"),
        vec![HttpMethod::Get],
        get_op,
    );

    let update_op = with_unprocessable(with_unsupported_media(with_not_found(with_common_errors(
        OperationBuilder::new()
            .parameter(id_param.clone())
            .request_body(Some(json_body(model_ref.clone()))),
    ))))
    .response(
        "200",
        ResponseBuilder::new()
            .description("Mis à jour")
            .content("application/json", json_content(model_ref.clone()))
            .build(),
    )
    .build();
    paths.add_path_operation(
        format!("/api/v1/{resource}/{{id}}"),
        vec![HttpMethod::Put],
        update_op,
    );

    let delete_op = with_unprocessable(with_not_found(with_common_errors(
        OperationBuilder::new().parameter(id_param),
    )))
    .response("204", ResponseBuilder::new().description("Supprimé").build())
    .build();
    paths.add_path_operation(
        format!("/api/v1/{resource}/{{id}}"),
        vec![HttpMethod::Delete],
        delete_op,
    );

    paths
}

/// Contenu `application/json` portant ou référençant le schéma donné.
fn json_content(schema: RefOr<Schema>) -> Content {
    ContentBuilder::new().schema(Some(schema)).build()
}

/// Contenu `text/plain` de type chaîne — forme déclarée de tout corps d'erreur texte
/// (`openapi.sdd` `Must`).
fn text_content() -> Content {
    ContentBuilder::new()
        .schema(Some(RefOr::T(Schema::Object(Object::with_type(Type::String)))))
        .build()
}

/// Réponse d'erreur texte : description littérale + unique corps `text/plain` de type chaîne.
fn text_error(description: &str) -> Response {
    ResponseBuilder::new()
        .description(description)
        .content("text/plain", text_content())
        .build()
}

/// `422` « Corps non traitable ou rejet métier » (arbitré 2026-09-29) — les deux saveurs du
/// `422` partagé de `rest/error.sdd` : texte pour la désérialisation `axum` et `MRD-REST-005`,
/// JSON `{code, message}` (`HookErrorBody`, `code` nullable) pour un `HookError`.
fn hook_error_response() -> Response {
    ResponseBuilder::new()
        .description("Corps non traitable ou rejet métier")
        .content("text/plain", text_content())
        .content(
            "application/json",
            json_content(RefOr::Ref(Ref::from_schema_name(HOOK_ERROR_BODY_SCHEMA))),
        )
        .build()
}

/// Les statuts d'erreur communs aux cinq opérations (`Must` : `400` « Requête invalide »,
/// `401` « Non authentifié », `403` « Refusé », `500` « Erreur interne »), corps texte.
fn with_common_errors(op: OperationBuilder) -> OperationBuilder {
    op.response("400", text_error("Requête invalide"))
        .response("401", text_error("Non authentifié"))
        .response("403", text_error("Refusé"))
        .response("500", text_error("Erreur interne"))
}

/// + `404` « Non trouvé » (`MRD-REST-001`) — propre aux trois opérations d'item.
fn with_not_found(op: OperationBuilder) -> OperationBuilder {
    op.response("404", text_error("Non trouvé"))
}

/// + `415` « Content-Type non supporté » — propre aux opérations à corps (`POST`/`PUT`).
fn with_unsupported_media(op: OperationBuilder) -> OperationBuilder {
    op.response("415", text_error("Content-Type non supporté"))
}

/// `422` en plus — propre à `POST`/`PUT`/`DELETE` (rejet métier `HookError`, désérialisation
/// d'axum et `MRD-REST-005`).
fn with_unprocessable(op: OperationBuilder) -> OperationBuilder {
    op.response("422", hook_error_response())
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
            // la fixture `recipe` déclare `filter_column` (Scenario « paramètres page/per_page/
            // filter ») : son fragment doit déclarer `filter` ; `ingredient` ne la déclare pas
            // (défaut `None`) et n'a donc rien à déclarer.
            fn filter_column() -> Option<Column> {
                Some(Column::Title)
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

    // ——— Selecteurs JSON : toutes les affirmations structurelles du contrat ———
    // Le document est comparé sur des sélecteurs précis (chemins, clés, descriptions),
    // jamais par `contains` lâche sur la chaîne brute.

    /// Le fragment sérialisé en JSON — base des sélecteurs.
    fn spec_json(spec: &OpenApi) -> serde_json::Value {
        serde_json::to_value(spec).expect("fragment serializes")
    }

    /// Sélectionne l'opération d'une méthode sur un chemin du document sérialisé.
    fn operation<'a>(json: &'a serde_json::Value, path: &str, method: &str) -> &'a serde_json::Value {
        &json["paths"][path][method]
    }

    /// Codes de réponse d'une opération, triés (utoipa 5.5 : `Responses` est une
    /// `BTreeMap<String, _>`).
    fn response_codes(op: &serde_json::Value) -> Vec<String> {
        op["responses"]
            .as_object()
            .expect("responses object")
            .keys()
            .cloned()
            .collect()
    }

    /// `(nom, type du schéma, requis)` de chaque paramètre d'une opération pour un emplacement
    /// (`query`/`path`) donnés.
    fn parameters(op: &serde_json::Value, parameter_in: &str) -> Vec<(String, String, bool)> {
        let mut params: Vec<(String, String, bool)> = op["parameters"]
            .as_array()
            .expect("parameters array")
            .iter()
            .filter(|param| param["in"] == parameter_in)
            .map(|param| {
                (
                    param["name"].as_str().expect("parameter name").to_owned(),
                    param["schema"]["type"]
                        .as_str()
                        .expect("parameter schema type")
                        .to_owned(),
                    param["required"].as_bool().expect("parameter required flag"),
                )
            })
            .collect();
        params.sort();
        params
    }

    /// Clés triées du contenu d'une réponse sérialisée.
    fn content_types(response: &serde_json::Value) -> Vec<String> {
        response["content"]
            .as_object()
            .expect("response content")
            .keys()
            .cloned()
            .collect()
    }

    /// Vérifie un statut d'erreur texte : description littérale, unique corps `text/plain`
    /// de type chaîne (`Must` : « Les corps d'erreur texte sont déclarés `text/plain` de type
    /// chaîne »).
    fn text_error(op: &serde_json::Value, code: &str, description: &str) {
        let response = &op["responses"][code];
        assert_eq!(response["description"], description);
        assert_eq!(content_types(response), ["text/plain"]);
        assert_eq!(response["content"]["text/plain"]["schema"]["type"], "string");
    }

    /// Vérifie le `422` bilatéral (arbitré 2026-09-29) : texte pour la désérialisation `axum`
    /// et `MRD-REST-005`, JSON référençant `HookErrorBody` pour un `HookError`.
    fn unprocessable(op: &serde_json::Value) {
        let response = &op["responses"]["422"];
        assert_eq!(response["description"], "Corps non traitable ou rejet métier");
        assert_eq!(content_types(response), ["application/json", "text/plain"]);
        assert_eq!(response["content"]["text/plain"]["schema"]["type"], "string");
        assert_eq!(
            response["content"]["application/json"]["schema"]["$ref"],
            "#/components/schemas/HookErrorBody"
        );
    }

    /// Requête `GET` simple sur un routeur cloné, sans aucun état ni en-tête d'authentification.
    async fn get(app: &axum::Router, uri: &str) -> axum::response::Response {
        app.clone()
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .body(Body::empty())
                    .expect("valid request"),
            )
            .await
            .expect("router does not fail")
    }

    /// Descend récursivement le JSON sérialisé et échoue sur toute clé `x-miryad-*`
    /// (interdiction héritée de `miryad-core.sdd`).
    fn assert_no_x_miryad_keys(value: &serde_json::Value, doc: &str) {
        match value {
            serde_json::Value::Object(entries) => {
                for (key, inner) in entries {
                    assert!(
                        !key.starts_with("x-miryad-"),
                        "{doc} porte l'extension interdite {key:?}"
                    );
                    assert_no_x_miryad_keys(inner, doc);
                }
            }
            serde_json::Value::Array(items) => {
                for inner in items {
                    assert_no_x_miryad_keys(inner, doc);
                }
            }
            _ => {}
        }
    }

    /// `Scenario` : « `@resource_openapi déclare les cinq routes CRUD` ».
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

        // « aucun autre chemin n'apparaît dans les chemins du fragment »
        assert_eq!(spec.paths.paths.len(), 2);
    }

    /// `Scenario` : « `l'enveloppe Paged{Model} broche les cinq clés de @PagedResult` » — les
    /// cinq clés wire, les cinq marques `required`, ` items` en tableau de références `Recipe`.
    #[test]
    fn resource_openapi_declares_model_and_paged_schemas() {
        let spec = resource_openapi::<recipe::Entity>();
        let components = spec.components.as_ref().expect("components present");
        assert!(components.schemas.contains_key("Recipe"));
        assert!(components.schemas.contains_key("PagedRecipe"));

        let json = spec_json(&spec);
        let paged = &json["components"]["schemas"]["PagedRecipe"];
        let properties: Vec<String> = paged["properties"]
            .as_object()
            .expect("PagedRecipe properties")
            .keys()
            .cloned()
            .collect();
        assert_eq!(
            properties,
            ["items", "page", "per_page", "total_items", "total_pages"]
        );

        let required: Vec<String> = paged["required"]
            .as_array()
            .expect("PagedRecipe required")
            .iter()
            .map(|value| value.as_str().expect("required entry").to_owned())
            .collect();
        assert_eq!(
            required,
            ["items", "page", "per_page", "total_items", "total_pages"]
        );

        assert_eq!(paged["properties"]["items"]["type"], "array");
        assert_eq!(
            paged["properties"]["items"]["items"]["$ref"],
            "#/components/schemas/Recipe"
        );
    }

    /// `Scenario` : « paramètres `page`, `per_page` et `filter` en query, `id` en path » —
    /// `filter` déclaré par la fixture qui a une `filter_column`, absent de celle qui n'en a
    /// pas (arbitré 2026-09-29), `id` requis sur les trois opérations d'item, ni `format`
    /// ni bornes nulle part.
    #[test]
    fn resource_openapi_declares_query_and_path_parameters() {
        let spec = resource_openapi::<recipe::Entity>();
        let json = spec_json(&spec);

        // la fixture `recipe` déclare `filter_column` : exactement trois query parameters
        assert_eq!(
            parameters(operation(&json, "/api/v1/recipes", "get"), "query"),
            vec![
                ("filter".to_owned(), "string".to_owned(), false),
                ("page".to_owned(), "integer".to_owned(), false),
                ("per_page".to_owned(), "integer".to_owned(), false),
            ]
        );

        // entité sans `filter_column` (`ingredient`, défaut `None`) : `filter` n'est pas déclaré
        let ingredient_json = spec_json(&resource_openapi::<ingredient::Entity>());
        assert_eq!(
            parameters(operation(&ingredient_json, "/api/v1/ingredients", "get"), "query"),
            vec![
                ("page".to_owned(), "integer".to_owned(), false),
                ("per_page".to_owned(), "integer".to_owned(), false),
            ]
        );

        // chacune des trois opérations d'item : le path parameter `id`, integer, requis
        for method in ["get", "put", "delete"] {
            assert_eq!(
                parameters(operation(&json, "/api/v1/recipes/{id}", method), "path"),
                vec![("id".to_owned(), "integer".to_owned(), true)]
            );
        }

        // But : aucun `format` (`int32`/`int64`) ni borne (`1`, plafond `1000`) déclaré sur
        // les schémas de ces paramètres
        for (path, method) in [
            ("/api/v1/recipes", "get"),
            ("/api/v1/recipes/{id}", "get"),
            ("/api/v1/recipes/{id}", "put"),
            ("/api/v1/recipes/{id}", "delete"),
        ] {
            for param in operation(&json, path, method)["parameters"]
                .as_array()
                .expect("parameters array")
            {
                let schema = &param["schema"];
                for forbidden in [
                    "format",
                    "minimum",
                    "maximum",
                    "exclusiveMinimum",
                    "exclusiveMaximum",
                    "minLength",
                    "maxLength",
                ] {
                    assert!(
                        schema.get(forbidden).is_none(),
                        "le paramètre {:?} ne doit pas déclarer {forbidden}",
                        param["name"]
                    );
                }
            }
        }
    }

    /// `Scenario` : « chaque opération déclare son jeu de statuts et ses corps » — succès,
    /// corps requis, jeu d'erreurs exact par opération (`Must`, arbitré 2026-09-29), corps
    /// `text/plain` chaîne, et `422` JSON vers `HookErrorBody`.
    #[test]
    fn resource_openapi_declares_operations_responses_and_bodies() {
        let spec = resource_openapi::<recipe::Entity>();
        let json = spec_json(&spec);

        let list = operation(&json, "/api/v1/recipes", "get");
        let create = operation(&json, "/api/v1/recipes", "post");
        let get_item = operation(&json, "/api/v1/recipes/{id}", "get");
        let update = operation(&json, "/api/v1/recipes/{id}", "put");
        let delete = operation(&json, "/api/v1/recipes/{id}", "delete");

        // GET collection : `200` « Liste paginée », `application/json` → `PagedRecipe`,
        // rien d'autre dans ce corps
        assert_eq!(list["responses"]["200"]["description"], "Liste paginée");
        assert_eq!(content_types(&list["responses"]["200"]), ["application/json"]);
        assert_eq!(
            list["responses"]["200"]["content"]["application/json"]["schema"]["$ref"],
            "#/components/schemas/PagedRecipe"
        );

        // POST : corps requis référençant `Recipe` ; `201` « Créé » rend `Recipe`
        assert_eq!(create["requestBody"]["required"], true);
        assert_eq!(
            create["requestBody"]["content"]["application/json"]["schema"]["$ref"],
            "#/components/schemas/Recipe"
        );
        assert_eq!(create["responses"]["201"]["description"], "Créé");
        assert_eq!(
            create["responses"]["201"]["content"]["application/json"]["schema"]["$ref"],
            "#/components/schemas/Recipe"
        );

        // GET item : `200` « Trouvé » rend `Recipe`
        assert_eq!(get_item["responses"]["200"]["description"], "Trouvé");
        assert_eq!(
            get_item["responses"]["200"]["content"]["application/json"]["schema"]["$ref"],
            "#/components/schemas/Recipe"
        );

        // PUT item : corps requis référençant `Recipe` ; `200` « Mis à jour »
        assert_eq!(update["requestBody"]["required"], true);
        assert_eq!(
            update["requestBody"]["content"]["application/json"]["schema"]["$ref"],
            "#/components/schemas/Recipe"
        );
        assert_eq!(update["responses"]["200"]["description"], "Mis à jour");

        // DELETE item : `204` « Supprimé » sans contenu
        assert_eq!(delete["responses"]["204"]["description"], "Supprimé");
        assert!(
            delete["responses"]["204"].get("content").is_none(),
            "204 se déclare sans contenu"
        );

        // jeu de statuts exact par opération : succès + les erreurs du `Must`, rien d'autre
        assert_eq!(response_codes(list), ["200", "400", "401", "403", "500"]);
        assert_eq!(
            response_codes(create),
            ["201", "400", "401", "403", "415", "422", "500"]
        );
        assert_eq!(
            response_codes(get_item),
            ["200", "400", "401", "403", "404", "500"]
        );
        assert_eq!(
            response_codes(update),
            ["200", "400", "401", "403", "404", "415", "422", "500"]
        );
        assert_eq!(
            response_codes(delete),
            ["204", "400", "401", "403", "404", "422", "500"]
        );

        // corps `text/plain` de type chaîne, descriptions littérales du contrat
        for op in [list, create, get_item, update, delete] {
            text_error(op, "400", "Requête invalide");
            text_error(op, "401", "Non authentifié");
            text_error(op, "403", "Refusé");
            text_error(op, "500", "Erreur interne");
        }
        for op in [get_item, update, delete] {
            text_error(op, "404", "Non trouvé");
        }
        for op in [create, update] {
            text_error(op, "415", "Content-Type non supporté");
        }
        for op in [create, update, delete] {
            unprocessable(op);
        }

        // schéma `HookErrorBody` enregistré : `{code, message}`, `code` nullable
        let hook = &json["components"]["schemas"]["HookErrorBody"];
        assert_eq!(hook["type"], "object");
        assert_eq!(
            hook["properties"]
                .as_object()
                .expect("HookErrorBody properties")
                .keys()
                .cloned()
                .collect::<Vec<_>>(),
            ["code", "message"]
        );
        assert_eq!(
            hook["properties"]["code"]["type"],
            serde_json::json!(["string", "null"])
        );
        assert_eq!(hook["properties"]["message"]["type"], "string");
    }

    /// `Scenario` : « `le fragment ne fixe pas Info et se versionne OpenAPI 3.1.0` ».
    #[test]
    fn resource_openapi_fragment_leaves_info_unset_and_versions_3_1_0() {
        let spec = resource_openapi::<recipe::Entity>();

        assert_eq!(spec.info.title, "");
        assert_eq!(spec.info.version, "");
        assert_eq!(spec_json(&spec)["openapi"], "3.1.0");
    }

    /// `Scenario` : « `le schéma bearer_auth porte la description anti-préfixe` » — `Http`
    /// `Bearer`, description mentionnant « bearer » à n'importe quelle casse (fix #21/#23 :
    /// préfixe collé deux fois, l'auth échoue en `MRD-AUTH-014`), exigence globale unique,
    /// et rien pour le cookie de session.
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

        // une unique exigence de sécurité globale `{"bearer_auth": []}`
        let security = spec.security.expect("global security requirement present");
        assert_eq!(security.len(), 1);
        assert!(
            security[0] == SecurityRequirement::new(BEARER_SECURITY_SCHEME, Vec::<String>::new()),
            "l'unique exigence globale doit référencer {BEARER_SECURITY_SCHEME:?} avec des scopes vides"
        );

        // But : aucun schéma pour le cookie de session — l'unique `SecurityScheme` est `bearer_auth`
        assert_eq!(
            components.security_schemes.keys().collect::<Vec<_>>(),
            vec![BEARER_SECURITY_SCHEME]
        );
    }

    /// `Scenario` : « fusionner deux fragments d'entités expose toutes les routes ».
    #[test]
    fn merged_fragments_expose_all_paths_without_collision() {
        let mut spec = resource_openapi::<recipe::Entity>();
        spec.merge(resource_openapi::<ingredient::Entity>());

        assert!(spec.paths.get_path_item("/api/v1/recipes").is_some());
        assert!(spec.paths.get_path_item("/api/v1/recipes/{id}").is_some());
        assert!(spec.paths.get_path_item("/api/v1/ingredients").is_some());
        assert!(spec.paths.get_path_item("/api/v1/ingredients/{id}").is_some());
    }

    /// Régression / `Scenario` : « la fusion garde un schéma de sécurité et une exigence
    /// uniques » — `OpenApi::merge` dédoublonne `security_schemes`/`security` par nom/égalité.
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

    /// `Scenario` : « `openapi_router sert le document JSON sans authentification` » — la
    /// requête ci-dessous n'envoie aucun en-tête `Authorization`, et le routeur monté sur
    /// `axum::Router` (état `()`) prouve qu'aucun état applicatif ni `MiryadAuthState` n'est
    /// requis.
    #[tokio::test]
    async fn openapi_router_serves_the_spec_as_json() {
        let spec = resource_openapi::<recipe::Entity>();
        let app: axum::Router = openapi_router(spec);

        let resp = get(&app, "/api/openapi.json").await;
        assert_eq!(resp.status(), StatusCode::OK);

        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("readable body");
        let parsed: OpenApi = serde_json::from_slice(&bytes).expect("valid OpenApi JSON");
        assert!(parsed.paths.get_path_item("/api/v1/recipes").is_some());
    }

    /// `Scenario` : « `swagger_ui_router sert l'UI et son propre JSON sous la feature` » —
    /// `GET /api/swagger-ui` redirection `303 See Other` vers `/api/swagger-ui/`
    /// (`utoipa-swagger-ui-9.0.2`), l'UI en `200` sur cette voie, et le même document JSON
    /// servi par `.url(...)` lui-même.
    #[cfg(feature = "swagger-ui")]
    #[tokio::test]
    async fn swagger_ui_router_serves_the_ui() {
        let app: axum::Router = swagger_ui_router(resource_openapi::<recipe::Entity>());

        let resp = get(&app, "/api/swagger-ui").await;
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            resp.headers()
                .get(axum::http::header::LOCATION)
                .expect("Location header present")
                .to_str()
                .expect("valid Location"),
            "/api/swagger-ui/"
        );

        let resp = get(&app, "/api/swagger-ui/").await;
        assert_eq!(resp.status(), StatusCode::OK);

        let resp = get(&app, "/api/openapi.json").await;
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("readable body");
        let parsed: OpenApi = serde_json::from_slice(&bytes).expect("valid OpenApi JSON");
        assert!(parsed.paths.get_path_item("/api/v1/recipes").is_some());
    }

    /// `Scenario` : « hors feature swagger-ui la route de l'UI répond 404 » — s'exécute sur
    /// les runs sans `swagger-ui` de la batterie `tooling.sdd` : `swagger_ui_router` n'existe
    /// pas à la compilation, `openapi_router` seul sert toujours son document.
    #[cfg(not(feature = "swagger-ui"))]
    #[tokio::test]
    async fn swagger_ui_route_absent_without_feature() {
        let app: axum::Router = openapi_router(resource_openapi::<recipe::Entity>());

        let resp = get(&app, "/api/swagger-ui").await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        let resp = get(&app, "/api/openapi.json").await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    /// `Scenario` : « combiner les deux routeurs fait paniquer le montage » — refus levé par
    /// `Router::merge` (source `axum-0.8.9`, `panic_on_err!`). Les monter l'un ou l'autre ne
    /// panique jamais : c'est déjà ce que prouvent `openapi_router_serves_the_spec_as_json`
    /// et `swagger_ui_router_serves_the_ui`, chacun monté seul.
    #[cfg(feature = "swagger-ui")]
    #[test]
    #[should_panic(expected = "Overlapping method route")]
    fn merging_openapi_and_swagger_ui_routers_panics() {
        let spec = resource_openapi::<recipe::Entity>();
        let merged: axum::Router = axum::Router::new()
            .merge(openapi_router(spec.clone()))
            .merge(swagger_ui_router(spec));
        let _unused = merged;
    }

    /// `Scenario` : « le document publié ne porte aucune extension x-miryad » — fragment
    /// seul et fusion sérialisés, aucune clé `x-miryad-*` nulle part.
    #[test]
    fn resource_openapi_emits_no_x_miryad_extension() {
        let fragment = resource_openapi::<recipe::Entity>();
        let mut merged = resource_openapi::<recipe::Entity>();
        merged.merge(resource_openapi::<ingredient::Entity>());

        assert_no_x_miryad_keys(&spec_json(&fragment), "le fragment d'une entité");
        assert_no_x_miryad_keys(&spec_json(&merged), "la fusion de deux fragments");
    }
}
