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

/// Représentation intermédiaire d'une colonne — le vocabulaire de typage d'`OpenAPI` (`type` +
/// `format`) appliqué aux colonnes `SeaORM`, dérivé des métadonnées déjà obligatoires pour
/// `MiryadResource`, sans annotation supplémentaire de l'app.
#[derive(Debug, Clone, Serialize)]
pub struct FieldIr {
    /// Nom SQL de la colonne (`Iden::to_string`) — aussi la clé de comparaison avec la clé
    /// primaire et les colonnes déclarées par `MiryadResource`.
    pub name: String,
    /// Type primitif `OpenAPI` ("string" | "integer" | "number" | "boolean" | "object" | "array") —
    /// vocabulaire repris d'`OpenAPI`, pas un enum maison, déjà compris par l'outillage JS/TS.
    pub r#type: &'static str,
    /// Format affinateur d'`OpenAPI` (`int32`, `date-time`, …) apparié à `type` par la table de
    /// traduction interne — `None` quand la colonne n'en porte pas.
    pub format: Option<&'static str>,
    /// `true` si la colonne accepte `NULL` (`ColumnType::is_null` de `SeaORM`).
    pub nullable: bool,
    /// `true` si la colonne appartient à la clé primaire — comparaison par nom, la colonne
    /// `SeaORM` dérivée n'implémentant pas `PartialEq`.
    pub is_primary_key: bool,
    /// `resource_name` de l'entité référencée par une relation `belongs_to` sur cette colonne
    /// (`E::Relation`) — `None` si la colonne n'est pas une FK scalaire simple (pas de relation
    /// correspondante, FK composite, ou entité cible non enregistrée dans le même `IrRegistry`).
    /// Résolu par `IrRegistry`, cf. doc de module.
    pub references: Option<String>,
}

/// Représentation intermédiaire d'une entité — le contrat `MiryadResource` traduit pour le
/// générateur `TypeScript` du frontend : l'artefact interne, séparé d'`openapi.json` (deux
/// publics, deux artefacts).
#[derive(Debug, Clone, Serialize)]
pub struct EntityIr {
    /// Nom exposé côté API — `resource_name` déclaré par `MiryadResource`, clé d'identification
    /// de l'entité dans l'`IR`.
    pub resource_name: String,
    /// `IR` de chaque colonne, dans l'ordre d'itération de l'enum `Column` dérivé.
    pub fields: Vec<FieldIr>,
    /// `read_policy` déclarée par `MiryadResource` — relue telle quelle, sérialisée par `serde`
    /// en représentation externe.
    pub read_policy: AccessPolicy,
    /// `write_policy` déclarée par `MiryadResource`, même translation que `read_policy`.
    pub write_policy: AccessPolicy,
    /// Nom SQL de la colonne propriétaire déclarée, `None` si l'entité n'a pas de notion de
    /// propriétaire.
    pub owner_column: Option<String>,
    /// Nom SQL de la colonne filtrable déclarée (surfée par REST et MCP), `None` si déclarée
    /// absente.
    pub filter_column: Option<String>,
    /// Nom SQL de la colonne de libellé déclarée, `None` par défaut : le générateur retombe
    /// alors sur la clé primaire.
    pub label_column: Option<String>,
}

/// Traduit un `ColumnType` `SeaORM` en couple `(type, format)` `OpenAPI`. Volontairement pas
/// exhaustif au sens "une variante = un mapping unique garanti stable dans le temps" — `Decimal`/
/// `Money` restent en `string` pour ne pas perdre de précision en JSON, `Enum`/`Custom`/`Array`
/// retombent sur un type générique plutôt que d'échouer.
fn openapi_type(column_type: &ColumnType) -> (&'static str, Option<&'static str>) {
    use ColumnType::{
        Array, BigInteger, BigUnsigned, Binary, Blob, Boolean, Date, DateTime, Double, Float, Integer, Json,
        JsonBinary, SmallInteger, SmallUnsigned, Time, Timestamp, TimestampWithTimeZone, TinyInteger,
        TinyUnsigned, Unsigned, Uuid, VarBinary, Vector, Year,
    };
    match column_type {
        Blob | Binary(_) | VarBinary(_) => ("string", Some("byte")),
        TinyInteger | SmallInteger | Integer | TinyUnsigned | SmallUnsigned | Unsigned | Year => {
            ("integer", Some("int32"))
        }
        BigInteger | BigUnsigned => ("integer", Some("int64")),
        Float => ("number", Some("float")),
        Double => ("number", Some("double")),
        DateTime | Timestamp | TimestampWithTimeZone => ("string", Some("date-time")),
        Time => ("string", Some("time")),
        Date => ("string", Some("date")),
        Boolean => ("boolean", None),
        Json | JsonBinary => ("object", None),
        Uuid => ("string", Some("uuid")),
        Array(_) | Vector(_) => ("array", None),
        // Repli contractuel figé (arbitré 2026-09-29, `ir.sdd` `Must`/`Example`) : les variantes
        // `("string", None)` — `Char`/`String`/`Text`/`Custom`/`Interval`/`Bit`/`VarBit`/`Cidr`/
        // `Inet`/`MacAddr`/`LTree`/`Enum`, et `Decimal`/`Money` pour ne pas perdre de précision en
        // JSON — comme toute variante amont future (`ColumnType` est `#[non_exhaustive]` côté
        // sea-query) tombent ici, plutôt que de casser la compilation à chaque variante ajoutée.
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
#[must_use]
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
    /// Registre vide — comportement identique à `Default::default`.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Empile l'`IR` de l'entité `E` (via `resource_ir::<E>`) et sa paire `(table SQL,
    /// resource_name)` ; rend `&mut Self` pour l'accumulation en chaîne. À ce stade les
    /// `FieldIr::references` portent encore le nom de table brut : la résolution en
    /// `resource_name` n'a lieu qu'à l'écriture, une fois toutes les entités enregistrées.
    ///
    /// # Panics
    ///
    /// Refuse les doublons à l'enregistrement par `assert!` explicite (arbitré 2026-09-29,
    /// `ir.sdd` `Must` « Refuser les doublons à l'enregistrement ») : un `resource_name` déjà
    /// enregistré — y compris par le même type enregistré deux fois — ou une table SQL déjà
    /// revendiquée par une entité enregistrée est une erreur de programmation détectée au
    /// démarrage, même logique que la collision refusée au montage du routeur REST
    /// (`rest::resource_router`) et dans le `McpToolRegistry`. Plus de résolution silencieuse
    /// sur la première entité ni de doublon dans le tableau JSON ; le message cite le nom en
    /// collision et la branche violée.
    pub fn register<E: MiryadResource>(&mut self) -> &mut Self {
        let table_name = E::default().table_name().to_string();
        let resource_name = E::resource_name().to_string();
        assert!(
            !self
                .entities
                .iter()
                .any(|entity| entity.resource_name == resource_name),
            "`{resource_name}` is already registered in the IR registry — duplicate `resource_name` (the same entity type registered twice counts) is a programming error, refusing to register it a second time"
        );
        assert!(
            !self.table_names.iter().any(|(table, _)| table == &table_name),
            "`{table_name}` is already claimed by a registered entity in the IR registry — duplicate `table_name` is a programming error, refusing to register `{resource_name}` under it"
        );
        self.table_names.push((table_name, resource_name));
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

    /// # Errors
    ///
    /// Propage tel quel l'`io::Error` de `std::fs::write`, sans enrobage local ni code `MRD-*` :
    /// `NotFound` quand le dossier parent manque, `PermissionDenied`, cible en usage, etc. Sur
    /// échec de sérialisation — branche inatteignable par la surface publique d'aujourd'hui car
    /// tout champ d'`EntityIr` se sérialise — l'`io::Error` est bâti depuis `serde_json::Error`
    /// par la conversion amont (catégories `Syntax`/`Data` → `InvalidData`, `Eof` →
    /// `UnexpectedEof`).
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
        assert_eq!(ir.owner_column, None);
        assert_eq!(ir.filter_column, None);
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

    /// Fixture déclarée `AccessPolicy::Group` avec le groupe `admins` en lecture et en écriture —
    /// `Scenario` « la politique `AccessPolicy::Group` s'écrite taguée externement ».
    mod audited {
        use crate::resource::{AccessPolicy, MiryadResource};
        use sea_orm::entity::prelude::*;

        #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
        #[sea_orm(table_name = "audited_records")]
        pub struct Model {
            #[sea_orm(primary_key)]
            pub id: i32,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {}

        impl ActiveModelBehavior for ActiveModel {}

        impl MiryadResource for Entity {
            fn resource_name() -> &'static str {
                "audited-records"
            }
            fn read_policy() -> AccessPolicy {
                AccessPolicy::Group("admins")
            }
            fn write_policy() -> AccessPolicy {
                AccessPolicy::Group("admins")
            }
            fn owner_column() -> Option<Column> {
                None
            }
        }
    }

    /// Fichier temporaire à chemin unique par test (les tests tournent en parallèle), nettoyé à
    /// la sortie de scope — panic compris. `write_to_file` ne créant jamais de dossier, le
    /// dossier parent est créé ici pour les tests qui l'exigent.
    struct TempIrFile {
        dir: std::path::PathBuf,
        path: std::path::PathBuf,
    }

    impl TempIrFile {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("miryad-ir-{name}-{}", std::process::id()));
            std::fs::create_dir_all(&dir).expect("create tmp dir");
            Self {
                path: dir.join("ir.json"),
                dir,
            }
        }
    }

    impl Drop for TempIrFile {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.dir).ok();
        }
    }

    /// `Scenario` « `read_policy` et `write_policy` rendus tels que déclarés » — contraste
    /// `recipe::Entity` (`Public`/`OwnerOnly`) et `ingredient::Entity` (`AdminOnly` dans les deux
    /// sens) : chaque `EntityIr` porte les deux politiques exactement déclarées, sans croisement.
    #[test]
    fn resource_ir_reports_declared_policies() {
        let recipe_ir = resource_ir::<recipe::Entity>();
        assert_eq!(recipe_ir.read_policy, AccessPolicy::Public);
        assert_eq!(recipe_ir.write_policy, AccessPolicy::OwnerOnly);

        let ingredient_ir = resource_ir::<ingredient::Entity>();
        assert_eq!(ingredient_ir.read_policy, AccessPolicy::AdminOnly);
        assert_eq!(ingredient_ir.write_policy, AccessPolicy::AdminOnly);
    }

    /// `Scenario` « champs conservés dans l'ordre de `E::Column::iter` » — `recipe::Entity` itère
    /// en ordre de déclaration (`id`, `title`, `owner_id`, `notes`), non alphabétique : un tri
    /// furtif est discriminé par la double comparaison ordre attendu / ordre trié.
    #[test]
    fn resource_ir_fields_follow_column_iteration_order() {
        let ir = resource_ir::<recipe::Entity>();
        let names: Vec<&str> = ir.fields.iter().map(|field| field.name.as_str()).collect();
        assert_eq!(names, ["id", "title", "owner_id", "notes"]);

        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_ne!(names, sorted, "l'ordre de déclaration est justement non trié");
    }

    /// `Scenario` « `openapi_type` map toutes les variantes couvertes de `ColumnType` » — appel
    /// direct de l'auxiliaire privé variante par variante, selon la table `Example` de `ir.sdd`
    /// (même pattern que les tests de `resolve_reference_table`). Le rendu `null` de `format` en
    /// JSON (jamais une clé absente) est épinglé par `write_to_file_emits_exact_json_shape`.
    #[test]
    fn openapi_type_maps_every_covered_column_type_variant() {
        use sea_orm::sea_query::{ColumnType, IntoIden, RcOrArc};

        let cases: Vec<(ColumnType, (&str, Option<&str>))> = vec![
            (ColumnType::Char(None), ("string", None)),
            (ColumnType::string(None), ("string", None)),
            (ColumnType::Text, ("string", None)),
            (ColumnType::custom("citext"), ("string", None)),
            (ColumnType::Interval(None, None), ("string", None)),
            (ColumnType::Bit(None), ("string", None)),
            (ColumnType::VarBit(8), ("string", None)),
            (ColumnType::Cidr, ("string", None)),
            (ColumnType::Inet, ("string", None)),
            (ColumnType::MacAddr, ("string", None)),
            (ColumnType::LTree, ("string", None)),
            (
                ColumnType::Enum {
                    name: "mood".into_iden(),
                    variants: vec!["happy".into_iden()],
                },
                ("string", None),
            ),
            (ColumnType::Decimal(None), ("string", None)),
            (ColumnType::Money(None), ("string", None)),
            (ColumnType::TinyInteger, ("integer", Some("int32"))),
            (ColumnType::SmallInteger, ("integer", Some("int32"))),
            (ColumnType::Integer, ("integer", Some("int32"))),
            (ColumnType::TinyUnsigned, ("integer", Some("int32"))),
            (ColumnType::SmallUnsigned, ("integer", Some("int32"))),
            (ColumnType::Unsigned, ("integer", Some("int32"))),
            (ColumnType::Year, ("integer", Some("int32"))),
            (ColumnType::BigInteger, ("integer", Some("int64"))),
            (ColumnType::BigUnsigned, ("integer", Some("int64"))),
            (ColumnType::Float, ("number", Some("float"))),
            (ColumnType::Double, ("number", Some("double"))),
            (ColumnType::DateTime, ("string", Some("date-time"))),
            (ColumnType::Timestamp, ("string", Some("date-time"))),
            (ColumnType::TimestampWithTimeZone, ("string", Some("date-time"))),
            (ColumnType::Time, ("string", Some("time"))),
            (ColumnType::Date, ("string", Some("date"))),
            (ColumnType::Boolean, ("boolean", None)),
            (ColumnType::Json, ("object", None)),
            (ColumnType::JsonBinary, ("object", None)),
            (ColumnType::Uuid, ("string", Some("uuid"))),
            (ColumnType::Blob, ("string", Some("byte"))),
            (ColumnType::Binary(8), ("string", Some("byte"))),
            (ColumnType::var_binary(16), ("string", Some("byte"))),
            (
                ColumnType::Array(RcOrArc::new(ColumnType::Integer)),
                ("array", None),
            ),
            (ColumnType::Vector(None), ("array", None)),
        ];

        for (column_type, expected) in cases {
            assert_eq!(openapi_type(&column_type), expected, "mapping de {column_type:?}");
        }
    }

    /// `Scenario` « les clés et les valeurs `null` du fichier sont épinglées » — octet pour octet
    /// la sortie de `serde_json::to_string_pretty` sur le tableau résolu, sans newline final,
    /// clés exactement épinglées, chaque `None` rendu `null` explicite : `filter_column` à `None`
    /// sur `recipe`, `label_column` à `None` sur une entité sans libellé (`ingredient`), `format`
    /// et `references` nuls.
    #[test]
    fn write_to_file_emits_exact_json_shape() {
        use std::collections::BTreeSet;

        let file = TempIrFile::new("exact-shape");
        let mut registry = IrRegistry::new();
        registry.register::<recipe::Entity>();
        // `ingredient` complète la fixture : ses trois colonnes déclarées absentes épinglent le
        // `null` de `label_column` sur une entité sans libellé.
        registry.register::<ingredient::Entity>();
        registry.write_to_file(&file.path).expect("writes file");

        let content = std::fs::read_to_string(&file.path).expect("reads file");
        assert_eq!(
            content,
            serde_json::to_string_pretty(&registry.resolved_entities()).expect("pretty json"),
            "le fichier est octet pour octet la sortie de to_string_pretty"
        );
        assert!(!content.ends_with('\n'), "aucun newline final");

        let parsed: Vec<serde_json::Value> = serde_json::from_str(&content).expect("valid json");
        let entity_keys: BTreeSet<&str> = parsed[0]
            .as_object()
            .expect("entity object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            entity_keys,
            [
                "resource_name",
                "fields",
                "read_policy",
                "write_policy",
                "owner_column",
                "filter_column",
                "label_column",
            ]
            .into_iter()
            .collect(),
            "les sept clés de l'objet entité, aucune sautée ni renommée"
        );
        assert!(
            parsed[0]["filter_column"].is_null(),
            "filter_column `None` rendu `null`"
        );
        assert!(
            parsed[1]["label_column"].is_null(),
            "label_column `None` rendu `null`"
        );

        let notes = parsed[0]["fields"]
            .as_array()
            .expect("fields array")
            .iter()
            .find(|field| field["name"] == "notes")
            .expect("notes field");
        let field_keys: BTreeSet<&str> = notes
            .as_object()
            .expect("field object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            field_keys,
            [
                "name",
                "type",
                "format",
                "nullable",
                "is_primary_key",
                "references",
            ]
            .into_iter()
            .collect(),
            "les six clés du champ, `format` et `references` présents même nuls"
        );
        assert!(
            notes["format"].is_null(),
            "`format` nul explicite, pas clé absente"
        );
        assert!(notes["references"].is_null(), "`references` nul explicite");
    }

    /// `Scenario` « la politique `AccessPolicy::Group` s'écrite taguée externement » — fixture
    /// `audited::Entity` en `Group("admins")` dans les deux sens : le JSON porte l'objet à clé
    /// unique `{"Group":"admins"}` (tag externe amont de `resource::AccessPolicy` restitué tel
    /// quel), pas une chaîne plate.
    #[test]
    fn entity_ir_serializes_group_policy_externally_tagged() {
        let file = TempIrFile::new("group-policy");
        let mut registry = IrRegistry::new();
        registry.register::<audited::Entity>();
        registry.write_to_file(&file.path).expect("writes file");

        let content = std::fs::read_to_string(&file.path).expect("reads file");
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&content).expect("valid json");
        assert_eq!(parsed[0]["read_policy"], serde_json::json!({ "Group": "admins" }));
        assert_eq!(
            parsed[0]["write_policy"],
            serde_json::json!({ "Group": "admins" })
        );
    }

    /// `Scenario` « un registre vide écrit le tableau `[]` » — le vide est un artefact légal :
    /// le fichier porte exactement les deux octets `[]` et l'appel rend `Ok`.
    #[test]
    fn write_to_file_writes_empty_array_for_empty_registry() {
        let file = TempIrFile::new("empty-array");
        let registry = IrRegistry::new();
        registry
            .write_to_file(&file.path)
            .expect("un registre vide est un artefact légal");
        let content = std::fs::read(&file.path).expect("reads file");
        assert_eq!(content, b"[]", "exactement `[]`, sans espace ni newline");
    }

    /// `Scenario` « un dossier parent manquant lève `NotFound` » — l'`io::Error` de
    /// `std::fs::write` est propagé nu (kind `NotFound`, aucun enrobage ni code `MRD-*` ajouté),
    /// et aucun dossier n'est créé par le module.
    #[test]
    fn write_to_file_on_missing_parent_dir_yields_not_found_error() {
        let root = std::env::temp_dir().join(format!("miryad-ir-missing-parent-{}", std::process::id()));
        let path = root.join("absent-subdir").join("ir.json");

        let registry = IrRegistry::new();
        let error = registry
            .write_to_file(&path)
            .expect_err("un dossier parent inexistant échoue");
        assert_eq!(
            error.kind(),
            std::io::ErrorKind::NotFound,
            "l'erreur io traverse sans enrobage"
        );
        assert!(
            !root.join("absent-subdir").exists(),
            "write_to_file ne crée aucun dossier parent"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// `Scenario` « l'écriture est répétible sans muter le registre, et les enregistrements
    /// supplémentaires y sont lus » — preuve discriminante par `tag_id` : la table brute `tags`
    /// résout en `recipe-tags` à chaque écriture (si `resolved_entities` mutait l'état interne,
    /// la seconde écriture retombereit à `null`, `tags` ne correspondant plus à rien une fois
    /// consommé). Puis enregistrement d'`ingredient` sur le même registre : la troisième
    /// écriture le fait apparaître et `ingredient_id` résout enfin.
    #[test]
    fn write_to_file_is_repeatable_and_includes_later_registrations() {
        let file = TempIrFile::new("repeatable");
        let mut registry = IrRegistry::new();
        registry.register::<recipe::Entity>();
        registry.register::<tag::Entity>();
        registry.register::<recipe_ingredient::Entity>();

        registry.write_to_file(&file.path).expect("first write");
        let first = std::fs::read_to_string(&file.path).expect("reads file");

        registry.write_to_file(&file.path).expect("second write");
        let second = std::fs::read_to_string(&file.path).expect("reads file");
        assert_eq!(
            first, second,
            "la seconde écriture est identique octet pour octet"
        );

        let parsed: Vec<serde_json::Value> = serde_json::from_str(&second).expect("valid json");
        let links = &parsed[2];
        assert_eq!(links["resource_name"], "recipe-ingredients");
        let tag_id = links["fields"]
            .as_array()
            .expect("fields array")
            .iter()
            .find(|field| field["name"] == "tag_id")
            .expect("tag_id field");
        assert_eq!(
            tag_id["references"], "recipe-tags",
            "la résolution est rejouée depuis l'état interne resté brut"
        );

        registry.register::<ingredient::Entity>();
        registry.write_to_file(&file.path).expect("third write");
        let third = std::fs::read_to_string(&file.path).expect("reads file");
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&third).expect("valid json");
        let names: Vec<&str> = parsed
            .iter()
            .map(|entity| entity["resource_name"].as_str().expect("resource_name"))
            .collect();
        assert_eq!(
            names,
            ["recipes", "recipe-tags", "recipe-ingredients", "ingredients"],
            "l'enregistrement tardif apparaît au troisième écrit"
        );
        let ingredient_id = parsed[2]["fields"]
            .as_array()
            .expect("fields array")
            .iter()
            .find(|field| field["name"] == "ingredient_id")
            .expect("ingredient_id field");
        assert_eq!(
            ingredient_id["references"], "ingredients",
            "la référence devenue résoluble est résolue au rejou"
        );
    }

    /// `Scenario` « les entités apparaissent dans l'ordre d'enregistrement, chaînage inclus » —
    /// chaînage builder `register` → `&mut Self`, ordre d'enregistrement sans tri. Un second
    /// `register` de la même entité échoue par `assert!` (arbitré 2026-09-29) — verrouillé par
    /// `register_panics_on_duplicate_resource_name`, plus de second enregistrement supposé ici.
    #[test]
    fn write_to_file_preserves_registration_order_and_register_chains() {
        let file = TempIrFile::new("registration-order");
        let mut registry = IrRegistry::new();
        registry
            .register::<recipe::Entity>()
            .register::<ingredient::Entity>();
        registry.write_to_file(&file.path).expect("writes file");

        let content = std::fs::read_to_string(&file.path).expect("reads file");
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&content).expect("valid json");
        let names: Vec<&str> = parsed
            .iter()
            .map(|entity| entity["resource_name"].as_str().expect("resource_name"))
            .collect();
        assert_eq!(
            names,
            ["recipes", "ingredients"],
            "ordre d'enregistrement, sans tri"
        );
    }

    /// Fixture de la collision de `table_name` (`Tasks` « Refuser les doublons à
    /// l'enregistrement », arbitré 2026-09-29) : type distinct de `tag::Entity`, `resource_name`
    /// distinct (`duplicated-tags`), mais même table SQL physique `tags` — seul le garde de
    /// table doit se déclencher.
    mod duplicated_tag {
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
            fn resource_name() -> &'static str {
                "duplicated-tags"
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

    /// `Scenario` « doublon de `resource_name` » (`Tasks` « Refuser les doublons à
    /// l'enregistrement », arbitré 2026-09-29) : enregistrer deux fois la même entité —
    /// `register` doit échouer par `assert!` explicite, message citant `recipes` et la branche
    /// `resource_name`. Verrouille aussi le `Then` du `Scenario` « chaînage inclus » : « un
    /// second `register` de la même entité échoue par `assert!` ». Avant garde, le doublon est
    /// silencieusement absorbé (doublon dans le tableau JSON, résolution sur la première ligne).
    /// Un seul `expected` : le préfixe contiguous du message cite à la fois `recipes` et la
    /// branche `resource_name` (les `#[should_panic]` répétés sont un unused attribute).
    #[test]
    #[should_panic(
        expected = "`recipes` is already registered in the IR registry — duplicate `resource_name`"
    )]
    fn register_panics_on_duplicate_resource_name() {
        let mut registry = IrRegistry::new();
        registry.register::<recipe::Entity>();
        registry.register::<recipe::Entity>();
    }

    /// `Scenario` « collision de `table_name` » (`Tasks` « Refuser les doublons à
    /// l'enregistrement », arbitré 2026-09-29) : entité distincte au `resource_name` distinct
    /// (`duplicated-tags`) mais à la table SQL `tags` déjà revendiquée par `tag::Entity` — le
    /// contrôle de `resource_name` passe, c'est le contrôle de table qui doit se déclencher,
    /// message citant `tags` (deux tables qualifiées du même nom nu sont refusées, jamais
    /// confondues en silence — `Handles`). Un seul `expected` : le préfixe contiguous cite à la
    /// fois `tags` et la branche `table_name`.
    #[test]
    #[should_panic(
        expected = "`tags` is already claimed by a registered entity in the IR registry — duplicate `table_name`"
    )]
    fn register_panics_on_colliding_table_name() {
        let mut registry = IrRegistry::new();
        registry.register::<tag::Entity>();
        registry.register::<duplicated_tag::Entity>();
    }
}
