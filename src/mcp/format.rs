use vynil_core::hbs::HandleBars;

use crate::mcp::error::McpError;

/// Format de sortie des tools MCP — fixé une fois par l'app, au montage du serveur (pas
/// reconfigurable par appel, cohérent avec REST/GraphQL). `Json`/`Yaml`/`Markdown` sont des
/// templates Handlebars fournis en dur par miryad-core ; `Custom` est le même mécanisme, avec le
/// template de l'app à la place du défaut — pas un quatrième chemin de code séparé.
#[derive(Debug, Clone)]
pub enum OutputFormat {
    /// Sortie JSON via le template `Handlebars` intégré (rendu `json_to_str`
    /// préformaté), identique pour un enregistrement comme pour une page.
    Json,
    /// Sortie YAML via le template `Handlebars` intégré, identique pour un
    /// enregistrement comme pour une page.
    Yaml,
    /// Sortie Markdown via les templates `Handlebars` intégrés — liste de champs pour
    /// un enregistrement seul, page de résultats avec compteurs pour une liste.
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

    /// Page de référence des Scenario `list` — totaux tels que les rend `sea_orm` via
    /// `rest::core::list` (`../query.sdd`), `per_page` jamais consommé par le gabarit.
    fn page(items: serde_json::Value, page: u64, total_items: u64, total_pages: u64) -> serde_json::Value {
        let mut data = json!({
            "page": page,
            "per_page": 100,
            "total_items": total_items,
            "total_pages": total_pages,
        });
        data["items"] = items;
        data
    }

    /// `Scenario` : « enregistrement rendu en JSON par défaut » — texte exact pretty deux
    /// espaces sans saut de ligne final, clés lexicographiques (cartes `serde_json` en
    /// `BTreeMap`, contrat wire gelé 2026-09-29), et re-sérialisable en JSON relu.
    #[test]
    fn json_format_renders_valid_json() {
        let mut engine = HandleBars::new();
        let data = json!({"id": 1, "title": "Tarte"});
        let output = render(&mut engine, &OutputFormat::Json, RenderShape::Record, &data).expect("renders");
        assert_eq!(
            output, "{\n  \"id\": 1,\n  \"title\": \"Tarte\"\n}",
            "pretty deux espaces, sans saut de ligne final, `id` avant `title` en ordre \
             lexicographique"
        );
        let parsed: serde_json::Value = serde_json::from_str(&output).expect("valid json");
        assert_eq!(parsed["title"], "Tarte");
    }

    /// `Scenario` : « enregistrement rendu en YAML par défaut » — texte exact `---` retiré,
    /// saut de ligne final conservé ; ce texte n'est pas un JSON valide relu par `serde_json`.
    #[test]
    fn yaml_format_renders_yaml_not_json() {
        let mut engine = HandleBars::new();
        let data = json!({"id": 1, "title": "Tarte"});
        let output = render(&mut engine, &OutputFormat::Yaml, RenderShape::Record, &data).expect("renders");
        assert_eq!(
            output, "id: 1\ntitle: Tarte\n",
            "prefixe `---` retire, saut de ligne final conserve"
        );
        assert!(serde_json::from_str::<serde_json::Value>(&output).is_err());
    }

    /// `Scenario` : « enregistrement rendu en Markdown par défaut » — une ligne `- **clé**: valeur`
    /// par champ, clés lexicographiques ; l'objet vide rend sans erreur malgré le mode strict.
    #[test]
    fn markdown_format_renders_field_list() {
        let mut engine = HandleBars::new();
        let data = json!({"id": 1, "title": "Tarte"});
        let output =
            render(&mut engine, &OutputFormat::Markdown, RenderShape::Record, &data).expect("renders");
        assert_eq!(
            output, "- **id**: 1\n- **title**: Tarte\n",
            "une ligne par champ en ordre lexicographique ; les newline de gabarit adjacentes aux \
             blocs standalone sont consommees par le moteur"
        );
        let empty = render(
            &mut engine,
            &OutputFormat::Markdown,
            RenderShape::Record,
            &json!({}),
        )
        .expect("objet vide rend sans erreur en mode strict");
        assert_eq!(
            empty, "",
            "l'objet vide rend vide (newline de gabarit adjacentes aux blocs standalone \
             consommees par le moteur)"
        );
    }

    /// `Scenario` : « page rendue en Markdown par défaut » — ligne `- clé=valeur ` par item avec
    /// espace finale aussi après la dernière paire, pied de page toujours présent ; `per_page`
    /// n'apparaît nulle part.
    #[test]
    fn markdown_list_renders_exact_lines_and_footer() {
        let mut engine = HandleBars::new();
        let data = page(json!([{"id": 1, "owner_id": 7, "title": "Tarte"}]), 1, 1, 1);
        let output = render(&mut engine, &OutputFormat::Markdown, RenderShape::List, &data).expect("renders");
        assert_eq!(
            output, "- id=1 owner_id=7 title=Tarte \n_page 1/1, 1 item(s) au total_\n",
            "paires separees par une espace, espace finale apres la derniere, pied de page \
             present ; newline de gabarit adjacentes aux blocs standalone consommees par le moteur \
             (pas de `\n` initial ni de ligne vide avant le pied)"
        );
        assert!(
            !output.contains("per_page") && !output.contains("100"),
            "per_page jamais rendu : {output}"
        );
    }

    /// `Scenario` : « liste vide — aucune ligne d'items et pied 1/0 » — seul le pied est émis,
    /// avec les totaux bruts de `sea_orm` sur table vide (`1/0` jamais ramené à `1/1`, jamais
    /// remplacé par « aucun résultat »).
    #[test]
    fn markdown_list_empty_renders_footer_only() {
        let mut engine = HandleBars::new();
        let data = page(json!([]), 1, 0, 0);
        let output = render(&mut engine, &OutputFormat::Markdown, RenderShape::List, &data).expect("renders");
        assert_eq!(
            output, "_page 1/0, 0 item(s) au total_\n",
            "aucun caractere d'item emis"
        );
        assert!(
            output.contains("1/0"),
            "le 1/0 brut de la page vide n'est jamais reecrit : {output}"
        );
    }

    /// `Scenario` : « Custom remplace le gabarit par défaut et ignore la forme » — même source,
    /// même texte exact sous `Record` comme sous `List` (`template` ne consulte `shape` que pour
    /// `Markdown`).
    #[test]
    fn custom_format_uses_supplied_template_not_the_default() {
        let mut engine = HandleBars::new();
        let data = json!({"id": 1, "title": "Tarte"});
        let custom = OutputFormat::Custom("Recette : {{title}}".to_string());
        let record = render(&mut engine, &custom, RenderShape::Record, &data).expect("renders");
        assert_eq!(record, "Recette : Tarte");
        let list = render(&mut engine, &custom, RenderShape::List, &data).expect("renders");
        assert_eq!(
            list, "Recette : Tarte",
            "`shape` ne change rien pour Custom : le gabarit de l'app est indépendant de la forme"
        );
    }

    /// `Scenario` : « clé absente en mode strict — pas de fallback, MRD-MCP-004 » — gabarit
    /// `Custom` écrit pour un enregistrement appelé sur une page sans `title` : `Err(Render)`,
    /// aucun texte partiel (`Titre : ` comme repli sur le JSON par défaut sont tous deux exclus —
    /// un `Err` ne rend pas de texte).
    #[test]
    fn custom_missing_key_renders_mrd_mcp_004() {
        let mut engine = HandleBars::new();
        let data = page(json!([]), 1, 0, 0);
        let custom = OutputFormat::Custom("Titre : {{title}}".to_string());
        let err = render(&mut engine, &custom, RenderShape::List, &data)
            .expect_err("cle absente en mode strict ne rend pas, elle echoue");
        assert!(matches!(err, McpError::Render(_)), "{err}");
        assert!(
            err.to_string()
                .starts_with("MRD-MCP-004: output rendering error: "),
            "le prefixe reel de l'arbitrage 2026-09-29 porte le code MRD-MCP-004 : {err}"
        );
    }

    /// `Scenario` : « gabarit syntaxiquement invalide — MRD-MCP-004 » — bloc `{{#each}}` jamais
    /// fermé : la compilation échoue au rendu (`render_template`), pas au montage ; la fonction
    /// rend sans panic sur ce chemin.
    #[test]
    fn malformed_custom_template_renders_mrd_mcp_004() {
        let mut engine = HandleBars::new();
        let data = page(json!([]), 1, 0, 0);
        let custom = OutputFormat::Custom("{{#each items}}oops".to_string());
        let err = render(&mut engine, &custom, RenderShape::List, &data)
            .expect_err("gabarit mal forme : echec a la compilation au rendu");
        assert!(matches!(err, McpError::Render(_)), "{err}");
        assert!(
            err.to_string().starts_with("MRD-MCP-004:"),
            "prefixe MRD-MCP-004 : {err}"
        );
    }

    /// `Scenario` : « donnée `Value::Null` — Json et Yaml rendent vide » — propriété de `render`
    /// (`delete` ne l'appelle plus depuis 2026-09-29, confirmation fixe posée par `handler.rs`) :
    /// court-circuit amont de `json_to_str` avant tout sérialiseur, chaîne vide des deux côtés.
    #[test]
    fn null_data_renders_empty_text_in_json_and_yaml() {
        let mut engine = HandleBars::new();
        let json_out = render(
            &mut engine,
            &OutputFormat::Json,
            RenderShape::Record,
            &serde_json::Value::Null,
        )
        .expect("Json court-circuite Null en vide");
        assert_eq!(json_out, "");
        let yaml_out = render(
            &mut engine,
            &OutputFormat::Yaml,
            RenderShape::Record,
            &serde_json::Value::Null,
        )
        .expect("Yaml court-circuite Null en vide");
        assert_eq!(yaml_out, "");
    }

    /// `Scenario` : « donnée `Value::Null` — Markdown échoue en MRD-MCP-004 » — `{{#each this}}`
    /// sur une valeur `null` déclenche le `RenderError::strict_error` amont (handlebars 6.4.4,
    /// mode strict de `setup_handlebars`) : asymétrie assumée avec `Json`/`Yaml` sur la même
    /// donnée, jamais rendu vide ici.
    #[test]
    fn null_data_markdown_record_raises_mrd_mcp_004() {
        let mut engine = HandleBars::new();
        let err = render(
            &mut engine,
            &OutputFormat::Markdown,
            RenderShape::Record,
            &serde_json::Value::Null,
        )
        .expect_err("Markdown sur Null echoue en mode strict, contrairement a Json/Yaml");
        assert!(matches!(err, McpError::Render(_)), "{err}");
        assert!(
            err.to_string().starts_with("MRD-MCP-004:"),
            "prefixe MRD-MCP-004 : {err}"
        );
    }

    /// `Scenario` : « Markdown par défaut n'échappe pas le HTML » — `no_escape` amont :
    /// `&`/`<` traversent caractère par caractère, sans entité ; non-scalaires via `JsonRender`
    /// amont : objet → `[object]`, tableau → `[1, 2]`, `null` → vide après le deux-points.
    #[test]
    fn markdown_record_keeps_raw_html_chars() {
        let mut engine = HandleBars::new();
        let data = json!({
            "note": null,
            "meta": {"a": 1},
            "id": 1,
            "tags": [1, 2],
            "title": "Tarte & Pomme <sucré>"
        });
        let output =
            render(&mut engine, &OutputFormat::Markdown, RenderShape::Record, &data).expect("renders");
        assert!(
            output.contains("- **title**: Tarte & Pomme <sucré>\n"),
            "rendu brut sans entite HTML : {output}"
        );
        assert!(
            !output.contains("&amp;") && !output.contains("&lt;"),
            "aucune entite HTML : {output}"
        );
        assert!(
            output.contains("- **meta**: [object]\n"),
            "objet imbrique en [object] : {output}"
        );
        assert!(
            output.contains("- **tags**: [1, 2]\n"),
            "tableau en [v1, v2] : {output}"
        );
        assert!(
            output.contains("- **note**: \n"),
            "champ null rend vide apres le deux-points : {output}"
        );
    }
}
