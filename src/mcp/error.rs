use crate::resource::HookError;
use crate::rest::error::RestError;

/// Erreur de la surface MCP — chaque variante porte, outre sa `Display` préfixée d'un
/// code unique `MRD-MCP-NNN`, le code numérique JSON-RPC 2.0 rendu par `rpc_code` et,
/// pour le seul rejet de hook, le code libre du `HookError` dans le champ `data`. Le
/// message envoyé au client est `wire_message` : générique pour les `-32603` (le détail
/// reste dans la `Display`, qui nourrit la trace `error!` du handler — arbitré
/// 2026-09-29), exact pour les autres. Le fichier ne construit lui-même aucune erreur :
/// les variantes naissent dans le registre MCP, le rendu de sortie, le dispatch du
/// handler, ou la conversion `From<RestError>`.
#[derive(Debug, thiserror::Error)]
pub enum McpError {
    /// `MRD-MCP-001` — refus RBAC, miroir du `403` REST, code JSON-RPC `-32001`.
    #[error("MRD-MCP-001: forbidden")]
    Forbidden,
    /// `MRD-MCP-002` — ressource absente, miroir du `404` REST, code JSON-RPC `-32002`.
    #[error("MRD-MCP-002: resource not found")]
    NotFound,
    /// `MRD-MCP-003` — `DbErr` de `sea-orm` propagé (`#[from]`, seul `From` dérivé),
    /// code JSON-RPC `-32603`.
    #[error("MRD-MCP-003: database error: {0}")]
    Database(#[from] sea_orm::DbErr),
    /// `MRD-MCP-004` — échec de mise en forme de la sortie (rendu `Handlebars` ou
    /// sérialisation `serde_json`), propre à MCP, code JSON-RPC `-32603`. Le message
    /// couvre les deux sources (arbitré 2026-09-29).
    #[error("MRD-MCP-004: output rendering error: {0}")]
    Render(String),
    /// `MRD-MCP-005` — désérialisation ratée des arguments d'un tool (message `serde`
    /// verbatim), code JSON-RPC `-32602`.
    #[error("MRD-MCP-005: invalid params: {0}")]
    InvalidParams(String),
    /// `MRD-MCP-006` — nom d'outil inconnu du dispatch, code JSON-RPC `-32601`.
    #[error("MRD-MCP-006: unknown tool: {0}")]
    UnknownTool(String),
    /// Miroir de `RestError::Internal` — cf. le commentaire de ce variant pour le contexte
    /// (conversion explicite d'`AuthError`, pas de `From` générique trompeur).
    #[error("MRD-MCP-007: internal error: {0}")]
    Internal(String),
    /// Erreur métier applicative (hook) — jamais un `MRD-*`, cf. `HookError`.
    #[error("{}", .0.message)]
    Application(HookError),
    /// `MRD-MCP-008` — méthode JSON-RPC hors des trois connues du dispatch
    /// (`initialize`, `tools/list`, `tools/call`), code JSON-RPC `-32601` (arbitré
    /// 2026-09-29).
    #[error("MRD-MCP-008: method not found: {0}")]
    MethodNotFound(String),
    /// `MRD-MCP-009` — corps non JSON ou UTF-8 invalide (rejet `Parse` de
    /// `parse_request`), code JSON-RPC `-32700` (arbitré 2026-09-29).
    #[error("MRD-MCP-009: parse error")]
    ParseError,
    /// `MRD-MCP-010` — enveloppe JSON-RPC invalide : racine non objet, batch, `jsonrpc`
    /// autre que `2.0`, `method` absente ou non textuelle, `id` nul ou structuré
    /// (rejet `InvalidRequest` de `parse_request`), code JSON-RPC `-32600` (arbitré
    /// 2026-09-29).
    #[error("MRD-MCP-010: invalid request")]
    InvalidRequest,
}

impl McpError {
    /// Code JSON-RPC 2.0 — -32700..-32600 sont réservés au protocole, -32000..-32099 est la
    /// plage libre pour l'application (spec JSON-RPC 2.0). Match exhaustif nommé sur les
    /// onze variantes : une variante ajoutée interrompt la compilation avant qu'une
    /// enveloppe ne parte par défaut.
    pub(crate) fn rpc_code(&self) -> i32 {
        match self {
            McpError::Application(_) => -32000,
            McpError::Forbidden => -32001,
            McpError::NotFound => -32002,
            McpError::UnknownTool(_) | McpError::MethodNotFound(_) => -32601, // "Method not found", standard JSON-RPC
            McpError::InvalidRequest => -32600, // "Invalid request", standard JSON-RPC
            McpError::InvalidParams(_) => -32602, // "Invalid params", standard JSON-RPC
            McpError::ParseError => -32700,     // "Parse error", standard JSON-RPC
            McpError::Database(_) | McpError::Render(_) | McpError::Internal(_) => -32603, // "Internal error", standard JSON-RPC
        }
    }

    /// Message envoyé au client (arbitré par Sébastien le 2026-09-29) : les `-32603`
    /// deviennent génériques, sans charge utile — le détail (message `DbErr`, texte de
    /// moteur) reste dans la `Display`, que la trace `error!` du handler porte sur le
    /// fil du log, jamais sur le fil du client. Toutes les autres variantes, y compris
    /// `InvalidParams` (retour sur l'entrée du client, pas un interne du serveur),
    /// sortent sur leur `Display` exacte.
    pub(crate) fn wire_message(&self) -> String {
        match self {
            McpError::Database(_) => "MRD-MCP-003: database error".to_string(),
            McpError::Render(_) => "MRD-MCP-004: output rendering error".to_string(),
            McpError::Internal(_) => "MRD-MCP-007: internal error".to_string(),
            _ => self.to_string(),
        }
    }

    /// Code d'erreur applicatif libre (`HookError::code`), porté dans le champ JSON-RPC `data` —
    /// jamais mélangé au code numérique JSON-RPC lui-même, qui reste dans la plage standard.
    /// Seul `Application` porte un `data` : l'objet à l'unique clé `code`, la clé valant `null`
    /// quand le hook n'a pas de code (parité avec le `422` REST, arbitré 2026-09-29).
    pub(crate) fn data(&self) -> Option<serde_json::Value> {
        match self {
            McpError::Application(err) => Some(serde_json::json!({ "code": &err.code })),
            _ => None,
        }
    }
}

impl From<RestError> for McpError {
    fn from(err: RestError) -> Self {
        match err {
            RestError::NotFound => McpError::NotFound,
            RestError::Forbidden => McpError::Forbidden,
            RestError::Database(e) => McpError::Database(e),
            RestError::Application(e) => McpError::Application(e),
            RestError::Internal(msg) => McpError::Internal(msg),
            // Arbitré 2026-09-29 (`mcp/error.sdd`) : l'entrée invalide de la crate est une
            // entrée invalide du client en JSON-RPC — charge utile `String` déplacée intacte.
            RestError::InvalidInput(msg) => McpError::InvalidParams(msg),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::error::Error as _;

    /// Le triple que le handler passe à `JsonRpcResponse::err_with_data` :
    /// (`rpc_code`, `Display`, `data`) — l'affirmation des `Scenario`.
    fn triple(err: &McpError) -> (i32, String, Option<serde_json::Value>) {
        (err.rpc_code(), err.to_string(), err.data())
    }

    /// `DbErr::Custom` de texte `boom` — charge utile de référence des `Scenario`
    /// (préfixe `Custom Error: ` de sea-orm 2.0.2).
    fn database_boom() -> McpError {
        McpError::Database(sea_orm::DbErr::Custom("boom".to_string()))
    }

    // ——— Triples des variantes construites directement ———

    /// `Scenario` : « `McpError::Forbidden` rend le triple `-32001` ».
    #[test]
    fn forbidden_rend_le_triple_moins_32001() {
        assert_eq!(
            triple(&McpError::Forbidden),
            (-32001, "MRD-MCP-001: forbidden".to_string(), None)
        );
    }

    /// `Scenario` : « `McpError::NotFound` rend le triple `-32002` ».
    #[test]
    fn not_found_rend_le_triple_moins_32002() {
        assert_eq!(
            triple(&McpError::NotFound),
            (-32002, "MRD-MCP-002: resource not found".to_string(), None)
        );
    }

    /// `Scenario` : « `McpError::Database` rend `-32603` avec le `DbErr` verbatim » —
    /// la `Display` garde le détail (elle nourrit la trace), `data` reste absent.
    #[test]
    fn database_rend_moins_32603_avec_le_dberr_verbatim() {
        assert_eq!(
            triple(&database_boom()),
            (
                -32603,
                "MRD-MCP-003: database error: Custom Error: boom".to_string(),
                None
            )
        );
    }

    /// `Scenario` : « `McpError::Render` rend `-32603` avec son préfixe propre » —
    /// message « output rendering error » couvrant rendu Handlebars et
    /// `serde_json::to_value` (arbitré 2026-09-29).
    #[test]
    fn render_rend_moins_32603_avec_son_prefixe_propre() {
        assert_eq!(
            triple(&McpError::Render("bad template".to_string())),
            (
                -32603,
                "MRD-MCP-004: output rendering error: bad template".to_string(),
                None
            )
        );
    }

    /// `Scenario` : « `McpError::InvalidParams` rend `-32602` et accepte une charge
    /// vide » — message `serde` verbatim, charge vide = préfixe seul.
    #[test]
    fn invalid_params_rend_moins_32602_et_accepte_une_charge_vide() {
        assert_eq!(
            triple(&McpError::InvalidParams(
                "invalid type: integer -1, expected u64".to_string()
            )),
            (
                -32602,
                "MRD-MCP-005: invalid params: invalid type: integer -1, expected u64".to_string(),
                None
            )
        );
        assert_eq!(
            McpError::InvalidParams(String::new()).to_string(),
            "MRD-MCP-005: invalid params: "
        );
    }

    /// `Scenario` : « `McpError::UnknownTool` rend `-32601` quand il est construit » —
    /// variante posée par le dispatch depuis le 2026-09-29.
    #[test]
    fn unknown_tool_rend_moins_32601_quand_il_est_construit() {
        assert_eq!(
            triple(&McpError::UnknownTool("ghost_get".to_string())),
            (-32601, "MRD-MCP-006: unknown tool: ghost_get".to_string(), None)
        );
    }

    /// `Scenario` : « `McpError::Internal` rend `-32603` avec sa charge verbatim » —
    /// miroir défensif, sa `Display` porte la charge utile (le fil, lui, est générique).
    #[test]
    fn internal_rend_moins_32603_avec_sa_charge_verbatim() {
        assert_eq!(
            triple(&McpError::Internal("upstream exploded".to_string())),
            (
                -32603,
                "MRD-MCP-007: internal error: upstream exploded".to_string(),
                None
            )
        );
    }

    /// `Scenario` : « hook avec code : `-32000`, message nu, `code` dans `data` » —
    /// restitution du `HookError` sans taxonomie, `data` à l'unique clé `code`.
    #[test]
    fn hook_avec_code_rend_moins_32000_message_nu_et_code_dans_data() {
        let (code, message, data) = triple(&McpError::Application(HookError::with_code(
            "WIDGET-001",
            "label must not be empty",
        )));
        assert_eq!(code, -32000);
        assert_eq!(message, "label must not be empty");
        assert!(
            !message.contains("MRD-") && !message.contains("WIDGET-001"),
            "message nu : sans code, sans préfixe, sans MRD- : {message}"
        );
        let data = data.expect("Application porte toujours data (arbitré 2026-09-29)");
        let object = data.as_object().expect("data est un objet serde_json");
        assert_eq!(object.len(), 1, "l'unique clé est code");
        assert_eq!(object["code"], "WIDGET-001");
    }

    /// `Scenario` : « hook sans code : `-32000` et `data` `{"code":null}` » — la clé
    /// `code` est présente valant `null`, parité avec le `422` REST (arbitré 2026-09-29).
    #[test]
    fn hook_sans_code_rend_moins_32000_et_data_code_null() {
        let (code, message, data) = triple(&McpError::Application(HookError::new("boom")));
        assert_eq!((code, message), (-32000, "boom".to_string()));
        let data = data.expect("data vaut toujours {{\"code\": ...}}, code absent compris");
        let object = data.as_object().expect("data est un objet serde_json");
        assert!(object.contains_key("code"), "la clé code est présente");
        assert_eq!(object.len(), 1, "l'unique clé est code");
        assert_eq!(object["code"], serde_json::Value::Null);
    }

    // ——— Conversions ———

    /// `Scenario` : « la conversion dérivée `From<sea_orm::DbErr>` vaut la même monnaie
    /// que le chemin `RestError` » — seule conversion dérivée, disponible pour l'app.
    #[test]
    fn from_dberr_derive_vaut_la_meme_monnaie_que_le_chemin_rest() {
        let direct: McpError = sea_orm::DbErr::Custom("connection lost".to_string()).into();
        assert!(
            matches!(direct, McpError::Database(_)),
            "la conversion dérivée vaut Database : {direct:?}"
        );
        assert_eq!(
            direct.to_string(),
            "MRD-MCP-003: database error: Custom Error: connection lost"
        );
        assert_eq!(direct.rpc_code(), -32603);
        let via_rest: McpError =
            RestError::Database(sea_orm::DbErr::Custom("connection lost".to_string())).into();
        assert_eq!(direct.rpc_code(), via_rest.rpc_code());
        let source = direct.source().expect("l'erreur d'origine reste source");
        assert_eq!(
            source
                .downcast_ref::<sea_orm::DbErr>()
                .expect("la source est le DbErr embarqué")
                .to_string(),
            "Custom Error: connection lost"
        );
    }

    /// `Scenario` : « `From` `RestError::NotFound` renumérote en `MRD-MCP-002` » —
    /// le suffixe REST `001` ne transparaît nulle part (croisement Forbidden/NotFound).
    #[test]
    fn from_rest_not_found_renumero_en_mcp_002() {
        let err: McpError = RestError::NotFound.into();
        assert!(matches!(err, McpError::NotFound), "{err:?}");
        assert_eq!(
            triple(&err),
            (-32002, "MRD-MCP-002: resource not found".to_string(), None)
        );
        assert!(
            !err.to_string().contains("MRD-REST"),
            "aucune trace du suffixe REST"
        );
    }

    /// `Scenario` : « `From` `RestError::Forbidden` renumérote en `MRD-MCP-001` » —
    /// le suffixe REST `002` n'est pas reconduit.
    #[test]
    fn from_rest_forbidden_renumero_en_mcp_001() {
        let err: McpError = RestError::Forbidden.into();
        assert!(matches!(err, McpError::Forbidden), "{err:?}");
        assert_eq!(triple(&err), (-32001, "MRD-MCP-001: forbidden".to_string(), None));
        assert!(
            !err.to_string().contains("MRD-REST"),
            "aucune trace du suffixe REST"
        );
    }

    /// `Scenario` : « `From` `RestError::Database` préserve le `sea_orm::DbErr` intact »
    /// — seul le préfixe change, la source traverse.
    #[test]
    fn from_rest_database_preserve_le_dberr_intact() {
        let err: McpError = RestError::Database(sea_orm::DbErr::Custom("boom".to_string())).into();
        assert!(matches!(err, McpError::Database(_)), "{err:?}");
        assert_eq!(err.to_string(), "MRD-MCP-003: database error: Custom Error: boom");
        assert_eq!(err.rpc_code(), -32603);
        let source = err.source().expect("le DbErr embarqué reste accessible");
        assert_eq!(
            source
                .downcast_ref::<sea_orm::DbErr>()
                .expect("la source est le DbErr")
                .to_string(),
            "Custom Error: boom"
        );
    }

    /// `Scenario` : « `From` `RestError::Application` restitue le hook sans code
    /// `MRD-*` » — miroir MCP du `422` REST : message nu, `code` en `data`, le hook
    /// n'est pas `Error::source`.
    #[test]
    fn from_rest_application_restitue_le_hook_sans_code_mrd() {
        let err: McpError =
            RestError::Application(HookError::with_code("WIDGET-001", "label must not be empty")).into();
        let (code, message, data) = triple(&err);
        assert_eq!(code, -32000);
        assert_eq!(message, "label must not be empty");
        assert!(!message.contains("MRD-"), "aucune sous-chaîne MRD- introduite");
        let data = data.expect("le code du hook passe en data");
        assert_eq!(data, json!({ "code": "WIDGET-001" }));
        assert!(
            err.source().is_none(),
            "le HookError n'est pas Error::source de Application"
        );
    }

    /// `Scenario` : « `From` `RestError::InvalidInput` devient `McpError::InvalidParams` »
    /// — verrou du bras posé en vague B3a (arbitré 2026-09-29) : charge utile `String`
    /// déplacée intacte sous le préfixe `MRD-MCP-005: invalid params: `.
    #[test]
    fn from_rest_invalid_input_devient_invalid_params() {
        let err: McpError = RestError::InvalidInput("expires_at must be in the future".to_string()).into();
        assert!(matches!(err, McpError::InvalidParams(_)), "{err:?}");
        assert_eq!(
            triple(&err),
            (
                -32602,
                "MRD-MCP-005: invalid params: expires_at must be in the future".to_string(),
                None
            )
        );
    }

    // ——— Variantes de protocole et message fil (arbitré 2026-09-29) ———

    /// `Scenario` : « `McpError::MethodNotFound`, `ParseError` et `InvalidRequest`
    /// rendent leur triple » — `-32601`/`-32700`/`-32600`, messages préfixés exacts,
    /// `data` absent pour les trois.
    #[test]
    fn method_not_found_parse_error_et_invalid_request_rendent_leur_triple() {
        assert_eq!(
            triple(&McpError::MethodNotFound("ping".to_string())),
            (-32601, "MRD-MCP-008: method not found: ping".to_string(), None)
        );
        assert_eq!(
            triple(&McpError::ParseError),
            (-32700, "MRD-MCP-009: parse error".to_string(), None)
        );
        assert_eq!(
            triple(&McpError::InvalidRequest),
            (-32600, "MRD-MCP-010: invalid request".to_string(), None)
        );
    }

    /// `Scenario` : « `Error::source` ne vaut `Some` que sur `McpError::Database` » —
    /// une valeur de chacune des onze variantes, `#[from]` vaut `#[source]`, les dix
    /// autres `None`, `Application` compris.
    #[test]
    fn error_source_ne_vaut_some_que_sur_database() {
        let errors: Vec<McpError> = vec![
            McpError::Forbidden,
            McpError::NotFound,
            database_boom(),
            McpError::Render("bad template".to_string()),
            McpError::InvalidParams("x".to_string()),
            McpError::UnknownTool("ghost_get".to_string()),
            McpError::Internal("upstream exploded".to_string()),
            McpError::Application(HookError::new("boom")),
            McpError::MethodNotFound("ping".to_string()),
            McpError::ParseError,
            McpError::InvalidRequest,
        ];
        assert_eq!(errors.len(), 11, "une valeur par variante");
        let mut database_checked = false;
        for err in &errors {
            match err {
                McpError::Database(_) => {
                    let source = err.source().expect("Database porte sa source");
                    let db_err = source
                        .downcast_ref::<sea_orm::DbErr>()
                        .expect("la source est le DbErr embarqué");
                    assert_eq!(db_err.to_string(), "Custom Error: boom");
                    database_checked = true;
                }
                other => {
                    assert!(
                        other.source().is_none(),
                        "seule Database porte une source : {other}"
                    );
                }
            }
        }
        assert!(database_checked, "la variante Database était absente de la table");
    }

    /// `Scenario` : « `From` `RestError::Internal` garde le double code dans la
    /// `Display` seule » — le `MRD-AUTH-NNN` imbriqué ne sort que dans la `Display`
    /// (qui nourrit la trace) ; `wire_message` est générique (arbitré 2026-09-29).
    #[test]
    fn from_rest_internal_garde_le_double_code_dans_la_display_seule() {
        let auth_err = crate::auth::AuthError::Oidc("discovery failed".to_string());
        let err: McpError = RestError::Internal(auth_err.to_string()).into();
        assert_eq!(
            err.to_string(),
            "MRD-MCP-007: internal error: MRD-AUTH-003: OIDC error: discovery failed"
        );
        assert_eq!(err.wire_message(), "MRD-MCP-007: internal error");
        assert_eq!(err.rpc_code(), -32603);
        assert_eq!(err.data(), None);
    }

    /// `Scenario` : « `wire_message` généralise les `-32603` et laisse le reste
    /// verbatim » — `Database`/`Render`/`Internal` génériques sans charge utile,
    /// `InvalidParams` et les autres sur leur `Display` exacte (arbitré 2026-09-29).
    #[test]
    fn wire_message_generalise_les_moins_32603_et_laisse_le_reste_verbatim() {
        assert_eq!(database_boom().wire_message(), "MRD-MCP-003: database error");
        assert_eq!(
            McpError::Render("bad template".to_string()).wire_message(),
            "MRD-MCP-004: output rendering error"
        );
        assert_eq!(
            McpError::Internal("x".to_string()).wire_message(),
            "MRD-MCP-007: internal error"
        );
        let params = McpError::InvalidParams("invalid type: integer -1, expected u64".to_string());
        assert_eq!(params.wire_message(), params.to_string());
        assert_eq!(
            params.wire_message(),
            "MRD-MCP-005: invalid params: invalid type: integer -1, expected u64"
        );
        assert_eq!(McpError::Forbidden.wire_message(), "MRD-MCP-001: forbidden");
    }

    /// `Scenario` : « la porte feature `mcp` gouverne toute la surface du fichier » —
    /// le fichier ne compile que sous le gate de `lib.rs`, sans aucun `#[cfg(feature
    /// = ...)]` interne : ce test ne s'exécute que sous `mcp` et `--all-features`, et
    /// le témoin ci-dessous adresse les onze variantes et les trois fonctions
    /// `pub(crate)` sans le moindre branchement de feature. Sous
    /// `--no-default-features`, ni ce test ni aucun `MRD-MCP-*` n'existent (prouvé
    /// par le run batterie concerné, qui compile sans le module `mcp`).
    #[test]
    fn surface_mcp_error_presente_sous_feature_mcp() {
        let surface: Vec<McpError> = vec![
            McpError::Forbidden,
            McpError::NotFound,
            McpError::Database(sea_orm::DbErr::Custom("x".to_string())),
            McpError::Render("x".to_string()),
            McpError::InvalidParams("x".to_string()),
            McpError::UnknownTool("x".to_string()),
            McpError::Internal("x".to_string()),
            McpError::Application(HookError::new("x")),
            McpError::MethodNotFound("x".to_string()),
            McpError::ParseError,
            McpError::InvalidRequest,
        ];
        assert_eq!(surface.len(), 11);
        for err in &surface {
            assert!(err.to_string().contains("MRD-") || matches!(err, McpError::Application(_)));
            let _ = (err.rpc_code(), err.wire_message(), err.data());
        }
        let _: McpError = RestError::NotFound.into();
        let _: McpError = sea_orm::DbErr::Custom("x".to_string()).into();
    }
}
