use std::collections::BTreeMap;

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
///
/// `entities` est un `BTreeMap` (arbitré 2026-09-29) : l'ordre d'exposition est l'ordre
/// croissant des `resource_name`, stable entre exécutions — `tools/list` (`handler.rs`) itère
/// `entities.values()` dans cet ordre sans le retrier.
pub struct McpToolRegistry {
    pub(crate) format: OutputFormat,
    pub(crate) entities: BTreeMap<&'static str, Box<dyn McpEntity>>,
}

impl McpToolRegistry {
    /// Construit un registre vide (aucune entité montée) avec le format de sortie
    /// choisi une fois pour toute l'app ; les entités s'ajoutent ensuite par
    /// [`register`](Self::register).
    #[must_use]
    pub fn new(format: OutputFormat) -> Self {
        Self {
            format,
            entities: BTreeMap::new(),
        }
    }

    /// Enregistre les 5 tools (`{resource_name}_list`, `_get`, `_create`, `_update`, `_delete`)
    /// pour `E`, sous la clé `E::resource_name()`, et rend `&mut Self` pour l'enchaînement.
    ///
    /// # Panics
    ///
    /// Refuse toute collision de clé par `assert!` explicite (arbitré 2026-09-29, `registry.sdd`
    /// `Must` « une clé déjà présente fait échouer `register` ») : un `resource_name` déjà
    /// enregistré — y compris par le même type enregistré deux fois — est une erreur de
    /// programmation détectée au démarrage, même logique que la collision refusée au montage du
    /// routeur REST (`rest/mod.rs`) et dans l'`IrRegistry`. Plus aucun remplacement silencieux,
    /// aucune re-registration idempotente ; le message cite le nom en collision.
    pub fn register<E: RestEntity>(&mut self) -> &mut Self {
        let name = E::resource_name();
        assert!(
            !self.entities.contains_key(name),
            "`{name}` is already registered in the MCP tool registry — duplicate `resource_name` \
             (the same entity type registered twice counts) is a programming error, refusing to \
             register it a second time"
        );
        self.entities
            .insert(name, Box::new(McpEntityImpl::<E>(std::marker::PhantomData)));
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::PrincipalSource;
    use crate::migration::Migrator;
    use crate::resource::{AccessPolicy, HookError, MiryadResource};
    // Le prelude sea-orm exporte aussi son propre `Value` : import explicite de `serde_json`
    // pour lever l'ambiguite des glob imports (celui des arguments et rendus MCP).
    use sea_orm::entity::prelude::*;
    use sea_orm::{ActiveValue::Set, ConnectionTrait, Database, EntityTrait, PaginatorTrait, Schema};
    use sea_orm_migration::MigratorTrait;
    use serde::{Deserialize, Serialize};
    use serde_json::{Value, json};

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
            // Colonne de filtre déclarée — exercée par le Scenario « `_list` : `page`,
            // `per_page` et `filter` lus puis délégués une seule fois ».
            fn filter_column() -> Option<Column> {
                Some(Column::Title)
            }
        }
    }

    /// Type distinct dont `MiryadResource::resource_name` retourne aussi `recipes` — fixture de
    /// la collision d'enregistrement (`Scenario` « enregistrement : collision de `resource_name`
    /// refusée par assertion »).
    mod alt {
        use super::*;

        #[derive(Clone, Debug, PartialEq, Serialize, Deserialize, DeriveEntityModel)]
        #[sea_orm(table_name = "alt_recipes")]
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

    /// Seconde entité montée par les scenarios de chaînage et d'ordre — `resource_name`
    /// `ingredients`, antérioritaire lexicographiquement sur `recipes`.
    mod ingredient {
        use super::*;

        #[derive(Clone, Debug, PartialEq, Serialize, Deserialize, DeriveEntityModel)]
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
                AccessPolicy::Public
            }
            fn write_policy() -> AccessPolicy {
                AccessPolicy::Public
            }
            fn owner_column() -> Option<Column> {
                None
            }
        }
    }

    /// Entité dont `read_policy` compte ses propres lectures puis retourne `Public` — fixture
    /// du Scenario « enregistrement : rien de `MiryadResource` n'est retenu, les politiques sont
    /// relues en direct ». Un compteur propre à ce module évite toute collision entre tests.
    mod counted {
        use super::*;
        use std::sync::atomic::{AtomicUsize, Ordering};

        static READ_POLICY_READS: AtomicUsize = AtomicUsize::new(0);

        /// Nombre de lectures de `read_policy` sur le trait, à ce jour.
        pub fn reads() -> usize {
            READ_POLICY_READS.load(Ordering::SeqCst)
        }

        #[derive(Clone, Debug, PartialEq, Serialize, Deserialize, DeriveEntityModel)]
        #[sea_orm(table_name = "counters")]
        pub struct Model {
            #[sea_orm(primary_key)]
            pub id: i32,
            pub title: String,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {}

        impl ActiveModelBehavior for ActiveModel {}

        impl MiryadResource for Entity {
            fn resource_name() -> &'static str {
                "counters"
            }
            fn read_policy() -> AccessPolicy {
                READ_POLICY_READS.fetch_add(1, Ordering::SeqCst);
                AccessPolicy::Public
            }
            fn write_policy() -> AccessPolicy {
                AccessPolicy::Public
            }
            fn owner_column() -> Option<Column> {
                None
            }
        }
    }

    /// Entité `AdminOnly` — fixture des Scenario de refus `MRD-MCP-001` (`_get`, `_delete`,
    /// traduction) : `alice` sans appartenance admin est refusée avant toute requête sur la
    /// table (`rbac::static_verdict`, oracle d'existence de `rest/core.rs`).
    mod secret {
        use super::*;

        #[derive(Clone, Debug, PartialEq, Serialize, Deserialize, DeriveEntityModel)]
        #[sea_orm(table_name = "secrets")]
        pub struct Model {
            #[sea_orm(primary_key)]
            pub id: i32,
            pub title: String,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {}

        impl ActiveModelBehavior for ActiveModel {}

        impl MiryadResource for Entity {
            fn resource_name() -> &'static str {
                "secrets"
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

    /// Entité avec une colonne `Option<String>` (`note`) et une colonne obligatoire (`title`)
    /// — fixture du Scenario « `_create` : colonne non-`Option` absente rejetée avant la base,
    /// colonne `Option` absente est `None` ».
    mod draft {
        use super::*;

        #[derive(Clone, Debug, PartialEq, Serialize, Deserialize, DeriveEntityModel)]
        #[sea_orm(table_name = "drafts")]
        pub struct Model {
            #[sea_orm(primary_key)]
            pub id: i32,
            pub title: String,
            pub note: Option<String>,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {}

        impl ActiveModelBehavior for ActiveModel {}

        impl MiryadResource for Entity {
            fn resource_name() -> &'static str {
                "drafts"
            }
            fn read_policy() -> AccessPolicy {
                AccessPolicy::Public
            }
            fn write_policy() -> AccessPolicy {
                AccessPolicy::Public
            }
            fn owner_column() -> Option<Column> {
                None
            }
        }
    }

    /// Entité dont `before_create` rejette un `title` vide par `HookError::with_code(
    /// "WIDGET-001", "label must not be empty")` — fixture du Scenario « création MCP :
    /// `HookError` du hook applicatif traverse sans code `MRD-*` ».
    mod widget {
        use super::*;

        #[derive(Clone, Debug, PartialEq, Serialize, Deserialize, DeriveEntityModel)]
        #[sea_orm(table_name = "widgets")]
        pub struct Model {
            #[sea_orm(primary_key)]
            pub id: i32,
            pub title: String,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {}

        impl ActiveModelBehavior for ActiveModel {}

        impl MiryadResource for Entity {
            fn resource_name() -> &'static str {
                "widgets"
            }
            fn read_policy() -> AccessPolicy {
                AccessPolicy::Public
            }
            fn write_policy() -> AccessPolicy {
                AccessPolicy::Public
            }
            fn owner_column() -> Option<Column> {
                None
            }
            fn before_create(
                active: Self::ActiveModel,
                principal: &AuthPrincipal,
            ) -> Result<Self::ActiveModel, HookError> {
                let _ = principal;
                // `mark_all_set` de `rest::core` est passé avant : le corps client arrive en `Set`.
                if let Set(title) = &active.title
                    && title.is_empty()
                {
                    return Err(HookError::with_code("WIDGET-001", "label must not be empty"));
                }
                Ok(active)
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

    /// `Scenario` : « enregistrement : collision de `resource_name` refusée par assertion » —
    /// un type distinct dont `resource_name` retourne le même `recipes` déjà présent fait échouer
    /// `register` par `assert!`, message citant `recipes` (arbitré 2026-09-29 : plus de
    /// remplacement silencieux).
    #[test]
    #[should_panic(expected = "recipes")]
    fn register_collision_of_resource_name_panics() {
        let mut registry = McpToolRegistry::new(OutputFormat::Json);
        registry.register::<recipe::Entity>();
        registry.register::<alt::Entity>();
    }

    /// Second bras du `Scenario` « enregistrement : collision de `resource_name` refusée par
    /// assertion » : réenregistrer le même type une seconde fois échoue aussi — l'`assert!`
    /// n'est pas une re-registration idempotente (arbitré 2026-09-29, `Must` « une clé déjà
    /// présente fait échouer `register` »).
    #[test]
    #[should_panic(expected = "recipes")]
    fn register_same_type_twice_panics() {
        let mut registry = McpToolRegistry::new(OutputFormat::Json);
        registry.register::<recipe::Entity>();
        registry.register::<recipe::Entity>();
    }

    /// `Scenario` : « enregistrement : chaînage builder, une entrée par entité » — `register`
    /// rend `&mut Self` chaînable, une entrée par `resource_name`, clés en ordre croissant
    /// (`entities` est un `BTreeMap`, arbitré 2026-09-29).
    #[test]
    fn register_chains_and_orders_entities_by_name() {
        let mut registry = McpToolRegistry::new(OutputFormat::Json);
        let chained = registry
            .register::<recipe::Entity>()
            .register::<ingredient::Entity>();
        let keys: Vec<&'static str> = chained.entities.keys().copied().collect();
        assert_eq!(
            keys,
            ["ingredients", "recipes"],
            "une entrée par `resource_name`, dans l'ordre croissant des clés"
        );
    }

    /// `Scenario` : « deux entités enregistrées sont exposées dans l'ordre croissant des clés » —
    /// l'énumération des clés est stable et testable, indépendante de l'ordre d'enregistrement ;
    /// `handler.rs` itère `entities.values()` dans cet ordre pour `tools/list` sans le retrier.
    #[test]
    fn registered_entities_are_exposed_in_ascending_key_order() {
        let mut registry = McpToolRegistry::new(OutputFormat::Json);
        registry
            .register::<recipe::Entity>()
            .register::<ingredient::Entity>();
        let keys: Vec<&'static str> = registry.entities.keys().copied().collect();
        assert_eq!(
            keys,
            ["ingredients", "recipes"],
            "`recipes` enregistré en premier ne précède pas `ingredients` — l'ordre est celui \
             des clés, pas celui d'insertion"
        );
    }

    /// Base sqlite en mémoire migrée des seules tables de `crate::migration` (dont
    /// `miryad_users`, exigée par `users::resolve_user` côté `rest::core`) — table d'entité
    /// absente : toute atteinte de la base par la chaîne déléguée rendrait une `Database`
    /// observable.
    async fn migrated_db() -> DatabaseConnection {
        let db = Database::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite connects");
        Migrator::up(&db, None).await.expect("migrations apply cleanly");
        db
    }

    /// Même base, table `recipes` en plus — la base pleine des Scenario de dispatch.
    async fn test_db() -> DatabaseConnection {
        let db = migrated_db().await;
        create_entity_table(&db, recipe::Entity).await;
        db
    }

    /// Crée la table physique d'une entité fixture sur une base déjà migrée.
    async fn create_entity_table<E: sea_orm::EntityTrait>(db: &DatabaseConnection, entity: E) {
        let schema = Schema::new(db.get_database_backend());
        db.execute(&schema.create_table_from_entity(entity))
            .await
            .expect("entity table creates");
    }

    fn principal(subject: &str) -> AuthPrincipal {
        AuthPrincipal {
            subject: subject.to_string(),
            email: None,
            preferred_username: None,
            source: PrincipalSource::ApiToken { token_id: 0 },
        }
    }

    /// Ligne de fixture créée par le dispatch lui-même — le possesseur en est le principal,
    /// via l'injection d'`owner_column` de `rest::core::create`.
    async fn create_recipe(
        entity: &dyn McpEntity,
        db: &DatabaseConnection,
        principal: &AuthPrincipal,
        title: &str,
    ) -> Value {
        entity
            .call(
                McpOp::Create,
                db,
                principal,
                json!({"id": 0, "title": title, "owner_id": 0}),
            )
            .await
            .expect("seed create via dispatch succeeds")
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

    /// `Scenario` : « enregistrement : rien de `MiryadResource` n'est retenu, les politiques
    /// sont relues en direct » — le compteur de `read_policy` de la fixture strictement croissant
    /// entre deux `McpOp::Get` délégués prouve qu'aucun snapshot ne vit dans le registre :
    /// `rest::core::get` relit le trait à chaque appel (`rbac::static_verdict`).
    #[tokio::test]
    async fn registry_retains_no_metadata_reads_trait_live() {
        let db = migrated_db().await;
        create_entity_table(&db, counted::Entity).await;
        let inserted = counted::Entity::insert(counted::ActiveModel {
            title: Set("ligne".to_string()),
            ..Default::default()
        })
        .exec(&db)
        .await
        .expect("seed row inserts");
        let id = inserted.last_insert_id;
        let alice = principal("alice");
        let entity = McpEntityImpl::<counted::Entity>(std::marker::PhantomData);

        let before = counted::reads();
        entity
            .call(McpOp::Get, &db, &alice, json!({"id": id}))
            .await
            .expect("first delegated get succeeds");
        let after_first = counted::reads();
        entity
            .call(McpOp::Get, &db, &alice, json!({"id": id}))
            .await
            .expect("second delegated get succeeds");
        let after_second = counted::reads();

        assert!(
            after_first > before,
            "premier Get délégué : `read_policy` relu sur le trait ({before} -> {after_first})"
        );
        assert!(
            after_second > after_first,
            "second Get délégué : `read_policy` relu en direct, aucun snapshot ({after_first} -> {after_second})"
        );
    }

    /// `Scenario` : « `_list` sans arguments : trois `None` transmis tels quels à
    /// `rest::core::list` » — l'égalité des deux valeurs est la preuve de la non-réévaluation.
    #[tokio::test]
    async fn list_sans_arguments_egale_a_core_list_sans_options() {
        let db = test_db().await;
        let alice = principal("alice");
        let entity = McpEntityImpl::<recipe::Entity>(std::marker::PhantomData);
        for title in ["Tarte", "Pain"] {
            create_recipe(&entity, &db, &alice, title).await;
        }

        let dispatched = entity
            .call(McpOp::List, &db, &alice, json!({}))
            .await
            .expect("`_list` without arguments succeeds");
        let direct = crate::rest::core::list::<recipe::Entity>(&db, &alice, None, None, None)
            .await
            .expect("direct rest::core::list succeeds");
        assert_eq!(
            dispatched,
            serde_json::to_value(direct).expect("PagedResult serializes"),
            "la valeur rendue par le dispatch est identique au core::list(None, None, None) direct"
        );
        for key in ["items", "page", "per_page", "total_items", "total_pages"] {
            assert!(
                dispatched.get(key).is_some(),
                "clé `PagedResult` `{key}` présente"
            );
        }
    }

    /// `Scenario` : « `_list` : `page`, `per_page` et `filter` lus puis délégués une seule fois »
    /// — les trois valeurs partent telles quelles (`params.filter` en `as_deref`), aucune lecture
    /// intermédiaire ; le sort du `filter` hors fixture déclarée est le contrat `rest/core.sdd`.
    #[tokio::test]
    async fn list_arguments_forwarded_verbatim() {
        let db = test_db().await;
        let alice = principal("alice");
        let entity = McpEntityImpl::<recipe::Entity>(std::marker::PhantomData);
        for title in ["Tarte", "Tarte", "Pain"] {
            create_recipe(&entity, &db, &alice, title).await;
        }

        let dispatched = entity
            .call(
                McpOp::List,
                &db,
                &alice,
                json!({"page": 1, "per_page": 2, "filter": "Tarte"}),
            )
            .await
            .expect("`_list` avec page/per_page/filter réussit");
        let direct = crate::rest::core::list::<recipe::Entity>(&db, &alice, Some(1), Some(2), Some("Tarte"))
            .await
            .expect("core::list direct avec trois Some réussit");
        assert_eq!(
            dispatched,
            serde_json::to_value(direct).expect("PagedResult serializes"),
            "les trois valeurs sont transmises telles quelles, aucune lecture intermédiaire"
        );
    }

    /// `Scenario` : « tout outil appelé avec `arguments` `null` est `MRD-MCP-005` avant tout
    /// accès base » — les cinq bras, base intacte après les cinq tentatives.
    ///
    /// Qualification (Q6 arbitré par Sébastien le 2026-10-03, `registry.sdd` `Tasks`) : le test
    /// reste en deçà de l'exemple de `Raises` — lu comme illustratif — en verrouillant la
    /// sous-chaîne `expected struct` sans verrouiller le nom de structure (détail interne de
    /// `serde`). Le `Then` de la spec cite `invalid type: null` mesuré (harmonisé 2026-10-02).
    #[tokio::test]
    async fn null_arguments_rejected_before_any_dispatch() {
        let db = test_db().await;
        let alice = principal("alice");
        let entity = McpEntityImpl::<recipe::Entity>(std::marker::PhantomData);
        create_recipe(&entity, &db, &alice, "Tarte").await;
        let before = recipe::Entity::find().count(&db).await.expect("count succeeds");

        for op in [
            McpOp::List,
            McpOp::Get,
            McpOp::Create,
            McpOp::Update,
            McpOp::Delete,
        ] {
            let err = entity
                .call(op, &db, &alice, Value::Null)
                .await
                .expect_err("`Value::Null` n'est jamais une envelope Object acceptée");
            assert!(
                matches!(err, McpError::InvalidParams(_)),
                "{op:?} sur arguments nuls : {err}"
            );
            let display = err.to_string();
            assert!(
                display.starts_with("MRD-MCP-005: invalid params: "),
                "{op:?} : {display}"
            );
            assert!(
                display.contains("invalid type: null, expected struct"),
                "{op:?} sur arguments nuls — message reel de serde_json sous le graphe verrouille ; \
                 Q6 arbitre 2026-10-03 : sous-chaine verrouilee, nom de structure non verrouille : {display}"
            );
        }

        assert_eq!(
            recipe::Entity::find().count(&db).await.expect("count succeeds"),
            before,
            "aucune ligne écrite ni lue : la base est intacte après les cinq tentatives"
        );
    }

    /// `Scenario` : « paramètres de liste mal typés : `MRD-MCP-005`, clés absentes et inconnues
    /// tolérées » — les trois jeux mal typés heurtent le typage `u64`/`String` strict avant
    /// `rest::core::list` (observable : table absente ici ne produit aucune `Database`) ;
    /// clé inconnue et objet vide réussissent.
    #[tokio::test]
    async fn list_params_strict_typing_tolerates_unknown_keys() {
        let alice = principal("alice");
        let entity = McpEntityImpl::<recipe::Entity>(std::marker::PhantomData);

        let absent = migrated_db().await;
        for args in [
            json!({"page": "abc"}),
            json!({"per_page": -1}),
            json!({"filter": 42}),
        ] {
            let err = entity
                .call(McpOp::List, &absent, &alice, args.clone())
                .await
                .expect_err("le typage strict u64/String refuse avant deleg");
            assert!(matches!(err, McpError::InvalidParams(_)), "{args} -> {err}");
            assert!(err.to_string().starts_with("MRD-MCP-005"), "{args} : {err}");
        }

        let db = test_db().await;
        let unknown = entity
            .call(McpOp::List, &db, &alice, json!({"zzz": true}))
            .await
            .expect("clé inconnue tolérée (aucun deny_unknown_fields)");
        let empty = entity
            .call(McpOp::List, &db, &alice, json!({}))
            .await
            .expect("objet vide -> trois None (serde default)");
        assert_eq!(
            unknown, empty,
            "une clé inconnue au typage correct mais hors modèle est ignorée en silence, aligné \
             sur le Query axum de REST"
        );
    }

    /// `Scenario` : « `_get` : modèle `Ok`, refus `MRD-MCP-001`, inconnu `MRD-MCP-002` » — le `Ok`
    /// égale le `to_value` du `rest::core::get` direct (pas de second jugement de politique ici) ;
    /// `secret` `AdminOnly` refusé par un `alice` non-admin rend `Forbidden`, `id` jamais inséré
    /// rend `NotFound`, les deux depuis `From<RestError>` seuls.
    #[tokio::test]
    async fn get_dispatch_restitue_ok_forbidden_notfound() {
        let db = migrated_db().await;
        create_entity_table(&db, recipe::Entity).await;
        create_entity_table(&db, secret::Entity).await;
        let alice = principal("alice");
        let recipes = McpEntityImpl::<recipe::Entity>(std::marker::PhantomData);
        let secrets = McpEntityImpl::<secret::Entity>(std::marker::PhantomData);

        let created = create_recipe(&recipes, &db, &alice, "Tarte").await;
        let id = i32::try_from(created["id"].as_i64().expect("id présent")).expect("id tient en i32");
        let ok = recipes
            .call(McpOp::Get, &db, &alice, json!({"id": id}))
            .await
            .expect("le possesseur relit sa ligne");
        let direct = crate::rest::core::get::<recipe::Entity>(&db, &alice, id)
            .await
            .expect("core::get direct réussit");
        assert_eq!(
            ok,
            serde_json::to_value(direct).expect("le modèle se sérialise"),
            "aucun second jugement de politique dans registry.rs — le Ok est celui de rest::core"
        );

        // Ligne `secret` insérée directement : la création MCP serait elle-même refusée par
        // `can_create` sur la politique AdminOnly.
        let inserted = secret::Entity::insert(secret::ActiveModel {
            title: Set("classifié".to_string()),
            ..Default::default()
        })
        .exec(&db)
        .await
        .expect("secret row inserts");
        let secret_id = inserted.last_insert_id;
        let forbidden = secrets
            .call(McpOp::Get, &db, &alice, json!({"id": secret_id}))
            .await
            .expect_err("AdminOnly refuse l'étranger");
        assert!(matches!(forbidden, McpError::Forbidden), "{forbidden}");
        assert_eq!(forbidden.to_string(), "MRD-MCP-001: forbidden");

        let missing = recipes
            .call(McpOp::Get, &db, &alice, json!({"id": 9999}))
            .await
            .expect_err("`id` jamais inséré");
        assert!(matches!(missing, McpError::NotFound), "{missing}");
        assert_eq!(missing.to_string(), "MRD-MCP-002: resource not found");
    }

    /// `Scenario` : « `_get` : `id` absent ou hors `i32` est `MRD-MCP-005` sans requête » — base
    /// sans table d'entité (les seules tables de `crate::migration` existent) : toute atteinte
    /// base produirait une `Database` visible ; chaîne, flottant, hors plage et clé absente sont
    /// refusés à la lecture `IdParams`.
    #[tokio::test]
    async fn get_id_missing_or_off_i32_is_invalid_params() {
        let db = migrated_db().await;
        let alice = principal("alice");
        let entity = McpEntityImpl::<recipe::Entity>(std::marker::PhantomData);

        for args in [
            json!({}),
            json!({"id": "3"}),
            json!({"id": 3.0}),
            json!({"id": 3_000_000_000u64}),
        ] {
            let err = entity
                .call(McpOp::Get, &db, &alice, args.clone())
                .await
                .expect_err("IdParams refuse avant toute requête");
            assert!(matches!(err, McpError::InvalidParams(_)), "{args} -> {err}");
            assert!(err.to_string().starts_with("MRD-MCP-005"), "{args} : {err}");
        }
    }

    /// `Scenario` : « `_create` : le corps part entier, `rest::core::create` garde les invariants
    /// PK et owner » — `id: 4242` et `owner_id: 999` du corps ne survivent pas à la délégation :
    /// PK attribuée par la base, `owner_id` du principal résolu.
    #[tokio::test]
    async fn create_delegation_ignores_pk_and_body_owner() {
        let db = test_db().await;
        let alice = principal("alice");
        let entity = McpEntityImpl::<recipe::Entity>(std::marker::PhantomData);

        let created = entity
            .call(
                McpOp::Create,
                &db,
                &alice,
                json!({"id": 4242, "title": "Tarte", "owner_id": 999}),
            )
            .await
            .expect("`_create` accepte un corps portant id et owner étrangers");
        let user = crate::users::resolve_user(&db, "alice", None)
            .await
            .expect("alice résout vers une ligne users::user");

        let created_id = created["id"].as_i64().expect("id présent");
        assert_ne!(created_id, 4242, "la PK est attribuée par la base, jamais 4242");
        assert_eq!(
            created["owner_id"], user.id,
            "owner_id est celui du principal, jamais 999"
        );

        let stored = recipe::Entity::find_by_id(i32::try_from(created_id).expect("id tient en i32"))
            .one(&db)
            .await
            .expect("relecture réussie")
            .expect("la ligne insérée est présente");
        assert_eq!(stored.owner_id, user.id, "le même invariant est posé en base");
        assert_eq!(
            stored.title, "Tarte",
            "le hook before_create default (identité) a traversé"
        );
        assert_eq!(recipe::Entity::find().count(&db).await.expect("count réussit"), 1);
    }

    /// `Scenario` : « `_create` : colonne non-`Option` absente rejetée avant la base, colonne
    /// `Option` absente est `None` » — sans `id` puis sans `title`, `missing field` de `serde`
    /// avec table intacte ; le jeu complet sans `note` réussit avec `note` à `None`.
    #[tokio::test]
    async fn create_non_option_column_required_option_column_none() {
        let db = migrated_db().await;
        create_entity_table(&db, draft::Entity).await;
        let alice = principal("alice");
        let drafts = McpEntityImpl::<draft::Entity>(std::marker::PhantomData);

        for (args, missing) in [(json!({"title": "sans pk"}), "id"), (json!({"id": 0}), "title")] {
            let err = drafts
                .call(McpOp::Create, &db, &alice, args.clone())
                .await
                .expect_err("les colonnes non-Option (PK comprise) sont exigees a la lecture du corps");
            assert!(matches!(err, McpError::InvalidParams(_)), "{args} -> {err}");
            assert!(
                err.to_string().contains(&format!("missing field `{missing}`")),
                "{args} -> {err}"
            );
        }
        assert_eq!(
            draft::Entity::find().count(&db).await.expect("count réussit"),
            0,
            "table intacte : aucun des deux refus n'a atteint rest::core::create"
        );

        let created = drafts
            .call(McpOp::Create, &db, &alice, json!({"id": 0, "title": "ok"}))
            .await
            .expect("un jeu complet sans la colonne Option réussit");
        assert!(
            created.get("note").expect("la clé note est sérialisée").is_null(),
            "`note` absent lu en None (cas Option implicite de serde)"
        );
    }

    /// `Scenario` : « création MCP : `HookError` du hook applicatif traverse sans code `MRD-*` »
    /// — variante `Application`, `Display` == message d'origine, `code` `WIDGET-001` lisible,
    /// aucune chaîne `MRD-`, aucune ligne insérée.
    #[tokio::test]
    async fn create_hook_error_crosses_without_mrd_code() {
        let db = migrated_db().await;
        create_entity_table(&db, widget::Entity).await;
        let alice = principal("alice");
        let widgets = McpEntityImpl::<widget::Entity>(std::marker::PhantomData);

        let err = widgets
            .call(McpOp::Create, &db, &alice, json!({"id": 0, "title": ""}))
            .await
            .expect_err("le hook rejette le titre vide");

        match &err {
            McpError::Application(hook) => {
                assert_eq!(
                    hook.code.as_deref(),
                    Some("WIDGET-001"),
                    "le code du hook passe intact"
                );
                assert_eq!(hook.message, "label must not be empty");
            }
            other => {
                panic!("variante Application attendue, HookError jamais affublé d'un code MRD : {other:?}")
            }
        }
        assert_eq!(
            err.to_string(),
            "label must not be empty",
            "Display == message d'origine"
        );
        assert!(
            !err.to_string().contains("MRD-"),
            "aucune chaîne MRD- nulle part : {err}"
        );
        assert_eq!(
            widget::Entity::find().count(&db).await.expect("count réussit"),
            0,
            "aucune ligne n'a été insérée"
        );
    }

    /// `Scenario` : « `_update` : une divergence d'`id` est inexprimable, l'`id` de l'enveloppe
    /// est la seule cible » — propriété du protocole (arbitré 2026-10-03) : la divergence de deux
    /// `id` dans une même envelope n'est pas constructible par `McpEntity::call` (les deux passes
    /// de lecture, `IdParams` puis `E::Model`, lisent la même clé `id` du `Value`). Conséquence
    /// verrouillée au niveau registre : la cible est la ligne de l'`id` de l'enveloppe, l'autre
    /// reste intacte, le `Ok` porte cet `id` — le forçage du `WHERE` est décidé et prouvé par
    /// `rest::core::update` (`../rest/core.sdd`).
    #[tokio::test]
    async fn update_uses_dispatch_id_not_body_id_as_target() {
        let db = test_db().await;
        let alice = principal("alice");
        let recipes = McpEntityImpl::<recipe::Entity>(std::marker::PhantomData);

        let r1 = create_recipe(&recipes, &db, &alice, "R1").await;
        let r2 = create_recipe(&recipes, &db, &alice, "R2").await;
        let id1 = i32::try_from(r1["id"].as_i64().expect("id de R1")).expect("id tient en i32");
        let id2 = i32::try_from(r2["id"].as_i64().expect("id de R2")).expect("id tient en i32");

        let updated = recipes
            .call(
                McpOp::Update,
                &db,
                &alice,
                json!({"id": id1, "title": "R1 modifiee", "owner_id": 0}),
            )
            .await
            .expect("`_update` réussit");
        assert_eq!(updated["id"], id1, "le Ok rendu porte l'id de dispatch (R1)");
        assert_eq!(updated["title"], "R1 modifiee");

        let first = recipe::Entity::find_by_id(id1)
            .one(&db)
            .await
            .expect("relecture de R1 réussit")
            .expect("R1 est présente");
        assert_eq!(
            first.title, "R1 modifiee",
            "c'est la ligne de l'id d'envelope qui mute"
        );
        let second = recipe::Entity::find_by_id(id2)
            .one(&db)
            .await
            .expect("relecture de R2 réussit")
            .expect("R2 est présente");
        assert_eq!(second.title, "R2", "la ligne R2 est intacte après l'appel");
    }

    /// `Scenario` : « `_update` : sans `id` ou sans modèle complet, `MRD-MCP-005` sans écriture »
    /// — la passe qui échoue est désignée par le message `serde` respectif (`missing field `id``
    /// sur la première, `missing field `title`` sur la seconde) ; la ligne existante est inchangée.
    #[tokio::test]
    async fn update_missing_id_or_incomplete_body_invalid_params_no_write() {
        let db = test_db().await;
        let alice = principal("alice");
        let recipes = McpEntityImpl::<recipe::Entity>(std::marker::PhantomData);
        let created = create_recipe(&recipes, &db, &alice, "Tarte").await;
        let id = i32::try_from(created["id"].as_i64().expect("id présent")).expect("id tient en i32");

        let err = recipes
            .call(McpOp::Update, &db, &alice, json!({"title": "x", "owner_id": 0}))
            .await
            .expect_err("update sans id");
        assert!(matches!(err, McpError::InvalidParams(_)), "{err}");
        assert!(
            err.to_string().contains("missing field `id`"),
            "première passe (IdParams) désignée par le message serde : {err}"
        );

        let err = recipes
            .call(McpOp::Update, &db, &alice, json!({"id": id, "owner_id": 0}))
            .await
            .expect_err("update au corps incomplet");
        assert!(matches!(err, McpError::InvalidParams(_)), "{err}");
        assert!(
            err.to_string().contains("missing field `title`"),
            "seconde passe (E::Model) désignée par le message serde : {err}"
        );

        let stored = recipe::Entity::find_by_id(id)
            .one(&db)
            .await
            .expect("relecture réussit")
            .expect("la ligne est présente");
        assert_eq!(
            stored.title, "Tarte",
            "aucune validation d'enveloppe n'atteint la base"
        );
    }

    /// `Scenario` : « `_delete` : `Value::Null` pour seul succès, inconnu `MRD-MCP-002`, refus
    /// `MRD-MCP-001` » — premier `Delete` rend `Value::Null` brut (pas de `to_value` de modèle),
    /// la ligne déjà partie rend `NotFound`, la ligne `secret` rend `Forbidden` pour `alice`
    /// non-admin.
    #[tokio::test]
    async fn delete_returns_json_null_then_unknown_id_not_found() {
        let db = migrated_db().await;
        create_entity_table(&db, recipe::Entity).await;
        create_entity_table(&db, secret::Entity).await;
        let alice = principal("alice");
        let recipes = McpEntityImpl::<recipe::Entity>(std::marker::PhantomData);
        let secrets = McpEntityImpl::<secret::Entity>(std::marker::PhantomData);
        let created = create_recipe(&recipes, &db, &alice, "Tarte").await;
        let id = created["id"].as_i64().expect("id présent");

        let ok = recipes
            .call(McpOp::Delete, &db, &alice, json!({"id": id}))
            .await
            .expect("le possesseur supprime sa ligne");
        assert_eq!(
            ok,
            Value::Null,
            "seul succès fabriquable : Value::Null, sans to_value de modèle"
        );

        let again = recipes
            .call(McpOp::Delete, &db, &alice, json!({"id": id}))
            .await
            .expect_err("ligne déjà partie");
        assert!(matches!(again, McpError::NotFound), "{again}");
        assert_eq!(again.to_string(), "MRD-MCP-002: resource not found");

        let inserted = secret::Entity::insert(secret::ActiveModel {
            title: Set("classifié".to_string()),
            ..Default::default()
        })
        .exec(&db)
        .await
        .expect("secret row inserts");
        let secret_id = inserted.last_insert_id;
        let forbidden = secrets
            .call(McpOp::Delete, &db, &alice, json!({"id": secret_id}))
            .await
            .expect_err("alice non-admin ne supprime pas une ligne AdminOnly");
        assert!(matches!(forbidden, McpError::Forbidden), "{forbidden}");
        assert_eq!(forbidden.to_string(), "MRD-MCP-001: forbidden");
    }

    /// `Scenario` : « traduction `RestError` → `McpError` sans perte de décision, `UnknownTool`
    /// jamais construit d'ici » — `Get` sur `recipes` (`OwnerOnly`) dont la table est absente rend
    /// `Database` (`MRD-MCP-003`), `Delete` d'un `id` jamais inséré rend `NotFound`
    /// (`MRD-MCP-002`), `Get` d'une ligne `secret` (`AdminOnly`) présente par `alice` sans admin
    /// rend `Forbidden` (`MRD-MCP-001` — `static_verdict` refuse avant tout accès table). Trois
    /// verdicts distincts par le seul `From<RestError>` de `error.rs`, mêmes verdicts que REST sur
    /// les mêmes entrées brutes ; ni `UnknownTool` ni `Internal` ne sont observables par `call`.
    #[tokio::test]
    async fn rest_to_mcp_error_translation_loses_no_decision() {
        let alice = principal("alice");
        let recipes = McpEntityImpl::<recipe::Entity>(std::marker::PhantomData);

        let absent = migrated_db().await;
        let database = recipes
            .call(McpOp::Get, &absent, &alice, json!({"id": 1}))
            .await
            .expect_err("table d'entité absente");
        assert!(matches!(database, McpError::Database(_)), "{database}");
        assert!(
            database.to_string().starts_with("MRD-MCP-003: database error"),
            "le préfixe du From traverse : {database}"
        );

        let db = migrated_db().await;
        create_entity_table(&db, recipe::Entity).await;
        create_entity_table(&db, secret::Entity).await;
        let missing = recipes
            .call(McpOp::Delete, &db, &alice, json!({"id": 9999}))
            .await
            .expect_err("id jamais inséré");
        assert!(matches!(missing, McpError::NotFound), "{missing}");
        assert_eq!(missing.to_string(), "MRD-MCP-002: resource not found");

        let inserted = secret::Entity::insert(secret::ActiveModel {
            title: Set("classifié".to_string()),
            ..Default::default()
        })
        .exec(&db)
        .await
        .expect("secret row inserts");
        let secret_id = inserted.last_insert_id;
        let secrets = McpEntityImpl::<secret::Entity>(std::marker::PhantomData);
        let forbidden = secrets
            .call(McpOp::Get, &db, &alice, json!({"id": secret_id}))
            .await
            .expect_err("alice sans droit sur la ligne secret");
        assert!(matches!(forbidden, McpError::Forbidden), "{forbidden}");
        assert_eq!(forbidden.to_string(), "MRD-MCP-001: forbidden");

        for err in [database, missing, forbidden] {
            assert!(
                !matches!(err, McpError::UnknownTool(_) | McpError::Internal(_)),
                "call ne peut retourner ni UnknownTool ni Internal depuis registry.rs : {err}"
            );
        }
    }
}
