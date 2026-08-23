use vynil_core::hbs::HandleBars;

use crate::mcp::error::McpError;

/// Format de sortie des tools MCP — fixé une fois par l'app, au montage du serveur (pas
/// reconfigurable par appel, cohérent avec REST/GraphQL). `Json`/`Yaml`/`Markdown` sont des
/// templates Handlebars fournis en dur par miryad-core ; `Custom` est le même mécanisme, avec le
/// template de l'app à la place du défaut — pas un quatrième chemin de code séparé.
#[derive(Debug, Clone)]
pub enum OutputFormat {
    Json,
    Yaml,
    Markdown,
    /// Template Handlebars fourni par l'app, appliqué à tous les tools (list/get/create/update/
    /// delete) quelle que soit la forme des données — à l'app de gérer les deux formes si besoin
    /// (un enregistrement seul, ou une page `{ items, page, per_page, total_items, total_pages }`).
    Custom(String),
}

/// Un enregistrement seul (`get`/`create`/`update`) et une page de résultats (`list`) n'ont pas
/// la même forme JSON — le template par défaut à appliquer diffère selon le cas, même pour un
/// seul et même `OutputFormat`. Non exposé à l'app : c'est un détail de rendu interne.
#[derive(Debug, Clone, Copy)]
pub(crate) enum RenderShape {
    Record,
    List,
}

const DEFAULT_JSON_TEMPLATE: &str = r#"{{json_to_str this format="json_pretty"}}"#;
const DEFAULT_YAML_TEMPLATE: &str = r#"{{json_to_str this format="yaml"}}"#;
const DEFAULT_MARKDOWN_RECORD_TEMPLATE: &str = "{{#each this}}\n- **{{@key}}**: {{this}}\n{{/each}}\n";
const DEFAULT_MARKDOWN_LIST_TEMPLATE: &str = "\
{{#each items}}\n\
- {{#each this}}{{@key}}={{this}} {{/each}}\n\
{{/each}}\n\
_page {{page}}/{{total_pages}}, {{total_items}} item(s) au total_\n";

impl OutputFormat {
    fn template(&self, shape: RenderShape) -> &str {
        match (self, shape) {
            (OutputFormat::Custom(template), _) => template,
            (OutputFormat::Json, _) => DEFAULT_JSON_TEMPLATE,
            (OutputFormat::Yaml, _) => DEFAULT_YAML_TEMPLATE,
            (OutputFormat::Markdown, RenderShape::Record) => DEFAULT_MARKDOWN_RECORD_TEMPLATE,
            (OutputFormat::Markdown, RenderShape::List) => DEFAULT_MARKDOWN_LIST_TEMPLATE,
        }
    }
}

pub(crate) fn render(
    engine: &mut HandleBars,
    format: &OutputFormat,
    shape: RenderShape,
    data: &serde_json::Value,
) -> Result<String, McpError> {
    engine
        .render(format.template(shape), data)
        .map_err(|e| McpError::Render(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn json_format_renders_valid_json() {
        let mut engine = HandleBars::new();
        let data = json!({"id": 1, "title": "Tarte"});
        let output = render(&mut engine, &OutputFormat::Json, RenderShape::Record, &data).expect("renders");
        let parsed: serde_json::Value = serde_json::from_str(&output).expect("valid json");
        assert_eq!(parsed["title"], "Tarte");
    }

    #[test]
    fn yaml_format_renders_yaml_not_json() {
        let mut engine = HandleBars::new();
        let data = json!({"id": 1, "title": "Tarte"});
        let output = render(&mut engine, &OutputFormat::Yaml, RenderShape::Record, &data).expect("renders");
        assert!(output.contains("title: Tarte") || output.contains("title: \"Tarte\""));
        assert!(serde_json::from_str::<serde_json::Value>(&output).is_err());
    }

    #[test]
    fn markdown_format_renders_field_list() {
        let mut engine = HandleBars::new();
        let data = json!({"id": 1, "title": "Tarte"});
        let output =
            render(&mut engine, &OutputFormat::Markdown, RenderShape::Record, &data).expect("renders");
        assert!(output.contains("title"));
        assert!(output.contains("Tarte"));
    }

    #[test]
    fn custom_format_uses_supplied_template_not_the_default() {
        let mut engine = HandleBars::new();
        let data = json!({"id": 1, "title": "Tarte"});
        let custom = OutputFormat::Custom("Recette : {{title}}".to_string());
        let output = render(&mut engine, &custom, RenderShape::Record, &data).expect("renders");
        assert_eq!(output, "Recette : Tarte");
    }
}
