//! Porteur du prédicat JSON structuré d'un champ `when` de step — contrat porté par
//! `./predicate.sdd` (issue #84, DSD `workflow` `FR-20`, `DEC-14`, `NFR-1`).
//!
//! Langage volontairement borné : une feuille `{step | var, path, op, value}`, `all` et `any` ;
//! aucune expression, aucun gabarit, aucun moteur, aucun effet de bord — donc une validation
//! statique triviale (les steps lus sont connus sans exécuter quoi que ce soit) et un
//! déterminisme évident au rejeu après crash.

use std::cmp::Ordering;
use std::collections::HashMap;

use serde::Deserialize;
use serde::Serialize;

/// Opérateur d'une feuille de prédicat.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Op {
    /// Égalité JSON, les nombres comparés par valeur numérique (`1` égale `1.0`).
    Eq,
    /// Inverse de [`Op::Eq`] sur une valeur présente (une valeur absente rend `false`).
    Ne,
    /// Deux nombres (comparés en flottant) ou deux chaînes (ordre lexicographique des octets).
    Gt,
    /// Comme [`Op::Gt`], égalité acceptée.
    Ge,
    /// Comme [`Op::Gt`], sens inverse.
    Lt,
    /// Comme [`Op::Lt`], égalité acceptée.
    Le,
    /// La valeur résolue est égale à un élément du tableau porté par `value`.
    In,
    /// La valeur résolue est présente et n'est pas JSON `null`.
    Exists,
}

/// Prédicat structuré d'un champ `when` de step : feuille, conjonction `all` ou alternative
/// `any`. Les trois formes refusent tout champ inconnu à la désérialisation ; une forme
/// structurellement valide n'est pas pour autant correcte — voir [`Predicate::validate`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum Predicate {
    /// Feuille : exactement une source (`step` ou `var`), un pointeur `path` facultatif
    /// (RFC 6901, défaut : la valeur entière), un opérateur et une `value` selon l'opérateur.
    Leaf {
        /// Sortie du step d'`id` donné, telle que rangée dans les résultats du run.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        step: Option<String>,
        /// Variable `vars` du run (constante, journalisée avec l'entrée).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        var: Option<String>,
        /// Pointeur JSON RFC 6901 appliqué à la source ; mal formé ou absent → valeur absente.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
        /// Opérateur appliqué à la valeur résolue.
        op: Op,
        /// Opérande de droite, selon l'opérateur ; absente pour `exists`, tableau pour `in`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        value: Option<serde_json::Value>,
    },
    /// Conjonction : vrai si tous les éléments sont vrais.
    All {
        /// Les éléments, dans l'ordre du document.
        all: Vec<Predicate>,
    },
    /// Alternative : vrai si au moins un élément est vrai.
    Any {
        /// Les éléments, dans l'ordre du document.
        any: Vec<Predicate>,
    },
}

impl Predicate {
    /// Évalue le prédicat : fonction **pure et totale** — jamais de panique, jamais d'erreur,
    /// aucune horloge, aucun hasard, aucune entrée-sortie (`./predicate.sdd` `Must`, `NFR-1`).
    /// Une feuille résout sa source (`results` pour `step`, `vars` pour `var`), applique son
    /// pointeur `path`, puis son opérateur. **Une valeur absente rend `false` pour tous les
    /// opérateurs**, `exists` et `ne` compris. Si une feuille porte par construction les deux
    /// sources (forme que `validate` refuse), `step` est résolue en premier : le résultat reste
    /// déterminé, jamais une panique.
    pub(crate) fn evaluate(
        &self,
        vars: &serde_json::Map<String, serde_json::Value>,
        results: &HashMap<String, serde_json::Value>,
    ) -> bool {
        match self {
            // Court-circuit : les deux conjoncteurs sont purs, aucun ordre observable exigé.
            Self::All { all } => all.iter().all(|element| element.evaluate(vars, results)),
            Self::Any { any } => any.iter().any(|element| element.evaluate(vars, results)),
            Self::Leaf {
                step,
                var,
                path,
                op,
                value,
            } => resoudre(step.as_deref(), var.as_deref(), path.as_deref(), results, vars)
                .is_some_and(|valeur| appliquer(*op, valeur, value.as_ref())),
        }
    }

    /// Validation structurelle seule — premier défaut rencontré, parcours en profondeur dans
    /// l'ordre du document. Le texte décrit le défaut sans citer aucune valeur de l'utilisateur.
    pub(crate) fn validate(&self) -> Result<(), String> {
        let mut noeuds = 0usize;
        self.valider(1, &mut noeuds)
    }

    /// Les `id` de step lus par les feuilles (sources `step`), dans l'ordre du document,
    /// doublons conservés ; les sources `var` n'y figurent pas.
    pub(crate) fn referenced_steps(&self) -> Vec<&str> {
        let mut references = Vec::new();
        self.collecter_references(&mut references);
        references
    }

    /// Cœur de [`Self::validate`] : un compteur de nœuds et une profondeur portés par la
    /// récursion, parcours en profondeur dans l'ordre du document.
    fn valider(&self, profondeur: usize, noeuds: &mut usize) -> Result<(), String> {
        *noeuds = noeuds.saturating_add(1);
        if *noeuds > 64 {
            return Err("le prédicat `when` dépasse 64 nœuds au total".to_string());
        }
        if profondeur > 8 {
            return Err("le prédicat `when` dépasse 8 niveaux d'imbrication".to_string());
        }
        match self {
            Self::All { all } | Self::Any { any: all } => {
                if all.is_empty() {
                    return Err("le prédicat `when` contient une liste `all`/`any` vide".to_string());
                }
                for element in all {
                    element.valider(profondeur.saturating_add(1), noeuds)?;
                }
                Ok(())
            }
            Self::Leaf {
                step,
                var,
                path,
                op,
                value,
            } => {
                // Ordre des défauts tel que `Must` : source, contenu des sources, `path`,
                // `value` selon l'opérateur.
                match (step.as_deref(), var.as_deref()) {
                    (Some(_), Some(_)) => {
                        return Err(
                            "une feuille `when` porte à la fois une source `step` et une source `var`"
                                .to_string(),
                        );
                    }
                    (None, None) => {
                        return Err("une feuille `when` ne porte aucune source (`step` ni `var`)".to_string());
                    }
                    (Some(""), None) => {
                        return Err("une feuille `when` porte une source `step` vide".to_string());
                    }
                    (None, Some("")) => {
                        return Err("une feuille `when` porte une source `var` vide".to_string());
                    }
                    _ => {}
                }
                if path
                    .as_deref()
                    .is_some_and(|chemin| !chemin.is_empty() && !chemin.starts_with('/'))
                {
                    return Err(
                        "une feuille `when` porte un `path` non vide qui ne commence pas par `/`".to_string(),
                    );
                }
                match op {
                    Op::Exists => {
                        if value.is_some() {
                            return Err(
                                "la feuille `when` à l'opérateur `exists` ne doit pas porter de `value`"
                                    .to_string(),
                            );
                        }
                    }
                    Op::In => match value {
                        None => {
                            return Err(
                                "la feuille `when` à l'opérateur `in` doit porter une `value`".to_string()
                            );
                        }
                        Some(serde_json::Value::Array(_)) => {}
                        Some(_) => {
                            return Err(
                                "la feuille `when` à l'opérateur `in` exige une `value` de forme tableau"
                                    .to_string(),
                            );
                        }
                    },
                    _ => {
                        if value.is_none() {
                            return Err(
                                "une feuille `when` dont l'opérateur n'est pas `exists` doit porter une `value`"
                                    .to_string(),
                            );
                        }
                    }
                }
                Ok(())
            }
        }
    }

    /// Cœur de [`Self::referenced_steps`] : parcours en profondeur dans l'ordre du document,
    /// doublons conservés.
    fn collecter_references<'a>(&'a self, acc: &mut Vec<&'a str>) {
        match self {
            Self::Leaf { step: Some(id), .. } => acc.push(id.as_str()),
            Self::Leaf { .. } => {}
            Self::All { all } => {
                for element in all {
                    element.collecter_references(acc);
                }
            }
            Self::Any { any } => {
                for element in any {
                    element.collecter_references(acc);
                }
            }
        }
    }
}

/// Résolution de la source d'une feuille : sortie du step (`results`) ou variable (`vars`),
/// puis pointeur `path` (`Value::pointer` — mal formé ou chemin absent → `None`, donc valeur
/// absente). Totalité : si les deux sources sont posées par construction (forme que `validate`
/// refuse), `step` est résolue en premier, `var` ignorée — jamais de panique.
fn resoudre<'a>(
    step: Option<&str>,
    var: Option<&str>,
    path: Option<&str>,
    results: &'a HashMap<String, serde_json::Value>,
    vars: &'a serde_json::Map<String, serde_json::Value>,
) -> Option<&'a serde_json::Value> {
    let racine = match (step, var) {
        (Some(id), _) => results.get(id),
        (None, Some(nom)) => vars.get(nom),
        (None, None) => None,
    }?;
    path.map_or(Some(racine), |chemin| racine.pointer(chemin))
}

/// Application de l'opérateur à une valeur **présente** (`evaluate` n'appelle cette fonction
/// que sur une résolution `Some` — l'absence rend `false` avant toute application, `exists` et
/// `ne` compris). Une opérande `value` absente (forme que `validate` refuse, sauf `exists`) ne
/// rend jamais vrai.
fn appliquer(op: Op, valeur: &serde_json::Value, operande: Option<&serde_json::Value>) -> bool {
    match op {
        Op::Exists => !valeur.is_null(),
        Op::Eq => operande.is_some_and(|attendu| eq_json(valeur, attendu)),
        Op::Ne => operande.is_some_and(|attendu| !eq_json(valeur, attendu)),
        Op::Gt | Op::Ge | Op::Lt | Op::Le => operande
            .and_then(|attendu| comparer(valeur, attendu))
            .is_some_and(|ordre| match op {
                Op::Gt => ordre == Ordering::Greater,
                Op::Ge => ordre != Ordering::Less,
                Op::Lt => ordre == Ordering::Less,
                Op::Le => ordre != Ordering::Greater,
                _ => false,
            }),
        Op::In => match operande {
            Some(serde_json::Value::Array(elements)) => {
                elements.iter().any(|element| eq_json(valeur, element))
            }
            _ => false,
        },
    }
}

/// Égalité JSON avec comparaison des nombres par valeur numérique (`1` égale `1.0`) — la seule
/// égalité utilisée par `eq`, `ne` et `in`, récursive sur les tableaux et objets.
fn eq_json(a: &serde_json::Value, b: &serde_json::Value) -> bool {
    use serde_json::Value;
    match (a, b) {
        (Value::Null, Value::Null) => true,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Number(x), Value::Number(y)) => match (x.as_f64(), y.as_f64()) {
            (Some(xf), Some(yf)) => xf.total_cmp(&yf) == Ordering::Equal,
            _ => x == y,
        },
        (Value::String(x), Value::String(y)) => x == y,
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y.iter()).all(|(x, y)| eq_json(x, y))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len() && x.iter().all(|(cle, x)| y.get(cle).is_some_and(|y| eq_json(x, y)))
        }
        _ => false,
    }
}

/// Comparaison bornée aux couples de même famille : deux nombres (en flottant — `total_cmp`,
/// sans NaN dans JSON), deux chaînes (ordre lexicographique des octets de `str::cmp`). Tout
/// autre couple est incomparable → `None` → `false` côté appelant.
fn comparer(a: &serde_json::Value, b: &serde_json::Value) -> Option<Ordering> {
    use serde_json::Value;
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => match (x.as_f64(), y.as_f64()) {
            (Some(xf), Some(yf)) => Some(xf.total_cmp(&yf)),
            _ => None,
        },
        (Value::String(x), Value::String(y)) => Some(x.cmp(y)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::Predicate;
    use serde_json::json;
    use std::collections::HashMap;

    // ── Fixtures — purement mémoire : `Predicate` est un type porté par le DAG, `evaluate` et
    // `validate` sont des fonctions pures sans `ctx` ni base (`./predicate.sdd` `Must`). Les
    // prédicats sont construits par leur forme JSON réelle — c'est elle que l'entité stocke.

    fn pred(forme: serde_json::Value) -> Predicate {
        serde_json::from_value(forme).expect("la fixture est un prédicat désérialisable")
    }

    fn vars_unique(objet: &serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
        objet
            .as_object()
            .expect("les vars de fixture sont un objet")
            .clone()
    }

    fn eval(
        forme: serde_json::Value,
        vars: &serde_json::Map<String, serde_json::Value>,
        results: &HashMap<String, serde_json::Value>,
    ) -> bool {
        pred(forme).evaluate(vars, results)
    }

    /// Chaîne distinctive jamais admise dans un texte d'erreur : les messages de `validate` ne
    /// doivent reprendre aucune valeur de l'utilisateur (`./predicate.sdd` `Must`).
    const VALEUR_UTILISATEUR: &str = "alpha-s3cr3t";

    /// Feuille de `profondeur` niveaux (racine `all` imbriquée × (profondeur − 1)).
    fn feuille_a_profondeur(profondeur: u32) -> serde_json::Value {
        let mut forme = json!({ "step": VALEUR_UTILISATEUR, "path": "/x", "op": "exists" });
        for _ in 1..profondeur {
            forme = json!({ "all": [forme] });
        }
        forme
    }

    /// Scenario « chaque opérateur sur une valeur présente » : table d'assertions sur les
    /// résultats `{"A": {"status": "ok", "n": 3, "tags": ["x", "y"]}}` et les `vars`
    /// `{"mode": "fast"}` — `eq`, `ne`, `gt`, `ge`, `lt`, `le`, `in`, `exists`, feuille `step`
    /// et feuille `var`, plus l'égalité numérique `1`/`1.0` du `Must`.
    #[test]
    fn evaluate_chaque_operateur_sur_une_valeur_presente() {
        let results: HashMap<String, serde_json::Value> = HashMap::from([(
            "A".to_string(),
            json!({ "status": "ok", "n": 3, "tags": ["x", "y"] }),
        )]);
        let vars = vars_unique(&json!({ "mode": "fast" }));

        let cas: Vec<(serde_json::Value, bool, &str)> = vec![
            (
                json!({"step": "A", "path": "/status", "op": "eq", "value": "ok"}),
                true,
                "eq vrai",
            ),
            (
                json!({"step": "A", "path": "/status", "op": "eq", "value": "ko"}),
                false,
                "eq faux",
            ),
            (
                json!({"step": "A", "path": "/n", "op": "eq", "value": 3.0}),
                true,
                "eq numérique 3 vs 3.0",
            ),
            (
                json!({"step": "A", "path": "/status", "op": "ne", "value": "ko"}),
                true,
                "ne vrai",
            ),
            (
                json!({"step": "A", "path": "/status", "op": "ne", "value": "ok"}),
                false,
                "ne faux",
            ),
            (
                json!({"step": "A", "path": "/n", "op": "gt", "value": 2}),
                true,
                "gt vrai",
            ),
            (
                json!({"step": "A", "path": "/n", "op": "gt", "value": 3}),
                false,
                "gt borne",
            ),
            (
                json!({"step": "A", "path": "/n", "op": "ge", "value": 3}),
                true,
                "ge égalité",
            ),
            (
                json!({"step": "A", "path": "/n", "op": "lt", "value": 3}),
                false,
                "lt borne",
            ),
            (
                json!({"step": "A", "path": "/n", "op": "le", "value": 3}),
                true,
                "le égalité",
            ),
            (
                json!({"step": "A", "path": "/tags/0", "op": "in", "value": ["x", "z"]}),
                true,
                "in vrai",
            ),
            (
                json!({"step": "A", "path": "/tags/0", "op": "in", "value": ["z"]}),
                false,
                "in faux",
            ),
            (
                json!({"step": "A", "path": "/status", "op": "exists"}),
                true,
                "exists vrai",
            ),
            (
                json!({"step": "A", "path": "/absent", "op": "exists"}),
                false,
                "exists sur absence",
            ),
            (
                json!({"var": "mode", "op": "eq", "value": "fast"}),
                true,
                "var eq vrai",
            ),
            (
                json!({"var": "mode", "op": "eq", "value": "lent"}),
                false,
                "var eq faux",
            ),
            (json!({"var": "mode", "op": "exists"}), true, "var exists vrai"),
        ];
        for (forme, attendu, motif) in cas {
            assert_eq!(
                eval(forme, &vars, &results),
                attendu,
                "volet « {motif} » du Scenario"
            );
        }
    }

    /// Scenario « une valeur absente rend toujours faux » : `exists` et `ne` compris — aucun
    /// opérateur ne rend vrai sur une absence ; sans panique.
    #[test]
    fn une_valeur_absente_rend_toujours_faux() {
        let results: HashMap<String, serde_json::Value> =
            HashMap::from([("A".to_string(), json!({ "status": "ok" }))]);
        let vars = serde_json::Map::new();

        for forme in [
            json!({"step": "A", "path": "/inconnu", "op": "exists"}),
            json!({"step": "A", "path": "/inconnu", "op": "ne", "value": "x"}),
            json!({"step": "A", "path": "/inconnu", "op": "eq", "value": null}),
            json!({"step": "step-absent-des-resultats", "op": "exists"}),
            json!({"var": "variable-absente-de-vars", "op": "exists"}),
        ] {
            assert!(
                !eval(forme.clone(), &vars, &results),
                "une valeur absente rend faux, sans panique : {forme}"
            );
        }
    }

    /// Scenario « des types incomparables rendent faux, sans erreur » : nombre contre chaîne
    /// dans les deux sens.
    #[test]
    fn des_types_incomparables_rendent_faux_sans_erreur() {
        let results: HashMap<String, serde_json::Value> =
            HashMap::from([("A".to_string(), json!({ "n": 3, "s": "abc" }))]);
        let vars = serde_json::Map::new();

        assert!(
            !eval(
                json!({"step": "A", "path": "/n", "op": "gt", "value": "2"}),
                &vars,
                &results
            ),
            "`/n gt \"2\"` : couple de types incomparable"
        );
        assert!(
            !eval(
                json!({"step": "A", "path": "/s", "op": "lt", "value": 5}),
                &vars,
                &results
            ),
            "`/s lt 5` : couple de types incomparable"
        );
    }

    /// Scenario « all et any composent, null n'existe pas » : `all[v eq 1, k exists]` faux,
    /// `any[v eq 2, v eq 1]` vrai, `any[k exists, v exists]` vrai.
    #[test]
    fn all_et_any_composent_null_n_existe_pas() {
        let results: HashMap<String, serde_json::Value> =
            HashMap::from([("A".to_string(), json!({ "k": null, "v": 1 }))]);
        let vars = serde_json::Map::new();

        let feuille_v1 = json!({"step": "A", "path": "/v", "op": "eq", "value": 1});
        let feuille_v2 = json!({"step": "A", "path": "/v", "op": "eq", "value": 2});
        let feuille_k = json!({"step": "A", "path": "/k", "op": "exists"});
        let feuille_v = json!({"step": "A", "path": "/v", "op": "exists"});

        assert!(
            !eval(
                json!({"all": [feuille_v1.clone(), feuille_k.clone()]}),
                &vars,
                &results
            ),
            "`all[v eq 1, k exists]` : `null` n'existe pas"
        );
        assert!(
            eval(json!({"any": [feuille_v2, feuille_v1]}), &vars, &results),
            "`any[v eq 2, v eq 1]` : un élément vrai suffit"
        );
        assert!(
            eval(json!({"any": [feuille_k, feuille_v]}), &vars, &results),
            "`any[k exists, v exists]` : `v` existe"
        );
    }

    /// Scenario « un chemin inexistant est une absence » : `/v/x/y` est structurellement
    /// valide (validate `Ok`) mais inexistant — `eq 1` rend `false`, sans panique.
    #[test]
    fn un_chemin_inexistant_est_une_absence() {
        let results: HashMap<String, serde_json::Value> =
            HashMap::from([("A".to_string(), json!({ "v": 1 }))]);
        let vars = serde_json::Map::new();

        let forme = json!({"step": "A", "path": "/v/x/y", "op": "eq", "value": 1});
        assert!(
            pred(forme.clone()).validate().is_ok(),
            "le chemin est structurellement valide : {:?}",
            pred(forme.clone()).validate()
        );
        assert!(
            !eval(forme, &vars, &results),
            "un chemin inexistant est une absence, sans panique"
        );
    }

    /// Scenario « validate rejette les formes invalides » : les neuf défauts nommés, un texte
    /// qui ne reprend aucune valeur utilisateur, et un prédicat valide qui rend `Ok(())`.
    #[test]
    fn validate_rejette_les_formes_invalides() {
        let invalides: Vec<(serde_json::Value, &str)> = vec![
            (
                json!({"step": VALEUR_UTILISATEUR, "var": VALEUR_UTILISATEUR, "op": "exists"}),
                "les deux sources",
            ),
            (json!({"op": "exists"}), "aucune source"),
            (
                json!({"step": VALEUR_UTILISATEUR, "path": VALEUR_UTILISATEUR, "op": "exists"}),
                "`path` sans `/` initial",
            ),
            (
                json!({"step": VALEUR_UTILISATEUR, "op": "eq"}),
                "`eq` sans `value`",
            ),
            (
                json!({"step": VALEUR_UTILISATEUR, "op": "exists", "value": 1}),
                "`exists` avec `value`",
            ),
            (
                json!({"step": VALEUR_UTILISATEUR, "op": "in", "value": VALEUR_UTILISATEUR}),
                "`in` avec une valeur non tableau",
            ),
            (json!({"all": []}), "`all` vide"),
            (json!({"any": []}), "`any` vide"),
            (feuille_a_profondeur(9), "profondeur 9"),
            (
                json!({"all": vec![json!({"step": VALEUR_UTILISATEUR, "op": "exists"}); 64]}),
                "65 nœuds",
            ),
        ];
        for (forme, motif) in invalides {
            let resultat = pred(forme.clone()).validate();
            let Err(message) = resultat else {
                panic!("« {motif} » devait être rejeté, rendu : {resultat:?}");
            };
            assert!(
                !message.contains(VALEUR_UTILISATEUR),
                "le texte d'erreur de « {motif} » ne doit citer aucune valeur de l'utilisateur : {message}"
            );
        }

        // Épreuve de non-trivialité : les limites justes passent — profondeur 8, 64 nœuds — et
        // un prédicat ordinaire rend `Ok(())`.
        assert_eq!(
            pred(feuille_a_profondeur(8)).validate(),
            Ok(()),
            "profondeur 8 est la limite haute valide"
        );
        assert_eq!(
            pred(json!({"all": vec![json!({"step": VALEUR_UTILISATEUR, "op": "exists"}); 63]})).validate(),
            Ok(()),
            "64 nœuds est la limite haute valide"
        );
        assert_eq!(
            pred(json!({"all": [
                {"step": "A", "path": "/status", "op": "eq", "value": "ok"},
                {"var": "mode", "op": "exists"}
            ]}))
            .validate(),
            Ok(())
        );
    }

    /// Scenario « `referenced_steps` ne rend que les sources `step` » : `all[{step A}, {var mode},
    /// any[{step B}, {step A}]]` rend `["A", "B", "A"]` — ordre du document, doublons conservés,
    /// `var` jamais citée.
    #[test]
    fn referenced_steps_ne_rend_que_les_sources_step() {
        let forme = json!({"all": [
            {"step": "A", "path": "/x", "op": "exists"},
            {"var": "mode", "op": "exists"},
            {"any": [
                {"step": "B", "op": "exists"},
                {"step": "A", "op": "exists"}
            ]}
        ]});
        assert_eq!(pred(forme).referenced_steps(), vec!["A", "B", "A"]);
    }

    /// Scenario « aller-retour JSON stable » : les trois formes resérialisées sont identiques
    /// (aucune clé inventée par `skip_serializing_if`, aucune clé perdue), et un champ inconnu
    /// est rejeté à la désérialisation.
    #[test]
    fn aller_retour_json_stable() {
        let formes = [
            json!({"step": "A", "path": "/status", "op": "eq", "value": "ok"}),
            json!({"var": "mode", "op": "exists"}),
            json!({"all": [
                {"step": "A", "op": "exists"},
                {"any": [
                    {"var": "mode", "path": "/x", "op": "ne", "value": 1},
                    {"step": "B", "path": "/y", "op": "in", "value": ["a", "b"]}
                ]}
            ]}),
        ];
        for forme in formes {
            let rendu = serde_json::to_value(pred(forme.clone())).expect("un prédicat se sérialise toujours");
            assert_eq!(
                rendu, forme,
                "l'aller-retour doit rendre le JSON identique : {forme}"
            );
        }

        let inconnu =
            serde_json::from_value::<Predicate>(json!({"step": "A", "op": "eq", "value": 1, "extra": 1}));
        assert!(
            inconnu.is_err(),
            "un champ inconnu doit être rejeté à la désérialisation : {inconnu:?}"
        );
    }
}
