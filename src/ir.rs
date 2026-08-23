//! Représentation intermédiaire (IR) par entité — feature 8. Dérivée des métadonnées SeaORM déjà
//! obligatoires pour toute `MiryadResource` (aucune annotation supplémentaire à ajouter par
//! l'app), destinée au générateur frontend TypeScript du template `miryad`. Séparée d'`openapi.json`
//! (feature 4b) : deux publics différents, deux artefacts — cf. `docs/architecture.md`.

use std::io;
use std::path::Path;

use sea_orm::{ColumnTrait, Iden, Iterable, PrimaryKeyToColumn, sea_query::ColumnType};
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

/// Produit l'IR d'une entité — fonction pure, comme `resource_openapi::<E>()`.
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
            FieldIr {
                is_primary_key: pk_names.contains(&name),
                name,
                r#type: ty,
                format,
                nullable: def.is_null(),
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
pub struct IrRegistry(Vec<EntityIr>);

impl IrRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register<E: MiryadResource>(&mut self) -> &mut Self {
        self.0.push(resource_ir::<E>());
        self
    }

    pub fn write_to_file(&self, path: impl AsRef<Path>) -> io::Result<()> {
        let json = serde_json::to_string_pretty(&self.0)?;
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
}
