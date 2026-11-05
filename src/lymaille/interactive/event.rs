//! Miroir typé des événements de session (`src/lymaille/interactive/event.sdd`).
//!
//! Contrat de fil (DSD `FR-15`, `DEC-30`) : un enum `SessionEvent` diffusé par le hub
//! (./hub.sdd), sérialisé par le flux SSE (../http/sse.sdd), rendu en texte par le pont Matrix
//! (../chat/render.sdd) et exporté vers TypeScript par [`export_typescript`] pour remplacer un
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
/// `type` en `snake_case`. Les variantes non reflétées de l'enum amont tombent dans [`Other`] ;
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
    /// Une réponse vide a été écartée (`EventKind::EmptyResponse`, `lymaille` 0.2.0) :
    /// `will_retry` dit si le modèle est relancé, aucun contenu.
    EmptyResponse {
        /// Le modèle est relancé après la réponse vide.
        will_retry: bool,
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

/// Écrit dans `dir` un fichier `.ts` par type exporté (`SessionEvent`, `SessionEventKind`,
/// `SessionOutcome`) et rend les chemins écrits ; `dir` est créé au besoin. L'export TypeScript
/// se fait **côté application** (arbitré le 2026-11-03) : la crate ne commite ni ne publie
/// aucun fichier `.ts`, les types ne portent pas `#[ts(export)]` et `cargo test` n'en écrit
/// jamais.
///
/// # Errors
///
/// [`LymailleError::Store`] (`MRD-LYMAILLE-009`) : panne d'entrée-sortie (création du répertoire,
/// écriture d'un fichier) ou panne de génération ; le texte rendu ne cite aucun chemin hors de
/// `dir`.
pub fn export_typescript(dir: &Path) -> Result<Vec<PathBuf>, LymailleError> {
    std::fs::create_dir_all(dir)
        .map_err(|error| LymailleError::Store(format!("export directory unavailable: {error}")))?;
    let session_event = export_one::<SessionEvent>(dir, "SessionEvent")?;
    let session_event_kind = export_one::<SessionEventKind>(dir, "SessionEventKind")?;
    let session_outcome = export_one::<SessionOutcome>(dir, "SessionOutcome")?;
    Ok(vec![session_event, session_event_kind, session_outcome])
}

/// Génère le texte TypeScript d'un type et l'écrit dans `dir/{name}.ts` ; rend le chemin écrit.
fn export_one<T>(dir: &Path, name: &str) -> Result<PathBuf, LymailleError>
where
    T: TS + 'static,
{
    let text = T::export_to_string(&ts_rs::Config::new())
        .map_err(|error| LymailleError::Store(format!("typescript generation failed: {error}")))?;
    let path = dir.join(format!("{name}.ts"));
    std::fs::write(&path, text)
        .map_err(|error| LymailleError::Store(format!("typescript export write failed: {error}")))?;
    Ok(path)
}

/// Variante amont vers sa variante typée : les 15 variantes reflétées champ à champ, toute autre
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
mod tests {
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
    /// dev-dependencies de la crate).
    fn scratch_dir(tag: &str) -> std::path::PathBuf {
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
        assert_eq!(
            written,
            vec![
                dir.join("SessionEvent.ts"),
                dir.join("SessionEventKind.ts"),
                dir.join("SessionOutcome.ts"),
            ]
        );
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
