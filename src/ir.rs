//! Représentation intermédiaire (IR) par entité — feature 8. Dérivée des métadonnées SeaORM déjà
//! obligatoires pour toute `MiryadResource` (aucune annotation supplémentaire à ajouter par
//! l'app), destinée au générateur frontend TypeScript du template `miryad`. Séparée d'`openapi.json`
//! (feature 4b) : deux publics différents, deux artefacts — cf. `docs/architecture.md`.
//!
//! `FieldIr::references` (relations, #19) est résolu en deux temps : `resource_ir::<E>()` est pure
//! et ne connaît que le nom de table SQL physique cible (via `E::Relation`, même déclaration que
//! GraphQL — cf. `docs/architecture.md`, section "API GraphQL") ; `IrRegistry` résout ce nom de
//! table en `resource_name` une fois toutes les entités enregistrées (`resolved_entities`, appelée
//! par `write_to_file`) — une entité seule ne peut pas savoir quel `resource_name` porte une table
//! qu'elle référence.

use std::io;
use std::path::Path;

use sea_orm::{
    ColumnTrait, Iden, Identity, Iterable, PrimaryKeyToColumn, RelationDef, RelationTrait, RelationType,
    sea_query::{ColumnType, TableRef},
};
use serde::Serialize;

use crate::resource::{AccessPolicy, MiryadResource};

#[derive(Debug, Clone, Serialize)]
pub struct FieldIr {
    pub name: String,
    /// Type primitif OpenAPI ("string" | "integer" | "number" | "boolean" | "object" | "array") —
    /// vocabulaire repris d'OpenAPI, pas un enum maison, déjà compris par l'outillage JS/TS.
    pub r#type: &'static str,
    pub format: Option<&'static str>,
    pub nullable: bool,
    pub is_primary_key: bool,
    /// `resource_name` de l'entité référencée par une relation `belongs_to` sur cette colonne
    /// (`E::Relation`) — `None` si la colonne n'est pas une FK scalaire simple (pas de relation
    /// correspondante, FK composite, ou entité cible non enregistrée dans le même `IrRegistry`).
    /// Résolu par `IrRegistry`, cf. doc de module.
    pub references: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EntityIr {
    pub resource_name: String,
    pub fields: Vec<FieldIr>,
    pub read_policy: AccessPolicy,
    pub write_policy: AccessPolicy,
    pub owner_column: Option<String>,
    pub filter_column: Option<String>,
    pub label_column: Option<String>,
}

/// Traduit un `ColumnType` SeaORM en couple `(type, format)` OpenAPI. Volontairement pas
/// exhaustif au sens "une variante = un mapping unique garanti stable dans le temps" — `Decimal`/
/// `Money` restent en `string` pour ne pas perdre de précision en JSON, `Enum`/`Custom`/`Array`
/// retombent sur un type générique plutôt que d'échouer.
fn openapi_type(column_type: &ColumnType) -> (&'static str, Option<&'static str>) {
    use ColumnType::*;
    match column_type {
        Char(_)
        | String(_)
        | Text
        | Custom(_)
        | Interval(..)
        | Bit(_)
        | VarBit(_)
        | Cidr
        | Inet
        | MacAddr
        | LTree
        | Enum { .. } => ("string", None),
        Blob | Binary(_) | VarBinary(_) => ("string", Some("byte")),
        TinyInteger | SmallInteger | Integer | TinyUnsigned | SmallUnsigned | Unsigned | Year => {
            ("integer", Some("int32"))
        }
        BigInteger | BigUnsigned => ("integer", Some("int64")),
        Float => ("number", Some("float")),
        Double => ("number", Some("double")),
        Decimal(_) | Money(_) => ("string", None),
        DateTime | Timestamp | TimestampWithTimeZone => ("string", Some("date-time")),
        Time => ("string", Some("time")),
        Date => ("string", Some("date")),
        Boolean => ("boolean", None),
        Json | JsonBinary => ("object", None),
        Uuid => ("string", Some("uuid")),
        Array(_) | Vector(_) => ("array", None),
        // ColumnType est #[non_exhaustive] côté sea-query — un fallback générique plutôt que de
        // casser la compilation à chaque variante ajoutée en amont.
        _ => ("string", None),
    }
}

/// Table SQL physique référencée par `def`, si `def` est un `belongs_to` scalaire simple portant
/// `column_name` — `None` sinon (pas de correspondance, FK composite, ou relation inversée
/// `has_one`/`has_many` où `Self` ne porte pas la colonne).
///
/// Point d'attention : `RelationDef::is_owner` a une sémantique inversée par rapport à son propre
/// doc-comment dans `sea-orm 2.0.2` — `EntityTrait::belongs_to()` (où `Self` porte bien la FK)
/// construit avec `is_owner: false` ; `has_one()`/`has_many()` (où `Self` ne la porte pas, c'est
/// l'entité liée qui la porte) construisent avec `is_owner: true`. En clair `is_owner: true`
/// signifie "`Self` est parent/propriétaire de la relation" (cascade-save `ActiveModelEx`), pas
/// "porte la colonne FK" — vérifié dans `sea-orm-2.0.2/src/entity/relation.rs`
/// (`EntityTrait::belongs_to`/`has_one`/`has_many`). Une lecture littérale du doc-comment aurait
/// fait remonter la PK comme `references` sur les relations `has_one` inversées.
fn resolve_reference_table(def: &RelationDef, column_name: &str) -> Option<String> {
    if def.rel_type != RelationType::HasOne || def.is_owner {
        return None;
    }
    let Identity::Unary(from_col) = &def.from_col else {
        // FK composite (`Binary`/`Ternary`/`Many`) — non supporté, cf. #19.
        return None;
    };
    if from_col.to_string() != column_name {
        return None;
    }
    match &def.to_tbl {
        TableRef::Table(table_name, _) => Some(table_name.1.to_string()),
        _ => None,
    }
}

/// Produit l'IR d'une entité — fonction pure, comme `resource_openapi::<E>()`. `FieldIr::references`
/// porte ici le nom de table SQL brut, pas encore un `resource_name` — cf. doc de module.
pub fn resource_ir<E: MiryadResource>() -> EntityIr {
    // `Column` (dérivé par `DeriveEntityModel`) n'implémente pas `PartialEq` — comparaison par nom
    // (`Iden::to_string`), pas par `==` (cf. `docs/architecture.md`, "Point d'attention").
    let pk_names: Vec<String> = E::PrimaryKey::iter()
        .map(|pk| pk.into_column().to_string())
        .collect();

    let fields = E::Column::iter()
        .map(|col| {
            let def = col.def();
            let (ty, format) = openapi_type(def.get_column_type());
            let name = col.to_string();
            let references = E::Relation::iter().find_map(|rel| resolve_reference_table(&rel.def(), &name));
            FieldIr {
                is_primary_key: pk_names.contains(&name),
                name,
                r#type: ty,
                format,
                nullable: def.is_null(),
                references,
            }
        })
        .collect();

    EntityIr {
        resource_name: E::resource_name().to_string(),
        fields,
        read_policy: E::read_policy(),
        write_policy: E::write_policy(),
        owner_column: E::owner_column().map(|c| c.to_string()),
        filter_column: E::filter_column().map(|c| c.to_string()),
        label_column: E::label_column().map(|c| c.to_string()),
    }
}

/// Accumule l'IR de plusieurs entités et la sérialise — même registre-pattern que
/// `McpToolRegistry`/`PolicyRegistry` (feature 6/5), pour rester cohérent avec le reste du crate.
/// miryad-core fournit cette fonction ; produire le fichier (binaire dédié, ou sous-commande du
/// binaire backend) reste à la charge de l'app — pas d'exécutable ici.
#[derive(Debug, Default)]
pub struct IrRegistry {
    entities: Vec<EntityIr>,
    /// `(table SQL physique, resource_name)` par entité enregistrée — sert uniquement à résoudre
    /// `FieldIr::references` (nom de table brut → `resource_name`) dans `resolved_entities`.
    table_names: Vec<(String, String)>,
}

impl IrRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register<E: MiryadResource>(&mut self) -> &mut Self {
        self.table_names.push((
            E::default().table_name().to_string(),
            E::resource_name().to_string(),
        ));
        self.entities.push(resource_ir::<E>());
        self
    }

    /// Résout `FieldIr::references` (nom de table brut → `resource_name`) sur une copie des
    /// entités enregistrées — `resource_ir::<E>()` seule ne connaît pas les autres entités,
    /// cette résolution ne peut se faire qu'une fois toutes connues. `references` reste `None`
    /// si la table référencée n'appartient à aucune entité enregistrée dans ce registre (le
    /// frontend ne peut de toute façon pas lier vers une ressource absente de l'IR).
    fn resolved_entities(&self) -> Vec<EntityIr> {
        self.entities
            .iter()
            .cloned()
            .map(|mut entity| {
                for field in &mut entity.fields {
                    field.references = field.references.as_deref().and_then(|raw_table| {
                        self.table_names
                            .iter()
                            .find(|(table, _)| table == raw_table)
                            .map(|(_, resource_name)| resource_name.clone())
                    });
                }
                entity
            })
            .collect()
    }

    pub fn write_to_file(&self, path: impl AsRef<Path>) -> io::Result<()> {
        let json = serde_json::to_string_pretty(&self.resolved_entities())?;
        std::fs::write(path, json)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    mod recipe {
        use crate::resource::{AccessPolicy, MiryadResource};
        use sea_orm::entity::prelude::*;

        #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
        #[sea_orm(table_name = "recipes")]
        pub struct Model {
            #[sea_orm(primary_key)]
            pub id: i32,
            pub title: String,
            pub owner_id: i32,
            pub notes: Option<String>,
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
            fn filter_column() -> Option<Column> {
                None
            }
            fn label_column() -> Option<Column> {
                Some(Column::Title)
            }
        }
    }

    mod ingredient {
        use crate::resource::{AccessPolicy, MiryadResource};
        use sea_orm::entity::prelude::*;

        #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
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

    mod tag {
        use crate::resource::{AccessPolicy, MiryadResource};
        use sea_orm::entity::prelude::*;

        #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
        #[sea_orm(table_name = "tags")]
        pub struct Model {
            #[sea_orm(primary_key)]
            pub id: i32,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {}

        impl ActiveModelBehavior for ActiveModel {}

        impl MiryadResource for Entity {
            // Délibérément différent du nom de table ("tags") — rend le test de résolution
            // (nom de table brut -> resource_name) discriminant plutôt qu'une coïncidence.
            fn resource_name() -> &'static str {
                "recipe-tags"
            }
            fn read_policy() -> AccessPolicy {
                AccessPolicy::Public
            }
            fn write_policy() -> AccessPolicy {
                AccessPolicy::AdminOnly
            }
            fn owner_column() -> Option<Column> {
                None
            }
        }
    }

    /// Table de liaison recipe<->ingredient, avec une FK supplémentaire vers `tag` — fixture pour
    /// les tests de relations (#19) : `recipe_id`/`ingredient_id` couvrent le cas nominal,
    /// `tag_id` le cas où `resource_name` diffère du nom de table.
    mod recipe_ingredient {
        use crate::resource::{AccessPolicy, MiryadResource};
        use sea_orm::entity::prelude::*;

        #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
        #[sea_orm(table_name = "recipe_ingredients")]
        pub struct Model {
            #[sea_orm(primary_key)]
            pub id: i32,
            pub recipe_id: i32,
            pub ingredient_id: i32,
            pub tag_id: i32,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {
            #[sea_orm(
                belongs_to = "super::recipe::Entity",
                from = "Column::RecipeId",
                to = "super::recipe::Column::Id"
            )]
            Recipe,
            #[sea_orm(
                belongs_to = "super::ingredient::Entity",
                from = "Column::IngredientId",
                to = "super::ingredient::Column::Id"
            )]
            Ingredient,
            #[sea_orm(
                belongs_to = "super::tag::Entity",
                from = "Column::TagId",
                to = "super::tag::Column::Id"
            )]
            Tag,
        }

        impl ActiveModelBehavior for ActiveModel {}

        impl MiryadResource for Entity {
            fn resource_name() -> &'static str {
                "recipe-ingredients"
            }
            fn read_policy() -> AccessPolicy {
                AccessPolicy::Public
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
    fn resource_ir_reflects_fields_types_and_policy() {
        let ir = resource_ir::<recipe::Entity>();

        assert_eq!(ir.resource_name, "recipes");
        assert_eq!(ir.read_policy, AccessPolicy::Public);
        assert_eq!(ir.write_policy, AccessPolicy::OwnerOnly);
        assert_eq!(ir.owner_column.as_deref(), Some("owner_id"));
        assert_eq!(ir.label_column.as_deref(), Some("title"));
        assert_eq!(ir.filter_column, None);

        let id = ir.fields.iter().find(|f| f.name == "id").expect("id field");
        assert_eq!(id.r#type, "integer");
        assert!(id.is_primary_key);
        assert!(!id.nullable);

        let notes = ir.fields.iter().find(|f| f.name == "notes").expect("notes field");
        assert_eq!(notes.r#type, "string");
        assert!(notes.nullable);
    }

    #[test]
    fn entity_without_label_column_override_defaults_to_none() {
        let ir = resource_ir::<ingredient::Entity>();
        assert_eq!(ir.label_column, None);
    }

    #[test]
    fn write_to_file_produces_valid_json_array() {
        let dir = std::env::temp_dir().join(format!("miryad-ir-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create tmp dir");
        let path = dir.join("ir.json");

        let mut registry = IrRegistry::new();
        registry.register::<recipe::Entity>();
        registry.write_to_file(&path).expect("writes file");

        let content = std::fs::read_to_string(&path).expect("reads file");
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&content).expect("valid json");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0]["resource_name"], "recipes");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resource_ir_reports_raw_table_name_for_belongs_to_relations() {
        // Appelée seule (hors IrRegistry), resource_ir::<E>() ne peut pas connaître les
        // resource_name des autres entités — references porte le nom de table SQL brut.
        let ir = resource_ir::<recipe_ingredient::Entity>();

        let recipe_id = ir
            .fields
            .iter()
            .find(|f| f.name == "recipe_id")
            .expect("recipe_id field");
        assert_eq!(recipe_id.references.as_deref(), Some("recipes"));

        let ingredient_id = ir
            .fields
            .iter()
            .find(|f| f.name == "ingredient_id")
            .expect("ingredient_id field");
        assert_eq!(ingredient_id.references.as_deref(), Some("ingredients"));

        // tag::Entity::resource_name() == "recipe-tags", mais sa table SQL est "tags" — c'est bien
        // le nom de table qui doit apparaître ici, la résolution en resource_name est le rôle
        // d'IrRegistry, pas de resource_ir seule.
        let tag_id = ir
            .fields
            .iter()
            .find(|f| f.name == "tag_id")
            .expect("tag_id field");
        assert_eq!(tag_id.references.as_deref(), Some("tags"));

        let id = ir.fields.iter().find(|f| f.name == "id").expect("id field");
        assert_eq!(id.references, None);
    }

    #[test]
    fn ir_registry_resolves_references_to_resource_name() {
        let mut registry = IrRegistry::new();
        registry.register::<recipe::Entity>();
        registry.register::<ingredient::Entity>();
        registry.register::<tag::Entity>();
        registry.register::<recipe_ingredient::Entity>();

        let resolved = registry.resolved_entities();
        let recipe_ingredient_ir = resolved
            .iter()
            .find(|e| e.resource_name == "recipe-ingredients")
            .expect("recipe_ingredient registered");

        let recipe_id = recipe_ingredient_ir
            .fields
            .iter()
            .find(|f| f.name == "recipe_id")
            .expect("recipe_id field");
        assert_eq!(recipe_id.references.as_deref(), Some("recipes"));

        // Cas discriminant : la table "tags" doit résoudre vers le resource_name "recipe-tags",
        // pas rester à "tags" (ce qui prouverait un simple passthrough, pas une vraie résolution).
        let tag_id = recipe_ingredient_ir
            .fields
            .iter()
            .find(|f| f.name == "tag_id")
            .expect("tag_id field");
        assert_eq!(tag_id.references.as_deref(), Some("recipe-tags"));
    }

    #[test]
    fn ir_registry_leaves_reference_unresolved_when_target_entity_not_registered() {
        let mut registry = IrRegistry::new();
        registry.register::<recipe_ingredient::Entity>();
        // recipe/ingredient/tag volontairement non enregistrées.

        let resolved = registry.resolved_entities();
        let recipe_ingredient_ir = &resolved[0];

        for field in ["recipe_id", "ingredient_id", "tag_id"] {
            let field_ir = recipe_ingredient_ir
                .fields
                .iter()
                .find(|f| f.name == field)
                .unwrap_or_else(|| panic!("{field} field"));
            assert_eq!(field_ir.references, None, "{field} should stay unresolved");
        }
    }

    fn relation_def(
        rel_type: RelationType,
        is_owner: bool,
        from_col: Identity,
        to_tbl: &'static str,
    ) -> RelationDef {
        use sea_orm::sea_query::{ConditionType, IntoIden, IntoTableRef};

        RelationDef {
            rel_type,
            from_tbl: "from".into_table_ref(),
            to_tbl: to_tbl.into_table_ref(),
            from_col,
            to_col: Identity::Unary("id".into_iden()),
            is_owner,
            skip_fk: false,
            on_delete: None,
            on_update: None,
            on_condition: None,
            fk_name: None,
            condition_type: ConditionType::All,
        }
    }

    #[test]
    fn resolve_reference_table_matches_belongs_to_on_the_right_column() {
        use sea_orm::sea_query::IntoIden;

        let def = relation_def(
            RelationType::HasOne,
            false,
            Identity::Unary("recipe_id".into_iden()),
            "recipes",
        );
        assert_eq!(
            resolve_reference_table(&def, "recipe_id").as_deref(),
            Some("recipes")
        );
        // Mauvaise colonne — pas de correspondance.
        assert_eq!(resolve_reference_table(&def, "other_id"), None);
    }

    #[test]
    fn resolve_reference_table_ignores_reversed_has_one_has_many() {
        use sea_orm::sea_query::IntoIden;

        // has_one()/has_many() inversés : Self ne porte pas la colonne (is_owner: true) — cf.
        // Point d'attention sur resolve_reference_table.
        let has_one_reversed = relation_def(
            RelationType::HasOne,
            true,
            Identity::Unary("id".into_iden()),
            "recipes",
        );
        assert_eq!(resolve_reference_table(&has_one_reversed, "id"), None);

        let has_many = relation_def(
            RelationType::HasMany,
            false,
            Identity::Unary("recipe_id".into_iden()),
            "recipes",
        );
        assert_eq!(resolve_reference_table(&has_many, "recipe_id"), None);
    }

    #[test]
    fn resolve_reference_table_ignores_composite_foreign_keys() {
        use sea_orm::sea_query::IntoIden;

        let composite = relation_def(
            RelationType::HasOne,
            false,
            Identity::Binary("a_id".into_iden(), "b_id".into_iden()),
            "recipes",
        );
        assert_eq!(resolve_reference_table(&composite, "a_id"), None);
    }
}
