//! Enveloppes JSON-RPC 2.0 de la surface MCP — forme propre à miryad-core.
//!
//! `vynil-core`, l'amont de la feature `mcp`, n'expose aucun type JSON-RPC/MCP (ses modules
//! utilitaires seulement) : ce fichier est l'intégralité du protocole de la crate. La lecture
//! du corps entrant passe par `parse_request` seul — la requête n'est jamais désérialisée
//! directement — et les réponses se construisent par `JsonRpcResponse::{ok, err,
//! err_with_data}`, qui posent le littéral `"2.0"` et un `id` toujours présent (`null` pour
//! les erreurs à id inconnu). Les codes numériques et la taxonomie `MRD-*` vivent dans
//! `./error.rs`, le pont HTTP et l'assemblage des réponses dans `./handler.rs` : ce fichier
//! n'importe que `serde` et `serde_json`, ne trace rien et ne fait aucune I/O. La version MCP
//! annoncée par `initialize` est `PROTOCOL_VERSION`, déclarée ici ; sa négociation (le serveur
//! annonce la sienne, le client décide) appartient au handler.

use serde::Serialize;
use serde_json::Value;

/// Version du protocole MCP annoncée par `initialize` — le serveur annonce toujours la
/// sienne, le client décide de se déconnecter sinon (arbitré 2026-09-29).
pub(crate) const PROTOCOL_VERSION: &str = "2024-11-05";

/// Requête JSON-RPC appelant une réponse — construite par `parse_request` seul, jamais
/// désérialisée directement (`jsonrpc` est validé à la lecture, pas porté ici).
#[derive(Debug, PartialEq)]
pub(crate) struct JsonRpcRequest {
    /// Identifiant de la requête : chaîne ou nombre JSON, jamais `null` (`null` est
    /// une enveloppe invalide, plus une notification — arbitré 2026-09-29).
    pub id: Value,
    /// Nom de la méthode — chaîne JSON, obligatoire à la lecture.
    pub method: String,
    /// `params` brut : absent et `null` JSON sont confondus en `Value::Null`.
    pub params: Value,
}

/// Issue de la lecture d'un corps par `parse_request`.
#[derive(Debug, PartialEq)]
pub(crate) enum ParsedRequest {
    /// Requête avec `id` — une réponse lui est due.
    Call(JsonRpcRequest),
    /// Clé `id` absente — aucune réponse n'est due (le `202` sans enveloppe appartient
    /// au handler).
    Notification,
}

/// Rejet de l'enveloppe JSON-RPC — sans code ni message : la traduction en
/// `McpError::ParseError`/`McpError::InvalidRequest` (`-32700`/`-32600`, HTTP `200`,
/// enveloppe à `id` `null`) appartient au handler et à `./error.rs` (arbitré 2026-09-29).
#[derive(Debug, PartialEq)]
pub(crate) enum ProtocolRejection {
    /// Le corps n'est pas du JSON valide (ou pas de l'UTF-8).
    Parse,
    /// JSON valide mais enveloppe invalide : racine non objet (batch inclus), `jsonrpc`
    /// absent ou autre que `"2.0"`, `method` absente ou non textuelle, `id` présent
    /// valant `null` ou autre chose qu'une chaîne ou qu'un nombre.
    InvalidRequest,
}

/// Lit le corps de `POST /mcp` en octets — point d'entrée unique de la lecture, pure
/// (`&[u8]` vers un `Result`), sans I/O ni trace. L'ordre des règles fait le rejet :
/// (1) JSON invalide → `Parse` ; (2) racine non objet (tableau — batch non supporté —,
/// scalaire) → `InvalidRequest` ; (3) `jsonrpc` absent ou différent de `"2.0"` →
/// `InvalidRequest` ; (4) `method` absente ou non chaîne → `InvalidRequest` ; (5) clé
/// `id` présente valant `null` ou autre chose qu'une chaîne ou qu'un nombre →
/// `InvalidRequest` ; (6) clé `id` absente → `Notification` ; sinon `Call`. Les clés
/// inconnues sont ignorées.
pub(crate) fn parse_request(body: &[u8]) -> Result<ParsedRequest, ProtocolRejection> {
    let Ok(root) = serde_json::from_slice::<Value>(body) else {
        return Err(ProtocolRejection::Parse);
    };
    let Some(object) = root.as_object() else {
        return Err(ProtocolRejection::InvalidRequest);
    };
    if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(ProtocolRejection::InvalidRequest);
    }
    let Some(method) = object.get("method").and_then(Value::as_str) else {
        return Err(ProtocolRejection::InvalidRequest);
    };
    let id = match object.get("id") {
        Some(id @ (Value::String(_) | Value::Number(_))) => id.clone(),
        Some(_) => return Err(ProtocolRejection::InvalidRequest),
        None => return Ok(ParsedRequest::Notification),
    };
    Ok(ParsedRequest::Call(JsonRpcRequest {
        id,
        method: method.to_owned(),
        params: object.get("params").cloned().unwrap_or(Value::Null),
    }))
}

/// Enveloppe de réponse JSON-RPC du `POST /mcp` — construite par les constructeurs ci
/// dessous, qui tiennent l'exclusivité `result`/`error` et le littéral `"2.0"`. La
/// sérialisation (`axum::Json` dans le handler) n'émet jamais les deux à la fois ; les
/// champs restent `pub` sous le `pub(crate)` du struct.
#[derive(Serialize)]
pub(crate) struct JsonRpcResponse {
    /// Toujours présent, toujours `"2.0"` — littéral codé en dur dans les deux
    /// constructeurs qui construisent vraiment.
    pub jsonrpc: &'static str,
    /// `id` toujours sérialisé (JSON-RPC 2.0 l'exige dans toute réponse) : écho de la
    /// requête, ou `Value::Null` pour les erreurs de parse et de requête invalide dont
    /// l'id est inconnu (arbitré 2026-09-29).
    pub id: Value,
    /// Charge utile de succès : `None` supprime la clé, `Some(Value::Null)` la rend
    /// présente valant JSON `null`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// Charge utile d'échec : `None` supprime la clé.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

/// Contenu de la clé `error` — `code` et `message` sont les deux seuls champs toujours
/// émis ; `data` disparaît quand il vaut `None`.
#[derive(Serialize)]
pub(crate) struct RpcError {
    /// Code numérique JSON-RPC — transporté sans validation de plage ni filtrage : la
    /// plage réservée `-32700..-32600` documentée par `./error.rs` n'est imposée nulle
    /// part ici.
    pub code: i32,
    /// Message verbatim — aucun préfixe, re-codage ni troncature ajoutés par ce type.
    pub message: String,
    /// `data` optionnel que le type borne sans l'interpréter (le `code` libre du
    /// `HookError` y transite via `McpError::data`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl JsonRpcResponse {
    /// Enveloppe de succès : `result` posé `Some`, `error` absent.
    pub(crate) fn ok(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: Some(result),
            error: None,
        }
    }

    /// Enveloppe d'erreur sans `data` — délègue à `err_with_data` avec `None` : la clé
    /// `data` est absente de la réponse, jamais `null`.
    pub(crate) fn err(id: Value, code: i32, message: impl Into<String>) -> Self {
        Self::err_with_data(id, code, message, None)
    }

    /// Enveloppe d'erreur avec `data` optionnel transporté verbatim ; `id` inconnu se
    /// passe en `Value::Null`.
    pub(crate) fn err_with_data(
        id: Value,
        code: i32,
        message: impl Into<String>,
        data: Option<Value>,
    ) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(RpcError {
                code,
                message: message.into(),
                data,
            }),
        }
    }
}

/// Résultat typé de `initialize` (arbitré 2026-09-29, jusque-là un `json!` anonyme du
/// handler) — construits par `./handler.rs`. Les clés wire camelCase
/// (`protocolVersion`, `serverInfo`) portent un `rename` de payload : les clés de
/// l'enveloppe JSON-RPC, elles, ne sont jamais renommées.
#[derive(Serialize)]
pub(crate) struct InitializeResult {
    /// Version supportée par le serveur — `PROTOCOL_VERSION`, sans lecture des `params`
    /// du client.
    #[serde(rename = "protocolVersion")]
    pub protocol_version: &'static str,
    /// Capacités du serveur — exactement `{"tools":{}}` sous le contrat actuel.
    pub capabilities: Value,
    /// `{"name": ..., "version": ...}` — nom de la crate et `CARGO_PKG_VERSION` du build.
    #[serde(rename = "serverInfo")]
    pub server_info: Value,
}

/// Déclaration d'un tool pour `tools/list` (arbitré 2026-09-29, jusque-là des `json!`
/// anonymes du handler) — construits par `./handler.rs`.
#[derive(Serialize)]
pub(crate) struct ToolDeclaration {
    /// Nom complet `{ressource}{suffixe}` tel que servi.
    pub name: String,
    /// Description littérale du tool.
    pub description: String,
    /// Schéma `JSON` des arguments acceptés.
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(body: &str) -> Result<ParsedRequest, ProtocolRejection> {
        parse_request(body.as_bytes())
    }

    fn wire(response: &JsonRpcResponse) -> Value {
        serde_json::to_value(response).expect("l'enveloppe est toujours sérialisable")
    }

    /// Clés wire interprétées par `serde_json`, triées (comparaison d'ensemble de clés,
    /// jamais d'octets — contrat de la spec).
    fn keys(value: &Value) -> Vec<&str> {
        let object = value.as_object().expect("objet JSON");
        let mut names: Vec<&str> = object.keys().map(String::as_str).collect();
        names.sort_unstable();
        names
    }

    fn error_keys(value: &Value) -> Vec<&str> {
        keys(&value["error"])
    }

    // ——— Lecture par parse_request ———

    /// `Scenario` : « Requête complète acceptée ».
    #[test]
    fn requete_complete_acceptee() {
        let parsed = parse(r#"{"jsonrpc":"2.0","id":7,"method":"tools/list","params":{"cursor":"x"}}"#)
            .expect("requête valide");
        assert_eq!(
            parsed,
            ParsedRequest::Call(JsonRpcRequest {
                id: json!(7),
                method: "tools/list".to_string(),
                params: json!({ "cursor": "x" }),
            })
        );
    }

    /// `Scenario` : « params absent ou null confondus » — les deux corps rendent la même
    /// valeur, sans distinction possible.
    #[test]
    fn params_absent_ou_null_confondus() {
        let attendu = ParsedRequest::Call(JsonRpcRequest {
            id: json!(1),
            method: "ping".to_string(),
            params: Value::Null,
        });
        assert_eq!(
            parse(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#).expect("params absent"),
            attendu
        );
        assert_eq!(
            parse(r#"{"jsonrpc":"2.0","id":1,"method":"ping","params":null}"#).expect("params null"),
            attendu
        );
    }

    /// `Scenario` : « id absent est une notification ».
    #[test]
    fn id_absent_est_une_notification() {
        assert_eq!(
            parse(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#),
            Ok(ParsedRequest::Notification)
        );
    }

    /// `Scenario` : « id null explicite est une requête invalide » — plus jamais une
    /// notification (arbitré 2026-09-29).
    #[test]
    fn id_null_explicite_est_une_requete_invalide() {
        assert_eq!(
            parse(r#"{"jsonrpc":"2.0","method":"x","id":null}"#),
            Err(ProtocolRejection::InvalidRequest)
        );
    }

    /// `Scenario` : « id structuré ou booléen refusé » — seuls chaîne et nombre sont admis.
    #[test]
    fn id_structure_ou_booleen_refuse() {
        assert_eq!(
            parse(r#"{"jsonrpc":"2.0","id":{"k":[1]},"method":"x"}"#),
            Err(ProtocolRejection::InvalidRequest)
        );
        assert_eq!(
            parse(r#"{"jsonrpc":"2.0","id":true,"method":"x"}"#),
            Err(ProtocolRejection::InvalidRequest)
        );
    }

    /// `Scenario` : « id chaîne accepté verbatim ».
    #[test]
    fn id_chaine_acceptee_verbatim() {
        assert_eq!(
            parse(r#"{"jsonrpc":"2.0","id":"req-1","method":"x"}"#),
            Ok(ParsedRequest::Call(JsonRpcRequest {
                id: json!("req-1"),
                method: "x".to_string(),
                params: Value::Null,
            }))
        );
    }

    /// `Scenario` : « version jsonrpc autre que 2.0 refusée » — `1.0` comme l'absence
    /// rendent `InvalidRequest`, plus jamais de tolérance.
    #[test]
    fn version_jsonrpc_autre_que_2_0_refusee() {
        assert_eq!(
            parse(r#"{"jsonrpc":"1.0","id":1,"method":"x"}"#),
            Err(ProtocolRejection::InvalidRequest)
        );
        assert_eq!(
            parse(r#"{"id":1,"method":"x"}"#),
            Err(ProtocolRejection::InvalidRequest)
        );
    }

    /// `Scenario` : « method absent ou non textuel refusé ».
    #[test]
    fn method_absente_ou_non_textuelle_refusee() {
        assert_eq!(
            parse(r#"{"jsonrpc":"2.0","id":1}"#),
            Err(ProtocolRejection::InvalidRequest)
        );
        assert_eq!(
            parse(r#"{"jsonrpc":"2.0","id":1,"method":5}"#),
            Err(ProtocolRejection::InvalidRequest)
        );
    }

    /// `Scenario` : « JSON invalide rend Parse » — tronqué et non JSON.
    #[test]
    fn json_invalide_rend_parse() {
        assert_eq!(parse(r#"{"jsonrpc":"#), Err(ProtocolRejection::Parse));
        assert_eq!(parse("not json"), Err(ProtocolRejection::Parse));
    }

    /// `Scenario` : « batch et racine non objet refusés » — pas de support de batch
    /// (arbitré 2026-09-29).
    #[test]
    fn batch_et_racine_non_objet_refuses() {
        assert_eq!(
            parse(r#"[{"jsonrpc":"2.0","id":1,"method":"x"}]"#),
            Err(ProtocolRejection::InvalidRequest)
        );
        assert_eq!(parse("42"), Err(ProtocolRejection::InvalidRequest));
    }

    /// `Scenario` : « clés inconnues ignorées » — les trois champs du `Call` sont
    /// peuplés, `shard` et `telemetry` disparaissent sans trace.
    #[test]
    fn cles_inconnues_ignorees() {
        assert_eq!(
            parse(r#"{"jsonrpc":"2.0","id":1,"method":"x","shard":5,"telemetry":true}"#),
            Ok(ParsedRequest::Call(JsonRpcRequest {
                id: json!(1),
                method: "x".to_string(),
                params: Value::Null,
            }))
        );
    }

    // ——— Formes wire des enveloppes (serde_json interprété) ———

    /// `Scenario` : « Enveloppe de succès sérialisée » — exactement trois clés, aucune
    /// clé `error`.
    #[test]
    fn enveloppe_de_succes_serialisee() {
        let value = wire(&JsonRpcResponse::ok(json!(7), json!({ "ok": true })));
        assert_eq!(keys(&value), ["id", "jsonrpc", "result"]);
        assert_eq!(value["jsonrpc"], "2.0");
        assert_eq!(value["id"], json!(7));
        assert_eq!(value["result"], json!({ "ok": true }));
        assert!(
            !value.as_object().expect("objet").contains_key("error"),
            "aucune clé error : {value}"
        );
    }

    /// `Scenario` : « result null rendu explicitement » — le skip ne porte que sur
    /// `None` ; `error` reste absente.
    #[test]
    fn result_null_rendu_explicitement() {
        let value = wire(&JsonRpcResponse::ok(json!(7), Value::Null));
        let object = value.as_object().expect("objet");
        assert!(object.contains_key("result"), "la clé result est présente");
        assert_eq!(value["result"], Value::Null);
        assert!(!object.contains_key("error"), "error reste absente");
    }

    /// `Scenario` : « Enveloppe d'erreur sans result ni data » — forme exacte, pas de
    /// clé `result`, pas de clé `data`.
    #[test]
    fn enveloppe_erreur_sans_result_ni_data() {
        let value = wire(&JsonRpcResponse::err(json!(7), -32601, "Method not found: x"));
        assert_eq!(
            value,
            json!({"jsonrpc":"2.0","id":7,"error":{"code":-32601,"message":"Method not found: x"}})
        );
        assert!(!value.as_object().expect("objet").contains_key("result"));
        assert!(
            !value["error"]
                .as_object()
                .expect("objet error")
                .contains_key("data")
        );
    }

    /// `Scenario` : « data transportée verbatim par `err_with_data` » — et `err` sur les
    /// mêmes `id`/`code`/`message` produit la même enveloppe amputée de la clé `data`,
    /// preuve de la délégation.
    #[test]
    fn data_transportee_verbatim_par_err_with_data() {
        let avec = wire(&JsonRpcResponse::err_with_data(
            json!(1),
            -32000,
            "boom",
            Some(json!({ "code": "WIDGET-001" })),
        ));
        assert_eq!(
            avec,
            json!({"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"boom","data":{"code":"WIDGET-001"}}})
        );
        let sans = wire(&JsonRpcResponse::err(json!(1), -32000, "boom"));
        assert_eq!(error_keys(&sans), ["code", "message"], "aucune clé data");
        let mut ampute = avec["error"].clone();
        ampute.as_object_mut().expect("objet error").remove("data");
        assert_eq!(ampute, sans["error"], "err délègue avec data absent");
    }

    /// `Scenario` : « Enveloppe d'erreur à id null » — la clé `id` est présente valant
    /// `null` (JSON-RPC exige `id` dans toute réponse, arbitré 2026-09-29).
    #[test]
    fn enveloppe_erreur_a_id_null() {
        let value = wire(&JsonRpcResponse::err(Value::Null, -32700, "Parse error"));
        assert_eq!(
            value,
            json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"Parse error"}})
        );
        assert!(
            value.as_object().expect("objet").contains_key("id"),
            "id est présent, pas omis"
        );
    }

    /// `Scenario` : « Code libre transporté sans filtre » — hors plage réservée, aucun
    /// refus ni repli sur un code standard.
    #[test]
    fn code_libre_transporté_sans_filtre() {
        let value = wire(&JsonRpcResponse::err(json!(1), -31337, "x"));
        assert_eq!(value["error"]["code"], -31337);
    }

    // ——— Types de résultat typés (arbitré 2026-09-29) ———

    /// Verrou de tâche hors `Scenario` : `PROTOCOL_VERSION` vaut `2024-11-05` et les
    /// formes wire de `InitializeResult` (`protocolVersion`, `capabilities`,
    /// `serverInfo`) et `ToolDeclaration` (`name`, `description`, `inputSchema`) sont
    /// exactes, affirmées par `serde_json` interprété.
    #[test]
    fn initialize_result_et_tool_declaration_portent_leurs_cles_wire() {
        assert_eq!(PROTOCOL_VERSION, "2024-11-05");
        let init = serde_json::to_value(InitializeResult {
            protocol_version: PROTOCOL_VERSION,
            capabilities: json!({ "tools": {} }),
            server_info: json!({ "name": "miryad-core", "version": "0.0.0-test" }),
        })
        .expect("InitializeResult est sérialisable");
        assert_eq!(keys(&init), ["capabilities", "protocolVersion", "serverInfo"]);
        assert_eq!(init["protocolVersion"], "2024-11-05");
        assert_eq!(init["capabilities"], json!({ "tools": {} }));
        let tool = serde_json::to_value(ToolDeclaration {
            name: "recipes_list".to_string(),
            description: "List recipes, paginated".to_string(),
            input_schema: json!({ "type": "object" }),
        })
        .expect("ToolDeclaration est sérialisable");
        assert_eq!(keys(&tool), ["description", "inputSchema", "name"]);
        assert_eq!(tool["inputSchema"], json!({ "type": "object" }));
    }
}
