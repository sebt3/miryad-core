use std::future::Future;

use sea_orm::DatabaseConnection;
use serde_json::Value;

use crate::auth::AuthPrincipal;
use crate::mcp::error::McpError;
use crate::rbac::policy_verdict;
use crate::resource::AccessPolicy;
use crate::users::resolve_user;

/// Gestionnaire asynchrone d'un `AppTool` : le contrat que ./registry.rs reçoit sous forme de
/// `Fn` de l'hôte et que `AppTool::call` invoque après résolution de l'utilisateur, décision de
/// politique et validation des arguments. `DatabaseConnection` et `AuthPrincipal` sont passés
/// **par valeur** (clones peu coûteux, `Arc` interne pour la connexion) : un gestionnaire qui
/// rend un futur ne peut pas emprunter à l'appelant.
#[async_trait::async_trait]
pub(crate) trait AppToolHandler: Send + Sync {
    /// Exécute l'action de l'hôte. `Ok` comme `Err` sont rendus **tels quels** par
    /// `AppTool::call` : aucune traduction, aucune réécriture (`McpError::Application` traverse
    /// sans code `MRD-*`).
    ///
    /// # Errors
    ///
    /// Toute `McpError` que l'hôte décide de rendre — dont `McpError::Application` pour un refus
    /// métier de sa surface.
    async fn call(
        &self,
        db: DatabaseConnection,
        principal: AuthPrincipal,
        arguments: Value,
    ) -> Result<Value, McpError>;
}

/// Implémentation blanche du trait précédent pour tout `Fn` de l'hôte : la forme sous laquelle
/// ./registry.rs reçoit le gestionnaire (`AppTool::new` emballe ce `Fn` dans une
/// `Box<dyn AppToolHandler>`).
#[async_trait::async_trait]
impl<F, Fut> AppToolHandler for F
where
    F: Fn(DatabaseConnection, AuthPrincipal, Value) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Value, McpError>> + Send + 'static,
{
    async fn call(
        &self,
        db: DatabaseConnection,
        principal: AuthPrincipal,
        arguments: Value,
    ) -> Result<Value, McpError> {
        self(db, principal, arguments).await
    }
}

/// Récupérateur de `$ref` refusant **toute** résolution distante ou de fichier
/// (`dsd/miryad-core/mcp.md` `DEC-3`, arbitré 2026-11-16) : Cargo unifie les features et
/// `lymaille` tire `jsonschema` avec `resolve-http` et `resolve-file` — sous `--all-features`
/// ou `lymaille` + `mcp` c'est ce récupérateur, posé explicitement à la construction du
/// validateur, qui garantit l'interdit. Les références locales (`#/$defs/…`) passent par le
/// registre du document, jamais par ici.
struct NoExternalRetrieval;

impl jsonschema::Retrieve for NoExternalRetrieval {
    fn retrieve(
        &self,
        uri: &jsonschema::Uri<String>,
    ) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        Err(format!(
            "app tool schemas resolve no remote or file `$ref`: `{}` is refused",
            uri.as_str()
        )
        .into())
    }
}

/// Outil MCP applicatif — une action de l'hôte exposée par MCP, hors CRUD d'entité (issue #86,
/// `dsd/miryad-core/mcp.md` `FR-6` à `FR-9`). Réunit nom, description, schéma JSON des arguments
/// (conservé tel qu'enregistré, publié tel quel par `tools/list`), politique d'accès, validateur
/// compilé une fois à l'enregistrement, et gestionnaire asynchrone de l'hôte.
// Consommé par ./registry.rs (`register_tool`, tâche A2) et ./handler.rs (`tools/list` et
// dispatch, tâche A3) de app_tool.sdd `Tasks`.
pub(crate) struct AppTool {
    name: String,
    description: String,
    input_schema: Value,
    policy: AccessPolicy,
    validator: jsonschema::Validator,
    handler: Box<dyn AppToolHandler>,
}

// Consommé par ./registry.rs (tâche A2 de app_tool.sdd `Tasks`).
impl AppTool {
    /// Compile `input_schema` une fois pour toutes et emballe `handler` ; `AppTool::call` n'a
    /// plus rien à construire.
    ///
    /// # Panics
    ///
    /// Deux erreurs de programmation d'enregistrement, détectées au démarrage, font paniquer :
    /// - `input_schema` n'est pas un schéma JSON valide, ou sa construction échoue (y compris
    ///   une `$ref` distante ou de fichier : la compilation n'autorise **aucune** résolution
    ///   externe, quelles que soient les features unifiées par Cargo — `dsd/miryad-core/mcp.md`
    ///   `DEC-3`) ;
    /// - `policy` vaut `AccessPolicy::OwnerOnly` : la politique compare la colonne propriétaire
    ///   d'un enregistrement, un outil applicatif n'en a aucun à l'évaluation
    ///   (`dsd/miryad-core/mcp.md` `DEC-2`, arbitré 2026-11-16).
    ///
    /// Le message cite le nom de l'outil dans les deux cas, même logique que la collision
    /// refusée par `assert!` dans `McpToolRegistry::register`.
    #[must_use]
    #[allow(clippy::panic)] // app_tool.sdd `Forbids` : les deux panics d'enregistrement sont les seuls autorisés, documentés au `# Panics` ci-dessus
    pub(crate) fn new<F, Fut>(
        name: String,
        description: String,
        input_schema: Value,
        policy: AccessPolicy,
        handler: F,
    ) -> Self
    where
        F: Fn(DatabaseConnection, AuthPrincipal, Value) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Value, McpError>> + Send + 'static,
    {
        assert!(
            !matches!(policy, AccessPolicy::OwnerOnly),
            "`{name}`: `AccessPolicy::OwnerOnly` compares the owner column of a record — an app \
             tool has none at evaluation time, refusing to register it \
             (app_tool.sdd `Must`, mcp.md `DEC-2`)"
        );
        let validator = match jsonschema::options()
            .with_retriever(NoExternalRetrieval)
            .build(&input_schema)
        {
            Ok(validator) => validator,
            Err(err) => panic!(
                "`{name}`: `input_schema` does not compile as a JSON Schema — invalid schema, or \
                 a `$ref` no app tool may resolve (no remote, no file; app_tool.sdd `Must`, \
                 mcp.md `DEC-3`): {err}"
            ),
        };
        Self {
            name,
            description,
            input_schema,
            policy,
            validator,
            handler: Box::new(handler),
        }
    }

    /// Résout l'utilisateur, évalue la politique, valide les arguments, puis appelle le
    /// gestionnaire — **dans cet ordre**, sans passe supplémentaire. La politique est évaluée
    /// avant la validation : un appelant refusé n'apprend rien du schéma par ses messages.
    ///
    /// # Errors
    ///
    /// - `McpError::Database` (`MRD-MCP-003`) : `DbErr` de `resolve_user` ou de
    ///   `rbac::policy_verdict` ;
    /// - `McpError::Forbidden` (`MRD-MCP-001`, `-32001`) : verdict `Some(false)` — sans valider
    ///   les arguments ni appeler le gestionnaire ;
    /// - `McpError::InvalidParams` (`MRD-MCP-005`, `-32602`) : première violation du schéma
    ///   (chemin de l'instance et motif, jamais la valeur fautive) ;
    /// - toute `McpError` rendue par le gestionnaire, rendue telle quelle.
    pub(crate) async fn call(
        &self,
        db: &DatabaseConnection,
        principal: &AuthPrincipal,
        arguments: Value,
    ) -> Result<Value, McpError> {
        let user = resolve_user(db, &principal.subject, principal.email.as_deref()).await?;
        // Aucune trace ici : les traces `-32603` et `-32001` sont celles de ./handler.rs
        // (app_tool.sdd `Must not`).
        match policy_verdict(db, self.policy, &user).await? {
            Some(true) => {}
            // `None` (seul `OwnerOnly` le rend) est irreachable ici : la politique est refusée à
            // `new` ; le fail-closed en `Forbidden` reste le pli conservateur si elle y
            // parvenait jamais.
            Some(false) | None => return Err(McpError::Forbidden),
        }
        if let Err(err) = self.validator.validate(&arguments) {
            let location = err.instance_path();
            let where_ = if location.is_empty() {
                "the root instance".to_string()
            } else {
                format!("instance at `{location}`")
            };
            // Chemin de l'instance et motif (mot-clé violé) seulement : la valeur fautive peut
            // être un secret et ne sort jamais (app_tool.sdd `Must`, point 3).
            return Err(McpError::InvalidParams(format!(
                "{where_} violates the `{}` keyword",
                err.kind().keyword()
            )));
        }
        self.handler.call(db.clone(), principal.clone(), arguments).await
    }

    /// Nom de l'outil, tel qu'enregistré (accesseur pour ./handler.rs, `tools/list`).
    #[must_use]
    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    /// Description publiée par `tools/list` (accesseur pour ./handler.rs).
    #[must_use]
    pub(crate) fn description(&self) -> &str {
        &self.description
    }

    /// Schéma JSON des arguments, tel qu'enregistré sans réécriture (accesseur pour
    /// ./handler.rs).
    #[must_use]
    pub(crate) fn input_schema(&self) -> &Value {
        &self.input_schema
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::PrincipalSource;
    use crate::migration::Migrator;
    use crate::resource::HookError;
    use crate::users::resolve_user;
    use sea_orm::Database;
    use sea_orm_migration::MigratorTrait;
    use serde_json::{Value, json};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    /// Schéma des `Scenario` « arguments conformes / non conformes » : un objet exigeant la
    /// propriété entière `occurrence`.
    fn occurrence_schema() -> Value {
        json!({
            "type": "object",
            "required": ["occurrence"],
            "properties": { "occurrence": { "type": "integer" } }
        })
    }

    fn principal(subject: &str) -> AuthPrincipal {
        AuthPrincipal {
            subject: subject.to_string(),
            email: None,
            preferred_username: None,
            source: PrincipalSource::ApiToken { token_id: 1 },
        }
    }

    async fn migrated_db() -> DatabaseConnection {
        let db = Database::connect("sqlite::memory:")
            .await
            .expect("sqlite mémoire se connecte");
        Migrator::up(&db, None)
            .await
            .expect("les migrations de la crate s'appliquent proprement");
        db
    }

    /// `sqlite::memory:` jamais migrée — fixture du `Scenario` « une erreur de base est
    /// restituée en `MRD-MCP-003` » : `resolve_user` bute sur l'absence de `miryad_users`.
    async fn unmigrated_db() -> DatabaseConnection {
        Database::connect("sqlite::memory:")
            .await
            .expect("sqlite mémoire se connecte")
    }

    /// Outil dont le gestionnaire rend `{"ok": true}`, compte ses appels et consigne les
    /// arguments reçus — matière première des `Scenario` sur l'ordre politique/validation/
    /// gestionnaire.
    fn counting_tool(
        name: &str,
        schema: Value,
        policy: AccessPolicy,
    ) -> (AppTool, Arc<AtomicUsize>, Arc<Mutex<Vec<Value>>>) {
        let counter = Arc::new(AtomicUsize::new(0));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let handler_counter = Arc::clone(&counter);
        let handler_seen = Arc::clone(&seen);
        let tool = AppTool::new(
            name.to_string(),
            "compteur d'appels".to_string(),
            schema,
            policy,
            move |_db, _principal, arguments| {
                let counter = Arc::clone(&handler_counter);
                let seen = Arc::clone(&handler_seen);
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    seen.lock()
                        .expect("verrou du gestionnaire non empoisonné")
                        .push(arguments);
                    Ok(json!({ "ok": true }))
                }
            },
        );
        (tool, counter, seen)
    }

    fn count(counter: &Arc<AtomicUsize>) -> usize {
        counter.load(Ordering::SeqCst)
    }

    /// `Scenario` : « un principal autorisé et des arguments conformes atteignent le
    /// gestionnaire » — `Public`, arguments conformes : `Ok({"ok": true})`, un appel, arguments
    /// reçus tels quels.
    #[tokio::test]
    async fn principal_autorise_et_arguments_conformes_atteignent_le_gestionnaire() {
        let db = migrated_db().await;
        let (tool, counter, seen) = counting_tool("analyse", occurrence_schema(), AccessPolicy::Public);

        let outcome = tool
            .call(&db, &principal("analyste"), json!({ "occurrence": 7 }))
            .await;

        assert!(
            matches!(&outcome, Ok(value) if *value == json!({ "ok": true })),
            "{outcome:?}"
        );
        assert_eq!(count(&counter), 1, "le gestionnaire a été appelé une fois");
        let received = seen.lock().expect("verrou non empoisonné");
        assert_eq!(
            received.as_slice(),
            &[json!({ "occurrence": 7 })],
            "les arguments sont parvenus au gestionnaire tels quels"
        );
    }

    /// `Scenario` : « un principal refusé n'exécute pas le gestionnaire » — `AdminOnly` hors
    /// admin : `Forbidden`, compteur à zéro.
    #[tokio::test]
    async fn principal_refuse_nexecute_pas_le_gestionnaire() {
        let db = migrated_db().await;
        let (tool, counter, _seen) = counting_tool("analyse", occurrence_schema(), AccessPolicy::AdminOnly);

        let outcome = tool
            .call(&db, &principal("etranger"), json!({ "occurrence": 7 }))
            .await;

        assert!(matches!(outcome, Err(McpError::Forbidden)), "{outcome:?}");
        assert_eq!(count(&counter), 0, "le gestionnaire n'a pas été appelé");
    }

    /// `Scenario` : « la politique passe avant la validation » — `AdminOnly` refusé **et**
    /// arguments non conformes : `Forbidden`, jamais `InvalidParams`.
    #[tokio::test]
    async fn la_politique_passe_avant_la_validation() {
        let db = migrated_db().await;
        let (tool, counter, _seen) = counting_tool("analyse", occurrence_schema(), AccessPolicy::AdminOnly);

        let outcome = tool.call(&db, &principal("etranger"), json!({})).await;

        assert!(
            matches!(outcome, Err(McpError::Forbidden)),
            "un appelant refusé ne reçoit aucun message de schéma : {outcome:?}"
        );
        assert_eq!(count(&counter), 0, "le gestionnaire n'a pas été appelé");
    }

    /// `Scenario` : « des arguments non conformes rendent -32602 sans exécuter le gestionnaire »
    /// — `{}`, `{"occurrence": "7"}` puis `[]` sur un outil `Public` : chaque appel est
    /// `InvalidParams` (`MRD-MCP-005`), le message ne contient pas la valeur fautive `"7"`, le
    /// compteur reste à zéro.
    #[tokio::test]
    async fn arguments_non_conformes_rendent_moins_32602_sans_executer_le_gestionnaire() {
        let db = migrated_db().await;
        let (tool, counter, _seen) = counting_tool("analyse", occurrence_schema(), AccessPolicy::Public);

        for arguments in [json!({}), json!({ "occurrence": "7" }), json!([])] {
            let outcome = tool.call(&db, &principal("analyste"), arguments.clone()).await;
            match outcome {
                Err(McpError::InvalidParams(message)) => {
                    assert!(
                        !message.contains("\"7\""),
                        "le message ne cite jamais la valeur fautive : {message}"
                    );
                }
                other => panic!("InvalidParams attendue pour {arguments} : {other:?}"),
            }
        }
        assert_eq!(count(&counter), 0, "le gestionnaire n'a pas été appelé");
    }

    /// `Scenario` : « un groupe est évalué comme pour une entité » — `Group("analystes")` : le
    /// membre et l'admin atteignent le gestionnaire, l'autre reçoit `Forbidden`.
    #[tokio::test]
    async fn un_groupe_est_evalue_comme_pour_une_entite() {
        use crate::users::sync_group_memberships;

        let db = migrated_db().await;
        let membre = resolve_user(&db, "membre", None)
            .await
            .expect("utilisateur membre créé");
        sync_group_memberships(&db, membre.id, &["analystes".to_string()])
            .await
            .expect("appartenance au groupe analystes");
        let admin = resolve_user(&db, "admin", None)
            .await
            .expect("utilisateur admin créé");
        sync_group_memberships(&db, admin.id, &["admin".to_string()])
            .await
            .expect("appartenance au groupe admin");
        resolve_user(&db, "etranger", None)
            .await
            .expect("utilisateur étranger créé");

        let (tool, counter, _seen) =
            counting_tool("analyse", occurrence_schema(), AccessPolicy::Group("analystes"));

        for subject in ["membre", "admin"] {
            let outcome = tool
                .call(&db, &principal(subject), json!({ "occurrence": 7 }))
                .await;
            assert!(
                matches!(&outcome, Ok(value) if *value == json!({ "ok": true })),
                "`{subject}` (membre ou admin) atteint le gestionnaire : {outcome:?}"
            );
        }
        let refused = tool
            .call(&db, &principal("etranger"), json!({ "occurrence": 7 }))
            .await;
        assert!(
            matches!(refused, Err(McpError::Forbidden)),
            "le non-membre est refusé : {refused:?}"
        );
        assert_eq!(count(&counter), 2, "deux appels, pas trois");
    }

    /// `Scenario` : « l'erreur applicative de l'hôte traverse sans code MRD » — le
    /// `McpError::Application(HookError)` rendu par le gestionnaire est le résultat, `Display`
    /// sans `MRD-`, `code = "MON-001"`.
    #[tokio::test]
    async fn erreur_applicative_de_l_hote_traverse_sans_code_mrd() {
        let db = migrated_db().await;
        let tool = AppTool::new(
            "analyse".to_string(),
            "gestionnaire qui refuse".to_string(),
            occurrence_schema(),
            AccessPolicy::Public,
            |_db, _principal, _arguments| async move {
                Err::<Value, McpError>(McpError::Application(HookError::with_code(
                    "MON-001",
                    "occurrence close",
                )))
            },
        );

        let outcome = tool
            .call(&db, &principal("analyste"), json!({ "occurrence": 7 }))
            .await;

        match outcome {
            Err(McpError::Application(hook)) => {
                assert_eq!(hook.code.as_deref(), Some("MON-001"));
                assert_eq!(hook.message, "occurrence close");
                assert!(
                    !hook.message.contains("MRD-"),
                    "aucun code MRD n'est ajouté au hook de l'hôte : {hook:?}"
                );
            }
            other => panic!("McpError::Application attendue telle quelle : {other:?}"),
        }
    }

    /// `Scenario` : « une erreur de base est restituée en MRD-MCP-003 » — tables de la crate
    /// absentes : `Database`, gestionnaire jamais appelé.
    #[tokio::test]
    async fn erreur_de_base_est_restituee_en_mrd_mcp_003() {
        let db = unmigrated_db().await;
        let (tool, counter, _seen) = counting_tool("analyse", occurrence_schema(), AccessPolicy::Public);

        let outcome = tool
            .call(&db, &principal("analyste"), json!({ "occurrence": 7 }))
            .await;

        assert!(matches!(outcome, Err(McpError::Database(_))), "{outcome:?}");
        assert_eq!(count(&counter), 0, "le gestionnaire n'a pas été appelé");
    }

    /// `Scenario` : « un schéma invalide ou `OwnerOnly` échoue à la construction » — volet
    /// schéma : `{"type": 12}` n'est pas un schéma JSON, `new` panique et le message cite le nom
    /// de l'outil.
    #[test]
    #[should_panic(expected = "analyse")]
    fn schema_invalide_panique_a_la_construction() {
        let _tool = AppTool::new(
            "analyse".to_string(),
            "schéma invalide".to_string(),
            json!({ "type": 12 }),
            AccessPolicy::Public,
            |_db, _principal, _arguments| async { Ok(Value::Null) },
        );
    }

    /// `Scenario` : « un schéma invalide ou `OwnerOnly` échoue à la construction » — volet
    /// politique : `OwnerOnly` est refusée à l'enregistrement (aucun enregistrement à comparer à
    /// l'évaluation, `dsd/miryad-core/mcp.md` `DEC-2`), même panique citant le nom.
    #[test]
    #[should_panic(expected = "analyse")]
    fn owner_only_panique_a_la_construction() {
        let _tool = AppTool::new(
            "analyse".to_string(),
            "politique invalide".to_string(),
            occurrence_schema(),
            AccessPolicy::OwnerOnly,
            |_db, _principal, _arguments| async { Ok(Value::Null) },
        );
    }

    /// `Scenario` : « aucune référence distante ni de fichier n'est résolue, même sous features
    /// unifiées » — un écouteur TCP local et une `$ref` `http://`, puis une `$ref` `file://` :
    /// `new` panique pour chacune (référence non résoluble, le récupérateur refuse tout), et
    /// l'écouteur n'a reçu aucune connexion. Que le fichier ne soit pas lu non plus est la
    /// conséquence du récupérateur lui-même : c'est lui, et uniquement lui, qui répond à toute
    /// référence externe, et il ne lit rien.
    #[test]
    fn aucune_reference_distante_ni_de_fichier_nest_resolue() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("écouteur TCP local");
        let port = listener
            .local_addr()
            .expect("adresse locale de l'écouteur")
            .port();
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                if sender.send(stream.is_ok()).is_err() {
                    break;
                }
            }
        });

        let external_schemas = [
            json!({ "$ref": format!("http://127.0.0.1:{port}/schema.json") }),
            json!({ "$ref": "file:///etc/hostname" }),
        ];
        for schema in external_schemas {
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                AppTool::new(
                    "analyse".to_string(),
                    "référence externe interdite".to_string(),
                    schema.clone(),
                    AccessPolicy::Public,
                    |_db, _principal, _arguments| async { Ok(Value::Null) },
                )
            }));
            let message = match outcome {
                Err(payload) => panic_message(&payload),
                Ok(_tool) => panic!("`new` devait paniquer sur une `$ref` non résoluble : {schema}"),
            };
            assert!(
                message.contains("analyse"),
                "le message de panique cite le nom de l'outil : {message}"
            );
        }

        let accepted = receiver.recv_timeout(std::time::Duration::from_millis(250));
        assert!(
            accepted.is_err(),
            "l'écouteur n'a reçu aucune connexion : {accepted:?}"
        );
    }

    /// `Scenario` : « un $ref local reste résolu » — `#/$defs/…` se résout sans récupération :
    /// la construction réussit et l'appel avec une valeur fautive rend `InvalidParams`.
    #[tokio::test]
    async fn ref_local_reste_resolu() {
        let db = migrated_db().await;
        let (tool, counter, _seen) = counting_tool(
            "analyse",
            json!({
                "$defs": { "id": { "type": "integer" } },
                "type": "object",
                "properties": { "occurrence": { "$ref": "#/$defs/id" } }
            }),
            AccessPolicy::Public,
        );

        let outcome = tool
            .call(&db, &principal("analyste"), json!({ "occurrence": "x" }))
            .await;

        assert!(
            matches!(outcome, Err(McpError::InvalidParams(_))),
            "le `$ref` local est résolu à la construction, la valeur est validée à l'appel : {outcome:?}"
        );
        assert_eq!(count(&counter), 0, "le gestionnaire n'a pas été appelé");
    }

    /// Texte d'un payload de `catch_unwind` (`String` pour un `panic!` formaté, `&str` sinon).
    fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> String {
        if let Some(text) = payload.downcast_ref::<String>() {
            text.clone()
        } else if let Some(text) = payload.downcast_ref::<&str>() {
            (*text).to_string()
        } else {
            String::new()
        }
    }
}
