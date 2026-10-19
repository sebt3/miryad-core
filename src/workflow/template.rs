//! Rendu Handlebars central de la `config` d'un step (feature `workflow`) — contrat : `./template.sdd`.
//!
//! Fichier pur : aucun Restate, aucun `tracing`, aucun accès fichier ni environnement. Le moteur est
//! `vynil_core::hbs::HandleBars`, durci par [`engine`] (mode strict, sans échappement, helpers
//! d'accès à l'hôte retirés).

use std::collections::HashMap;

use serde_json::Value;
use vynil_core::hbs::HandleBars;

use super::step::{RunInfo, StepError};

/// Helpers de `vynil-core` retirés du moteur : accès fichiers, environnement et chemins hôte
/// (`./template.sdd` `Must`, issue sebt3/vynil-core#13 — liste noire transitoire).
const DENIED_HELPERS: &[&str] = &[
    "read_to_str",
    "env_var",
    "canonicalize",
    "parent",
    "file_name",
    "extension",
    "jsonnet",
];

/// Plafond d'un rendu de chaîne, en octets (`./template.sdd` `Must`).
const MAX_RENDERED_BYTES: usize = 1_048_576;

/// Moteur neuf, durci : mode strict, aucun échappement HTML, helpers d'accès à l'hôte retirés.
fn engine() -> HandleBars<'static> {
    let mut hbs = HandleBars::new();
    let registry = hbs.engine_mut();
    registry.set_strict_mode(true);
    registry.register_escape_fn(|s: &str| s.to_owned());
    for name in DENIED_HELPERS {
        registry.unregister_helper(name);
    }
    hbs
}

/// Contexte de rendu : `{"inputs": <inputs>, "run": {"run_key", "step_id", "depth"}}`.
fn context(run: &RunInfo, inputs: &HashMap<String, Value>) -> Value {
    let inputs: serde_json::Map<String, Value> = inputs.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    serde_json::json!({
        "inputs": inputs,
        "run": {
            "run_key": run.run_key,
            "step_id": run.step_id,
            "depth": run.depth,
        },
    })
}

/// Pointeur JSON (RFC 6901) d'un enfant d'objet : `~` puis `/` échappés.
fn child_pointer(pointer: &str, key: &str) -> String {
    format!("{pointer}/{}", key.replace('~', "~0").replace('/', "~1"))
}

/// Erreur déterministe d'un template : jamais retryable, le message ne porte aucune entrée.
fn invalid(pointer: &str, detail: impl std::fmt::Display) -> StepError {
    StepError {
        message: format!("template invalide à {pointer}: {detail}"),
        retryable: false,
    }
}

/// Rend une chaîne ; sans `{{`, rendu identique sans appeler le moteur.
fn render_string(
    hbs: &mut HandleBars,
    template: &str,
    ctx: &Value,
    pointer: &str,
) -> Result<String, StepError> {
    if !template.contains("{{") {
        return Ok(template.to_owned());
    }
    let rendered = hbs.render(template, ctx).map_err(|e| invalid(pointer, e))?;
    if rendered.len() > MAX_RENDERED_BYTES {
        return Err(StepError {
            message: format!("rendu trop long à {pointer}"),
            retryable: false,
        });
    }
    Ok(rendered)
}

/// Rend récursivement une valeur de `config` (`./template.sdd` `Must`, règles 1-3 et 5-6).
fn render_value(hbs: &mut HandleBars, value: &Value, ctx: &Value, pointer: &str) -> Result<Value, StepError> {
    match value {
        Value::String(template) => render_string(hbs, template, ctx, pointer).map(Value::String),
        Value::Array(items) => items
            .iter()
            .enumerate()
            .map(|(i, item)| render_value(hbs, item, ctx, &format!("{pointer}/{i}")))
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
        Value::Object(map) => {
            if map.len() == 1 {
                if let Some(raw) = map.get("$raw") {
                    return Ok(raw.clone());
                }
                if let Some(json) = map.get("$json") {
                    let Value::String(template) = json else {
                        return Err(invalid(pointer, "$json doit porter une chaîne"));
                    };
                    let rendered = render_string(hbs, template, ctx, pointer)?;
                    return serde_json::from_str(&rendered).map_err(|e| invalid(pointer, e));
                }
            }
            let mut out = serde_json::Map::new();
            for (key, item) in map {
                let child = child_pointer(pointer, key);
                out.insert(key.clone(), render_value(hbs, item, ctx, &child)?);
            }
            Ok(Value::Object(out))
        }
        Value::Number(_) | Value::Bool(_) | Value::Null => Ok(value.clone()),
    }
}

/// Rend récursivement la `config` d'un step ; `raw_fields` (premier niveau seulement) sont
/// recopiés sans rendu.
pub(crate) fn render_config(
    config: &Value,
    run: &RunInfo,
    inputs: &HashMap<String, Value>,
    raw_fields: &[&str],
) -> Result<Value, StepError> {
    let ctx = context(run, inputs);
    let mut hbs = engine();
    if let Value::Object(map) = config
        && !(map.len() == 1 && (map.contains_key("$raw") || map.contains_key("$json")))
    {
        let mut out = serde_json::Map::new();
        for (key, item) in map {
            let rendered = if raw_fields.contains(&key.as_str()) {
                item.clone()
            } else {
                render_value(&mut hbs, item, &ctx, &child_pointer("", key))?
            };
            out.insert(key.clone(), rendered);
        }
        return Ok(Value::Object(out));
    }
    render_value(&mut hbs, config, &ctx, "")
}

/// Rend une seule chaîne avec le même moteur et le même contexte.
pub(crate) fn render_str(
    template: &str,
    run: &RunInfo,
    inputs: &HashMap<String, Value>,
) -> Result<String, StepError> {
    let ctx = context(run, inputs);
    let mut hbs = engine();
    render_string(&mut hbs, template, &ctx, "")
}

#[cfg(test)]
mod tests {
    use super::{render_config, render_str};
    use crate::workflow::step::{RunInfo, StepError};
    use serde_json::{Value, json};
    use std::collections::HashMap;

    /// `RunInfo` neutre pour les scénarios qui n'exercent pas l'identité du run.
    fn run() -> RunInfo {
        RunInfo {
            run_key: "t".into(),
            step_id: "s".into(),
            depth: 0,
        }
    }

    /// Construit l'objet `inputs` à partir d'un `json!({...})` ; une valeur non-objet donne vide.
    fn inputs_of(value: Value) -> HashMap<String, Value> {
        match value {
            Value::Object(map) => map.into_iter().collect(),
            _ => HashMap::new(),
        }
    }

    /// Extrait l'erreur d'un résultat, ou échoue le test avec un message explicite.
    fn expect_err<T: std::fmt::Debug>(result: Result<T, StepError>) -> StepError {
        match result {
            Err(error) => error,
            Ok(value) => panic!("un Err était attendu, obtenu Ok({value:?})"),
        }
    }

    /// Scenario « une chaîne est rendue avec les sorties amont ».
    #[test]
    fn une_chaine_est_rendue_avec_les_sorties_amont() {
        let inputs = inputs_of(json!({"fetch": {"user": "ada"}}));
        let config = json!({"msg": "bonjour {{inputs.fetch.user}}"});
        let rendu = render_config(&config, &run(), &inputs, &[]).expect("rendu sans erreur");
        assert_eq!(rendu, json!({"msg": "bonjour ada"}));
    }

    /// Scenario « les valeurs non-chaînes et les clés sont inchangées ».
    #[test]
    fn les_valeurs_non_chaines_et_les_cles_sont_inchangees() {
        let inputs = inputs_of(json!({"a": "Z"}));
        let config = json!({"{{k}}": 1, "b": true, "n": null, "l": ["x{{inputs.a}}", 2]});
        let rendu = render_config(&config, &run(), &inputs, &[]).expect("rendu sans erreur");
        assert_eq!(rendu, json!({"{{k}}": 1, "b": true, "n": null, "l": ["xZ", 2]}));
    }

    /// Scenario « un champ brut de premier niveau n'est pas rendu ».
    #[test]
    fn un_champ_brut_de_premier_niveau_n_est_pas_rendu() {
        let inputs = inputs_of(json!({"x": "v"}));
        let config = json!({"script": "#{ a: \"{{inputs.x}}\" }", "to": "{{inputs.x}}"});
        let rendu = render_config(&config, &run(), &inputs, &["script"]).expect("rendu sans erreur");
        assert_eq!(rendu, json!({"script": "#{ a: \"{{inputs.x}}\" }", "to": "v"}));
    }

    /// Scenario « $raw protège un sous-arbre ».
    #[test]
    fn raw_protege_un_sous_arbre() {
        let inputs = inputs_of(json!({"q": "Q"}));
        let config = json!({"dag": {"$raw": [{"id": "a", "config": {"m": "{{inputs.q}}"}}]}});
        let rendu = render_config(&config, &run(), &inputs, &[]).expect("rendu sans erreur");
        assert_eq!(
            rendu,
            json!({"dag": [{"id": "a", "config": {"m": "{{inputs.q}}"}}]})
        );
    }

    /// Scenario « $json produit une valeur typée ».
    #[test]
    fn json_produit_une_valeur_typee() {
        let inputs = inputs_of(json!({"count": 3}));
        let config = json!({
            "n": {"$json": "{{inputs.count}}"},
            "o": {"$json": "{\"a\": {{inputs.count}} }"}
        });
        let rendu = render_config(&config, &run(), &inputs, &[]).expect("rendu sans erreur");
        assert_eq!(rendu, json!({"n": 3, "o": {"a": 3}}));
    }

    /// Scenario « $json non analysable est une erreur non retryable ».
    #[test]
    fn json_non_analysable_est_une_erreur_non_retryable() {
        let inputs = inputs_of(json!({"count": 3}));
        let config = json!({"n": {"$json": "pas du json {{inputs.count}}"}});
        let error = expect_err(render_config(&config, &run(), &inputs, &[]));
        assert!(!error.retryable, "un $json non analysable n'est jamais retryable");
        assert!(
            error.message.starts_with("template invalide à /n:"),
            "message : {}",
            error.message
        );
    }

    /// Scenario « une chaîne rendue reste une chaîne ».
    #[test]
    fn une_chaine_rendue_reste_une_chaine() {
        let inputs = inputs_of(json!({"count": 3}));
        let config = json!({"n": "{{inputs.count}}"});
        let rendu = render_config(&config, &run(), &inputs, &[]).expect("rendu sans erreur");
        assert_eq!(rendu, json!({"n": "3"}));
    }

    /// Scenario « une variable absente échoue en mode strict ».
    #[test]
    fn une_variable_absente_echoue_en_mode_strict() {
        let inputs = HashMap::new();
        let config = json!({"a": {"b": "{{inputs.absent.champ}}"}});
        let error = expect_err(render_config(&config, &run(), &inputs, &[]));
        assert!(!error.retryable);
        assert!(
            error.message.starts_with("template invalide à /a/b:"),
            "message : {}",
            error.message
        );
    }

    /// Scenario « aucun échappement HTML ».
    #[test]
    fn aucun_echappement_html() {
        let inputs = inputs_of(json!({"m": "<b>a & b</b>"}));
        let config = json!({"x": "{{inputs.m}}"});
        let rendu = render_config(&config, &run(), &inputs, &[]).expect("rendu sans erreur");
        assert_eq!(rendu, json!({"x": "<b>a & b</b>"}));
    }

    /// Scenario « les helpers d'accès à l'hôte sont refusés ».
    #[test]
    fn les_helpers_d_acces_a_l_hote_sont_refuses() {
        let inputs = HashMap::new();
        let lecture = json!({"x": "{{read_to_str \"/etc/hostname\"}}"});
        let error = expect_err(render_config(&lecture, &run(), &inputs, &[]));
        assert!(!error.retryable, "read_to_str doit échouer, non retryable");
        let environnement = json!({"y": "{{env_var \"HOME\"}}"});
        let error = expect_err(render_config(&environnement, &run(), &inputs, &[]));
        assert!(!error.retryable, "env_var doit échouer, non retryable");
    }

    /// Scenario « les helpers utiles restent disponibles ».
    #[test]
    fn les_helpers_utiles_restent_disponibles() {
        let inputs = inputs_of(json!({"a": {"k": 1}}));
        let config = json!({"x": "{{json_to_str inputs.a}}", "y": "{{uuid_new_v4}}"});
        let rendu = render_config(&config, &run(), &inputs, &[]).expect("rendu sans erreur");
        assert_eq!(rendu["x"], json!("{\"k\":1}"));
        let uuid = rendu["y"].as_str().unwrap_or_default();
        assert_eq!(uuid.len(), 36, "un UUID fait 36 caractères : {uuid}");
    }

    /// Scenario « un échappement natif produit un littéral ».
    #[test]
    fn un_echappement_natif_produit_un_litteral() {
        let inputs = inputs_of(json!({"a": "Z"}));
        let config = json!({"x": "\\{{inputs.a}}"});
        let rendu = render_config(&config, &run(), &inputs, &[]).expect("rendu sans erreur");
        assert_eq!(rendu, json!({"x": "{{inputs.a}}"}));
    }

    /// Scenario « l'identité du run est disponible ».
    #[test]
    fn l_identite_du_run_est_disponible() {
        let run = RunInfo {
            run_key: "r1".into(),
            step_id: "s1".into(),
            depth: 2,
        };
        let config = json!({"x": "{{run.run_key}}/{{run.step_id}}/{{run.depth}}"});
        let rendu = render_config(&config, &run, &HashMap::new(), &[]).expect("rendu sans erreur");
        assert_eq!(rendu, json!({"x": "r1/s1/2"}));
    }

    /// Scenario « un rendu trop long est refusé ».
    #[test]
    fn un_rendu_trop_long_est_refuse() {
        let big = "a".repeat(2 * 1024 * 1024);
        let inputs = inputs_of(json!({"big": big}));
        let config = json!({"x": "{{inputs.big}}"});
        let error = expect_err(render_config(&config, &run(), &inputs, &[]));
        assert!(!error.retryable);
        assert!(
            error.message.starts_with("rendu trop long à /x"),
            "message : {}",
            error.message
        );
    }

    /// Scenario « `render_str` utilise le même moteur et le même contexte ».
    #[test]
    fn render_str_utilise_le_meme_moteur_et_le_meme_contexte() {
        let inputs = inputs_of(json!({"a": "Z"}));
        let rendu = render_str("v={{inputs.a}}", &run(), &inputs).expect("rendu sans erreur");
        assert_eq!(rendu, "v=Z");
        let error = expect_err(render_str("v={{inputs.nope}}", &run(), &inputs));
        assert!(!error.retryable);
    }
}
