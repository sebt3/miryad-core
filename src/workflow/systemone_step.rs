//! Kind natif `"systemone"` (features `workflow` **et** `systemone`) : une question typée à un
//! modèle de décision System One (`POST /v1/systemone`, client `crate::systemone`), avec des
//! seuils (`gates`) qui peuvent barrer l'exécution du workflow.
//!
//! Une réponse System One est une probabilité calibrée, pas un texte : chaque gate porte des
//! bornes `min` / `max` (inclusives) sur la valeur extraite de la réponse (`noul`, probabilité
//! d'une option de `choice`, `score`). Un gate non franchi arrête le step par un [`StepError`]
//! non retryable ; `on_fail: "continue"` rend plutôt `passed: false` et laisse l'aval décider.
//! Le step ne fait aucune nouvelle tentative : une erreur transitoire du client reste
//! `retryable: true` pour le retry Restate du dispatcher.
//!
//! L'application enregistre elle-même le kind, comme [`RhaiStep`](super::rhai_step::RhaiStep) :
//! `registry.register(SystemOneStep::new(client))`. Le step ne journalise jamais `state` ni
//! `answers` (données potentiellement personnelles).

use std::collections::HashMap;

use serde::Deserialize;
use serde_json::Map;
use serde_json::Value;
use serde_json::json;

use crate::systemone::SystemOneClient;
use crate::systemone::SystemOneRequest;

use super::step::MiryadWorkflowStep;
use super::step::RunInfo;
use super::step::StepError;

/// Kind de step `"systemone"` : pose des questions typées à un modèle de décision System One et
/// barre l'exécution selon les seuils de ses `gates`.
pub struct SystemOneStep {
    client: SystemOneClient,
}

impl SystemOneStep {
    /// Construit le step autour d'un client déjà configuré (`crate::systemone`).
    #[must_use]
    pub fn new(client: SystemOneClient) -> Self {
        Self { client }
    }
}

/// `config` du kind `"systemone"`, après rendu de template. Clés inconnues refusées.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SystemOneStepConfig {
    model: String,
    state: Value,
    questions: Map<String, Value>,
    #[serde(default)]
    gates: Vec<Gate>,
    #[serde(default)]
    on_fail: OnFail,
}

/// Comportement d'un gate non franchi.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum OnFail {
    /// Défaut : un gate non franchi arrête le step par un `StepError` non retryable.
    #[default]
    Fail,
    /// Un gate non franchi rend `passed: false`, sans erreur.
    Continue,
}

/// Seuil sur la réponse à une question : bornes inclusives sur la valeur extraite.
#[derive(Debug, Clone, Deserialize)]
struct Gate {
    question: String,
    option: Option<String>,
    min: Option<f64>,
    max: Option<f64>,
}

/// Erreur de configuration : non retryable, jamais de requête émise.
fn invalid(detail: impl std::fmt::Display) -> StepError {
    StepError {
        message: format!("config systemone invalide: {detail}"),
        retryable: false,
    }
}

/// Réponse absente ou illisible pour un gate : non retryable.
fn unexpected(question: &str) -> StepError {
    StepError {
        message: format!("réponse systemone inattendue pour {question}"),
        retryable: false,
    }
}

/// Type déclaré d'une question (`"type"` de sa valeur dans `questions`), vide si absent.
fn question_kind(question: &Value) -> &str {
    question.get("type").and_then(Value::as_str).unwrap_or("")
}

/// Validation pure de la `config`, avant tout appel réseau (`Must` étape 2).
fn validate(cfg: &SystemOneStepConfig) -> Result<(), StepError> {
    if cfg.questions.is_empty() {
        return Err(invalid("questions vide"));
    }
    for gate in &cfg.gates {
        let Some(question) = cfg.questions.get(&gate.question) else {
            return Err(invalid(format!(
                "gate sur une question absente : {}",
                gate.question
            )));
        };
        if gate.min.is_none() && gate.max.is_none() {
            return Err(invalid(format!("gate sans min ni max : {}", gate.question)));
        }
        for bound in [gate.min, gate.max].into_iter().flatten() {
            if !bound.is_finite() {
                return Err(invalid(format!("borne non finie : {}", gate.question)));
            }
        }
        if let (Some(min), Some(max)) = (gate.min, gate.max)
            && min > max
        {
            return Err(invalid(format!("min > max : {}", gate.question)));
        }
        match (question_kind(question), gate.option.is_some()) {
            ("noul" | "score", false) | ("choice", true) => {}
            ("choice", false) => {
                return Err(invalid(format!(
                    "option requise pour un gate choice : {}",
                    gate.question
                )));
            }
            ("noul" | "score", true) => {
                return Err(invalid(format!(
                    "option interdite pour un gate noul ou score : {}",
                    gate.question
                )));
            }
            (kind, _) => {
                return Err(invalid(format!(
                    "type de question non supporté par un gate ({kind}) : {}",
                    gate.question
                )));
            }
        }
    }
    Ok(())
}

/// Vrai si `value` est dans les bornes du gate, bornes inclusives (`Must` étape 4).
fn passes(value: f64, gate: &Gate) -> bool {
    gate.min.is_none_or(|min| value >= min) && gate.max.is_none_or(|max| value <= max)
}

/// Valeur d'un gate extraite de la réponse selon le type de question (`Must` étape 4).
fn extract(answers: &Map<String, Value>, gate: &Gate, kind: &str) -> Result<f64, StepError> {
    let inattendue = || unexpected(&gate.question);
    let answer = answers.get(&gate.question).ok_or_else(inattendue)?;
    let field = match kind {
        "noul" => answer.get("noul"),
        "score" => answer.get("score"),
        "choice" => {
            let option = gate.option.as_deref().ok_or_else(inattendue)?;
            answer.get("probabilities").and_then(|p| p.get(option))
        }
        _ => None,
    };
    field
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite())
        .ok_or_else(inattendue)
}

/// Libellé d'un gate échoué : `question=valeur (min, max)`, borne absente omise.
fn describe_failure(gate: &Gate, value: f64) -> String {
    let bounds: Vec<String> = [
        gate.min.map(|min| format!("min {min}")),
        gate.max.map(|max| format!("max {max}")),
    ]
    .into_iter()
    .flatten()
    .collect();
    format!("{}={value} ({})", gate.question, bounds.join(", "))
}

#[async_trait::async_trait]
impl MiryadWorkflowStep for SystemOneStep {
    fn kind(&self) -> &'static str {
        "systemone"
    }

    async fn run(
        &self,
        _run: &RunInfo,
        config: Value,
        _inputs: HashMap<String, Value>,
    ) -> Result<Value, StepError> {
        let cfg: SystemOneStepConfig = serde_json::from_value(config).map_err(invalid)?;
        validate(&cfg)?;
        let SystemOneStepConfig {
            model,
            state,
            questions,
            gates,
            on_fail,
        } = cfg;
        let request = SystemOneRequest {
            model,
            state,
            questions: questions.clone(),
        };
        let response = self.client.ask(&request).await.map_err(|err| StepError {
            retryable: err.retryable(),
            message: err.to_string(),
        })?;

        let mut evaluated = Vec::with_capacity(gates.len());
        let mut failures = Vec::new();
        for gate in &gates {
            let kind = questions.get(&gate.question).map_or("", question_kind);
            let value = extract(&response.answers, gate, kind)?;
            let passed = passes(value, gate);
            if !passed {
                failures.push(describe_failure(gate, value));
            }
            evaluated.push(json!({
                "question": gate.question,
                "option": gate.option,
                "value": value,
                "min": gate.min,
                "max": gate.max,
                "passed": passed,
            }));
        }

        let passed = failures.is_empty();
        if !passed && on_fail == OnFail::Fail {
            return Err(StepError {
                message: format!("gate systemone non franchi: {}", failures.join("; ")),
                retryable: false,
            });
        }
        Ok(json!({
            "passed": passed,
            "model": response.model,
            "answers": response.answers,
            "gates": evaluated,
            "usage": response.usage,
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::Mutex;

    use axum::Router;
    use axum::body::Bytes;
    use axum::http::StatusCode;
    use axum::http::header;
    use axum::response::IntoResponse;
    use axum::routing::post;
    use serde_json::Map;
    use serde_json::Value;
    use serde_json::json;

    use super::super::step::MiryadWorkflowStep;
    use super::super::step::RunInfo;
    use super::super::step::StepError;
    use super::super::template::render_config;
    use super::Gate;
    use super::SystemOneStep;
    use super::SystemOneStepConfig;
    use super::extract;
    use super::passes;
    use super::validate;
    use crate::systemone::SystemOneClient;
    use crate::systemone::SystemOneConfig;

    /// Corps JSON reçus par un serveur local, dans l'ordre d'arrivée.
    type Received = Arc<Mutex<Vec<Value>>>;

    /// Réponses System One de référence.
    const NOUL_09: &str = r#"{"model":"m","answers":{"q":{"type":"noul","noul":0.9}}}"#;
    const NOUL_031: &str = r#"{"model":"m","answers":{"q":{"type":"noul","noul":0.31}}}"#;
    const CHOICE_088: &str = r#"{"model":"m","answers":{"c":{"type":"choice","choice":"billing","probabilities":{"billing":0.88,"sales":0.12}}}}"#;
    const SCORE_105: &str = r#"{"model":"m","answers":{"s":{"type":"score","score":1.05}}}"#;

    /// Lance un serveur axum sur `127.0.0.1:0` : chaque `POST /v1/systemone` est journalisé
    /// puis reçoit `status` et `body`. Renvoie l'URL de base et le journal des requêtes.
    async fn serve(status: StatusCode, body: &'static str) -> (String, Received) {
        let received: Received = Arc::new(Mutex::new(Vec::new()));
        let journal = Arc::clone(&received);
        let router = Router::new().route(
            "/v1/systemone",
            post(move |payload: Bytes| {
                let journal = Arc::clone(&journal);
                async move {
                    let value = serde_json::from_slice::<Value>(&payload).unwrap_or(Value::Null);
                    journal.lock().unwrap().push(value);
                    (status, [(header::CONTENT_TYPE, "application/json")], body).into_response()
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        (format!("http://{addr}"), received)
    }

    fn run_info() -> RunInfo {
        RunInfo {
            run_key: "run".into(),
            step_id: "step".into(),
            depth: 0,
        }
    }

    fn step_for(base: &str) -> SystemOneStep {
        let client = SystemOneClient::new(SystemOneConfig::new(base)).unwrap();
        SystemOneStep::new(client)
    }

    async fn run_with(base: &str, config: Value) -> Result<Value, StepError> {
        step_for(base).run(&run_info(), config, HashMap::new()).await
    }

    /// `config` valide avec une question `q` de type `noul`, et les `gates` donnés.
    fn config_with_gates(gates: &Value) -> Value {
        json!({
            "model": "m",
            "state": "hello",
            "questions": {"q": {"type": "noul", "instructions": "Oui ?"}},
            "gates": gates,
        })
    }

    fn parse(config: Value) -> SystemOneStepConfig {
        serde_json::from_value(config).unwrap()
    }

    fn answers(value: Value) -> Map<String, Value> {
        match value {
            Value::Object(map) => map,
            _ => Map::new(),
        }
    }

    fn gate(question: &str, option: Option<&str>, min: Option<f64>, max: Option<f64>) -> Gate {
        Gate {
            question: question.to_string(),
            option: option.map(str::to_string),
            min,
            max,
        }
    }

    /// Scenario « un gate noul franchi rend passed true ».
    #[tokio::test]
    async fn un_gate_noul_franchi_rend_passed_true() {
        let (base, _) = serve(StatusCode::OK, NOUL_09).await;
        let out = run_with(&base, config_with_gates(&json!([{"question": "q", "min": 0.8}])))
            .await
            .expect("un gate franchi doit rendre Ok");
        assert_eq!(out["passed"], json!(true));
        assert_eq!(
            out["gates"][0],
            json!({"question": "q", "option": null, "value": 0.9, "min": 0.8, "max": null, "passed": true})
        );
        assert_eq!(out["answers"], json!({"q": {"type": "noul", "noul": 0.9}}));
    }

    /// Scenario « un gate non franchi échoue par défaut ».
    #[tokio::test]
    async fn un_gate_non_franchi_echoue_par_defaut() {
        let (base, _) = serve(StatusCode::OK, NOUL_031).await;
        let err = run_with(&base, config_with_gates(&json!([{"question": "q", "min": 0.8}])))
            .await
            .unwrap_err();
        assert!(!err.retryable, "un gate non franchi n'est jamais retryable");
        assert!(err.message.contains("q=0.31"), "message : {}", err.message);
        assert!(err.message.contains("min 0.8"), "message : {}", err.message);
    }

    /// Scenario « `on_fail` continue rend `passed` false ».
    #[tokio::test]
    async fn on_fail_continue_rend_passed_false() {
        let (base, _) = serve(StatusCode::OK, NOUL_031).await;
        let mut config = config_with_gates(&json!([{"question": "q", "min": 0.8}]));
        config["on_fail"] = json!("continue");
        let out = run_with(&base, config).await.expect("on_fail continue rend Ok");
        assert_eq!(out["passed"], json!(false));
        assert_eq!(out["gates"][0]["passed"], json!(false));
    }

    /// Scenario « un gate choice lit la probabilité de l'option ».
    #[tokio::test]
    async fn un_gate_choice_lit_la_probabilite_de_l_option() {
        let (base, _) = serve(StatusCode::OK, CHOICE_088).await;
        let config = json!({
            "model": "m",
            "state": "hello",
            "questions": {"c": {"type": "choice", "instructions": "Service ?"}},
            "gates": [{"question": "c", "option": "billing", "min": 0.8}],
        });
        let out = run_with(&base, config).await.expect("le gate choice doit passer");
        assert_eq!(out["gates"][0]["value"], json!(0.88));
        assert_eq!(out["gates"][0]["passed"], json!(true));
    }

    /// Scenario « un gate score lit le score ».
    #[tokio::test]
    async fn un_gate_score_lit_le_score() {
        let (base, _) = serve(StatusCode::OK, SCORE_105).await;
        let config = json!({
            "model": "m",
            "state": "hello",
            "questions": {"s": {"type": "score"}},
            "gates": [{"question": "s", "max": 1.0}],
        });
        let err = run_with(&base, config).await.unwrap_err();
        assert!(!err.retryable);
        assert!(err.message.contains("s=1.05"), "message : {}", err.message);
    }

    /// Scenario « une config de gate invalide ne fait aucun appel réseau ».
    #[tokio::test]
    async fn une_config_de_gate_invalide_ne_fait_aucun_appel_reseau() {
        let (base, received) = serve(StatusCode::OK, NOUL_09).await;
        let cas = [
            json!([{"question": "absente", "min": 0.5}]),
            json!([{"question": "q"}]),
            json!([{"question": "q", "option": "x", "min": 0.5}]),
            json!([{"question": "q", "min": 0.9, "max": 0.1}]),
        ];
        for gates in cas {
            let err = run_with(&base, config_with_gates(&gates)).await.unwrap_err();
            assert!(!err.retryable, "config invalide : jamais retryable ({gates})");
            assert!(
                err.message.starts_with("config systemone invalide:"),
                "message : {}",
                err.message
            );
        }
        let choice = json!({
            "model": "m",
            "state": "hello",
            "questions": {"c": {"type": "choice"}},
            "gates": [{"question": "c", "min": 0.5}],
        });
        let err = run_with(&base, choice).await.unwrap_err();
        assert!(err.message.starts_with("config systemone invalide:"));
        assert!(
            received.lock().unwrap().is_empty(),
            "aucune requête ne doit partir pour une config invalide"
        );
    }

    /// Scenario « un champ inconnu dans la config est refusé ».
    #[tokio::test]
    async fn un_champ_inconnu_dans_la_config_est_refuse() {
        let (base, received) = serve(StatusCode::OK, NOUL_09).await;
        let config = json!({
            "model": "m",
            "state": "hello",
            "questions": {"q": {"type": "noul"}},
            "gate": [{"question": "q", "min": 0.8}],
        });
        let err = run_with(&base, config).await.unwrap_err();
        assert!(!err.retryable);
        assert!(
            err.message.starts_with("config systemone invalide:"),
            "message : {}",
            err.message
        );
        assert!(received.lock().unwrap().is_empty());
    }

    /// Scenario « les erreurs du client suivent leur retryabilité ».
    #[tokio::test]
    async fn les_erreurs_du_client_suivent_leur_retryabilite() {
        let config = json!({"model": "m", "state": "hello", "questions": {"q": {"type": "noul"}}});
        let (base_529, _) = serve(StatusCode::from_u16(529).unwrap(), "{}").await;
        let transitoire = run_with(&base_529, config.clone()).await.unwrap_err();
        assert!(transitoire.retryable, "529 doit rester retryable : {transitoire}");
        let (base_422, _) = serve(StatusCode::UNPROCESSABLE_ENTITY, "{}").await;
        let permanent = run_with(&base_422, config).await.unwrap_err();
        assert!(
            !permanent.retryable,
            "422 ne doit pas être retryable : {permanent}"
        );
    }

    /// Scenario « une réponse sans le champ attendu échoue proprement ».
    #[tokio::test]
    async fn une_reponse_sans_le_champ_attendu_echoue_proprement() {
        let (base, _) = serve(StatusCode::OK, r#"{"model":"m","answers":{"q":{"type":"noul"}}}"#).await;
        let err = run_with(&base, config_with_gates(&json!([{"question": "q", "min": 0.8}])))
            .await
            .unwrap_err();
        assert!(!err.retryable);
        assert!(
            err.message.contains("réponse systemone inattendue pour q"),
            "message : {}",
            err.message
        );
    }

    /// Scenario « sans gates, le step est un simple appel ».
    #[tokio::test]
    async fn sans_gates_le_step_est_un_simple_appel() {
        let (base, _) = serve(StatusCode::OK, NOUL_09).await;
        let config = json!({"model": "m", "state": "hello", "questions": {"q": {"type": "noul"}}});
        let out = run_with(&base, config).await.expect("sans gates, Ok");
        assert_eq!(out["passed"], json!(true));
        assert_eq!(out["gates"], json!([]));
        assert_eq!(out["answers"], json!({"q": {"type": "noul", "noul": 0.9}}));
    }

    /// Scenario « la config est rendue avant run » — version au niveau du step : le rendu est
    /// `render_config` (./template.sdd) suivi de `run`. La preuve par le dispatcher reste à
    /// écrire dans `dispatcher.rs` (hors périmètre de ce fichier).
    #[tokio::test]
    async fn la_config_est_rendue_avant_run() {
        let (base, received) = serve(StatusCode::OK, NOUL_09).await;
        let raw = json!({
            "model": "m",
            "state": "{{inputs.fetch.text}}",
            "questions": {"q": {"type": "noul"}},
            "gates": [{"question": "q", "min": 0.5}],
        });
        let inputs = HashMap::from([("fetch".to_string(), json!({"text": "panne"}))]);
        let rendered = render_config(&raw, &run_info(), &inputs, &[]).unwrap();
        run_with(&base, rendered).await.expect("run doit rendre Ok");
        let sent = received.lock().unwrap();
        assert_eq!(sent[0]["state"], json!("panne"));
    }

    /// `Handles` : chaque cas invalide est refusé par `validate` seul, sans réseau.
    #[test]
    fn validate_refuse_chaque_config_invalide_sans_reseau() {
        let cas = [
            (json!({"model": "m", "state": "s", "questions": {}}), ""),
            (
                json!({"model": "m", "state": "s", "questions": {"q": {"type": "noul"}}, "gates": [{"question": "absente", "min": 0.5}]}),
                "absente",
            ),
            (
                json!({"model": "m", "state": "s", "questions": {"q": {"type": "noul"}}, "gates": [{"question": "q"}]}),
                "min ni max",
            ),
            (
                json!({"model": "m", "state": "s", "questions": {"q": {"type": "noul"}}, "gates": [{"question": "q", "option": "x", "min": 0.5}]}),
                "option interdite",
            ),
            (
                json!({"model": "m", "state": "s", "questions": {"q": {"type": "noul"}}, "gates": [{"question": "q", "min": 0.9, "max": 0.1}]}),
                "min > max",
            ),
            (
                json!({"model": "m", "state": "s", "questions": {"c": {"type": "choice"}}, "gates": [{"question": "c", "min": 0.5}]}),
                "option requise",
            ),
            (
                json!({"model": "m", "state": "s", "questions": {"t": {"type": "text"}}, "gates": [{"question": "t", "min": 0.5}]}),
                "type de question non supporté par un gate",
            ),
            (
                json!({"model": "m", "state": "s", "questions": {"s": {"type": "score"}}, "gates": [{"question": "s", "option": "x", "max": 1.0}]}),
                "option interdite",
            ),
        ];
        for (config, detail) in cas {
            let err = validate(&parse(config.clone())).unwrap_err();
            assert!(!err.retryable, "{config} : jamais retryable");
            assert!(
                err.message.starts_with("config systemone invalide:"),
                "{config} : {}",
                err.message
            );
            assert!(
                err.message.contains(detail),
                "{config} : détail `{detail}` absent de {}",
                err.message
            );
        }
        let valide = parse(
            json!({"model": "m", "state": "s", "questions": {"q": {"type": "noul"}, "c": {"type": "choice"}}, "gates": [{"question": "q", "min": 0.5}, {"question": "c", "option": "billing", "max": 0.9}]}),
        );
        assert!(validate(&valide).is_ok());
    }

    /// `Handles` : une option absente de `probabilities` échoue à l'extraction, non retryable.
    #[test]
    fn extract_option_absente_de_probabilities_echoue() {
        let reponses = answers(json!({"c": {"type": "choice", "probabilities": {"billing": 0.88}}}));
        let err = extract(&reponses, &gate("c", Some("sales"), Some(0.5), None), "choice").unwrap_err();
        assert!(!err.retryable);
        assert!(
            err.message.contains("réponse systemone inattendue pour c"),
            "message : {}",
            err.message
        );
    }

    /// `Handles` : plusieurs gates sur la même question sont évalués indépendamment.
    #[tokio::test]
    async fn plusieurs_gates_sur_la_meme_question_sont_independants() {
        let (base, _) = serve(StatusCode::OK, NOUL_09).await;
        let mut config =
            config_with_gates(&json!([{"question": "q", "min": 0.5}, {"question": "q", "max": 0.7}]));
        config["on_fail"] = json!("continue");
        let out = run_with(&base, config).await.expect("on_fail continue rend Ok");
        assert_eq!(out["gates"][0]["passed"], json!(true));
        assert_eq!(out["gates"][1]["passed"], json!(false));
        assert_eq!(out["passed"], json!(false));
        let reponses = answers(json!({"q": {"type": "noul", "noul": 0.6}}));
        let value = extract(&reponses, &gate("q", None, None, None), "noul").unwrap();
        assert!(passes(value, &gate("q", None, Some(0.5), None)));
        assert!(passes(value, &gate("q", None, None, Some(0.7))));
    }
}
