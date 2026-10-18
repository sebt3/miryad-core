//! Kind natif `"matrix_send"` (features `workflow` **et** `matrix`) : envoi d'un message dans une
//! salle Matrix via [`MatrixClient`].
//!
//! Le step dérive un `txn_id` déterministe de l'identité du run (`run_key`, `step_id`) : un
//! rejeu Restate envoie le même identifiant de transaction, et le homeserver dédoublonne.
//! Une erreur de configuration est non retryable et ne fait aucun appel réseau.
//!
//! L'application enregistre elle-même le kind, comme `RhaiStep` :
//! `registry.register(MatrixSendStep::new(client))`. Le step ne journalise jamais `body` ni `html`.

use std::collections::HashMap;
use std::fmt::Display;
use std::sync::Arc;

use serde::Deserialize;
use serde_json::Value;
use serde_json::json;
use sha2::Digest;
use sha2::Sha256;

use crate::matrix::MatrixClient;
use crate::matrix::MatrixError;
use crate::matrix::MessageKind;

use super::step::MiryadWorkflowStep;
use super::step::RunInfo;
use super::step::StepError;

/// Kind de step `"matrix_send"` : envoie un message dans une salle Matrix.
pub struct MatrixSendStep {
    sender: Arc<dyn Sender>,
}

impl MatrixSendStep {
    /// Construit le step autour d'un client Matrix déjà connecté.
    #[must_use]
    pub fn new(client: MatrixClient) -> Self {
        Self {
            sender: Arc::new(client),
        }
    }

    /// Construit le step autour d'un `Sender` quelconque (tests unitaires uniquement).
    #[cfg(test)]
    fn with_sender(sender: Arc<dyn Sender>) -> Self {
        Self { sender }
    }
}

/// Couture d'envoi : rend le step testable sans homeserver. Implémentée pour [`MatrixClient`].
#[async_trait::async_trait]
trait Sender: Send + Sync {
    async fn send(
        &self,
        room_id: &str,
        body: &str,
        html: Option<&str>,
        kind: MessageKind,
        txn_id: &str,
    ) -> Result<String, MatrixError>;
}

#[async_trait::async_trait]
impl Sender for MatrixClient {
    async fn send(
        &self,
        room_id: &str,
        body: &str,
        html: Option<&str>,
        kind: MessageKind,
        txn_id: &str,
    ) -> Result<String, MatrixError> {
        MatrixClient::send(self, room_id, body, html, kind, Some(txn_id)).await
    }
}

/// `config` du kind `"matrix_send"`, après rendu de template. Clés inconnues refusées.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MatrixSendConfig {
    room_id: String,
    body: String,
    html: Option<String>,
    #[serde(default)]
    kind: SendKind,
}

/// Type de message demandé par la `config`. Défaut : `text`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum SendKind {
    #[default]
    Text,
    Notice,
}

/// Identifiant de transaction déterministe d'un step : `"miryad-"` suivi des 32 premiers
/// caractères hexadécimaux de `sha256(run_key + "\0" + step_id)`.
pub(crate) fn txn_id(run: &RunInfo) -> String {
    let mut hasher = Sha256::new();
    hasher.update(run.run_key.as_bytes());
    hasher.update(b"\0");
    hasher.update(run.step_id.as_bytes());
    let digest = hex::encode(hasher.finalize());
    format!("miryad-{}", digest.chars().take(32).collect::<String>())
}

/// Erreur de configuration : non retryable, jamais d'appel réseau.
fn invalid(detail: impl Display) -> StepError {
    StepError {
        message: format!("config matrix_send invalide: {detail}"),
        retryable: false,
    }
}

/// Validation pure de la `config`, avant tout appel réseau.
fn validate(cfg: &MatrixSendConfig) -> Result<(), StepError> {
    if !cfg.room_id.starts_with('!') {
        return Err(invalid("room_id doit commencer par '!'"));
    }
    if cfg.body.trim().is_empty() {
        return Err(invalid("body vide"));
    }
    if cfg.html.as_deref().is_some_and(str::is_empty) {
        return Err(invalid("html vide"));
    }
    Ok(())
}

/// Type de message Matrix correspondant au `kind` de la `config`.
fn message_kind(kind: SendKind) -> MessageKind {
    match kind {
        SendKind::Text => MessageKind::Text,
        SendKind::Notice => MessageKind::Notice,
    }
}

#[async_trait::async_trait]
impl MiryadWorkflowStep for MatrixSendStep {
    fn kind(&self) -> &'static str {
        "matrix_send"
    }

    async fn run(
        &self,
        run: &RunInfo,
        config: Value,
        _inputs: HashMap<String, Value>,
    ) -> Result<Value, StepError> {
        let cfg: MatrixSendConfig = serde_json::from_value(config).map_err(invalid)?;
        validate(&cfg)?;
        let MatrixSendConfig {
            room_id,
            body,
            html,
            kind,
        } = cfg;
        let event_id = self
            .sender
            .send(&room_id, &body, html.as_deref(), message_kind(kind), &txn_id(run))
            .await
            .map_err(|err| StepError {
                retryable: err.retryable(),
                message: err.to_string(),
            })?;
        Ok(json!({"event_id": event_id, "room_id": room_id}))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::collections::VecDeque;
    use std::sync::Arc;
    use std::sync::Mutex;

    use serde_json::Value;
    use serde_json::json;

    use super::super::step::MiryadWorkflowStep;
    use super::super::step::RunInfo;
    use super::super::step::StepError;
    use super::MatrixSendStep;
    use super::Sender;
    use super::txn_id;
    use crate::matrix::MatrixError;
    use crate::matrix::MessageKind;

    /// Appel reçu par le faux `Sender`.
    #[derive(Debug, Clone, PartialEq)]
    struct Call {
        room_id: String,
        body: String,
        html: Option<String>,
        kind: MessageKind,
        txn_id: String,
    }

    /// Journal des appels reçus par le faux `Sender`.
    type Calls = Arc<Mutex<Vec<Call>>>;

    /// Faux `Sender` : enregistre chaque appel et rend `outcome` au premier appel.
    struct FakeSender {
        calls: Calls,
        outcomes: Mutex<VecDeque<Result<String, MatrixError>>>,
    }

    #[async_trait::async_trait]
    impl Sender for FakeSender {
        async fn send(
            &self,
            room_id: &str,
            body: &str,
            html: Option<&str>,
            kind: MessageKind,
            txn_id: &str,
        ) -> Result<String, MatrixError> {
            self.calls.lock().unwrap().push(Call {
                room_id: room_id.to_string(),
                body: body.to_string(),
                html: html.map(str::to_string),
                kind,
                txn_id: txn_id.to_string(),
            });
            self.outcomes
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Ok("$evt".to_string()))
        }
    }

    /// Step branché sur un faux `Sender` qui rendra `outcome`, et son journal d'appels.
    fn step_with(outcome: Result<String, MatrixError>) -> (MatrixSendStep, Calls) {
        let calls: Calls = Arc::new(Mutex::new(Vec::new()));
        let sender: Arc<dyn Sender> = Arc::new(FakeSender {
            calls: Arc::clone(&calls),
            outcomes: Mutex::new(VecDeque::from([outcome])),
        });
        (MatrixSendStep::with_sender(sender), calls)
    }

    fn run_info() -> RunInfo {
        RunInfo {
            run_key: "r".into(),
            step_id: "s".into(),
            depth: 0,
        }
    }

    /// Scenario « `txn_id` est stable pour une identité de run ».
    #[test]
    fn txn_id_est_stable_pour_une_identite_de_run() {
        let a = RunInfo {
            run_key: "r".into(),
            step_id: "s".into(),
            depth: 0,
        };
        let b = a.clone();
        let (ida, idb) = (txn_id(&a), txn_id(&b));
        assert_eq!(ida, idb);
        assert!(ida.starts_with("miryad-"), "obtenu : {ida}");
        assert_eq!(ida.len(), 39, "obtenu : {ida}");
    }

    /// Scenario « `txn_id` change avec le run ou le step ».
    #[test]
    fn txn_id_change_avec_le_run_ou_le_step() {
        let ids = [
            txn_id(&RunInfo {
                run_key: "r1".into(),
                step_id: "s".into(),
                depth: 0,
            }),
            txn_id(&RunInfo {
                run_key: "r2".into(),
                step_id: "s".into(),
                depth: 0,
            }),
            txn_id(&RunInfo {
                run_key: "r1".into(),
                step_id: "s2".into(),
                depth: 0,
            }),
        ];
        assert_ne!(ids[0], ids[1]);
        assert_ne!(ids[0], ids[2]);
        assert_ne!(ids[1], ids[2]);
    }

    /// Scenario « une config invalide ne fait aucun appel réseau ».
    #[tokio::test]
    async fn une_config_invalide_ne_fait_aucun_appel_reseau() {
        let cas = [
            json!({"room_id": "#alias:x", "body": "ok"}),
            json!({"room_id": "!r:x", "body": "  "}),
            json!({"room_id": "!r:x", "body": "ok", "html": ""}),
            json!({"room_id": "!r:x", "body": "ok", "rooms": "x"}),
        ];
        for config in cas {
            let (step, calls) = step_with(Ok("$evt".into()));
            let err = step
                .run(&run_info(), config.clone(), HashMap::new())
                .await
                .unwrap_err();
            assert!(!err.retryable, "config invalide : jamais retryable ({config})");
            assert!(
                err.message.starts_with("config matrix_send invalide:"),
                "message : {}",
                err.message
            );
            assert!(calls.lock().unwrap().is_empty(), "aucun envoi pour {config}");
        }
    }

    /// Scenario « kind notice et html sont transmis ».
    #[tokio::test]
    async fn kind_notice_et_html_sont_transmis() {
        let (step, calls) = step_with(Ok("$evt".into()));
        let config = json!({"room_id": "!r:x", "body": "ok", "html": "<b>ok</b>", "kind": "notice"});
        step.run(&run_info(), config, HashMap::new())
            .await
            .expect("config valide");
        let sent = calls.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].kind, MessageKind::Notice);
        assert_eq!(sent[0].html, Some("<b>ok</b>".to_string()));
        assert_eq!(sent[0].txn_id, txn_id(&run_info()));
    }

    /// Scenario « les erreurs du client suivent leur retryabilité ».
    #[tokio::test]
    async fn les_erreurs_du_client_suivent_leur_retryabilite() {
        let config = json!({"room_id": "!r:x", "body": "ok"});
        let (transitoire, _) = step_with(Err(MatrixError::Network {
            message: "x".into(),
            retryable: true,
        }));
        let err: StepError = transitoire
            .run(&run_info(), config.clone(), HashMap::new())
            .await
            .unwrap_err();
        assert!(err.retryable, "réseau transitoire : retryable");
        let (permanent, _) = step_with(Err(MatrixError::RoomUnavailable("x".into())));
        let err = permanent
            .run(&run_info(), config, HashMap::new())
            .await
            .unwrap_err();
        assert!(!err.retryable, "salle indisponible : non retryable");
    }

    /// Scenario « la sortie porte l'`event_id` ».
    #[tokio::test]
    async fn la_sortie_porte_l_event_id() {
        let (step, _) = step_with(Ok("$abc".into()));
        let config = json!({"room_id": "!r:x", "body": "ok"});
        let out: Value = step
            .run(&run_info(), config, HashMap::new())
            .await
            .expect("envoi réussi");
        assert_eq!(out, json!({"event_id": "$abc", "room_id": "!r:x"}));
    }
}
