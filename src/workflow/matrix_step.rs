//! Kinds natifs `"matrix_send"` et `"matrix_edit"` (features `workflow` **et** `matrix`) : envoi
//! d'un message dans une salle Matrix et édition d'un message déjà publié via [`MatrixClient`].
//!
//! Le step dérive un `txn_id` déterministe de l'identité du run (`run_key`, `step_id`) : un
//! rejeu Restate envoie le même identifiant de transaction, et le homeserver dédoublonne.
//! Une erreur de configuration est non retryable et ne fait aucun appel réseau.
//! La `config` accepte une relation optionnelle (`thread_root`/`thread_latest` ou `reply_to`,
//! champs à plat validés avant tout envoi — issue #87).
//!
//! L'application enregistre elle-même chaque kind, comme `RhaiStep` :
//! `registry.register(MatrixSendStep::new(client))`, `registry.register(MatrixEditStep::new(client))`.
//! Le step ne journalise jamais `body` ni `html`.

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
use crate::matrix::client::MessageRelation;

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

/// Couture d'envoi et d'édition : rend les steps testables sans homeserver. Implémentée pour
/// [`MatrixClient`].
#[async_trait::async_trait]
trait Sender: Send + Sync {
    async fn send(
        &self,
        room_id: &str,
        body: &str,
        html: Option<&str>,
        kind: MessageKind,
        relation: Option<&MessageRelation>,
        txn_id: &str,
    ) -> Result<String, MatrixError>;

    async fn edit(
        &self,
        room_id: &str,
        event_id: &str,
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
        relation: Option<&MessageRelation>,
        txn_id: &str,
    ) -> Result<String, MatrixError> {
        MatrixClient::send(self, room_id, body, html, kind, relation, Some(txn_id)).await
    }

    async fn edit(
        &self,
        room_id: &str,
        event_id: &str,
        body: &str,
        html: Option<&str>,
        kind: MessageKind,
        txn_id: &str,
    ) -> Result<String, MatrixError> {
        MatrixClient::edit(self, room_id, event_id, body, html, kind, Some(txn_id)).await
    }
}

/// `config` du kind `"matrix_send"`, après rendu de template. Clés inconnues refusées.
/// Les trois champs de relation sont des chaînes à plat, rendues par le gabarit comme les autres
/// (amendement 2026-11-16, issue #87).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MatrixSendConfig {
    room_id: String,
    body: String,
    html: Option<String>,
    #[serde(default)]
    kind: SendKind,
    #[serde(default)]
    thread_root: Option<String>,
    #[serde(default)]
    thread_latest: Option<String>,
    #[serde(default)]
    reply_to: Option<String>,
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
    if cfg.thread_root.is_some() && cfg.reply_to.is_some() {
        return Err(invalid("thread_root et reply_to sont exclusifs"));
    }
    if cfg.thread_latest.is_some() && cfg.thread_root.is_none() {
        return Err(invalid("thread_latest exige thread_root"));
    }
    for (name, value) in [
        ("thread_root", cfg.thread_root.as_deref()),
        ("thread_latest", cfg.thread_latest.as_deref()),
        ("reply_to", cfg.reply_to.as_deref()),
    ] {
        if value.is_some_and(|v| v.is_empty() || !v.starts_with('$')) {
            return Err(invalid(format!(
                "{name} doit commencer par '$' et ne pas être vide"
            )));
        }
    }
    Ok(())
}

/// Dérivation de la relation à partir des champs validés de la `config` : `thread_root` →
/// `Thread` (avec `thread_latest` en repli), `reply_to` → `Reply`, sinon `None`.
fn relation(cfg: &MatrixSendConfig) -> Option<MessageRelation> {
    cfg.thread_root
        .as_ref()
        .map(|root| MessageRelation::Thread {
            root: root.clone(),
            latest: cfg.thread_latest.clone(),
        })
        .or_else(|| cfg.reply_to.clone().map(|to| MessageRelation::Reply { to }))
}

/// Type de message Matrix correspondant au `kind` de la `config`.
fn message_kind(kind: SendKind) -> MessageKind {
    match kind {
        SendKind::Text => MessageKind::Text,
        SendKind::Notice => MessageKind::Notice,
    }
}

/// Kind de step `"matrix_edit"` (amendement 2026-11-16, issue #87) : remplace un message déjà
/// publié dans une salle Matrix par un événement `m.replace`.
pub struct MatrixEditStep {
    sender: Arc<dyn Sender>,
}

impl MatrixEditStep {
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

/// `config` du kind `"matrix_edit"`, après rendu de template. Clés inconnues refusées
/// (amendement 2026-11-16, issue #87).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MatrixEditConfig {
    room_id: String,
    event_id: String,
    body: String,
    html: Option<String>,
    #[serde(default)]
    kind: SendKind,
}

/// Erreur de configuration d'un `matrix_edit` : non retryable, jamais d'appel réseau.
fn invalid_edit(detail: impl Display) -> StepError {
    StepError {
        message: format!("config matrix_edit invalide: {detail}"),
        retryable: false,
    }
}

/// Validation pure de la `config` d'un `matrix_edit`, avant tout appel réseau : mêmes règles que
/// `matrix_send` pour `room_id`, `body` et `html`, plus `event_id` en `$…` non vide.
fn validate_edit(cfg: &MatrixEditConfig) -> Result<(), StepError> {
    if !cfg.room_id.starts_with('!') {
        return Err(invalid_edit("room_id doit commencer par '!'"));
    }
    if cfg.body.trim().is_empty() {
        return Err(invalid_edit("body vide"));
    }
    if cfg.html.as_deref().is_some_and(str::is_empty) {
        return Err(invalid_edit("html vide"));
    }
    if cfg.event_id.is_empty() || !cfg.event_id.starts_with('$') {
        return Err(invalid_edit(
            "event_id doit commencer par '$' et ne pas être vide",
        ));
    }
    Ok(())
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
        let relation = relation(&cfg);
        let MatrixSendConfig {
            room_id,
            body,
            html,
            kind,
            thread_root: _,
            thread_latest: _,
            reply_to: _,
        } = cfg;
        let event_id = self
            .sender
            .send(
                &room_id,
                &body,
                html.as_deref(),
                message_kind(kind),
                relation.as_ref(),
                &txn_id(run),
            )
            .await
            .map_err(|err| StepError {
                retryable: err.retryable(),
                message: err.to_string(),
            })?;
        Ok(json!({"event_id": event_id, "room_id": room_id}))
    }
}

#[async_trait::async_trait]
impl MiryadWorkflowStep for MatrixEditStep {
    fn kind(&self) -> &'static str {
        "matrix_edit"
    }

    async fn run(
        &self,
        run: &RunInfo,
        config: Value,
        _inputs: HashMap<String, Value>,
    ) -> Result<Value, StepError> {
        let cfg: MatrixEditConfig = serde_json::from_value(config).map_err(invalid_edit)?;
        validate_edit(&cfg)?;
        let MatrixEditConfig {
            room_id,
            event_id,
            body,
            html,
            kind,
        } = cfg;
        let replacement_id = self
            .sender
            .edit(
                &room_id,
                &event_id,
                &body,
                html.as_deref(),
                message_kind(kind),
                &txn_id(run),
            )
            .await
            .map_err(|err| StepError {
                retryable: err.retryable(),
                message: err.to_string(),
            })?;
        Ok(json!({
            "event_id": replacement_id,
            "edited_event_id": event_id,
            "room_id": room_id,
        }))
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
    use super::MatrixEditStep;
    use super::MatrixSendStep;
    use super::Sender;
    use super::txn_id;
    use crate::matrix::MatrixError;
    use crate::matrix::MessageKind;
    use crate::matrix::client::MessageRelation;

    /// Appel d'envoi reçu par le faux `Sender`.
    #[derive(Debug, Clone, PartialEq)]
    struct Call {
        room_id: String,
        body: String,
        html: Option<String>,
        kind: MessageKind,
        relation: Option<MessageRelation>,
        txn_id: String,
    }

    /// Appel d'édition reçu par le faux `Sender`.
    #[derive(Debug, Clone, PartialEq)]
    struct EditCall {
        room_id: String,
        event_id: String,
        body: String,
        html: Option<String>,
        kind: MessageKind,
        txn_id: String,
    }

    /// Journal des appels de réception du faux `Sender`.
    type Calls = Arc<Mutex<Vec<Call>>>;

    /// Journal des appels d'édition reçus par le faux `Sender`.
    type EditCalls = Arc<Mutex<Vec<EditCall>>>;

    /// Faux `Sender` : enregistre chaque appel (`send` comme `edit`) et rend ses `outcomes` en file.
    struct FakeSender {
        calls: Calls,
        outcomes: Mutex<VecDeque<Result<String, MatrixError>>>,
        edit_calls: EditCalls,
        edit_outcomes: Mutex<VecDeque<Result<String, MatrixError>>>,
    }

    #[async_trait::async_trait]
    impl Sender for FakeSender {
        async fn send(
            &self,
            room_id: &str,
            body: &str,
            html: Option<&str>,
            kind: MessageKind,
            relation: Option<&MessageRelation>,
            txn_id: &str,
        ) -> Result<String, MatrixError> {
            self.calls.lock().unwrap().push(Call {
                room_id: room_id.to_string(),
                body: body.to_string(),
                html: html.map(str::to_string),
                kind,
                relation: relation.cloned(),
                txn_id: txn_id.to_string(),
            });
            self.outcomes
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Ok("$evt".to_string()))
        }

        async fn edit(
            &self,
            room_id: &str,
            event_id: &str,
            body: &str,
            html: Option<&str>,
            kind: MessageKind,
            txn_id: &str,
        ) -> Result<String, MatrixError> {
            self.edit_calls.lock().unwrap().push(EditCall {
                room_id: room_id.to_string(),
                event_id: event_id.to_string(),
                body: body.to_string(),
                html: html.map(str::to_string),
                kind,
                txn_id: txn_id.to_string(),
            });
            self.edit_outcomes
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Ok("$new".to_string()))
        }
    }

    /// Construction partagée du faux `Sender` (journaux vides, outcomes en file).
    fn fake_sender(
        outcomes: VecDeque<Result<String, MatrixError>>,
        edit_outcomes: VecDeque<Result<String, MatrixError>>,
    ) -> (FakeSender, Calls, EditCalls) {
        let calls: Calls = Arc::new(Mutex::new(Vec::new()));
        let edit_calls: EditCalls = Arc::new(Mutex::new(Vec::new()));
        let sender = FakeSender {
            calls: Arc::clone(&calls),
            outcomes: Mutex::new(outcomes),
            edit_calls: Arc::clone(&edit_calls),
            edit_outcomes: Mutex::new(edit_outcomes),
        };
        (sender, calls, edit_calls)
    }

    /// Step d'envoi branché sur un faux `Sender` qui rendra `outcome`, et son journal d'envois.
    fn step_with(outcome: Result<String, MatrixError>) -> (MatrixSendStep, Calls) {
        let (sender, calls, _edits) = fake_sender(VecDeque::from([outcome]), VecDeque::new());
        let sender: Arc<dyn Sender> = Arc::new(sender);
        (MatrixSendStep::with_sender(sender), calls)
    }

    /// Step d'édition branché sur un faux `Sender` qui rendra `outcome` sur `edit`, et son
    /// journal d'éditions.
    fn edit_step_with(outcome: Result<String, MatrixError>) -> (MatrixEditStep, EditCalls) {
        let (sender, _calls, edits) = fake_sender(VecDeque::new(), VecDeque::from([outcome]));
        let sender: Arc<dyn Sender> = Arc::new(sender);
        (MatrixEditStep::with_sender(sender), edits)
    }

    fn run_info() -> RunInfo {
        RunInfo {
            run_key: "r".into(),
            step_id: "s".into(),
            depth: 0,
            vars: json!({}),
            trigger: None,
        }
    }

    /// Scenario « `txn_id` est stable pour une identité de run ».
    #[test]
    fn txn_id_est_stable_pour_une_identite_de_run() {
        let a = RunInfo {
            run_key: "r".into(),
            step_id: "s".into(),
            depth: 0,
            vars: json!({}),
            trigger: None,
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
                vars: json!({}),
                trigger: None,
            }),
            txn_id(&RunInfo {
                run_key: "r2".into(),
                step_id: "s".into(),
                depth: 0,
                vars: json!({}),
                trigger: None,
            }),
            txn_id(&RunInfo {
                run_key: "r1".into(),
                step_id: "s2".into(),
                depth: 0,
                vars: json!({}),
                trigger: None,
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
        assert_eq!(
            sent[0].relation, None,
            "aucun champ de relation : aucune relation"
        );
        assert_eq!(sent[0].txn_id, txn_id(&run_info()));
    }

    /// Scenario « la relation d'un `matrix_send` est dérivée et validée (issue #87) ».
    #[tokio::test]
    async fn la_relation_d_un_matrix_send_est_derivee_et_validee() {
        // Dérivations valides : `thread_root` (+ `thread_latest`) → Thread ; `reply_to` → Reply.
        let (step, calls) = step_with(Ok("$evt".into()));
        step.run(
            &run_info(),
            json!({"room_id": "!r:x", "body": "ok", "thread_root": "$root", "thread_latest": "$last"}),
            HashMap::new(),
        )
        .await
        .expect("config de fil valide");
        // Deuxième envoi sur le même faux `Sender` pour la configuration `reply_to`.
        step.run(
            &run_info(),
            json!({"room_id": "!r:x", "body": "ok", "reply_to": "$e"}),
            HashMap::new(),
        )
        .await
        .expect("config de réponse valide");
        {
            let sent = calls.lock().unwrap();
            assert_eq!(sent.len(), 2);
            assert_eq!(
                sent[0].relation,
                Some(MessageRelation::Thread {
                    root: "$root".into(),
                    latest: Some("$last".into()),
                })
            );
            assert_eq!(sent[1].relation, Some(MessageRelation::Reply { to: "$e".into() }));
        }
        // Validations : `thread_root` et `reply_to` exclusifs ; `thread_latest` exige
        // `thread_root` ; chaque valeur présente commence par `$` et n'est pas vide. Aucun appel.
        let cas = [
            json!({"room_id": "!r:x", "body": "ok", "thread_root": "$root", "reply_to": "$e"}),
            json!({"room_id": "!r:x", "body": "ok", "thread_latest": "$last"}),
            json!({"room_id": "!r:x", "body": "ok", "thread_root": "root"}),
        ];
        for config in cas {
            let (step, calls) = step_with(Ok("$evt".into()));
            let err = step
                .run(&run_info(), config.clone(), HashMap::new())
                .await
                .unwrap_err();
            assert!(!err.retryable, "relation invalide : jamais retryable ({config})");
            assert!(
                err.message.starts_with("config matrix_send invalide:"),
                "message : {}",
                err.message
            );
            assert!(calls.lock().unwrap().is_empty(), "aucun envoi pour {config}");
        }
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

    /// Scenario « `matrix_edit` transmet l'identifiant visé et rend le remplacement (issue #87) ».
    #[tokio::test]
    async fn matrix_edit_transmet_l_identifiant_vise_et_rend_le_remplacement() {
        let (step, edits) = edit_step_with(Ok("$new".into()));
        let config = json!({"room_id": "!r:x", "event_id": "$old", "body": "mis à jour"});
        let out: Value = step
            .run(&run_info(), config, HashMap::new())
            .await
            .expect("édition réussie");
        let sent = edits.lock().unwrap();
        assert_eq!(sent.len(), 1, "un seul appel d'édition");
        assert_eq!(sent[0].event_id, "$old", "l'identifiant visé est transmis");
        assert_eq!(sent[0].room_id, "!r:x");
        assert_eq!(sent[0].body, "mis à jour");
        assert_eq!(sent[0].html, None);
        assert_eq!(sent[0].kind, MessageKind::Text, "défaut : text");
        assert_eq!(
            sent[0].txn_id,
            txn_id(&run_info()),
            "le txn_id est dérivé comme pour matrix_send"
        );
        assert_eq!(
            out,
            json!({"event_id": "$new", "edited_event_id": "$old", "room_id": "!r:x"})
        );
    }

    /// Scenario « une config `matrix_edit` invalide ne fait aucun appel réseau (issue #87) ».
    #[tokio::test]
    async fn une_config_matrix_edit_invalide_ne_fait_aucun_appel_reseau() {
        let cas = [
            json!({"room_id": "!r:x", "body": "ok"}),
            json!({"room_id": "!r:x", "event_id": "pas-un-id", "body": "ok"}),
            json!({"room_id": "!r:x", "event_id": "$old", "body": "  "}),
            json!({"room_id": "!r:x", "event_id": "$old", "body": "ok", "thread_root": "$r"}),
        ];
        for config in cas {
            let (step, edits) = edit_step_with(Ok("$new".into()));
            let err = step
                .run(&run_info(), config.clone(), HashMap::new())
                .await
                .unwrap_err();
            assert!(!err.retryable, "config invalide : jamais retryable ({config})");
            assert!(
                err.message.starts_with("config matrix_edit invalide:"),
                "message : {}",
                err.message
            );
            assert!(edits.lock().unwrap().is_empty(), "aucune édition pour {config}");
        }
    }

    /// Scenario « les refus d'édition ne sont pas retryables (issue #87) ».
    #[tokio::test]
    async fn les_refus_d_edition_ne_sont_pas_retryables() {
        let config = json!({"room_id": "!r:x", "event_id": "$old", "body": "ok"});
        let cas = [
            (Err(MatrixError::EventNotFound("$old".into())), false),
            (Err(MatrixError::NotAuthor("$old".into())), false),
            (
                Err(MatrixError::Network {
                    message: "x".into(),
                    retryable: true,
                }),
                true,
            ),
        ];
        for (outcome, expected) in cas {
            let (step, _) = edit_step_with(outcome);
            let err: StepError = step
                .run(&run_info(), config.clone(), HashMap::new())
                .await
                .unwrap_err();
            assert_eq!(err.retryable, expected, "retryabilité attendue : {expected}");
        }
    }
}
