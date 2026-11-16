//! Miroir typé des événements de session (`src/lymaille/interactive/event.sdd`).
//!
//! Contrat de fil (DSD `FR-15`, `DEC-30`) : un enum `SessionEvent` diffusé par le hub
//! (./hub.sdd), sérialisé par le flux SSE (../http/sse.sdd), rendu en texte par le pont Matrix
//! (../chat/render.sdd) et exporté vers TypeScript par [`export_typescript`](crate::lymaille::interactive::event::export_typescript) pour remplacer un
//! `ChatEvent` écrit à la main. Les types amont (`Event`, `EventKind`, `ScriptRecord`,
//! `McpRecord`) sont `#[non_exhaustive]` et ne peuvent pas porter `ts-rs` (règle des traits
//! étrangers) : ce fichier est le miroir de la partie utile à un client, plus la variante
//! d'extension `Host` que l'hôte type de son côté et que la crate n'interprète jamais.
//!
//! Les enregistrements `Script` et `McpCall` sont réduits à un **résumé** (arbitré le
//! 2026-11-03) : ni texte de script, ni arguments MCP — le détail reste lisible par la route
//! d'historique (../http/routes.sdd) sous le droit du propriétaire.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use ts_rs::TS;

use ::lymaille::Event;
use ::lymaille::EventKind;
use ::lymaille::Outcome;
use ::lymaille::Timestamp;

use crate::lymaille::error::LymailleError;

/// Événement de session tel que le hub le diffuse et le client le consomme : la session,
/// l'instant en RFC 3339 UTC et ce qui s'est passé.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct SessionEvent {
    /// Identifiant de la session.
    pub session: String,
    /// Instant de l'événement, RFC 3339 UTC (conversion de l'instant amont).
    pub at: String,
    /// Ce qui s'est passé.
    pub kind: SessionEventKind,
}

/// Ce qui s'est passé dans une session, en variantes typées et sérialisées avec un discriminant
/// `type` en `snake_case`. Les variantes non reflétées de l'enum amont tombent dans [`Other`](Self::Other) ;
/// les événements émis par l'hôte seul tombent dans [`Host`](Self::Host).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEventKind {
    /// Un tour a commencé.
    TurnStarted {
        /// Rang du tour dans sa session.
        #[ts(type = "number")]
        turn: u64,
    },
    /// Fragment du texte streamé d'un tour.
    TextDelta {
        /// Rang du tour auquel appartient le fragment.
        #[ts(type = "number")]
        turn: u64,
        /// Texte du fragment.
        text: String,
    },
    /// Un tour s'est terminé avec son résultat et sa réponse complète.
    TurnEnded {
        /// Rang du tour dans sa session.
        #[ts(type = "number")]
        turn: u64,
        /// Résultat du tour.
        outcome: SessionOutcome,
        /// Réponse complète du tour.
        response: String,
    },
    /// Un run s'est terminé avec son résultat.
    RunEnded {
        /// Identifiant du run.
        run: String,
        /// Résultat du run.
        outcome: SessionOutcome,
    },
    /// Jetons utilisés par un appel d'inférence.
    TokenUsage {
        /// Jetons du prompt.
        #[ts(type = "number")]
        prompt: u64,
        /// Jetons de la complétion.
        #[ts(type = "number")]
        completion: u64,
    },
    /// La liste des tâches a changé.
    TodoChanged,
    /// Le titre de la session a changé.
    TitleChanged {
        /// Le nouveau titre, normalisé et masqué par l'amont.
        title: String,
    },
    /// La session a été signalée pour analyse.
    ReviewRequested {
        /// Raison courte du signalement, bornée et masquée par l'amont.
        reason: String,
    },
    /// Un script a tourné — résumé uniquement : la `result` est celle, déjà tronquée, de
    /// `ScriptRecord::result`, et le texte du script n'est jamais porté ici.
    Script {
        /// Agent qui a exécuté le script.
        agent: String,
        /// Durée du script en millisecondes.
        #[ts(type = "number")]
        duration_ms: u64,
        /// Résultat du script, déjà masqué et tronqué par l'amont.
        result: String,
        /// Le résultat a perdu des caractères à la troncature.
        truncated: bool,
        /// Erreur levée par le script, masquée par l'amont, le cas échéant.
        error: Option<String>,
    },
    /// Un appel MCP a été fait — résumé uniquement : les arguments ne sont jamais portés ici.
    McpCall {
        /// Rôle du serveur MCP dans la session.
        role: String,
        /// Nom de la fonction MCP appelée.
        function: String,
        /// Statut de l'appel, masqué par l'amont.
        status: String,
        /// Durée de l'appel en millisecondes.
        #[ts(type = "number")]
        duration_ms: u64,
    },
    /// Une reconnexion à un serveur MCP est tentée.
    McpReconnect {
        /// Rôle du serveur MCP dans la définition d'agent.
        role: String,
        /// Numéro de la tentative, à partir de 1.
        attempt: u32,
        /// Délai avant la tentative, en millisecondes.
        #[ts(type = "number")]
        delay_ms: u64,
    },
    /// Un appel d'inférence est rejoué après une erreur transitoire.
    InferenceRetry {
        /// Numéro du rejouage, à partir de 1.
        attempt: u32,
        /// Délai avant le rejouage, en millisecondes.
        #[ts(type = "number")]
        delay_ms: u64,
        /// Classe de l'erreur qui a motivé le rejouage.
        error_class: String,
    },
    /// Un tour a dépassé son délai configuré, sans être interrompu.
    Overrun {
        /// Temps écoulé depuis le début du tour, en millisecondes.
        #[ts(type = "number")]
        elapsed_ms: u64,
    },
    /// Une réponse a été coupée par la limite de sortie du modèle.
    ResponseTruncated {
        /// La réponse coupée porte du texte.
        has_text: bool,
        /// La réponse coupée porte des appels d'outils.
        has_tool_calls: bool,
    },
    /// Le message d'un utilisateur — **propre à l'historique** (DSD `DEC-30`, arbitrage du
    /// 2026-11-05) : seule la route d'historique (../http/routes.sdd) l'émet, aucune conversion
    /// `From<&Event>` ne la produit ; `text` est déjà expurgé par l'appelant.
    UserMessage {
        /// Rang du tour auquel appartient le message.
        #[ts(type = "number")]
        turn: u64,
        /// Texte du message, déjà expurgé par l'appelant.
        text: String,
    },
    /// Résumé d'un appel d'outil — **propre à l'historique** (DSD `DEC-30`, arbitrage du
    /// 2026-11-05) : seule la route d'historique (../http/routes.sdd) l'émet, aucune conversion
    /// `From<&Event>` ne la produit ; **sans arguments** ni résultat.
    ToolCallSummary {
        /// Rang du tour auquel appartient l'appel.
        #[ts(type = "number")]
        turn: u64,
        /// Nom de l'outil appelé.
        tool: String,
        /// Identifiant de l'appel.
        call_id: String,
        /// L'appel a un résultat, sans que son contenu soit porté ici.
        has_result: bool,
    },
    /// Une réponse vide a été écartée (`EventKind::EmptyResponse`, `lymaille` 0.2.0) :
    /// `will_retry` dit si le modèle est relancé, aucun contenu.
    EmptyResponse {
        /// Le modèle est relancé après la réponse vide.
        will_retry: bool,
    },
    /// Le bail de la porte de tour a été perdu (`EventKind::TurnLeaseLost`, `lymaille` 0.3.0) :
    /// le tour est abandonné sans sauvegarde ; `turn` est son rang, aucun contenu.
    TurnLeaseLost {
        /// Rang du tour abandonné.
        #[ts(type = "number")]
        turn: u64,
    },
    /// Événement amont non reflété (`MemoryChanged`, `TargetOpened`, `RunInterrupted` et toute
    /// variante future) : le nom de variante tel que `lymaille` le sérialise, aucun contenu.
    Other {
        /// Nom de la variante amont.
        name: String,
    },
    /// Événement émis par l'hôte seul (./hub.sdd, `publish_host`) : la crate transporte `kind`
    /// et `payload` sans jamais interpréter ce dernier.
    Host {
        /// Nature de l'événement, choisie par l'hôte.
        kind: String,
        /// Contenu libre typé par l'hôte, jamais interprété ni journalisé par la crate.
        #[ts(type = "unknown")]
        payload: Value,
    },
}

/// Résultat d'un tour ou d'un run — miroir de `lymaille::Outcome` en `snake_case`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum SessionOutcome {
    /// Le résultat est valide.
    Valid,
    /// Le tour ou le run a échoué.
    Failed,
    /// L'hôte a annulé le tour ou le run.
    Cancelled,
}

/// Conversion infaillible de l'événement amont vers son miroir typé : variante reflétée →
/// variante typée équivalente, toute autre variante → [`Other`](SessionEventKind::Other) avec le
/// nom de variante. Les champs textuels sont recopiés tels quels (déjà masqués par `lymaille`,
/// jamais un secret) ; le texte d'un script et les arguments d'un appel MCP sont laissés derrière.
impl From<&Event> for SessionEvent {
    fn from(event: &Event) -> Self {
        Self {
            session: event.session.as_str().to_owned(),
            at: timestamp_rfc3339(event.at),
            kind: kind_of(&event.kind),
        }
    }
}

/// Écrit dans `dir` un fichier `.ts` par type exporté et rend les chemins écrits ; `dir` est
/// créé au besoin. L'ensemble (amendement du 2026-11-13, issue #72) : les événements de session
/// (`SessionEvent`, `SessionEventKind`, `SessionOutcome`), les types de définition de `lymaille`
/// (`AgentDefinition`, `ModelProfile`, `Provider`, `Toolset` avec `Skill`/`TypeRequirement`/
/// `ParamDecl`/`RoleDecl`, `ContextType` avec `RoleSpec`, `ToolsetContext` avec `ContextLevel`)
/// et les types dont leurs déclarations importent les déclarations (`AgentKind`, `SandboxLimits`,
/// `SamplingParams`, `ReplayMode`, `ProviderLevel`, `SessionKind`, et `JsonValue` pour les champs
/// `serde_json::Value` de la bibliothèque amont), les `Model` des dix ressources de
/// `../definitions/` (un fichier nommé d'après l'entité : `agent.ts`, `private_agent.ts`, …), et
/// les corps de requête et de réponse de la surface de session de `../http/routes.sdd`
/// (`CreateBody`, `TextBody`, `SessionBody`, `ResumeBody`, `HistoryBody`).
///
/// Un seul appel, un seul répertoire, la liste rendue contient tous les chemins écrits ; les
/// fichiers importables entre eux (mêmes noms que les imports `ts-rs`, répertoire plat à
/// l'exception de `serde_json/JsonValue.ts`). L'ensemble est déterministe : mêmes chemins et
/// mêmes octets à chaque appel, sans horodatage. L'export TypeScript se fait **côté application**
/// (arbitré le 2026-11-03) : la crate ne commite ni ne publie aucun fichier `.ts`, les types ne
/// portent pas `#[ts(export)]` et `cargo test` n'en écrit jamais.
///
/// # Errors
///
/// [`LymailleError::Store`] (`MRD-LYMAILLE-009`) : panne d'entrée-sortie (création du répertoire,
/// écriture d'un fichier) ou panne de génération ; le texte rendu ne cite aucun chemin hors de
/// `dir`.
pub fn export_typescript(dir: &Path) -> Result<Vec<PathBuf>, LymailleError> {
    std::fs::create_dir_all(dir)
        .map_err(|error| LymailleError::Store(format!("export directory unavailable: {error}")))?;
    let files = [
        export_one::<SessionEvent>(dir, "SessionEvent"),
        export_one::<SessionEventKind>(dir, "SessionEventKind"),
        export_one::<SessionOutcome>(dir, "SessionOutcome"),
        // Le type des champs JSON de la bibliothèque amont, cité par ses imports.
        export_one::<serde_json::Value>(dir, "serde_json/JsonValue"),
        // Les types que les définitions ci-dessous importent, rendus avant elles (ordre stable).
        export_one::<::lymaille::AgentKind>(dir, "AgentKind"),
        export_one::<::lymaille::SandboxLimits>(dir, "SandboxLimits"),
        export_one::<::lymaille::SamplingParams>(dir, "SamplingParams"),
        export_one::<::lymaille::ReplayMode>(dir, "ReplayMode"),
        export_one::<::lymaille::ProviderLevel>(dir, "ProviderLevel"),
        export_one::<::lymaille::ParamDecl>(dir, "ParamDecl"),
        export_one::<::lymaille::RoleDecl>(dir, "RoleDecl"),
        export_one::<::lymaille::Skill>(dir, "Skill"),
        export_one::<::lymaille::TypeRequirement>(dir, "TypeRequirement"),
        export_one::<::lymaille::RoleSpec>(dir, "RoleSpec"),
        export_one::<::lymaille::SessionKind>(dir, "SessionKind"),
        export_one::<::lymaille::ContextLevel>(dir, "ContextLevel"),
        // Les types de définition de `lymaille`.
        export_one::<::lymaille::AgentDefinition>(dir, "AgentDefinition"),
        export_one::<::lymaille::ModelProfile>(dir, "ModelProfile"),
        export_one::<::lymaille::Provider>(dir, "Provider"),
        export_one::<::lymaille::Toolset>(dir, "Toolset"),
        export_one::<::lymaille::ContextType>(dir, "ContextType"),
        export_one::<::lymaille::ToolsetContext>(dir, "ToolsetContext"),
        // Les `Model` des dix ressources, un fichier par entité (nom du `Scenario` de chaque
        // spec de `../definitions/`).
        export_one::<crate::lymaille::definitions::agent::Model>(dir, "agent"),
        export_one::<crate::lymaille::definitions::private_agent::Model>(dir, "private_agent"),
        export_one::<crate::lymaille::definitions::profile::Model>(dir, "profile"),
        export_one::<crate::lymaille::definitions::provider::Model>(dir, "provider"),
        export_one::<crate::lymaille::definitions::toolset::Model>(dir, "toolset"),
        export_one::<crate::lymaille::definitions::private_toolset::Model>(dir, "private_toolset"),
        export_one::<crate::lymaille::definitions::context_type::Model>(dir, "context_type"),
        export_one::<crate::lymaille::definitions::mcp_server::Model>(dir, "mcp_server"),
        export_one::<crate::lymaille::definitions::catalog_context::Model>(dir, "catalog_context"),
        export_one::<crate::lymaille::definitions::user_context::Model>(dir, "user_context"),
        // Les corps de la surface de session (../http/routes.sdd).
        export_one::<crate::lymaille::http::routes::CreateBody>(dir, "CreateBody"),
        export_one::<crate::lymaille::http::routes::TextBody>(dir, "TextBody"),
        export_one::<crate::lymaille::http::routes::SessionBody>(dir, "SessionBody"),
        export_one::<crate::lymaille::http::routes::ResumeBody>(dir, "ResumeBody"),
        export_one::<crate::lymaille::http::routes::HistoryBody>(dir, "HistoryBody"),
    ];
    let mut written = Vec::with_capacity(files.len());
    for file in files {
        written.push(file?);
    }
    Ok(written)
}

/// Génère le texte TypeScript d'un type et l'écrit dans `dir/{name}.ts` (les sous-répertoires
/// de `name` sont créés au besoin, pour `serde_json/JsonValue`) ; rend le chemin écrit.
fn export_one<T>(dir: &Path, name: &str) -> Result<PathBuf, LymailleError>
where
    T: TS + 'static,
{
    let text = T::export_to_string(&ts_rs::Config::new())
        .map_err(|error| LymailleError::Store(format!("typescript generation failed: {error}")))?;
    let path = dir.join(format!("{name}.ts"));
    if let Some(parent) = path.parent()
        && let Err(error) = std::fs::create_dir_all(parent)
    {
        return Err(LymailleError::Store(format!(
            "export directory unavailable: {error}"
        )));
    }
    std::fs::write(&path, text)
        .map_err(|error| LymailleError::Store(format!("typescript export write failed: {error}")))?;
    Ok(path)
}

/// Variante amont vers sa variante typée : les 16 variantes reflétées champ à champ, toute autre
/// variante (connue ou future) en `Other { name }` — conversion infaillible, `Must` de ./event.sdd.
fn kind_of(kind: &EventKind) -> SessionEventKind {
    match kind {
        EventKind::TurnStarted { turn } => SessionEventKind::TurnStarted { turn: turn.get() },
        EventKind::TextDelta { turn, text } => SessionEventKind::TextDelta {
            turn: turn.get(),
            text: text.clone(),
        },
        EventKind::TurnEnded {
            turn,
            outcome,
            response,
        } => SessionEventKind::TurnEnded {
            turn: turn.get(),
            outcome: outcome_of(*outcome),
            response: response.clone(),
        },
        EventKind::RunEnded { run, outcome } => SessionEventKind::RunEnded {
            run: run.as_str().to_owned(),
            outcome: outcome_of(*outcome),
        },
        EventKind::TokenUsage { prompt, completion } => SessionEventKind::TokenUsage {
            prompt: *prompt,
            completion: *completion,
        },
        EventKind::TodoChanged => SessionEventKind::TodoChanged,
        EventKind::TitleChanged { title } => SessionEventKind::TitleChanged { title: title.clone() },
        EventKind::ReviewRequested { reason } => SessionEventKind::ReviewRequested {
            reason: reason.clone(),
        },
        // Résumé du record de script : agent, durée, résultat déjà tronqué, troncature et
        // erreur — jamais le texte du script, qui reste dans l'événement amont pour l'audit.
        EventKind::Script(record) => SessionEventKind::Script {
            agent: record.agent.clone(),
            duration_ms: record.duration_ms,
            result: record.result.clone(),
            truncated: record.truncated,
            error: record.error.clone(),
        },
        // Résumé de l'appel MCP : rôle, fonction, statut, durée — jamais les arguments.
        EventKind::McpCall(record) => SessionEventKind::McpCall {
            role: record.role.clone(),
            function: record.function.clone(),
            status: record.status.clone(),
            duration_ms: record.duration_ms,
        },
        EventKind::McpReconnect {
            role,
            attempt,
            delay_ms,
        } => SessionEventKind::McpReconnect {
            role: role.clone(),
            attempt: *attempt,
            delay_ms: *delay_ms,
        },
        EventKind::InferenceRetry {
            attempt,
            delay_ms,
            error_class,
        } => SessionEventKind::InferenceRetry {
            attempt: *attempt,
            delay_ms: *delay_ms,
            error_class: error_class.clone(),
        },
        EventKind::Overrun { elapsed_ms } => SessionEventKind::Overrun {
            elapsed_ms: *elapsed_ms,
        },
        EventKind::ResponseTruncated {
            has_text,
            has_tool_calls,
        } => SessionEventKind::ResponseTruncated {
            has_text: *has_text,
            has_tool_calls: *has_tool_calls,
        },
        EventKind::EmptyResponse { will_retry } => SessionEventKind::EmptyResponse {
            will_retry: *will_retry,
        },
        EventKind::TurnLeaseLost { turn } => SessionEventKind::TurnLeaseLost { turn: turn.get() },
        other => SessionEventKind::Other {
            name: other_name(other),
        },
    }
}

/// Instant amont (millisecondes depuis l'époque) en RFC 3339 UTC. Un instant hors de portée de
/// `chrono` est ramené à l'heure courante, comme dans `../store/audit.rs` : la conversion ne
/// peut pas échouer et une horodatage approximatif vaut mieux qu'un événement perdu.
fn timestamp_rfc3339(at: Timestamp) -> String {
    let millis = i64::try_from(at.as_millis()).unwrap_or(i64::MAX);
    let moment = chrono::DateTime::from_timestamp_millis(millis).unwrap_or_else(chrono::Utc::now);
    moment.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Résultat amont vers son miroir. `Outcome` est `#[non_exhaustive]` : un résultat futur,
/// inconnu de ce miroir, tombe dans `Failed` — seul `Valid` affirme un résultat valide.
fn outcome_of(outcome: Outcome) -> SessionOutcome {
    match outcome {
        Outcome::Valid => SessionOutcome::Valid,
        Outcome::Cancelled => SessionOutcome::Cancelled,
        _ => SessionOutcome::Failed,
    }
}

/// Nom de la variante amont telle que `lymaille` la sérialise : une variante unitaire rend un
/// texte, une variante à données un objet à une seule clé. Repli prudent `"Unknown"` si la
/// sérialisation surprenait (inatteignable sur ces types en pratique).
fn other_name(kind: &EventKind) -> String {
    match serde_json::to_value(kind) {
        Ok(Value::String(name)) => name,
        Ok(Value::Object(map)) => map
            .into_iter()
            .next()
            .map_or_else(|| String::from("Unknown"), |(name, _)| name),
        Ok(_) | Err(_) => String::from("Unknown"),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::time::Duration;

    use serde_json::json;

    use super::SessionEvent;
    use super::SessionEventKind;
    use super::SessionOutcome;
    use super::export_typescript;
    use ::lymaille::Event;
    use ::lymaille::EventKind;
    use ::lymaille::McpInput;
    use ::lymaille::McpRecord;
    use ::lymaille::Outcome;
    use ::lymaille::Redactor;
    use ::lymaille::RunId;
    use ::lymaille::ScriptInput;
    use ::lymaille::ScriptRecord;
    use ::lymaille::SessionId;
    use ::lymaille::Timestamp;
    use ::lymaille::TurnId;

    /// Instant fixe de fixture.
    const AT: u64 = 1_730_000_000_000;

    fn session(id: &str) -> SessionId {
        SessionId::new(id).expect("un texte non vide fait un identifiant de session")
    }

    fn event(session_id: &str, kind: EventKind) -> Event {
        Event::new(session(session_id), Timestamp::from_millis(AT), kind)
    }

    /// Marqueur de texte de script qui ne doit jamais sortir d'un résumé.
    const SCRIPT_MARKER: &str = "SK-marque-du-texte-du-script";

    fn secret_looking_script() -> String {
        format!("let cle = \"{SCRIPT_MARKER}\";")
    }

    /// Scenario « un événement de tour est typé » : `TextDelta` devient le JSON typé attendu,
    /// `turn` un nombre, `at` l'RFC 3339 UTC de l'instant amont.
    #[test]
    fn un_evenement_de_tour_est_type() {
        let upstream = event(
            "s1",
            EventKind::TextDelta {
                turn: TurnId::new(3),
                text: "bon".to_owned(),
            },
        );
        let value = serde_json::to_value(SessionEvent::from(&upstream)).expect("un miroir sérialise");
        assert_eq!(value["session"], json!("s1"));
        assert_eq!(
            value["kind"],
            json!({"type": "text_delta", "turn": 3, "text": "bon"})
        );
        assert!(
            value["kind"]["turn"].is_number(),
            "`turn` est un nombre JSON : {value}"
        );
        let at = value["at"].as_str().expect("at est un texte");
        let parsed = chrono::DateTime::parse_from_rfc3339(at)
            .unwrap_or_else(|error| panic!("`at` est de l'RFC 3339 : {at} : {error}"));
        assert_eq!(
            u64::try_from(parsed.timestamp_millis()).expect("un instant d'époque en portée u64"),
            AT
        );
    }

    /// Scenario « un script est résumé sans son texte » : les champs du résumé sont là, le
    /// texte du script jamais.
    #[test]
    fn un_script_est_resume_sans_son_texte() {
        let script = secret_looking_script();
        let input = ScriptInput::new("agent-a", &script, "42", Duration::from_millis(4))
            .with_error("runtime error: boom");
        let record = ScriptRecord::new(&Redactor::default(), 2000, input);
        let upstream = event("s1", EventKind::Script(record));
        let mirrored = SessionEvent::from(&upstream);
        let SessionEventKind::Script {
            agent,
            duration_ms,
            result,
            truncated,
            error,
        } = &mirrored.kind
        else {
            panic!("un Script se miroite en Script : {:?}", mirrored.kind)
        };
        assert_eq!(agent, "agent-a");
        assert_eq!(*duration_ms, 4);
        assert_eq!(result, "42");
        assert!(!truncated);
        assert_eq!(error.as_deref(), Some("runtime error: boom"));
        let text = serde_json::to_string(&mirrored).expect("un miroir sérialise");
        assert!(
            !text.contains(SCRIPT_MARKER),
            "le texte du script ne traverse jamais le miroir : {text}"
        );
    }

    /// Scenario « un appel MCP est résumé sans ses arguments » : `role`, `function`, `status`,
    /// `duration_ms` passent, les arguments jamais.
    #[test]
    fn un_appel_mcp_est_resume_sans_ses_arguments() {
        let record = McpRecord::new(
            &Redactor::default(),
            McpInput::new(
                "monitoring",
                "query",
                &json!({"token_like": "x"}),
                "ok",
                Duration::from_millis(9),
            ),
        );
        let upstream = event("s1", EventKind::McpCall(record));
        let mirrored = SessionEvent::from(&upstream);
        let SessionEventKind::McpCall {
            role,
            function,
            status,
            duration_ms,
        } = &mirrored.kind
        else {
            panic!("un McpCall se miroite en McpCall : {:?}", mirrored.kind)
        };
        assert_eq!(role, "monitoring");
        assert_eq!(function, "query");
        assert_eq!(status, "ok");
        assert_eq!(*duration_ms, 9);
        let text = serde_json::to_string(&mirrored).expect("un miroir sérialise");
        assert!(
            !text.contains("token_like"),
            "les arguments d'un appel MCP ne traversent jamais le miroir : {text}"
        );
    }

    /// Tâche « Alignement `lymaille` 0.2.0 » (`../interactive/event.sdd`) : `EmptyResponse` est
    /// un événement **typé** (miroir de la variante amont 0.2.0), pas un `Other` — sérialisé
    /// sous `type` = `empty_response` avec le booléen `will_retry` et aucun autre contenu.
    #[test]
    fn une_reponse_vide_est_miroitee_en_empty_response() {
        for will_retry in [true, false] {
            let upstream = event("s1", EventKind::EmptyResponse { will_retry });
            let value = serde_json::to_value(SessionEvent::from(&upstream)).expect("un miroir sérialise");
            assert_eq!(
                value["kind"],
                json!({"type": "empty_response", "will_retry": will_retry}),
                "`EmptyResponse` est une variante typée, pas un `Other`"
            );
        }
    }

    /// Tâche « Alignement `lymaille` 0.3.0 » (`../interactive/event.sdd`) : `TurnLeaseLost` est
    /// un événement **typé** (miroir de la variante amont 0.3.0), jamais un `Other` — sérialisé
    /// sous `type` = `turn_lease_lost` avec `turn` numérique et aucun autre contenu.
    #[test]
    fn une_perte_de_bail_est_miroitee_en_turn_lease_lost() {
        let upstream = event("s1", EventKind::TurnLeaseLost { turn: TurnId::new(3) });
        let value = serde_json::to_value(SessionEvent::from(&upstream)).expect("un miroir sérialise");
        assert_eq!(
            value["kind"],
            json!({"type": "turn_lease_lost", "turn": 3}),
            "`TurnLeaseLost` est une variante typée, pas un `Other`"
        );
        assert!(
            value["kind"]["turn"].is_number(),
            "`turn` est un nombre JSON : {value}"
        );
    }

    /// Tâche « Alignement `lymaille` 0.3.0 » : l'export TypeScript de `SessionEventKind` porte la
    /// variante `turn_lease_lost`, dont `turn` est typé `number` (jamais `bigint`).
    #[test]
    fn la_perte_de_bail_est_exportee_avec_turn_en_number() {
        let dir = scratch_dir("export-turn-lease-lost");
        export_typescript(&dir).expect("l'export réussit dans un répertoire créé");
        let kinds =
            std::fs::read_to_string(dir.join("SessionEventKind.ts")).expect("SessionEventKind.ts est écrit");
        let debut = kinds
            .find("turn_lease_lost")
            .expect("la variante turn_lease_lost figure dans l'export");
        let variante = kinds
            .get(debut..)
            .and_then(|reste| reste.find('}').and_then(|fin| reste.get(..fin)))
            .expect("la variante se termine par une accolade");
        assert!(
            variante.contains("turn: number"),
            "`turn` de la variante turn_lease_lost est typé number : {variante}"
        );
        std::fs::remove_dir_all(&dir).expect("le répertoire de test se nettoie");
    }

    /// Scenario « un message utilisateur et un appel d'outil se sérialisent sous des noms
    /// distincts » (amendement historique complet, DSD `DEC-30`) : `UserMessage` et
    /// `ToolCallSummary` portent `type` = `user_message` / `tool_call_summary`, `turn` est un
    /// nombre, aucun champ d'arguments n'existe, et l'aller-retour serde est identique.
    #[test]
    fn un_message_utilisateur_et_un_appel_d_outil_se_serialisent_sous_des_noms_distincts() {
        let user = SessionEvent {
            session: "s1".to_owned(),
            at: "2026-11-05T00:00:00.000Z".to_owned(),
            kind: SessionEventKind::UserMessage {
                turn: 1,
                text: "bonjour".to_owned(),
            },
        };
        let value = serde_json::to_value(&user).expect("un miroir sérialise");
        assert_eq!(
            value["kind"],
            json!({"type": "user_message", "turn": 1, "text": "bonjour"})
        );
        assert!(
            value["kind"]["turn"].is_number(),
            "`turn` est un nombre JSON : {value}"
        );

        let call = SessionEvent {
            session: "s1".to_owned(),
            at: "2026-11-05T00:00:00.000Z".to_owned(),
            kind: SessionEventKind::ToolCallSummary {
                turn: 1,
                tool: "rhai_pad".to_owned(),
                call_id: "c1".to_owned(),
                has_result: true,
            },
        };
        let value = serde_json::to_value(&call).expect("un miroir sérialise");
        assert_eq!(
            value["kind"],
            json!({"type": "tool_call_summary", "turn": 1, "tool": "rhai_pad", "call_id": "c1", "has_result": true})
        );
        assert!(
            value["kind"]["turn"].is_number(),
            "`turn` est un nombre JSON : {value}"
        );
        let kind = value["kind"].as_object().expect("kind est un objet");
        let mut keys: Vec<&str> = kind.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec!["call_id", "has_result", "tool", "turn", "type"],
            "`ToolCallSummary` ne porte aucun champ d'arguments : {value}"
        );

        for mirrored in [user, call] {
            let text = serde_json::to_string(&mirrored).expect("un miroir sérialise");
            let back: SessionEvent = serde_json::from_str(&text).expect("un miroir sérialisé se désérialise");
            assert_eq!(back, mirrored, "l'aller-retour serde est identique");
        }
    }

    /// Amendement historique complet (DSD `DEC-30`) : `UserMessage` et `ToolCallSummary` sont
    /// **propres à l'historique** — seule la route d'historique les émet. Aucun événement amont
    /// connu, reflété ou non, ne produit ces variantes par `From<&Event>` : `kind_of` ne porte
    /// aucun bras nouveau vers elles.
    #[test]
    fn from_event_ne_produit_jamais_les_variantes_propres_a_l_historique() {
        let record = ScriptRecord::new(
            &Redactor::default(),
            2000,
            ScriptInput::new("agent-a", "let x = 1;", "1", Duration::from_millis(1)),
        );
        let mcp = McpRecord::new(
            &Redactor::default(),
            McpInput::new("monitoring", "query", &json!({}), "ok", Duration::from_millis(1)),
        );
        let upstream_kinds = vec![
            EventKind::TurnStarted { turn: TurnId::new(1) },
            EventKind::TextDelta {
                turn: TurnId::new(1),
                text: "bon".to_owned(),
            },
            EventKind::TurnEnded {
                turn: TurnId::new(1),
                outcome: Outcome::Valid,
                response: "fin".to_owned(),
            },
            EventKind::RunEnded {
                run: RunId::new("run-7").expect("un texte non vide fait un identifiant de run"),
                outcome: Outcome::Valid,
            },
            EventKind::TokenUsage {
                prompt: 1,
                completion: 1,
            },
            EventKind::TodoChanged,
            EventKind::TitleChanged {
                title: "t".to_owned(),
            },
            EventKind::ReviewRequested {
                reason: "r".to_owned(),
            },
            EventKind::Script(record),
            EventKind::McpCall(mcp),
            EventKind::McpReconnect {
                role: "monitoring".to_owned(),
                attempt: 1,
                delay_ms: 1,
            },
            EventKind::InferenceRetry {
                attempt: 1,
                delay_ms: 1,
                error_class: "timeout".to_owned(),
            },
            EventKind::Overrun { elapsed_ms: 1 },
            EventKind::ResponseTruncated {
                has_text: true,
                has_tool_calls: false,
            },
            EventKind::EmptyResponse { will_retry: true },
            EventKind::MemoryChanged {
                level: "user".to_owned(),
                id: "m1".to_owned(),
            },
            EventKind::TargetOpened {
                kind: "definition".to_owned(),
                name: "agent/ctx".to_owned(),
            },
            EventKind::RunInterrupted {
                run: RunId::new("run-7").expect("un texte non vide fait un identifiant de run"),
                session: session("sub-7"),
            },
        ];
        for upstream in upstream_kinds {
            let mirrored = SessionEvent::from(&event("s1", upstream));
            assert!(
                !matches!(
                    mirrored.kind,
                    SessionEventKind::UserMessage { .. } | SessionEventKind::ToolCallSummary { .. }
                ),
                "aucun événement amont ne produit une variante propre à l'historique : {:?}",
                mirrored.kind
            );
        }
    }

    /// Scenario « une variante non reflétée devient Other » : `MemoryChanged` ne passe que par
    /// son nom, sans `level` ni `id`.
    #[test]
    fn une_variante_non_refletee_devient_other() {
        let upstream = event(
            "s1",
            EventKind::MemoryChanged {
                level: "user".to_owned(),
                id: "m1".to_owned(),
            },
        );
        let mirrored = SessionEvent::from(&upstream);
        assert_eq!(
            mirrored.kind,
            SessionEventKind::Other {
                name: "MemoryChanged".to_owned()
            }
        );
        assert_eq!(
            serde_json::to_value(&mirrored.kind).expect("un kind sérialise"),
            json!({"type": "other", "name": "MemoryChanged"})
        );
    }

    /// Scenario « un événement Host traverse tel quel » : aller-retour JSON à l'identique.
    #[test]
    fn un_evenement_host_traverse_tel_quel() {
        let mirrored = SessionEvent {
            session: "s1".to_owned(),
            at: "2026-11-04T00:00:00.000Z".to_owned(),
            kind: SessionEventKind::Host {
                kind: "dashboard.show".to_owned(),
                payload: json!({"a": [1, 2]}),
            },
        };
        let text = serde_json::to_string(&mirrored).expect("un miroir sérialise");
        let back: SessionEvent = serde_json::from_str(&text).expect("un miroir sérialisé se désérialise");
        assert_eq!(back, mirrored);
    }

    /// Chemin d'un répertoire temporaire unique, sans dépendance `tempfile` (absente des
    /// dev-dependencies de la crate). Partagé par les tests d'export des `Scenario` de
    /// `../definitions/*.sdd` et de `../http/routes.sdd` (issue #72).
    #[must_use]
    pub(crate) fn scratch_dir(tag: &str) -> std::path::PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.subsec_nanos());
        std::env::temp_dir().join(format!("miryad-event-{tag}-{}-{unique:x}", std::process::id()))
    }

    /// Scenario « l'export TypeScript est produit par l'outillage de la crate » : trois fichiers
    /// dans un répertoire inexistant, un second appel identique, `number` jamais `bigint`,
    /// `payload` inconnu, aucun texte de script ni d'arguments.
    #[test]
    fn lexport_typescript_est_produit_par_loutillage_de_la_crate() {
        let dir = scratch_dir("export");
        assert!(!dir.exists(), "le répertoire de test est inexistant");
        let written = export_typescript(&dir).expect("l'export réussit dans un répertoire créé");
        for name in ["SessionEvent.ts", "SessionEventKind.ts", "SessionOutcome.ts"] {
            assert!(
                written.contains(&dir.join(name)),
                "les trois fichiers des événements sont écrits : {name} : {written:?}"
            );
        }
        let contents: std::collections::HashMap<_, _> = written
            .iter()
            .map(|path| {
                let text = std::fs::read_to_string(path).expect("le fichier exporté s'écrit et se relit");
                (path.clone(), text)
            })
            .collect();
        let second = export_typescript(&dir).expect("le second appel réussit");
        assert_eq!(second, written, "le second appel rend les mêmes chemins");
        for path in &written {
            let again = std::fs::read_to_string(path).expect("le fichier exporté se relit");
            assert_eq!(
                again,
                *contents.get(path).expect("chemin connu"),
                "le second appel rend le même contenu"
            );
            assert!(
                !again.contains(SCRIPT_MARKER) && !again.contains("token_like"),
                "aucun texte de script ni d'arguments dans {}",
                path.display()
            );
        }
        let kinds =
            std::fs::read_to_string(dir.join("SessionEventKind.ts")).expect("SessionEventKind.ts est écrit");
        for field in ["turn", "duration_ms", "delay_ms"] {
            assert!(
                kinds.contains(&format!("{field}: number")),
                "`{field}` est typé number : {kinds}"
            );
        }
        assert!(
            kinds.contains("payload: unknown"),
            "`payload` est inconnu : {kinds}"
        );
        assert!(!kinds.contains("bigint"), "jamais de bigint : {kinds}");
        for name in ["user_message", "tool_call_summary"] {
            assert!(
                kinds.contains(name),
                "les variantes propres à l'historique apparaissent dans l'export : {name} : {kinds}"
            );
        }
        let outcome =
            std::fs::read_to_string(dir.join("SessionOutcome.ts")).expect("SessionOutcome.ts est écrit");
        for name in ["valid", "failed", "cancelled"] {
            assert!(outcome.contains(name), "`{name}` est en snake_case : {outcome}");
        }
        std::fs::remove_dir_all(&dir).expect("le répertoire de test se nettoie");
    }

    /// Scenario « un répertoire d'export inutilisable est une erreur » : `dir` est un fichier,
    /// `Store` et rien d'écrit.
    #[test]
    fn un_repertoire_export_inutilisable_est_une_erreur() {
        let path = scratch_dir("export-est-un-fichier");
        std::fs::write(&path, b"pas un repertoire").expect("le fichier-témoin s'écrit");
        let outcome = export_typescript(&path);
        let error = outcome.expect_err("un fichier n'est pas un répertoire d'export");
        assert!(
            matches!(error, crate::lymaille::LymailleError::Store(_)),
            "l'échec d'entrée-sortie est un `Store` : {error}"
        );
        let shown = path.display().to_string();
        assert!(
            !error.to_string().contains(&shown),
            "le texte de l'erreur ne cite pas le chemin : {error}"
        );
        std::fs::remove_file(&path).expect("le fichier-témoin se nettoie");
    }

    /// Les chemins attendus de `export_typescript` — le contrat de l'amendement du 2026-11-13
    /// (issue #72) : trois fichiers d'événements, le helper `JsonValue` des déclarations amont,
    /// les types que les définitions de `lymaille` importent, les six types de définition, les
    /// dix `Model` (fichiers nommés d'après l'entité, comme les `Scenario` de
    /// `../definitions/*.sdd`) et les cinq corps de la surface de session
    /// (`../http/routes.sdd`).
    const EXPECTED_EXPORTS: [&str; 37] = [
        "SessionEvent.ts",
        "SessionEventKind.ts",
        "SessionOutcome.ts",
        "serde_json/JsonValue.ts",
        "AgentKind.ts",
        "SandboxLimits.ts",
        "SamplingParams.ts",
        "ReplayMode.ts",
        "ProviderLevel.ts",
        "ParamDecl.ts",
        "RoleDecl.ts",
        "Skill.ts",
        "TypeRequirement.ts",
        "RoleSpec.ts",
        "SessionKind.ts",
        "ContextLevel.ts",
        "AgentDefinition.ts",
        "ModelProfile.ts",
        "Provider.ts",
        "Toolset.ts",
        "ContextType.ts",
        "ToolsetContext.ts",
        "agent.ts",
        "private_agent.ts",
        "profile.ts",
        "provider.ts",
        "toolset.ts",
        "private_toolset.ts",
        "context_type.ts",
        "mcp_server.ts",
        "catalog_context.ts",
        "user_context.ts",
        "CreateBody.ts",
        "TextBody.ts",
        "SessionBody.ts",
        "ResumeBody.ts",
        "HistoryBody.ts",
    ];

    /// Scenario « l'export couvre les définitions, les lignes et les corps de session »
    /// (amendement du 2026-11-13, issue #72) : un fichier pour chacun des types attendus, la
    /// liste rendue les contient tous et rien d'autre ; chaque import `./…` d'un fichier rendu
    /// désigne un chemin rendu (un seul répertoire) ; le second appel rend les mêmes chemins et
    /// les mêmes octets.
    #[test]
    fn lexport_couvre_les_definitions_les_lignes_et_les_corps() {
        let dir = scratch_dir("export-couvre");
        let written = export_typescript(&dir).expect("l'export réussit dans un répertoire créé");
        let expected: Vec<std::path::PathBuf> = EXPECTED_EXPORTS.iter().map(|name| dir.join(name)).collect();
        assert_eq!(
            written, expected,
            "la liste rendue est exactement celle des fichiers attendus"
        );
        for path in &written {
            assert!(
                path.is_file(),
                "chaque chemin rendu est un fichier écrit : {path:?}"
            );
        }
        for path in &written {
            let text = std::fs::read_to_string(path).expect("le fichier exporté se relit");
            for line in text.lines().filter(|line| line.starts_with("import type")) {
                let cible = line
                    .split('"')
                    .nth(1)
                    .unwrap_or_else(|| panic!("un import cite sa cible entre guillemets : {line}"));
                let relative = cible.trim_start_matches("./");
                let imported = dir.join(format!("{relative}.ts"));
                assert!(
                    written.contains(&imported),
                    "{} importe {} : hors des chemins rendus",
                    path.display(),
                    cible
                );
            }
        }
        let contents: std::collections::HashMap<_, _> = written
            .iter()
            .map(|path| {
                let text = std::fs::read_to_string(path).expect("le fichier exporté se relit");
                (path.clone(), text)
            })
            .collect();
        let second = export_typescript(&dir).expect("le second appel réussit");
        assert_eq!(second, written, "le second appel rend les mêmes chemins");
        for path in &written {
            let again = std::fs::read_to_string(path).expect("le fichier exporté se relit");
            assert_eq!(
                again,
                *contents.get(path).expect("chemin connu"),
                "le second appel rend les mêmes octets (déterminisme sans horodatage)"
            );
        }
        std::fs::remove_dir_all(&dir).expect("le répertoire de test se nettoie");
    }

    /// Scenario « chaque déclaration exportée correspond à la sérialisation serde »
    /// (amendement du 2026-11-13, issue #72) : pour chaque type de définition de `lymaille`, une
    /// valeur construite par la bibliothèque est sérialisée en JSON ; toutes les clés du JSON
    /// sont des propriétés de la déclaration exportée et toute propriété non optionnelle de la
    /// déclaration est une clé du JSON ; le JSON de la valeur d'exemple est accepté par la
    /// désérialisation de `lymaille`.
    #[test]
    fn chaque_declaration_exportee_correspond_a_la_serialisation_serde() {
        let dir = scratch_dir("export-serde");
        export_typescript(&dir).expect("l'export réussit");
        let agent = ::lymaille::AgentDefinition::new(
            "G_contrat".to_owned(),
            ::lymaille::AgentKind::Sub,
            "prompt de contrat".to_owned(),
        )
        .with_profile("qwen".to_owned());
        let profil =
            ::lymaille::ModelProfile::new("rapide".to_owned(), "fournisseur".to_owned(), "modele".to_owned());
        let fournisseur = ::lymaille::Provider::new(
            "interne".to_owned(),
            "openai".to_owned(),
            "https://api.example/v1".to_owned(),
            None,
            ::lymaille::ProviderLevel::User,
        )
        .expect("le provider de fixture se construit");
        let toolset = ::lymaille::Toolset::new(
            "ts_contrat".to_owned(),
            "toolset de contrat".to_owned(),
            vynil_core::Semver::parse("1.0.0").expect("la version de fixture se parse"),
            "catalogue".to_owned(),
        )
        .expect("le toolset de fixture se construit");
        let type_contexte = ::lymaille::ContextType::new(
            "G_type".to_owned(),
            vynil_core::Semver::parse("1.0.0").expect("la version de fixture se parse"),
            json!({ "type": "object" }),
        )
        .expect("le type de contexte de fixture se construit");
        let contexte =
            ::lymaille::ToolsetContext::new("c_contrat".to_owned(), ::lymaille::ContextLevel::User)
                .expect("le contexte de fixture se construit");
        let cas: Vec<(&str, serde_json::Value)> = vec![
            (
                "AgentDefinition",
                serde_json::to_value(&agent).expect("la fixture sérialise"),
            ),
            (
                "ModelProfile",
                serde_json::to_value(&profil).expect("la fixture sérialise"),
            ),
            (
                "Provider",
                serde_json::to_value(&fournisseur).expect("la fixture sérialise"),
            ),
            (
                "Toolset",
                serde_json::to_value(&toolset).expect("la fixture sérialise"),
            ),
            (
                "ContextType",
                serde_json::to_value(&type_contexte).expect("la fixture sérialise"),
            ),
            (
                "ToolsetContext",
                serde_json::to_value(&contexte).expect("la fixture sérialise"),
            ),
        ];
        for (name, value) in cas {
            let texte = std::fs::read_to_string(dir.join(format!("{name}.ts")))
                .unwrap_or_else(|error| panic!("{name}.ts est écrit : {error}"));
            let champs = decl_fields(&texte);
            let noms: std::collections::BTreeSet<&str> =
                champs.iter().map(|(nom, ..)| nom.as_str()).collect();
            let cles: std::collections::BTreeSet<&String> = value
                .as_object()
                .unwrap_or_else(|| panic!("la fixture {name} sérialise en objet"))
                .keys()
                .collect();
            for cle in &cles {
                assert!(
                    noms.contains(cle.as_str()),
                    "toute clé du JSON de {name} est propriété de la déclaration : {cle} dans {cles:?} vs {noms:?}"
                );
            }
            for (champ, optionnel, _) in &champs {
                assert!(
                    *optionnel || cles.contains(champ),
                    "aucune propriété obligatoire de {name} n'est en trop : {champ} absent de {cles:?}"
                );
            }
        }
        // Le JSON de la valeur d'exemple est accepté par la désérialisation de `lymaille`.
        let aller_retour: ::lymaille::AgentDefinition =
            serde_json::from_value(serde_json::to_value(&agent).expect("la fixture sérialise"))
                .expect("le JSON exportable se redésérialise en AgentDefinition");
        assert_eq!(aller_retour, agent);
        let aller_retour: ::lymaille::ModelProfile =
            serde_json::from_value(serde_json::to_value(&profil).expect("la fixture sérialise"))
                .expect("le JSON exportable se redésérialise en ModelProfile");
        assert_eq!(aller_retour, profil);
        let aller_retour: ::lymaille::ToolsetContext =
            serde_json::from_value(serde_json::to_value(&contexte).expect("la fixture sérialise"))
                .expect("le JSON exportable se redésérialise en ToolsetContext");
        assert_eq!(aller_retour, contexte);
        std::fs::remove_dir_all(&dir).expect("le répertoire de test se nettoie");
    }

    /// Propriétés de premier niveau de la première déclaration objet `export type … = { … };`
    /// d'un fichier exporté : `(nom, optionnel, type)`. Les commentaires de bloc sont retirés
    /// avant découpage (la documentation porte des virgules), accolades et chevrons protègent la
    /// profondeur. Partagé par les tests d'export des `Scenario` de `../definitions/*.sdd` et de
    /// `../http/routes.sdd` (issue #72).
    #[must_use]
    pub(crate) fn decl_fields(texte: &str) -> Vec<(String, bool, String)> {
        let sans_commentaires = retirer_commentaires(texte);
        let apres_type = sans_commentaires
            .find("export type")
            .expect("une déclaration export type");
        let debut = apres_type
            + sans_commentaires[apres_type..]
                .find('{')
                .expect("un objet déclaré");
        let mut profondeur = 1_i32;
        let mut chevron = 0_i32;
        let mut courant = String::new();
        let mut champs = Vec::new();
        for caractere in sans_commentaires[debut + 1..].chars() {
            match caractere {
                '{' => profondeur += 1,
                '}' => {
                    profondeur -= 1;
                    if profondeur == 0 {
                        if let Some(champ) = champ_de_propriete(&courant) {
                            champs.push(champ);
                        }
                        break;
                    }
                }
                '<' => chevron += 1,
                '>' => chevron -= 1,
                ',' if profondeur == 1 && chevron == 0 => {
                    if let Some(champ) = champ_de_propriete(&courant) {
                        champs.push(champ);
                    }
                    courant = String::new();
                    continue;
                }
                _ => {}
            }
            courant.push(caractere);
        }
        champs
    }

    /// Une propriété `nom?: type` découpée ; `None` pour un segment vide.
    fn champ_de_propriete(segment: &str) -> Option<(String, bool, String)> {
        let segment = segment.trim();
        if segment.is_empty() {
            return None;
        }
        let (nom, type_) = segment.split_once(':')?;
        let nom = nom.trim();
        let optionnel = nom.ends_with('?');
        Some((
            nom.trim_end_matches('?').to_owned(),
            optionnel,
            type_.trim().to_owned(),
        ))
    }

    /// Retire les commentaires `/* … */` et `// …` d'un texte TypeScript.
    fn retirer_commentaires(texte: &str) -> String {
        let mut sans = String::with_capacity(texte.len());
        let mut reste = texte;
        while let Some(index) = reste.find("/*") {
            let Some(fin) = reste[index..].find("*/") else {
                break;
            };
            sans.push_str(&reste[..index]);
            reste = &reste[index + fin + 2..];
        }
        for ligne in reste.lines() {
            if let Some(debut_commentaire) = ligne.find("//") {
                sans.push_str(&ligne[..debut_commentaire]);
            } else {
                sans.push_str(ligne);
            }
            sans.push('\n');
        }
        sans
    }

    /// Contrôle de cohérence des miroirs : `TurnEnded` et `RunEnded` portent le résultat amont
    /// en `snake_case`, `TokenUsage`/`Overrun`/`ResponseTruncated`/`McpReconnect`/
    /// `InferenceRetry`/`TitleChanged`/`ReviewRequested`/`TodoChanged`/`TurnStarted`/`RunEnded`
    /// se miroitent champ à champ (le Scenario « un événement de tour est typé » ne couvre pas
    /// la moitié des variantes typées).
    #[test]
    fn chaque_variante_refletee_se_miroite_champ_a_champ() {
        let cases: Vec<(EventKind, SessionEventKind)> = vec![
            (
                EventKind::TurnStarted { turn: TurnId::new(1) },
                SessionEventKind::TurnStarted { turn: 1 },
            ),
            (
                EventKind::TurnEnded {
                    turn: TurnId::new(2),
                    outcome: Outcome::Cancelled,
                    response: "stop".to_owned(),
                },
                SessionEventKind::TurnEnded {
                    turn: 2,
                    outcome: SessionOutcome::Cancelled,
                    response: "stop".to_owned(),
                },
            ),
            (
                EventKind::RunEnded {
                    run: RunId::new("run-7").expect("un texte non vide fait un identifiant de run"),
                    outcome: Outcome::Valid,
                },
                SessionEventKind::RunEnded {
                    run: "run-7".to_owned(),
                    outcome: SessionOutcome::Valid,
                },
            ),
            (
                EventKind::TokenUsage {
                    prompt: 120,
                    completion: 45,
                },
                SessionEventKind::TokenUsage {
                    prompt: 120,
                    completion: 45,
                },
            ),
            (EventKind::TodoChanged, SessionEventKind::TodoChanged),
            (
                EventKind::TitleChanged {
                    title: "Loki migration".to_owned(),
                },
                SessionEventKind::TitleChanged {
                    title: "Loki migration".to_owned(),
                },
            ),
            (
                EventKind::ReviewRequested {
                    reason: "wrong answer".to_owned(),
                },
                SessionEventKind::ReviewRequested {
                    reason: "wrong answer".to_owned(),
                },
            ),
            (
                EventKind::McpReconnect {
                    role: "monitoring".to_owned(),
                    attempt: 1,
                    delay_ms: 500,
                },
                SessionEventKind::McpReconnect {
                    role: "monitoring".to_owned(),
                    attempt: 1,
                    delay_ms: 500,
                },
            ),
            (
                EventKind::InferenceRetry {
                    attempt: 2,
                    delay_ms: 2000,
                    error_class: "timeout".to_owned(),
                },
                SessionEventKind::InferenceRetry {
                    attempt: 2,
                    delay_ms: 2000,
                    error_class: "timeout".to_owned(),
                },
            ),
            (
                EventKind::Overrun { elapsed_ms: 90_000 },
                SessionEventKind::Overrun { elapsed_ms: 90_000 },
            ),
            (
                EventKind::ResponseTruncated {
                    has_text: true,
                    has_tool_calls: false,
                },
                SessionEventKind::ResponseTruncated {
                    has_text: true,
                    has_tool_calls: false,
                },
            ),
            (
                EventKind::EmptyResponse { will_retry: true },
                SessionEventKind::EmptyResponse { will_retry: true },
            ),
        ];
        miroite_chacun(cases);
    }

    /// Contrôle de cohérence des miroirs (2/2) : les deux variantes amont connues non reflétées
    /// outre `MemoryChanged` (`TargetOpened`, `RunInterrupted`) tombent en `Other` avec leur
    /// nom de variante, contenu laissé derrière.
    #[test]
    fn les_autres_variants_connues_tombent_en_other() {
        let cases: Vec<(EventKind, SessionEventKind)> = vec![
            (
                EventKind::TargetOpened {
                    kind: "definition".to_owned(),
                    name: "agent/ctx".to_owned(),
                },
                SessionEventKind::Other {
                    name: "TargetOpened".to_owned(),
                },
            ),
            (
                EventKind::RunInterrupted {
                    run: RunId::new("run-7").expect("un texte non vide fait un identifiant de run"),
                    session: session("sub-7"),
                },
                SessionEventKind::Other {
                    name: "RunInterrupted".to_owned(),
                },
            ),
        ];
        miroite_chacun(cases);
    }

    /// Miroite chaque événement et vérifie le `kind` attendu et la session portee.
    fn miroite_chacun(cases: Vec<(EventKind, SessionEventKind)>) {
        for (upstream, expected) in cases {
            let mirrored = SessionEvent::from(&event("s1", upstream));
            assert_eq!(mirrored.kind, expected);
            assert_eq!(mirrored.session, "s1");
        }
    }
}
