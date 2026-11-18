//! Porte sortante HTTP nu vers un cluster Restate self-hosté : enregistrement idempotent du
//! déploiement auprès de l'admin API (`register_deployment`) et déclenchement asynchrone d'un run
//! via l'ingress (`trigger_run`, suffixe `/send` — jamais bloquant sur la complétion). Aucune
//! autre partie de la crate ne parle HTTP sortant à Restate : les fichiers service parlent
//! exclusivement le protocole `restate-sdk`. Compile seulement sous la feature `workflow`.

use crate::workflow::definition::DagSteps;
use crate::workflow::error::WorkflowError;
use crate::workflow::error::classify_transport_error;
use crate::workflow::interpreter::RunInput;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use uuid::Uuid;

/// Corps exact de `POST /deployments` : `uri` puis `force`, compact — l'ordre des champs porte
/// sur le fil (spike ./client.sdd, `Must`), d'où un struct dédié plutôt qu'une valeur `json!`
/// (dont la Map trierait les clés alphabétiquement : `force` avant `uri`).
#[derive(Serialize)]
struct DeploymentRequest<'a> {
    uri: &'a str,
    force: bool,
}

/// Corps `2xx` observé de l'ingress sur `/send` : seul `invocationId` est consommé, `status`
/// est ignoré (./client.sdd, `Must` — lecture du corps, jamais de l'en-tête `x-restate-id`).
#[derive(Deserialize)]
struct IngressSendResponse {
    #[serde(rename = "invocationId")]
    invocation_id: String,
}

/// Emplacement des trois URL Restate d'une application consommatrice — construit par elle,
/// aucune lecture d'environnement ici (même convention que `auth::OidcConfig`).
pub struct WorkflowConfig {
    /// URL de base de l'admin API Restate (`POST {admin_url}/deployments`).
    pub admin_url: String,
    /// URL de base de l'ingress Restate (`POST {ingress_url}/DagInterpreter/{run_key}/run/send`).
    pub ingress_url: String,
    /// URL que Restate doit pouvoir rappeler pour joindre ce processus (endpoint `restate-sdk`
    /// servi par l'app) ; forme non restreinte ni validée par ce fichier.
    pub deployment_url: String,
}

impl WorkflowConfig {
    /// Vérifie les deux URL que ce fichier emprunte pour son propre compte : `admin_url` et
    /// `ingress_url` doivent chacune se parser en URL absolue de schéma `http` ou `https`.
    /// Pure, sans I/O : aucune connexion réseau n'est tentée ici — le contrôle est un verdict
    /// rendu avant tout appel (`./client.sdd`, arbitré 2026-09-29). `deployment_url` n'est ni
    /// validée ni inspectée : sa forme reste à la charge de l'application consommatrice
    /// (décision conservée du 2026-09-23).
    ///
    /// # Errors
    ///
    /// `MRD-WORKFLOW-007` (`WorkflowError::InvalidConfig`) : `admin_url` ou `ingress_url` vide,
    /// non parsable en URL absolue, ou de schéma hors `http`/`https` ; le message nomme le
    /// champ fautif.
    pub fn validate(&self) -> Result<(), WorkflowError> {
        validate_http_url("admin_url", &self.admin_url)?;
        validate_http_url("ingress_url", &self.ingress_url)?;
        Ok(())
    }
}

/// Garde d'une seule URL de service : vide, non parsable ou de schéma autre que
/// `http`/`https` → `InvalidConfig` nommant `field`. `reqwest::Url` est le ré-export public de
/// `url` par `reqwest` (aucune dépendance ajoutée) ; le parse est syntaxique, jamais résolutif.
fn validate_http_url(field: &str, value: &str) -> Result<(), WorkflowError> {
    if value.is_empty() {
        return Err(WorkflowError::InvalidConfig(format!(
            "{field} is empty, expected an absolute http or https URL"
        )));
    }
    match reqwest::Url::parse(value) {
        Ok(url) if matches!(url.scheme(), "http" | "https") => Ok(()),
        _ => Err(WorkflowError::InvalidConfig(format!(
            "{field} is not an absolute http or https URL: {value}"
        ))),
    }
}

/// Poignée rendue par `trigger_run` : les deux identifiants du run déclenché, à l'appelant de
/// décider lesquels il expose (signal, statut — lectures hors du périmètre de ce fichier).
pub struct RunHandle {
    /// Clé de run générée (`uuid::Uuid::new_v4`, hyphéné), nécessaire à tout appel ultérieur
    /// ciblant ce workflow.
    pub run_key: String,
    /// `invocationId` du corps de réponse de l'ingress, identifiant Restate global de
    /// l'invocation.
    pub invocation_id: String,
}

/// Enregistre (ou ré-enregistre, `force: true` explicite à chaque appel) le déploiement auprès de
/// l'admin API : `POST {admin_url}/deployments`, corps `{"uri": deployment_url, "force": true}`,
/// `content-type: application/json`. Tout statut `2xx` rend `Ok(())` sans inspecter le corps —
/// idempotent, à appeler au démarrage de chaque réplica.
///
/// La configuration est validée en tête (`WorkflowConfig::validate`), avant toute ouverture de
/// connexion.
///
/// # Errors
///
/// - `MRD-WORKFLOW-001` (`WorkflowError::RestateUnreachable`) : admin API injoignable
///   (connexion refusée, DNS, timeout).
/// - `MRD-WORKFLOW-002` (`WorkflowError::RestateRejected`) : l'admin API répond hors `2xx` —
///   `status` observé, `body` verbatim.
/// - `MRD-WORKFLOW-007` (`WorkflowError::InvalidConfig`) : `admin_url` (ou `ingress_url`) vide,
///   non parsable ou de schéma hors `http`/`https` — refusée avant tout appel réseau.
pub async fn register_deployment(
    config: &WorkflowConfig,
    http: &reqwest::Client,
) -> Result<(), WorkflowError> {
    config.validate()?;
    let response = http
        .post(format!("{}/deployments", config.admin_url))
        .json(&DeploymentRequest {
            uri: &config.deployment_url,
            force: true,
        })
        .send()
        .await
        .map_err(classify_transport_error)?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.map_err(classify_transport_error)?;
        return Err(WorkflowError::RestateRejected {
            status: status.as_u16(),
            body,
        });
    }
    Ok(())
}

/// Déclenche une exécution de `DagInterpreter` sans attendre sa complétion : clé de run neuve
/// (`uuid::Uuid::new_v4`) à chaque appel — jamais réutilisée, jamais d'`Idempotency-Key` — puis
/// `POST {ingress_url}/DagInterpreter/{run_key}/run/send`, corps = `RunInput { dag, vars, trigger }`
/// sérialisé (./interpreter.sdd « Entrée du run », amendement 2026-10-30 lot B, 2026-11-02 lot 0
/// lymaille) — `{"dag": [..steps..], "vars": {..}}`, plus `"trigger": "<subject>"` quand le run a
/// un déclencheur, le DAG sous sa forme de tableau nu, sans autre enveloppe ni champ ajouté. Un
/// `202` (tout `2xx` en pratique) rend un `RunHandle` dont `invocation_id` est lu dans le **corps**
/// de réponse, pas dans un en-tête.
///
/// `vars` : variables constantes du run, un objet JSON (passez `&serde_json::Map::new()` si
/// l'appelant n'en a pas), transmis dans le corps avec le DAG sans transformation ni validation
/// de contenu. **Elles sont journalisées par Restate avec l'entrée du run et visibles dans son
/// interface : n'y mettez aucun secret** (`dsd/miryad-core/workflow.md` `CON-6`).
///
/// `trigger` : le `subject` de la personne qui déclenche le run, ou `None` pour un déclenchement
/// sans identité (tâche planifiée, appel interne). Transmis dans le corps (`RunInput.trigger`,
/// ./interpreter.sdd), jamais en en-tête ; `None` n'y écrit pas la clé. Chaîne opaque que ce
/// fichier ne valide pas, que le moteur **n'authentifie pas** : sa confiance repose entièrement
/// sur l'accès à l'ingress Restate, à ne rendre joignable que de l'application (« ingress privé »,
/// `dsd/miryad-core/lymaille.md` `DEC-12`).
///
/// La cible contractuelle est `&DagSteps` (./definition.rs, ./client.sdd `Exposes`) — rive
/// effectuée le 2026-09-23 au batch de ./definition.rs : le paramètre est le type réel de la
/// colonne `steps`, plus un générique lié par `serde::Serialize`. ./client.rs ne valide pas le
/// DAG, il le sérialise tel quel (sa validation relève de ./definition.rs).
///
/// La configuration est validée en tête (`WorkflowConfig::validate`), avant toute ouverture de
/// connexion — une `admin_url` invalide est donc refusée ici aussi, bien que seul l'`ingress_url`
/// soit emprunté par cette fonction.
///
/// # Errors
///
/// - `MRD-WORKFLOW-001` (`WorkflowError::RestateUnreachable`) : ingress injoignable.
/// - `MRD-WORKFLOW-002` (`WorkflowError::RestateRejected`) : l'ingress répond hors `2xx` (par
///   ex. service `DagInterpreter` non enregistré, `404`) — `body` verbatim.
/// - `MRD-WORKFLOW-003` (`WorkflowError::Serialization`) : corps `2xx` qui ne se décode pas en
///   la forme attendue (`invocationId` manquant par ex.).
/// - `MRD-WORKFLOW-007` (`WorkflowError::InvalidConfig`) : `admin_url` ou `ingress_url` vide,
///   non parsable ou de schéma hors `http`/`https` — refusée avant tout appel réseau.
pub async fn trigger_run(
    config: &WorkflowConfig,
    http: &reqwest::Client,
    dag: &DagSteps,
    vars: &serde_json::Map<String, serde_json::Value>,
    trigger: Option<&str>,
) -> Result<RunHandle, WorkflowError> {
    config.validate()?;
    let run_key = Uuid::new_v4().to_string();
    let response = http
        .post(format!(
            "{}/DagInterpreter/{}/run/send",
            config.ingress_url, run_key
        ))
        .json(&RunInput {
            dag: dag.clone(),
            vars: vars.clone(),
            trigger: trigger.map(str::to_string),
        })
        .send()
        .await
        .map_err(classify_transport_error)?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.map_err(classify_transport_error)?;
        return Err(WorkflowError::RestateRejected {
            status: status.as_u16(),
            body,
        });
    }
    let decoded: IngressSendResponse = response.json().await.map_err(classify_transport_error)?;
    Ok(RunHandle {
        run_key,
        invocation_id: decoded.invocation_id,
    })
}

// ───────────────── Client d'administration Restate (issue #80 (2), amendement du 2026-11-13) ──
//
// Formes **mesurées contre `restate-server` 1.8.1** (image
// `docker.restate.dev/restatedev/restate:latest`, podman, 2026-11-15) :
// - `POST {admin_url}/query`, corps `{"query": "<SQL>"}`, en-tête `Accept: application/json`
//   (sinon le serveur rend du Arrow IPC binaire) ; réponse `2xx` `{"rows":[…]}` ; erreur de
//   requête (table ou colonne inconnue, identifiant non décodable, tables pas encore servies)
//   → `500` `{"message": "Datafusion error: …"}` — notamment « expecting a partition store »
//   pendant le démarrage (sur 1.8.1 le `/query` répond dès `/health`, le message reste observé
//   sur 1.7.x : c'est une attente retryable, jamais un résultat vide).
// - `sys_invocation` : colonnes `id`, `status`, `target_service_name`, `target_handler_name`,
//   `target_service_key`, `created_at`, `completed_at`, `retry_count`, `completion_failure`
//   (liste complète des colonnes valides obtenue par sonde `SELECT <col>` — les tables système
//   1.8.1 ne portent PAS `instance`/`code`/`message`/`attempt`, noms à écarter).
// - `sys_journal` : colonnes `id`, `index`, `entry_type`, `name` (pas `type`/`command`/`time`).
// - `PATCH {admin_url}/invocations/{id}/cancel` : identifiable par la grammaire stricte mais
//   inconnu/terminé → la forme exacte des codes est mesurée par le test d'intégration
//   `#[ignore]` de ../../tests/workflow_restate.rs (restate 1.8.1, DevStation).
// Les erreurs restent dans la taxonomie existante (arbitré « réutilisation ») : `Transport` au
// sens de l'amendement = `RestateUnreachable`/`Serialization` via `classify_transport_error`,
// et une réponse d'administration inattendue est rendue `RestateRejected` (variante `Transport`
// proprement dite inexistante dans ./error.rs — aucune variante nouvelle, décidé à l'amendement).

/// Colonnes de `sys_invocation` sélectionnées par toute requête de statut (mesurées 1.8.1, voir
/// le bloc ci-dessus) — lecture par colonnes nommées, jamais `SELECT *` (la forme de `rows` reste
/// bornée à ce que ce fichier consomme).
const COLONNES_STATUT: &str = "id, status, target_service_name, target_handler_name, \
                               target_service_key, created_at, completed_at, retry_count, \
                               completion_failure";

/// Nombre maximal d'identifiants dans la clause `IN (…)` d'une seule requête (arbitré : une
/// requête jusqu'à 50 ids, découpage au-delà).
const IDS_PAR_REQUETE: usize = 50;

/// `LIMIT` par défaut de `list_invocations` quand le filtre n'en pose pas.
const LIMITE_DEFAUT: u32 = 100;

/// Plafond absolu du `LIMIT` de `list_invocations` (garde de taille de réponse).
const LIMITE_MAXIMALE: u32 = 1_000;

/// Statut d'une invocation lu dans `sys_invocation` — forme **minimale** (arbitré 2026-11-13 :
/// état, dates, service/handler, erreur terminale, tentatives ; le reste des colonnes mesurées
/// reste côté Restate). `failure` porte le texte de Restate `completion_failure` **tel que lu**,
/// non expurgé par la crate : c'est à l'hôte de décider de l'afficher.
#[derive(Debug, Clone, PartialEq)]
pub struct InvocationStatus {
    /// Identifiant Restate de l'invocation (`inv_…`).
    pub id: String,
    /// État Restate tel que lu (`pending`, `scheduled`, `ready`, `running`, `suspended`,
    /// `backoff`, `paused`, `completed` — orthographe du serveur, non normalisée ici).
    pub status: String,
    /// Nom du service cible (`target_service_name`).
    pub service: String,
    /// Nom du handler cible (`target_handler_name`).
    pub handler: String,
    /// Clé d'instance de l'invocation (`target_service_key`), `None` pour une invocation non
    /// keyée.
    pub key: Option<String>,
    /// Horodatage de création (`created_at`), texte de Restate tel que lu.
    pub created_at: String,
    /// Horodatage de complétion (`completed_at`), `None` tant que l'invocation court.
    pub completed_at: Option<String>,
    /// Nombre de tentatives déjà consommées (`retry_count`).
    pub retry_count: u64,
    /// Erreur terminale (`completion_failure`), texte de Restate tel que lu, non expurgé.
    pub failure: Option<String>,
}

/// Filtre de `list_invocations` : seuls les champs posés deviennent des conditions ; valeurs
/// validées par la grammaire stricte `[A-Za-z0-9._:-]` avant toute requête (`/query` n'a pas de
/// paramètres liés).
#[derive(Debug, Clone, Default)]
pub struct InvocationFilter {
    /// Filtrer sur `target_service_name`.
    pub service: Option<String>,
    /// Filtrer sur `target_handler_name`.
    pub handler: Option<String>,
    /// Filtrer sur `status`.
    pub status: Option<String>,
    /// Plafond du nombre de lignes (`LIMIT`), défaut 100, plafonné à 1000.
    pub limit: Option<u32>,
}

/// Entrée de journal d'une invocation (`sys_journal`), ordonnée par `index` — le canal de lecture
/// des noms d'étapes posés par ./dispatcher.rs (`step:<id>`, `render:<id>`).
#[derive(Debug, Clone, PartialEq)]
pub struct JournalEntry {
    /// Position dans le journal (`index`).
    pub index: u32,
    /// Nature de l'entrée (`entry_type` : `InvocationStart`, `Run`, …).
    pub kind: String,
    /// Nom porté par l'entrée (`name`), `None` quand l'entrée n'en porte pas — la chaîne vide
    /// mesurée sur `restate-server` 1.8.1 pour les entrées anonymes est normalisée en `None`.
    pub name: Option<String>,
}

/// Grammaire stricte d'un identifiant d'invocation : `inv_` suivi d'au moins un caractère
/// alphanumérique ASCII (`[A-Za-z0-9]`). `/query` n'a pas de paramètres liés — aucun texte libre
/// n'y est interpolé ; tout ce qui passe par un chemin (`cancel`) ou une clause (`IN (…)`) est
/// d'abord passé au crible ici (injection : `[!]` de la tâche, verrouillé par un test à zéro
/// requête).
fn identifiant_invocation_valide(id: &str) -> bool {
    id.strip_prefix("inv_")
        .is_some_and(|corps| !corps.is_empty() && corps.chars().all(|c| c.is_ascii_alphanumeric()))
}

/// Grammaire stricte d'une valeur de filtre : alphanumérique ASCII étendue à `.` `_` `:` `-`
/// (`[A-Za-z0-9._:-]`), non vide — mêmes motifs que [`identifiant_invocation_valide`].
fn valeur_filtre_valide(valeur: &str) -> bool {
    !valeur.is_empty()
        && valeur
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '-'))
}

/// Vérifie `config` puis un lot d'identifiants — tous avant la moindre requête : un seul fautif
/// dans le lot refuse tout l'appel (`InvalidConfig`, zéro octet envoyé).
fn kontroler_ids_avant_requête(config: &WorkflowConfig, ids: &[String]) -> Result<(), WorkflowError> {
    config.validate()?;
    for id in ids {
        if !identifiant_invocation_valide(id) {
            return Err(WorkflowError::InvalidConfig(format!(
                "invalid invocation id (expected inv_ followed by alphanumeric characters): {id}"
            )));
        }
    }
    Ok(())
}

/// Une requête `POST {admin_url}/query` (`{"query": …}`, `Accept: application/json`) et son lot
/// de lignes : erreurs classifiées par `classify_transport_error` sur `.send()`/`.json()`, hors
/// `2xx` → `RestateRejected` verbatim (dont le « expecting a partition store » du démarrage —
/// erreur retryable typée, jamais un lot vide), `2xx` sans tableau `rows` → forme
/// d'administration inattendue (`RestateRejected`, statut observé et corps verbatim, see le bloc
/// de commentaire en tête de section).
async fn requete_admin(
    config: &WorkflowConfig,
    http: &reqwest::Client,
    sql: &str,
) -> Result<Vec<Value>, WorkflowError> {
    let response = http
        .post(format!("{}/query", config.admin_url))
        .header("accept", "application/json")
        .json(&serde_json::json!({ "query": sql }))
        .send()
        .await
        .map_err(classify_transport_error)?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.map_err(classify_transport_error)?;
        return Err(WorkflowError::RestateRejected {
            status: status.as_u16(),
            body,
        });
    }
    let corps: Value = response.json().await.map_err(classify_transport_error)?;
    match corps.get("rows").and_then(Value::as_array) {
        Some(lignes) => Ok(lignes.clone()),
        None => Err(WorkflowError::RestateRejected {
            status: status.as_u16(),
            body: corps.to_string(),
        }),
    }
}

/// Texte d'une colonne : chaîne telle quelle, valeur non nulle rendue par son `Display` JSON
/// (jamais `null` → `None`). Absente ou `null` → `None`.
fn texte_de_ligne(ligne: &Value, colonne: &str) -> Option<String> {
    ligne.get(colonne).and_then(|valeur| match valeur {
        Value::Null => None,
        Value::String(texte) => Some(texte.clone()),
        autre => Some(autre.to_string()),
    })
}

/// Colonne obligatoire d'une ligne de statut : absente ou `null` = forme d'administration
/// inattendue (erreur typée avec la ligne verbatim, jamais un champ inventé).
fn colonne_obligatoire(ligne: &Value, colonne: &str, status: u16) -> Result<String, WorkflowError> {
    texte_de_ligne(ligne, colonne).ok_or_else(|| WorkflowError::RestateRejected {
        status,
        body: format!("colonne obligatoire absente: {colonne}: {ligne}"),
    })
}

/// Ligne `sys_invocation` → `InvocationStatus` (mapping mesuré 1.8.1, voir le bloc en tête de
/// section). `status` est le code HTTP observé, reporté dans l'erreur de colonne absente.
fn statut_de_ligne(ligne: &Value, status: u16) -> Result<InvocationStatus, WorkflowError> {
    Ok(InvocationStatus {
        id: colonne_obligatoire(ligne, "id", status)?,
        status: colonne_obligatoire(ligne, "status", status)?,
        service: colonne_obligatoire(ligne, "target_service_name", status)?,
        handler: colonne_obligatoire(ligne, "target_handler_name", status)?,
        key: texte_de_ligne(ligne, "target_service_key"),
        created_at: colonne_obligatoire(ligne, "created_at", status)?,
        completed_at: texte_de_ligne(ligne, "completed_at"),
        retry_count: ligne.get("retry_count").and_then(Value::as_u64).unwrap_or(0),
        failure: texte_de_ligne(ligne, "completion_failure"),
    })
}

/// Lit le statut de N invocations en **une** requête `POST /query` sur `sys_invocation` par lot
/// de 50 identifiants au plus (`WHERE id IN (…)` — une requête par lot au-delà). Un identifiant
/// inconnu est **absent** du résultat (Restate ne rend pas de ligne) ; liste vide → `Ok(vec![])`
/// sans requête.
///
/// Le nom de colonnes est celui mesuré sur `restate-server` 1.8.1 (bloc en tête de module de
/// section ; preuve live au test `#[ignore]` de [`../../tests/workflow_restate.rs`]).
///
/// # Errors
///
/// - `MRD-WORKFLOW-001` (`WorkflowError::RestateUnreachable`) : admin API injoignable.
/// - `MRD-WORKFLOW-002` (`WorkflowError::RestateRejected`) : réponse hors `2xx` — y compris le
///   « expecting a partition store » du démarrage (attente **retryable**, jamais un `Vec` vide) —
///   ou `2xx` de forme inattendue (tableau `rows` absent, colonne obligatoire absente).
/// - `MRD-WORKFLOW-007` (`WorkflowError::InvalidConfig`) : `admin_url`/`ingress_url` invalide, ou
///   un identifiant hors de la grammaire `inv_` + alphanumérique — refusé **sans requête**
///   (`/query` n'a pas de paramètres liés).
pub async fn invocation_statuses(
    config: &WorkflowConfig,
    http: &reqwest::Client,
    ids: &[String],
) -> Result<Vec<InvocationStatus>, WorkflowError> {
    kontroler_ids_avant_requête(config, ids)?;
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut statuts = Vec::new();
    for lot in ids.chunks(IDS_PAR_REQUETE) {
        let liste = lot
            .iter()
            .map(|id| format!("'{id}'"))
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!("SELECT {COLONNES_STATUT} FROM sys_invocation WHERE id IN ({liste})");
        let lignes = requete_admin(config, http, &sql).await?;
        for ligne in lignes {
            // Les lignes proviennent d'une réponse `2xx` (le statut non 2xx a déjà rendu une
            // erreur) : une colonne obligatoire absente y est une forme inattendue, reportée en
            // `RestateRejected` avec le statut 2xx observé.
            statuts.push(statut_de_ligne(&ligne, 200)?);
        }
    }
    Ok(statuts)
}

/// Liste les invocations de `sys_invocation` selon un filtre : seules les conditions posées sont
/// ajoutées (valeurs passées par la grammaire `[A-Za-z0-9._:-]` avant toute requête), tri par
/// `created_at` décroissant, `LIMIT` à 100 par défaut, plafonné à 1000.
///
/// # Errors
///
/// Comme [`invocation_statuses`] : transport/rejet/forme (`001`/`002`) et `InvalidConfig`
/// (`007`) pour une URL invalide ou une valeur de filtre hors grammaire, refusée sans requête.
pub async fn list_invocations(
    config: &WorkflowConfig,
    http: &reqwest::Client,
    filter: &InvocationFilter,
) -> Result<Vec<InvocationStatus>, WorkflowError> {
    config.validate()?;
    let mut conditions = Vec::new();
    for (colonne, valeur) in [
        ("target_service_name", filter.service.as_deref()),
        ("target_handler_name", filter.handler.as_deref()),
        ("status", filter.status.as_deref()),
    ] {
        let Some(valeur) = valeur else {
            continue;
        };
        if !valeur_filtre_valide(valeur) {
            return Err(WorkflowError::InvalidConfig(format!(
                "invalid invocation filter value for {colonne} (expected [A-Za-z0-9._:-]+): {valeur}"
            )));
        }
        conditions.push(format!("{colonne} = '{valeur}'"));
    }
    let where_ = if conditions.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", conditions.join(" AND "))
    };
    let limit = filter.limit.unwrap_or(LIMITE_DEFAUT).min(LIMITE_MAXIMALE);
    let sql = format!(
        "SELECT {COLONNES_STATUT} FROM sys_invocation{where_} ORDER BY created_at DESC LIMIT {limit}"
    );
    let lignes = requete_admin(config, http, &sql).await?;
    let mut statuts = Vec::with_capacity(lignes.len());
    for ligne in lignes {
        statuts.push(statut_de_ligne(&ligne, 200)?);
    }
    Ok(statuts)
}

/// Annule proprement une invocation (`PATCH {admin_url}/invocations/{id}/cancel` — `cancel`, pas
/// `kill` : la compensation s'exécute ; `kill` hors de cette livraison, arbitré 2026-11-13).
/// Idempotent par contrat : invocation inconnue (`404` « does not exist » mesuré) ou déjà
/// terminée (`409` « already completed » mesuré) → `Ok(())` — formes observées le 2026-11-15
/// contre `restate-server` 1.8.1 au test `#[ignore]` de
/// [`../../tests/workflow_restate.rs`] et au test unitaire `cancel_invocation_idempotence_*`.
///
/// # Errors
///
/// - `MRD-WORKFLOW-001` (`WorkflowError::RestateUnreachable`) : admin API injoignable.
/// - `MRD-WORKFLOW-002` (`WorkflowError::RestateRejected`) : tout autre statut que `2xx` et que
///   les codes d'idempotence mesurés (`404`/`409`) — corps verbatim.
/// - `MRD-WORKFLOW-007` (`WorkflowError::InvalidConfig`) : URL invalide ou identifiant hors de la
///   grammaire `inv_` + alphanumérique — refusé **sans requête** (l'identifiant entre nu dans un
///   chemin d'URL).
pub async fn cancel_invocation(
    config: &WorkflowConfig,
    http: &reqwest::Client,
    id: &str,
) -> Result<(), WorkflowError> {
    config.validate()?;
    if !identifiant_invocation_valide(id) {
        return Err(WorkflowError::InvalidConfig(format!(
            "invalid invocation id (expected inv_ followed by alphanumeric characters): {id}"
        )));
    }
    let response = http
        .patch(format!("{}/invocations/{id}/cancel", config.admin_url))
        .send()
        .await
        .map_err(classify_transport_error)?;
    let status = response.status();
    if status.is_success() {
        return Ok(());
    }
    // Branches idempotentes du contrat (./client.sdd, amendement du 2026-11-13), formes mesurées
    // contre `restate-server` 1.8.1 le 2026-11-15 : invocation inconnue → `404` « does not
    // exist », invocation déjà terminée → `409` « already completed ». Les deux → `Ok(())`.
    if matches!(status.as_u16(), 404 | 409) {
        return Ok(());
    }
    let body = response.text().await.map_err(classify_transport_error)?;
    Err(WorkflowError::RestateRejected {
        status: status.as_u16(),
        body,
    })
}

/// Lit le journal d'une invocation (`sys_journal`), ordonné par `index` — canal de lecture des
/// noms d'étapes posés par ./dispatcher.rs (`step:<id>`, `render:<id>` ; preuve live au test
/// `#[ignore]` de [`../../tests/workflow_restate.rs`]).
///
/// # Errors
///
/// Comme [`invocation_statuses`] : transport/rejet/forme (`001`/`002`) et `InvalidConfig`
/// (`007`) pour une URL invalide ou un identifiant hors grammaire, refusé sans requête.
pub async fn invocation_journal(
    config: &WorkflowConfig,
    http: &reqwest::Client,
    id: &str,
) -> Result<Vec<JournalEntry>, WorkflowError> {
    config.validate()?;
    if !identifiant_invocation_valide(id) {
        return Err(WorkflowError::InvalidConfig(format!(
            "invalid invocation id (expected inv_ followed by alphanumeric characters): {id}"
        )));
    }
    let sql = format!("SELECT index, entry_type, name FROM sys_journal WHERE id = '{id}' ORDER BY index");
    let lignes = requete_admin(config, http, &sql).await?;
    let mut entrees = Vec::with_capacity(lignes.len());
    for ligne in lignes {
        let index = ligne
            .get("index")
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .ok_or_else(|| WorkflowError::RestateRejected {
                status: 200,
                body: format!("colonne obligatoire absente: index: {ligne}"),
            })?;
        entrees.push(JournalEntry {
            index,
            kind: colonne_obligatoire(&ligne, "entry_type", 200)?,
            // Forme mesurée (restate 1.8.1) : les entrées sans nom portent la chaîne vide —
            // normalisée en `None`, l'absence de nom du contrat ./client.sdd.
            name: texte_de_ligne(&ligne, "name").filter(|nom| !nom.is_empty()),
        });
    }
    Ok(entrees)
}

/// Grammaire stricte d'un identifiant d'awakeable : préfixe `prom_1` (forme historique documentée
/// par `restate-sdk` 0.12.1) **ou** `sign_1` (forme mesurée sur `restate-server` **1.8.1** le
/// 2026-11-15 — `[!]` du rapport de tâche #26 : le serveur émet `sign_1…`, pas `prom_1…`), suivi
/// d'au moins un caractère base64url (`[A-Za-z0-9_-]`, alphabet URL-safe sans padding).
/// L'identifiant entre nu dans un chemin d'URL : aucun texte libre ne doit y passer (mêmes motifs
/// que [`identifiant_invocation_valide`]).
fn identifiant_awakeable_valide(id: &str) -> bool {
    let corps = id.strip_prefix("prom_1").or_else(|| id.strip_prefix("sign_1"));
    corps.is_some_and(|corps| !corps.is_empty() && corps.chars().all(base64url))
}

/// Caractère base64url : alphanumérique ASCII, `-` ou `_` (aucun padding `=` dans un identifiant
/// Restate).
fn base64url(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '_')
}

/// Poste `value` nue en JSON sur `POST {ingress_url}/restate/awakeables/{id}/resolve`
/// (`content-type: application/json` — forme issue de la doc `restate-sdk` 0.12.1, confirmée par
/// les tests `#[ignore]` de [`../../tests/workflow_restate.rs`] contre le `restate-server` de la
/// `DevStation`, statuts consignés au rapport de tâche #26 avec la version de Restate). Toute
/// réponse `2xx` rend `Ok(())`.
///
/// L'identifiant est un **secret porteur** : quiconque le détient peut résoudre l'awakeable. Ce
/// fichier ne le trace pas (aucune trace ici, contrat du module) et ne le cite dans une erreur
/// que pour un refus de grammaire, avant requête.
///
/// # Errors
///
/// - `MRD-WORKFLOW-001` (`WorkflowError::RestateUnreachable`) : ingress injoignable.
/// - `MRD-WORKFLOW-002` (`WorkflowError::RestateRejected`) : l'ingress répond hors `2xx` — corps
///   verbatim. La résolution d'un awakeable **déjà complet** (résolution tardive ou double) n'est
///   pas un cas d'erreur documenté ici : son comportement est MESURÉ par l'intégration et consigné
///   (./durable.sdd `Tasks`).
/// - `MRD-WORKFLOW-007` (`WorkflowError::InvalidConfig`) : URL invalide, ou identifiant hors de la
///   grammaire `prom_1` + base64url — refusé **sans requête**.
pub async fn resolve_awakeable(
    config: &WorkflowConfig,
    http: &reqwest::Client,
    id: &str,
    value: &serde_json::Value,
) -> Result<(), WorkflowError> {
    awakeable_par_ingress(config, http, id, "resolve", value.to_string(), "application/json").await
}

/// Rejette un awakeable en attente par l'ingress — `POST {ingress_url}/restate/awakeables/{id}/reject`,
/// `content-type: text/plain`, message nu (forme issue de la doc `restate-sdk` 0.12.1, confirmée
/// par les tests `#[ignore]` de [`../../tests/workflow_restate.rs`] ; le message transite verbatim
/// jusqu'au step en attente, ./durable.sdd).
///
/// # Errors
///
/// Comme [`resolve_awakeable`] (`001`/`002`/`007`) : le message n'entre dans aucune erreur de ce
/// fichier.
pub async fn reject_awakeable(
    config: &WorkflowConfig,
    http: &reqwest::Client,
    id: &str,
    message: &str,
) -> Result<(), WorkflowError> {
    awakeable_par_ingress(config, http, id, "reject", message.to_string(), "text/plain").await
}

/// Corps commun des deux opérations d'awakeable sur l'ingress : validation config + grammaire de
/// l'identifiant avant toute requête, `POST {ingress}/restate/awakeables/{id}/{action}`, corps nu
/// au `content-type` donné, tout `2xx` → `Ok(())`, hors `2xx` → `RestateRejected` verbatim.
async fn awakeable_par_ingress(
    config: &WorkflowConfig,
    http: &reqwest::Client,
    id: &str,
    action: &str,
    corps: String,
    content_type: &str,
) -> Result<(), WorkflowError> {
    config.validate()?;
    if !identifiant_awakeable_valide(id) {
        return Err(WorkflowError::InvalidConfig(format!(
            "invalid awakeable id (expected prom_1 followed by base64url characters): {id}"
        )));
    }
    let response = http
        .post(format!("{}/restate/awakeables/{id}/{action}", config.ingress_url))
        .header(reqwest::header::CONTENT_TYPE, content_type)
        .body(corps)
        .send()
        .await
        .map_err(classify_transport_error)?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.map_err(classify_transport_error)?;
        return Err(WorkflowError::RestateRejected {
            status: status.as_u16(),
            body,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::InvocationFilter;
    use super::JournalEntry;
    use super::RunHandle;
    use super::WorkflowConfig;
    use super::cancel_invocation;
    use super::invocation_journal;
    use super::invocation_statuses;
    use super::list_invocations;
    use super::register_deployment;
    use super::reject_awakeable;
    use super::resolve_awakeable;
    use super::trigger_run;
    use crate::workflow::definition::DagSteps;
    use crate::workflow::definition::StepDefinition;
    use crate::workflow::error::WorkflowError;
    use serde_json::Value;
    use std::io::Read as _;
    use std::io::Write as _;
    use std::net::TcpListener;
    use std::net::TcpStream;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::time::Duration;
    use uuid::Uuid;
    use uuid::Version;

    /// `deployment_url` de fixture — une forme de `Service` Kubernetes stable, aucune validation
    /// de forme n'étant attendue de ./client.rs (./client.sdd, `Accepts`).
    const DEPLOYMENT_URL: &str = "http://miryad-app.restate-callback.svc.cluster.local:9080";

    /// Corps de réponse `202` observé au spike Restate.
    const SEND_ACCEPTED_BODY: &str = "{\"invocationId\":\"inv_test123\",\"status\":\"Accepted\"}";

    // ---------------------------------------------------------------------------
    // Fixture : un vrai @DagSteps (./definition.rs). Le `Scenario` « le corps envoyé à
    // `trigger_run` est le `DagSteps` tel quel, sans enveloppe » l'exige dans son type réel —
    // la rive de la signature (./client.sdd `Tasks`) a retiré le générique. ./client.rs ne
    // valide pas le DAG, il le sérialise seulement ; la fixture reste structurellement valide
    // (aucune arête pendante, aucun cycle) pour refléter l'`Accepts` contractuel.
    // ---------------------------------------------------------------------------

    fn dag_fixture() -> DagSteps {
        DagSteps(vec![
            StepDefinition {
                id: "deploy".to_string(),
                depends_on: vec![],
                kind: "rhai".to_string(),
                config: serde_json::json!({ "script": "1 + 1" }),
                when: None,
                run_if_skipped: false,
                exports: std::collections::BTreeMap::new(),
            },
            StepDefinition {
                id: "notify".to_string(),
                depends_on: vec!["deploy".to_string()],
                kind: "rhai".to_string(),
                config: serde_json::json!({ "channel": "ops" }),
                when: None,
                run_if_skipped: false,
                exports: std::collections::BTreeMap::new(),
            },
        ])
    }

    /// DAG d'**un step** du Scenario « `trigger_run` envoie le DAG et les vars » (lot B) — le plus
    /// petit DAG structurellement valide, pour lire le corps capturé sans bruit.
    fn dag_fixture_un_step() -> DagSteps {
        DagSteps(vec![StepDefinition {
            id: "deploy".to_string(),
            depends_on: vec![],
            kind: "rhai".to_string(),
            config: serde_json::json!({ "script": "1 + 1" }),
            when: None,
            run_if_skipped: false,
            exports: std::collections::BTreeMap::new(),
        }])
    }

    // ---------------------------------------------------------------------------
    // Serveur HTTP de test : purs threads + TcpListener (même sobriété que ./error.rs,
    // pas de wiremock). Capture méthode, URI, en-têtes (clé en minuscules) et corps de
    // chaque requête reçue ; répond un statut et un corps programmés, une requête par
    // connexion (`connection: close`), en boucle d'acceptation pour les suites de deux
    // appels.
    // ---------------------------------------------------------------------------

    #[derive(Clone, Debug)]
    struct CapturedRequest {
        method: String,
        uri: String,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    }

    impl CapturedRequest {
        fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.as_str())
        }
    }

    struct StubServer {
        port: u16,
        requests: Arc<Mutex<Vec<CapturedRequest>>>,
    }

    impl StubServer {
        fn spawn(status: u16, body: String) -> StubServer {
            let Ok(listener) = TcpListener::bind("127.0.0.1:0") else {
                panic!("impossible de binder un port libre sur 127.0.0.1");
            };
            let Ok(addr) = listener.local_addr() else {
                panic!("adresse locale indisponible");
            };
            let port = addr.port();
            let requests = Arc::new(Mutex::new(Vec::new()));
            let shared = Arc::clone(&requests);
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { return };
                    let Ok(()) = stream.set_read_timeout(Some(Duration::from_secs(10))) else {
                        continue;
                    };
                    let Some(request) = read_one_request(&mut stream) else {
                        continue;
                    };
                    shared.lock().unwrap().push(request);
                    let response = format!(
                        "HTTP/1.1 {status} OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    if stream.write_all(response.as_bytes()).is_err() {
                        return;
                    }
                    let _ = stream.flush();
                }
            });
            StubServer { port, requests }
        }

        fn url(&self) -> String {
            format!("http://127.0.0.1:{}", self.port)
        }

        /// `WorkflowConfig` pointant ce stub comme admin API **et** comme ingress ; les deux
        /// fonctions n'exercent chacune qu'un seul de ces deux champs par scenario.
        fn config(&self) -> WorkflowConfig {
            WorkflowConfig {
                admin_url: self.url(),
                ingress_url: self.url(),
                deployment_url: DEPLOYMENT_URL.to_string(),
            }
        }

        fn requests(&self) -> Vec<CapturedRequest> {
            self.requests.lock().unwrap().clone()
        }
    }

    /// Lit exactement une requête HTTP (head jusqu'à `\r\n\r\n`, corps selon
    /// `content-length`) ; `None` si le flux se coupe avant la fin de la requête.
    fn read_one_request(stream: &mut TcpStream) -> Option<CapturedRequest> {
        let mut buffer: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 1024];
        let head_end = loop {
            if buffer.len() > 1024 * 1024 {
                return None;
            }
            let Ok(read) = stream.read(&mut chunk) else {
                return None;
            };
            if read == 0 {
                return None;
            }
            buffer.extend_from_slice(&chunk[..read]);
            if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
                break position;
            }
        };
        let head = std::str::from_utf8(&buffer[..head_end]).ok()?;
        let mut lines = head.split("\r\n");
        let mut request_line = lines.next()?.split(' ');
        let method = request_line.next()?.to_string();
        let uri = request_line.next()?.to_string();
        let mut headers = Vec::new();
        let mut content_length = 0usize;
        for line in lines {
            let Some((name, value)) = line.split_once(':') else {
                continue;
            };
            let name = name.trim().to_lowercase();
            if name == "content-length" {
                content_length = value.trim().parse().ok()?;
            }
            headers.push((name, value.trim().to_string()));
        }
        let total = head_end + 4 + content_length;
        while buffer.len() < total {
            let Ok(read) = stream.read(&mut chunk) else {
                return None;
            };
            if read == 0 {
                return None;
            }
            buffer.extend_from_slice(&chunk[..read]);
        }
        Some(CapturedRequest {
            method,
            uri,
            headers,
            body: buffer[head_end + 4..total].to_vec(),
        })
    }

    /// Se lie à un port libre puis le relâche : toute connexion suivante est refusée — le
    /// « serveur Restate down » des scenarios (même helper que ./error.rs, modules de tests
    /// séparés donc redéclaration locale).
    fn closed_port() -> u16 {
        let Ok(listener) = TcpListener::bind("127.0.0.1:0") else {
            panic!("impossible de binder un port libre sur 127.0.0.1");
        };
        let Ok(addr) = listener.local_addr() else {
            panic!("adresse locale indisponible");
        };
        drop(listener);
        addr.port()
    }

    /// Client reqwest sortant borné par un timeout, pour qu'aucun test ne suspende la suite.
    fn test_http() -> reqwest::Client {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("le client reqwest de test doit se construire")
    }

    /// Extrait la clé de run d'une URI capturée sous la forme contractée
    /// `/DagInterpreter/{run_key}/run/send`, `None` sinon.
    fn captured_run_key(request: &CapturedRequest) -> Option<&str> {
        request
            .uri
            .strip_prefix("/DagInterpreter/")?
            .strip_suffix("/run/send")
    }

    // ---------------------------------------------------------------------------
    // register_deployment
    // ---------------------------------------------------------------------------

    /// Scenario « enregistrement réussi » : `200` → `Ok(())`, et la requête capturée est
    /// exactement `POST /deployments`, corps `{"uri":"<deployment_url>","force":true}` octet
    /// pour octet, `content-type: application/json`.
    #[tokio::test]
    async fn register_deployment_reussi_corps_force_true() {
        let stub = StubServer::spawn(200, "{\"deploymentId\":\"dp_ZQ6q0J\"}".to_string());
        let config = stub.config();
        let result = register_deployment(&config, &test_http()).await;
        assert!(
            matches!(result, Ok(())),
            "le 200 devait rendre Ok(()) : {result:?}"
        );

        let requests = stub.requests();
        assert_eq!(
            requests.len(),
            1,
            "une seule requête POST /deployments était attendue"
        );
        let request = requests.first().expect("la requête capturée doit exister");
        assert_eq!(request.method, "POST");
        assert_eq!(request.uri, "/deployments");
        assert_eq!(request.header("content-type"), Some("application/json"));
        let expected_body = format!("{{\"uri\":\"{DEPLOYMENT_URL}\",\"force\":true}}");
        assert_eq!(
            String::from_utf8_lossy(&request.body),
            expected_body,
            "le corps doit être exactement uri puis force, compact"
        );
        assert!(
            request.header("idempotency-key").is_none(),
            "./client.rs ne pose jamais d'Idempotency-Key"
        );
    }

    /// Scenario « le schéma changé n'est repris qu'avec force: true » : le champ `force: true`
    /// est posé explicitement à chaque appel, dans tous les corps capturés — aucune branche
    /// d'omission n'existe.
    #[tokio::test]
    async fn register_deployment_force_toujours_pose() {
        let stub = StubServer::spawn(200, "{\"deploymentId\":\"dp_1\",\"revision\":2}".to_string());
        let config = stub.config();
        let http = test_http();
        register_deployment(&config, &http)
            .await
            .expect("le premier 200 devait rendre Ok(())");
        register_deployment(&config, &http)
            .await
            .expect("le second 200 devait rendre Ok(())");

        let requests = stub.requests();
        assert_eq!(requests.len(), 2, "deux appels = deux requêtes capturées");
        for request in &requests {
            let value: serde_json::Value =
                serde_json::from_slice(&request.body).expect("le corps doit être un JSON objet");
            assert_eq!(
                value.get("force"),
                Some(&serde_json::Value::Bool(true)),
                "force doit être posé explicitement à true sur chaque POST /deployments"
            );
            assert_eq!(
                value.get("uri"),
                Some(&serde_json::Value::String(DEPLOYMENT_URL.to_string()))
            );
        }
    }

    /// Scenario « enregistrement rejeté par l'admin API » : `400` avec corps
    /// `{"message":"invalid uri"}` → `RestateRejected { status: 400, body verbatim }`.
    #[tokio::test]
    async fn register_deployment_rejet_400() {
        let stub = StubServer::spawn(400, "{\"message\":\"invalid uri\"}".to_string());
        let Err(err) = register_deployment(&stub.config(), &test_http()).await else {
            panic!("le statut 400 devait rendre un Err");
        };
        let WorkflowError::RestateRejected { status, body } = err else {
            panic!("variante attendue RestateRejected : {err:?}");
        };
        assert_eq!(status, 400);
        assert_eq!(body, "{\"message\":\"invalid uri\"}");
    }

    /// Scenario « enregistrement sur serveur injoignable » : connexion refusée sur
    /// `admin_url` → `RestateUnreachable` (`MRD-WORKFLOW-001`).
    #[tokio::test]
    async fn register_deployment_injoignable() {
        let config = WorkflowConfig {
            admin_url: format!("http://127.0.0.1:{}", closed_port()),
            ingress_url: "http://127.0.0.1:80".to_string(),
            deployment_url: DEPLOYMENT_URL.to_string(),
        };
        let Err(err) = register_deployment(&config, &test_http()).await else {
            panic!("un port refusant la connexion devait rendre un Err");
        };
        assert!(
            matches!(err, WorkflowError::RestateUnreachable(_)),
            "variante attendue RestateUnreachable : {err:?}"
        );
        assert!(
            err.to_string()
                .starts_with("MRD-WORKFLOW-001: restate unreachable: "),
            "rendu inattendu : {err}"
        );
    }

    // ---------------------------------------------------------------------------
    // trigger_run
    // ---------------------------------------------------------------------------

    /// Scenario « déclenchement réussi génère une clé neuve et poste sur /send » : deux appels
    /// avec la même fixture produisent deux URIs `/DagInterpreter/{run_key}/run/send` aux clés
    /// distinctes (UUID v4 hyphéné), et chaque `RunHandle` porte la clé de SON appel et
    /// l'`invocationId` du CORPS de réponse — sans jamais poser d'`Idempotency-Key`.
    #[tokio::test]
    async fn trigger_run_cle_neuve_et_send() {
        let stub = StubServer::spawn(202, SEND_ACCEPTED_BODY.to_string());
        let config = stub.config();
        let http = test_http();
        let dag = dag_fixture();
        let first = trigger_run(&config, &http, &dag, &serde_json::Map::new(), None)
            .await
            .expect("le 202 devait rendre Ok(RunHandle)");
        let second = trigger_run(&config, &http, &dag, &serde_json::Map::new(), None)
            .await
            .expect("le second 202 devait rendre Ok(RunHandle)");

        let requests = stub.requests();
        assert_eq!(requests.len(), 2, "deux appels = deux requêtes capturées");
        let mut keys = Vec::new();
        for request in &requests {
            assert_eq!(request.method, "POST");
            let key = captured_run_key(request).expect(
                "l'URI doit être /DagInterpreter/{run_key}/run/send — jamais la forme bloquante sans /send",
            );
            keys.push(key.to_string());
            assert!(
                request.header("idempotency-key").is_none(),
                "./client.rs ne pose jamais d'Idempotency-Key (clé de run neuve à chaque appel)"
            );
        }
        assert_ne!(
            keys.first().expect("première clé"),
            keys.get(1).expect("seconde clé"),
            "deux appels ne doivent jamais partager la même clé"
        );
        for (handle, key) in [(first, &keys[0]), (second, &keys[1])] {
            assert_eq!(
                handle.run_key, *key,
                "le run_key rendu doit être la clé générée pour CET appel"
            );
            assert_eq!(
                handle.invocation_id, "inv_test123",
                "invocation_id doit être lu dans le CORPS de réponse (pas d'en-tête émis par le stub)"
            );
            let parsed = Uuid::parse_str(key).expect("run_key doit être un UUID v4");
            assert_eq!(&parsed.to_string(), key, "format hyphéné standard attendu");
            assert!(
                matches!(parsed.get_version(), Some(Version::Random)),
                "UUID v4 (Random) attendu"
            );
        }
    }

    /// Scenario « déclenchement sur service non enregistré » : `404` avec le corps exact
    /// observé au spike → `RestateRejected { status: 404, body verbatim }`.
    #[tokio::test]
    async fn trigger_run_service_inconnu_404() {
        let spike_body = "{\"code\":404,\"message\":\"service 'DagInterpreter' not found, \
                          make sure to register the service before calling it.\",\"source\":\"ingress\"}";
        let stub = StubServer::spawn(404, spike_body.to_string());
        let Err(err) = trigger_run(
            &stub.config(),
            &test_http(),
            &dag_fixture(),
            &serde_json::Map::new(),
            None,
        )
        .await
        else {
            panic!("le statut 404 devait rendre un Err");
        };
        let WorkflowError::RestateRejected { status, body } = err else {
            panic!("variante attendue RestateRejected : {err:?}");
        };
        assert_eq!(status, 404);
        assert_eq!(body, spike_body);
    }

    /// Scenario « déclenchement avec réponse au format inattendu » : `202` avec
    /// `{"status":"Accepted"}` seul (pas d'`invocationId`) → `Serialization`
    /// (`MRD-WORKFLOW-003`), jamais un `RunHandle` fabricqué.
    #[tokio::test]
    async fn trigger_run_reponse_sans_invocation_id() {
        let stub = StubServer::spawn(202, "{\"status\":\"Accepted\"}".to_string());
        let Err(err) = trigger_run(
            &stub.config(),
            &test_http(),
            &dag_fixture(),
            &serde_json::Map::new(),
            None,
        )
        .await
        else {
            panic!("un 202 sans invocationId devait rendre un Err");
        };
        assert!(
            matches!(err, WorkflowError::Serialization(_)),
            "variante attendue Serialization : {err:?}"
        );
        assert!(
            err.to_string()
                .starts_with("MRD-WORKFLOW-003: could not decode restate response: "),
            "rendu inattendu : {err}"
        );
    }

    /// Scenario « le corps envoyé à `trigger_run` est le `DagSteps` tel quel, sans enveloppe »,
    /// passé à la forme `RunInput` (amendement 2026-10-30, lot B, tâche B5 — ./interpreter.sdd
    /// « Entrée du run ») : le corps capturé est l'objet `{"dag": [..steps..], "vars": {}}` —
    /// régression verrouillée : **plus** le tableau nu des steps — et le champ `dag` est la
    /// fixture telle quelle, re-désérialisable octet pour octet en elle : rien d'ajouté au
    /// tableau de steps (pas de `run_key`, il ne vit que dans l'URI), rien de retiré.
    #[tokio::test]
    async fn corps_envoye_sans_enveloppe() {
        let stub = StubServer::spawn(202, SEND_ACCEPTED_BODY.to_string());
        let dag = dag_fixture();
        let handle: RunHandle =
            trigger_run(&stub.config(), &test_http(), &dag, &serde_json::Map::new(), None)
                .await
                .expect("le 202 devait rendre Ok(RunHandle)");

        let requests = stub.requests();
        assert_eq!(requests.len(), 1, "une seule requête était attendue");
        let request = requests.first().expect("la requête capturée doit exister");
        assert!(
            request.uri.ends_with("/run/send"),
            "URI capturée : {}",
            request.uri
        );

        let value: serde_json::Value =
            serde_json::from_slice(&request.body).expect("le corps doit être un JSON");
        assert!(
            value.as_array().is_none(),
            "régression lot B : le corps n'est plus un tableau nu de steps, c'est l'objet RunInput {{dag, vars}}"
        );
        let mut top_keys: Vec<&str> = value
            .as_object()
            .expect("le corps est un objet JSON RunInput")
            .keys()
            .map(String::as_str)
            .collect();
        top_keys.sort_unstable();
        assert_eq!(
            top_keys,
            vec!["dag", "vars"],
            "le corps porte exactement dag puis vars — aucun champ ajouté (pas de run_key, il ne vit que dans l'URI)"
        );
        assert_eq!(
            value.get("vars"),
            Some(&serde_json::json!({})),
            "vars vide sérialisé en objet JSON vide"
        );

        let dag_value = value.get("dag").expect("le corps porte le champ dag");
        let round_trip: DagSteps = serde_json::from_value(dag_value.clone())
            .expect("le champ dag doit se re-désérialiser en la fixture");
        assert_eq!(round_trip, dag, "aucun champ retiré du DAG");
        let steps = dag_value
            .as_array()
            .expect("le champ dag est le tableau nu des steps, sans enveloppe interne");
        assert_eq!(steps.len(), 2, "la fixture a deux steps");
        for step in steps {
            let object = step.as_object().expect("un step est un objet JSON");
            assert!(
                object.get("run_key").is_none(),
                "aucun run_key dans le corps — la clé ne vit que dans l'URI"
            );
            let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
            keys.sort_unstable();
            assert_eq!(
                keys,
                vec!["config", "depends_on", "id", "kind"],
                "aucun champ ajouté aux quatre champs du step"
            );
        }
        assert!(
            !handle.run_key.is_empty(),
            "une clé neuve est générée et rendue, elle ne passe pas par le corps"
        );
    }

    /// Scenario « `trigger_run` envoie le DAG et les vars (2026-10-30, lot B) » : le corps capturé
    /// est exactement `{"dag": [<le step>], "vars": {"project": "p1"}}` — le DAG et les `vars`
    /// côte à côte dans l'objet `RunInput`, le DAG sous sa forme de tableau nu tel que
    /// `DagSteps` le sérialise, les `vars` transmises sans transformation (./client.sdd `Must`,
    /// amendement lot B) — et avec `vars` vide le corps porte `"vars": {}`, jamais un champ omis.
    #[tokio::test]
    async fn trigger_run_envoie_dag_et_vars() {
        let stub = StubServer::spawn(202, SEND_ACCEPTED_BODY.to_string());
        let config = stub.config();
        let http = test_http();
        let dag = dag_fixture_un_step();
        let mut vars = serde_json::Map::new();
        vars.insert("project".to_string(), serde_json::json!("p1"));
        trigger_run(&config, &http, &dag, &vars, None)
            .await
            .expect("le 202 devait rendre Ok(RunHandle)");
        trigger_run(&config, &http, &dag, &serde_json::Map::new(), None)
            .await
            .expect("le second 202 devait rendre Ok(RunHandle)");

        let requests = stub.requests();
        assert_eq!(requests.len(), 2, "deux appels = deux requêtes capturées");
        let dag_bytes = serde_json::to_vec(&dag).expect("le DAG de fixture doit être sérialisable");
        let corps_attendu = |vars_json: &str| -> String {
            format!(
                "{{\"dag\":{},\"vars\":{vars_json}}}",
                String::from_utf8_lossy(&dag_bytes)
            )
        };
        let avec_vars = String::from_utf8_lossy(
            &requests
                .first()
                .expect("la première requête capturée doit exister")
                .body,
        )
        .to_string();
        assert_eq!(
            avec_vars,
            corps_attendu("{\"project\":\"p1\"}"),
            "le corps doit être exactement dag puis vars, compacts — les vars transmises sans transformation"
        );
        let sans_vars = String::from_utf8_lossy(
            &requests
                .get(1)
                .expect("la seconde requête capturée doit exister")
                .body,
        )
        .to_string();
        assert_eq!(
            sans_vars,
            corps_attendu("{}"),
            "vars vide : le corps porte \"vars\": {{}}, jamais le champ retiré"
        );
    }

    /// Scenario « `trigger_run` pose le trigger dans le corps, et l'omet sans déclencheur
    /// (2026-11-02, lot 0 lymaille) » : avec `trigger = Some("sub-42")` et `vars` vides le corps
    /// capturé est exactement `{"dag": [<le step>], "vars": {}, "trigger": "sub-42"}` — l'ordre
    /// des clés porté par la sérialisation serde de `RunInput` : dag, vars, trigger
    /// (./interpreter.sdd) ; avec `trigger = None` le corps est `{"dag": [<le step>], "vars": {}}`
    /// **sans clé `trigger`** (`skip_serializing_if` de `RunInput`, T4). La chaîne est transmise
    /// telle quelle dans le corps, jamais en en-tête.
    #[tokio::test]
    async fn trigger_run_pose_trigger_dans_corps_et_l_omet_sans_declencheur() {
        let stub = StubServer::spawn(202, SEND_ACCEPTED_BODY.to_string());
        let config = stub.config();
        let http = test_http();
        let dag = dag_fixture_un_step();
        trigger_run(&config, &http, &dag, &serde_json::Map::new(), Some("sub-42"))
            .await
            .expect("le 202 devait rendre Ok(RunHandle)");
        trigger_run(&config, &http, &dag, &serde_json::Map::new(), None)
            .await
            .expect("le second 202 devait rendre Ok(RunHandle)");

        let requests = stub.requests();
        assert_eq!(requests.len(), 2, "deux appels = deux requêtes capturées");
        let dag_bytes = serde_json::to_vec(&dag).expect("le DAG de fixture doit être sérialisable");
        let avec_trigger = String::from_utf8_lossy(
            &requests
                .first()
                .expect("la première requête capturée doit exister")
                .body,
        )
        .to_string();
        assert_eq!(
            avec_trigger,
            format!(
                "{{\"dag\":{},\"vars\":{{}},\"trigger\":\"sub-42\"}}",
                String::from_utf8_lossy(&dag_bytes)
            ),
            "le corps doit être exactement dag, vars puis trigger, compacts — l'ordre serde de RunInput"
        );
        let premier: serde_json::Value =
            serde_json::from_str(&avec_trigger).expect("le premier corps capturé doit être un JSON");
        assert_eq!(
            premier.get("trigger"),
            Some(&serde_json::json!("sub-42")),
            "le trigger est transmis intact dans le corps, jamais en en-tête"
        );
        assert!(
            requests
                .first()
                .expect("la première requête capturée doit exister")
                .headers
                .iter()
                .all(|(_, value)| !value.contains("sub-42")),
            "le trigger voyage dans le corps, jamais dans un en-tête (./client.sdd, amendement T5)"
        );

        let sans_trigger = String::from_utf8_lossy(
            &requests
                .get(1)
                .expect("la seconde requête capturée doit exister")
                .body,
        )
        .to_string();
        assert_eq!(
            sans_trigger,
            format!(
                "{{\"dag\":{},\"vars\":{{}}}}",
                String::from_utf8_lossy(&dag_bytes)
            ),
            "trigger None : le corps porte \"vars\": {{}} et aucune clé trigger (skip_serializing_if)"
        );
        let second: serde_json::Value =
            serde_json::from_str(&sans_trigger).expect("le second corps capturé doit être un JSON");
        assert!(
            second.get("trigger").is_none(),
            "aucune clé trigger, pas même nulle, quand le déclencheur est None"
        );
    }

    // ---------------------------------------------------------------------------
    // Validation de WorkflowConfig (arbitré 2026-09-29)
    // ---------------------------------------------------------------------------

    /// Scenario « URL d'admin vide refusée avant tout appel réseau » : `admin_url` vide →
    /// `InvalidConfig` (`MRD-WORKFLOW-007`) nommant `admin_url`. Preuve de l'absence d'appel
    /// réseau : la seule adresse joignable de cette config (l'`ingress_url` pointant sur un
    /// listener vivant) n'enregistre **aucune** connexion, et la variante `InvalidConfig` est
    /// la seule que `validate` construit — le chemin HTTP n'a donc jamais été entré (il n'en
    /// sort que `RestateUnreachable`/`Rejected`/`Serialization`, aucun `reqwest::Error` étant
    /// impliqué ici, ./error.sdd `Raises`).
    #[tokio::test]
    async fn register_deployment_url_admin_vide_refusee_sans_appel_reseau() {
        let stub = StubServer::spawn(200, "{\"deploymentId\":\"dp_unused\"}".to_string());
        let config = WorkflowConfig {
            admin_url: String::new(),
            ingress_url: stub.url(),
            deployment_url: DEPLOYMENT_URL.to_string(),
        };
        let Err(err) = register_deployment(&config, &test_http()).await else {
            panic!("un admin_url vide devait rendre Err(InvalidConfig)");
        };
        assert!(
            matches!(err, WorkflowError::InvalidConfig(_)),
            "variante attendue InvalidConfig : {err:?}"
        );
        let rendered = err.to_string();
        assert!(
            rendered.starts_with("MRD-WORKFLOW-007: invalid workflow configuration: "),
            "rendu inattendu : {rendered}"
        );
        assert!(
            rendered.contains("admin_url"),
            "le message doit nommer le champ fautif : {rendered}"
        );
        assert!(
            stub.requests().is_empty(),
            "le listener vivant sur le port n'aurait dû voir aucune connexion"
        );
    }

    /// Scenario « URL non parsable ou de schéma inattendu refusée », second volet :
    /// `admin_url` vaut `ftp://restate:9070` et `ingress_url` est parfaitement valide (elle
    /// pointe un stub vivant) — `trigger_run` doit refuser la config **sans appel réseau**.
    /// Sans validation en tête, `trigger_run` ne lit aujourd'hui que `ingress_url` et posterait
    /// sur le stub : le test échoue dès qu'une connexion est capturée.
    #[tokio::test]
    async fn trigger_run_url_admin_ftp_refusee_sans_appel_reseau() {
        let stub = StubServer::spawn(202, SEND_ACCEPTED_BODY.to_string());
        let config = WorkflowConfig {
            admin_url: "ftp://restate:9070".to_string(),
            ingress_url: stub.url(),
            deployment_url: DEPLOYMENT_URL.to_string(),
        };
        let Err(err) = trigger_run(
            &config,
            &test_http(),
            &dag_fixture(),
            &serde_json::Map::new(),
            None,
        )
        .await
        else {
            panic!("un admin_url de schéma ftp devait rendre Err(InvalidConfig), ingress valide ou non");
        };
        assert!(
            matches!(err, WorkflowError::InvalidConfig(_)),
            "variante attendue InvalidConfig : {err:?}"
        );
        let rendered = err.to_string();
        assert!(
            rendered.starts_with("MRD-WORKFLOW-007: invalid workflow configuration: "),
            "rendu inattendu : {rendered}"
        );
        assert!(
            rendered.contains("admin_url"),
            "le message doit nommer le champ fautif : {rendered}"
        );
        assert!(
            stub.requests().is_empty(),
            "aucun appel réseau : le stub ingresseur sur le port n'aurait rien dû capturer"
        );
    }

    /// Scenario « URL non parsable ou de schéma inattendu refusée », premier volet :
    /// `WorkflowConfig::validate` appelée sur `ingress_url` = `pas une url`, puis sur
    /// `admin_url` = `ftp://restate:9070` — chacune rend `Err(InvalidConfig)` (`MRD-WORKFLOW-007`)
    /// nommant le champ fautif. Aucune de ces deux configs n'est jamais jointe (`validate` est
    /// pure, sans I/O) : les hôtes `restate-admin`/`restate-ingress` ne sont jamais résolus.
    #[test]
    fn validate_refuse_url_non_parsable_et_schema_ftp() {
        let non_parsable = WorkflowConfig {
            admin_url: "https://restate-admin:9070".to_string(),
            ingress_url: "pas une url".to_string(),
            deployment_url: DEPLOYMENT_URL.to_string(),
        };
        let Err(err) = non_parsable.validate() else {
            panic!("`pas une url` en ingress_url devait rendre Err(InvalidConfig)");
        };
        assert!(
            matches!(err, WorkflowError::InvalidConfig(_)),
            "variante attendue InvalidConfig : {err:?}"
        );
        let rendered = err.to_string();
        assert!(
            rendered.starts_with("MRD-WORKFLOW-007: invalid workflow configuration: ")
                && rendered.contains("ingress_url"),
            "le message doit porter le code et nommer ingress_url : {rendered}"
        );

        let schema_ftp = WorkflowConfig {
            admin_url: "ftp://restate:9070".to_string(),
            ingress_url: "https://restate-ingress:9071".to_string(),
            deployment_url: DEPLOYMENT_URL.to_string(),
        };
        let Err(err) = schema_ftp.validate() else {
            panic!("`ftp://restate:9070` en admin_url devait rendre Err(InvalidConfig)");
        };
        assert!(
            matches!(err, WorkflowError::InvalidConfig(_)),
            "variante attendue InvalidConfig : {err:?}"
        );
        let rendered = err.to_string();
        assert!(
            rendered.starts_with("MRD-WORKFLOW-007: invalid workflow configuration: ")
                && rendered.contains("admin_url"),
            "le message doit porter le code et nommer admin_url : {rendered}"
        );
    }

    /// Scenario « `deployment_url` n'est pas validée » : une config valide dont
    /// `deployment_url` vaut `n'importe quoi` passe `validate` (`Ok`), et le contrat existant
    /// de 2026-09-23 tient côté appel — l'URL absurde traverse la validation et va au réseau :
    /// `register_deployment` atteint le stub, dont le corps capturé porte l'uri verbatim.
    #[tokio::test]
    async fn validate_ignore_totalement_deployment_url() {
        let stub = StubServer::spawn(200, "{\"deploymentId\":\"dp_X\"}".to_string());
        let config = WorkflowConfig {
            admin_url: stub.url(),
            ingress_url: stub.url(),
            deployment_url: "n'importe quoi".to_string(),
        };
        config
            .validate()
            .expect("la forme de deployment_url reste à la charge de l'application");

        register_deployment(&config, &test_http())
            .await
            .expect("une deployment_url absurde avec une config valide passe et va au réseau");
        let requests = stub.requests();
        assert_eq!(requests.len(), 1, "la requête a bien été postée au stub");
        let request = requests.first().expect("une requête capturée");
        assert_eq!(
            String::from_utf8_lossy(&request.body),
            "{\"uri\":\"n'importe quoi\",\"force\":true}",
            "le stub reçoit l'uri absurde verbatim — ./client.rs ne la restreint pas"
        );
    }

    // ---------------------------------------------------------------------------
    // Client d'administration Restate (issue #80 (2), amendement du 2026-11-13)
    //
    // Les formes de la requête et des colonnes (`POST /query` corps `{"query": …}`,
    // `Accept: application/json`, réponse `{"rows": […]}`, colonnes `sys_invocation`
    // `id / status / target_service_name / target_handler_name / target_service_key /
    // created_at / completed_at / retry_count / completion_failure`, colonnes `sys_journal`
    // `index / entry_type / name`) sont mesurées contre un `restate-server` **1.8.1** (podman,
    // 2026-11-15) ; la preuve en live est le test `#[ignore]` de
    // `../../tests/workflow_restate.rs`.
    // ---------------------------------------------------------------------------

    /// Ligne `sys_invocation` de fixture, dans la forme JSON mesurée sur `restate-server` 1.8.1
    /// (une clé par colonne sélectionnée, valeurs nulles littérales `null`).
    fn ligne_invocation(id: &str, statut: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "status": statut,
            "target_service_name": "DagInterpreter",
            "target_handler_name": "run",
            "target_service_key": null,
            "created_at": "2026-11-15T07:00:00.000Z",
            "completed_at": null,
            "retry_count": 0,
            "completion_failure": null,
        })
    }

    /// Réponse `{"rows": […]}` de l'admin API, sérialisée pour le stub.
    fn corps_rows(lignes: &[serde_json::Value]) -> String {
        serde_json::json!({ "rows": lignes }).to_string()
    }

    /// Scenario « le statut de cinquante invocations se lit en une requête » : cinquante
    /// identifiants → **une seule** `POST /query` dont le corps les porte tous ; un cinquante-
    /// unième déclenche une seconde requête (lot de 50 au plus). Vérification de la requête :
    /// méthode `POST`, URI `/query`, en-tête `accept: application/json`, corps `{"query": …}`
    /// avec les identifiants cités entre quotes simples.
    #[tokio::test]
    async fn invocation_statuses_cinquante_ids_une_seule_requête() {
        let stub = StubServer::spawn(200, corps_rows(&[]));
        let config = stub.config();
        let http = test_http();
        let ids: Vec<String> = (0..50).map(|i| format!("inv_a{i:02}")).collect();
        invocation_statuses(&config, &http, &ids)
            .await
            .expect("le 200 `rows` vide devait rendre un Vec vide");
        let requests = stub.requests();
        assert_eq!(requests.len(), 1, "cinquante ids = une seule requête");
        let request = requests.first().expect("une requête capturée");
        assert_eq!(request.method, "POST");
        assert_eq!(request.uri, "/query");
        assert_eq!(request.header("accept"), Some("application/json"));
        let corps: serde_json::Value = serde_json::from_slice(&request.body).expect("le corps est un JSON");
        let requete = corps.get("query").and_then(Value::as_str).expect("`query`");
        assert!(requete.contains("sys_invocation"), "{requete}");
        assert!(requete.contains("WHERE id IN ("), "{requete}");
        for id in &ids {
            assert!(
                requete.contains(&format!("'{id}'")),
                "l'identifiant {id} doit figurer dans l'IN de la requête unique"
            );
        }

        // Soixante-et-onze appels plus tard… ici l'unique branche de découpage : 51 ids → deux
        // requêtes (50 puis 1).
        let ids51: Vec<String> = (0..51).map(|i| format!("inv_b{i:02}")).collect();
        invocation_statuses(&config, &http, &ids51)
            .await
            .expect("le 200 `rows` vide devait rendre un Vec vide");
        let requests = stub.requests();
        assert_eq!(requests.len(), 3, "51 ids = deux requêtes de plus (50 puis 1)");
        let secondes: Vec<String> = requests[1..]
            .iter()
            .map(|r| {
                let corps: serde_json::Value = serde_json::from_slice(&r.body).expect("le corps est un JSON");
                corps
                    .get("query")
                    .and_then(Value::as_str)
                    .expect("`query`")
                    .to_string()
            })
            .collect();
        assert_eq!(
            secondes.iter().filter(|q| q.contains("inv_b00")).count()
                + secondes.iter().filter(|q| q.contains("inv_b50")).count(),
            2,
            "les deux lots ensemble couvrent les deux extrémités, une fois chacun : {secondes:?}"
        );
        assert!(
            secondes[0].contains("inv_b49") && !secondes[0].contains("inv_b50"),
            "le premier lot s'arrête à 50 ids : {secondes:?}"
        );
    }

    /// Scenario « le statut de cinquante invocations se lit en une requête », second volet : un
    /// identifiant absent de la réponse est **absent** du résultat (aucun `InvocationStatus`
    /// inventé pour les ids non cités par Restate).
    #[tokio::test]
    async fn invocation_statut_inconnu_absent_du_resultat() {
        let stub = StubServer::spawn(200, corps_rows(&[ligne_invocation("inv_presente", "Running")]));
        let rendu = invocation_statuses(
            &stub.config(),
            &test_http(),
            &["inv_presente".to_string(), "inv_absente".to_string()],
        )
        .await
        .expect("le 200 avec une ligne devait rendre un Vec");
        assert_eq!(rendu.len(), 1, "une seule ligne rendue");
        let premier = rendu.first().expect("la ligne rendue");
        assert_eq!(premier.id, "inv_presente");
        assert_eq!(premier.status, "Running");
        assert_eq!(premier.service, "DagInterpreter");
        assert_eq!(premier.handler, "run");
        assert_eq!(premier.key, None);
        assert_eq!(premier.created_at, "2026-11-15T07:00:00.000Z");
        assert_eq!(premier.completed_at, None);
        assert_eq!(premier.retry_count, 0);
        assert_eq!(premier.failure, None);
    }

    /// `Must` : liste vide → `Ok(vec![])` **sans requête**.
    #[tokio::test]
    async fn invocation_statuses_liste_vide_sans_requête() {
        let stub = StubServer::spawn(200, corps_rows(&[]));
        let rendu = invocation_statuses(&stub.config(), &test_http(), &[])
            .await
            .expect("la liste vide doit rendre Ok sans appel");
        assert_eq!(rendu.len(), 0, "la liste vide doit rendre un Vec vide");
        assert!(stub.requests().is_empty(), "aucune requête pour une liste vide");
    }

    /// Scenario « un identifiant mal formé n'atteint jamais le réseau » : la grammaire stricte
    /// (`inv_` + alphanumérique) filtre `inv_a'; DROP TABLE x; --` et `a b` **avant** toute
    /// requête — `InvalidConfig` (`MRD-WORKFLOW-007`) et zéro requête capturée, y compris quand
    /// un identifiant valide précède le fautif (validation de TOUS les ids avant le premier
    /// octet envoyé). Même verdict pour un `service` mal formé passé à `list_invocations` et
    /// pour un identifiant passé à `cancel_invocation`.
    #[tokio::test]
    async fn identifiant_mal_formé_n_atteint_jamais_le_réseau() {
        let injectés = ["inv_a'; DROP TABLE x; --", "a b"];
        let http = test_http();

        for id in injectés {
            let stub = StubServer::spawn(200, corps_rows(&[]));
            let Err(err) = invocation_statuses(
                &stub.config(),
                &http,
                &["inv_valide1".to_string(), id.to_string()],
            )
            .await
            else {
                panic!("l'identifiant {id:?} devait être refusé avant requête");
            };
            assert!(
                matches!(err, WorkflowError::InvalidConfig(_)),
                "variante attendue InvalidConfig : {err:?}"
            );
            assert!(
                err.to_string()
                    .starts_with("MRD-WORKFLOW-007: invalid workflow configuration: "),
                "rendu inattendu : {err}"
            );
            assert!(stub.requests().is_empty(), "aucune requête pour {id:?}");
        }

        let stub = StubServer::spawn(200, corps_rows(&[]));
        let filtre = InvocationFilter {
            service: Some("svc' OR 1=1 --".to_string()),
            ..InvocationFilter::default()
        };
        let Err(err) = list_invocations(&stub.config(), &http, &filtre).await else {
            panic!("un service mal formé devait être refusé avant requête");
        };
        assert!(
            matches!(err, WorkflowError::InvalidConfig(_)),
            "variante attendue InvalidConfig : {err:?}"
        );
        assert!(
            stub.requests().is_empty(),
            "aucune requête pour un service fautif"
        );

        let stub = StubServer::spawn(202, String::new());
        let Err(err) = cancel_invocation(&stub.config(), &http, "inv_x'; DELETE FROM y;--").await else {
            panic!("un identifiant d'annulation mal formé devait être refusé avant requête");
        };
        assert!(
            matches!(err, WorkflowError::InvalidConfig(_)),
            "variante attendue InvalidConfig : {err:?}"
        );
        assert!(stub.requests().is_empty(), "aucune requête d'annulation");
    }

    /// `Must` `cancel_invocation` (volet stub) : `PATCH {admin_url}/invocations/{id}/cancel`,
    /// `2xx` → `Ok(())`. La branche idempotente (`404`/`409` mesurés) est verrouillée par
    /// [`cancel_invocation_idempotence_404_et_409_mesurés`] et prouvée live au test `#[ignore]`
    /// de [`../../tests/workflow_restate.rs`].
    #[tokio::test]
    async fn cancel_invocation_poste_patch_cancel() {
        let stub = StubServer::spawn(202, String::new());
        cancel_invocation(&stub.config(), &test_http(), "inv_a1")
            .await
            .expect("le 202 devait rendre Ok(())");
        let requests = stub.requests();
        assert_eq!(requests.len(), 1);
        let request = requests.first().expect("une requête capturée");
        assert_eq!(request.method, "PATCH");
        assert_eq!(request.uri, "/invocations/inv_a1/cancel");
    }

    /// Branche idempotente du `Must` `cancel_invocation` (./client.sdd, amendement du 2026-11-13 :
    /// « invocation inconnue ou terminée → `Ok(())`, la forme exacte des codes est vérifiée contre
    /// Restate ») — codes mesurés contre `restate-server` **1.8.1** le 2026-11-15 (rapport de
    /// tâche #80) : invocation inconnue (id décodable, jamais vue) → `404` « does not exist » ;
    /// invocation déjà terminée → `409` « already completed ». Les deux rendent `Ok(())`. Tout
    /// autre statut (ici `503`) reste `RestateRejected` avec corps verbatim.
    #[tokio::test]
    async fn cancel_invocation_idempotence_404_et_409_mesurés() {
        for statut in [404_u16, 409] {
            let stub = StubServer::spawn(statut, "{\"message\":\"forme mesurée 1.8.1\"}".to_string());
            cancel_invocation(&stub.config(), &test_http(), "inv_a1")
                .await
                .unwrap_or_else(|erreur| {
                    panic!("le {statut} mesuré (idempotence) devait rendre Ok(()) : {erreur:?}")
                });
        }
        let stub = StubServer::spawn(503, "{\"message\":\"boom\"}".to_string());
        let Err(err) = cancel_invocation(&stub.config(), &test_http(), "inv_a1").await else {
            panic!("le 503 devait rendre RestateRejected, jamais Ok");
        };
        let WorkflowError::RestateRejected { status, body } = err else {
            panic!("variante attendue RestateRejected : {err:?}");
        };
        assert_eq!(status, 503);
        assert!(body.contains("boom"), "corps verbatim : {body}");
    }

    /// Scenario « les tables système pas encore prêtes sont une attente retryable » : le stub
    /// répond comme Restate au démarrage (`500`, message « expecting a partition store ») — le
    /// résultat est une erreur typée de la taxonomie existante (statut + corps verbatim, que
    /// l'appelant peut re-tenter), **jamais** un `Vec` vide qui masquerait l'attente.
    #[tokio::test]
    async fn tables_pas_prêtes_erreur_retryable_jamais_vec_vide() {
        let corps = "{\"message\":\"Datafusion error: expecting a partition store\"}";
        let stub = StubServer::spawn(500, corps.to_string());
        let Err(err) = invocation_statuses(&stub.config(), &test_http(), &["inv_a1".to_string()]).await
        else {
            panic!("le 500 « partition store » devait rendre une erreur, jamais un Vec");
        };
        let WorkflowError::RestateRejected { status, body } = err else {
            panic!("variante attendue RestateRejected : {err:?}");
        };
        assert_eq!(status, 500);
        assert!(
            body.contains("expecting a partition store"),
            "le corps doit rester verbatim : {body}"
        );
    }

    /// `Must` `list_invocations` : requête unique sur `sys_invocation`, filtres traduits en
    /// conditions sur `target_service_name` / `target_handler_name` / `status` (valeurs déjà
    /// passées par la grammaire stricte), tri `created_at` décroissant, `LIMIT` à 100 par
    /// défaut et plafonné à 1000.
    #[tokio::test]
    async fn list_invocations_filtres_tri_et_plafond() {
        let stub = StubServer::spawn(200, corps_rows(&[]));
        let config = stub.config();
        let http = test_http();
        list_invocations(&config, &http, &InvocationFilter::default())
            .await
            .expect("le 200 `rows` vide devait rendre un Vec vide");
        list_invocations(
            &config,
            &http,
            &InvocationFilter {
                service: Some("DagInterpreter".to_string()),
                handler: Some("run".to_string()),
                status: Some("Completed".to_string()),
                limit: Some(5000),
            },
        )
        .await
        .expect("le second 200 devait rendre un Vec vide");

        let requests = stub.requests();
        assert_eq!(requests.len(), 2, "une requête par appel");
        let requetes: Vec<String> = requests
            .iter()
            .map(|r| {
                let corps: serde_json::Value = serde_json::from_slice(&r.body).expect("le corps est un JSON");
                corps
                    .get("query")
                    .and_then(Value::as_str)
                    .expect("`query`")
                    .to_string()
            })
            .collect();
        assert!(
            requetes[0].contains("LIMIT 100"),
            "défaut de limit = 100 : {}",
            requetes[0]
        );
        assert!(
            !requetes[0].contains("WHERE"),
            "aucun filtre posé = aucune condition WHERE : {}",
            requetes[0]
        );
        assert!(
            requetes[0].contains("ORDER BY created_at DESC"),
            "tri created_at décroissant : {}",
            requetes[0]
        );
        assert!(
            requetes[1].contains("LIMIT 1000"),
            "plafond 1000 : {}",
            requetes[1]
        );
        assert!(
            requetes[1].contains("target_service_name = 'DagInterpreter'")
                && requetes[1].contains("target_handler_name = 'run'")
                && requetes[1].contains("status = 'Completed'"),
            "les trois filtres posés en conditions AND : {}",
            requetes[1]
        );
    }

    /// Scenario « le journal nomme les étapes » (volet stub) : lecture de `sys_journal`
    /// ordonnée par `index`, entrées `{ index, kind: entry_type, name }` — les noms posés par
    /// ./dispatcher.rs (`render:<id>`, `step:<id>`) remontent ; un nom absent (`null`) rend
    /// `None`. La preuve live est le test `#[ignore]` de [`../../tests/workflow_restate.rs`].
    #[tokio::test]
    async fn invocation_journal_ordonne_par_index() {
        let lignes = serde_json::json!([
            {"index": 0, "entry_type": "InvocationStart", "name": null},
            {"index": 1, "entry_type": "Run", "name": "render:A"},
            {"index": 2, "entry_type": "Run", "name": "step:A"}
        ]);
        let stub = StubServer::spawn(200, corps_rows(lignes.as_array().expect("table")));
        let rendu = invocation_journal(&stub.config(), &test_http(), "inv_a1")
            .await
            .expect("le 200 avec lignes devait rendre le journal");
        assert_eq!(rendu.len(), 3);
        assert_eq!(
            rendu,
            vec![
                JournalEntry {
                    index: 0,
                    kind: "InvocationStart".to_string(),
                    name: None,
                },
                JournalEntry {
                    index: 1,
                    kind: "Run".to_string(),
                    name: Some("render:A".to_string()),
                },
                JournalEntry {
                    index: 2,
                    kind: "Run".to_string(),
                    name: Some("step:A".to_string()),
                },
            ]
        );
        let requete_corps: serde_json::Value =
            serde_json::from_slice(&stub.requests().first().expect("une requête capturée").body)
                .expect("le corps est un JSON");
        let requete = requete_corps
            .get("query")
            .and_then(Value::as_str)
            .expect("`query`");
        assert!(
            requete.contains("FROM sys_journal WHERE id = 'inv_a1' ORDER BY index"),
            "requête sys_journal filtrée par id et triée par index : {requete}"
        );
    }

    /// Forme mesurée le 2026-11-15 contre `restate-server` **1.8.1** : les entrées **sans nom**
    /// remontent de `sys_journal` avec `name` à la chaîne **vide** (Jamais `null`, Jamais absent) —
    /// le champ `name` de [`JournalEntry`] doit rendre `None` pour ces entrées, seule forme sous
    /// laquelle le
    /// contrat « le journal nomme les runs » est lisible (sinon toute entrée anonyme porterait un
    /// nom fictif `""`).
    #[tokio::test]
    async fn invocation_journal_nom_vide_est_absence() {
        let lignes = serde_json::json!([
            {"index": 0, "entry_type": "InvocationStart", "name": ""},
            {"index": 1, "entry_type": "Run", "name": "step:A"},
            {"index": 2, "entry_type": "Sleep", "name": ""}
        ]);
        let stub = StubServer::spawn(200, corps_rows(lignes.as_array().expect("table")));
        let rendu = invocation_journal(&stub.config(), &test_http(), "inv_a1")
            .await
            .expect("le 200 avec lignes devait rendre le journal");
        assert_eq!(
            rendu.iter().map(|entree| entree.name.clone()).collect::<Vec<_>>(),
            vec![None, Some("step:A".to_string()), None],
            "le nom vide mesuré (restate 1.8.1) est l'absence de nom"
        );
    }

    // ---------------------------------------------------------------------------
    // Résolution et rejet d'awakeables par l'ingress (issue #26, amendement du 2026-11-14)
    //
    // Formes : `POST {ingress_url}/restate/awakeables/{id}/resolve` (`content-type:
    // application/json`, corps = valeur JSON nue) et `POST …/reject` (`content-type:
    // text/plain`, message nu en corps) — formes documentées par `restate-sdk` 0.12.1 et
    // confirmées mesurées contre `restate-server` **1.8.1** par les tests `#[ignore]` de
    // [`../../tests/workflow_restate.rs`] (rapport de tâche #26).
    // ---------------------------------------------------------------------------

    /// Scenario « un identifiant d'awakeable mal formé n'atteint jamais le réseau » : la grammaire
    /// stricte (`prom_1` + base64url `[A-Za-z0-9_-]` non vide) refuse préfixe absent, queue vide,
    /// espaces, injection et accents — `InvalidConfig` (`MRD-WORKFLOW-007`) et **zéro requête**,
    /// sur `resolve_awakeable` comme sur `reject_awakeable` (l'identifiant entre nu dans un
    /// chemin d'URL, aucun texte libre n'y passe).
    #[tokio::test]
    async fn identifiant_awakeable_mal_formé_sans_requête() {
        let injectés = [
            "awake_1abc",                // préfixe `prom_1` absent
            "prom_1",                    // préfixe seul, base64url vide
            "prom_1 a",                  // espace dans le chemin
            "prom_1'; DELETE FROM x;--", // injection
            "prom_1é",                   // hors base64url
            "",                          // vide
        ];
        let http = test_http();
        for id in injectés {
            let stub = StubServer::spawn(204, String::new());
            let Err(err) = resolve_awakeable(&stub.config(), &http, id, &serde_json::json!({"ok": 1})).await
            else {
                panic!("l'identifiant {id:?} devait être refusé avant requête (resolve)");
            };
            assert!(
                matches!(err, WorkflowError::InvalidConfig(_)),
                "variante attendue InvalidConfig : {err:?}"
            );
            assert!(
                err.to_string()
                    .starts_with("MRD-WORKFLOW-007: invalid workflow configuration: "),
                "rendu inattendu : {err}"
            );
            assert!(stub.requests().is_empty(), "aucune requête resolve pour {id:?}");

            let stub = StubServer::spawn(204, String::new());
            let Err(err) = reject_awakeable(&stub.config(), &http, id, "refus").await else {
                panic!("l'identifiant {id:?} devait être refusé avant requête (reject)");
            };
            assert!(
                matches!(err, WorkflowError::InvalidConfig(_)),
                "variante attendue InvalidConfig : {err:?}"
            );
            assert!(stub.requests().is_empty(), "aucune requête reject pour {id:?}");
        }
    }

    /// Scenario « `resolve_awakeable` poste la valeur en JSON » : `POST
    /// /restate/awakeables/{id}/resolve`, `content-type: application/json`, corps = la valeur nue
    /// compacte ; tout `2xx` (le stub pose `204`, statut nominal confirmé en live) rend `Ok(())`.
    #[tokio::test]
    async fn resolve_awakeable_poste_valeur_json() {
        let stub = StubServer::spawn(204, String::new());
        resolve_awakeable(
            &stub.config(),
            &test_http(),
            "prom_1PePOqp",
            &serde_json::json!({"ok": 42}),
        )
        .await
        .expect("le 204 mesuré devait rendre Ok(())");
        let requests = stub.requests();
        assert_eq!(requests.len(), 1);
        let request = requests.first().expect("une requête capturée");
        assert_eq!(request.method, "POST");
        assert_eq!(request.uri, "/restate/awakeables/prom_1PePOqp/resolve");
        assert_eq!(request.header("content-type"), Some("application/json"));
        assert_eq!(
            String::from_utf8_lossy(&request.body),
            "{\"ok\":42}",
            "le corps est la valeur JSON nue, compacte"
        );
    }

    /// Scenario « `reject_awakeable` poste le message en texte brut » : `POST
    /// /restate/awakeables/{id}/reject`, `content-type: text/plain`, corps = le message verbatim
    /// (forme mesurée : le texte du rejet transite nu jusqu'au step en attente).
    #[tokio::test]
    async fn reject_awakeable_poste_message_texte_brut() {
        let stub = StubServer::spawn(204, String::new());
        reject_awakeable(&stub.config(), &test_http(), "prom_1PePOqp", "refus externe")
            .await
            .expect("le 204 mesuré devait rendre Ok(())");
        let requests = stub.requests();
        assert_eq!(requests.len(), 1);
        let request = requests.first().expect("une requête capturée");
        assert_eq!(request.method, "POST");
        assert_eq!(request.uri, "/restate/awakeables/prom_1PePOqp/reject");
        assert_eq!(request.header("content-type"), Some("text/plain"));
        assert_eq!(
            String::from_utf8_lossy(&request.body),
            "refus externe",
            "le message voyage nu dans le corps"
        );
    }

    /// Erreurs : statut hors `2xx` de l'ingress → `RestateRejected` (`MRD-WORKFLOW-002`) corps
    /// verbatim, variante existante (aucune nouveauté, ./error.sdd) ; un port refusant →
    /// `RestateUnreachable` (`MRD-WORKFLOW-001`) par `classify_transport_error`.
    #[tokio::test]
    async fn resolve_awakeable_erreurs_taxonomie_existante() {
        let stub = StubServer::spawn(404, "{\"message\":\"awakeable not found\"}".to_string());
        let Err(err) = resolve_awakeable(
            &stub.config(),
            &test_http(),
            "prom_1inconnu",
            &serde_json::json!(null),
        )
        .await
        else {
            panic!("le 404 devait rendre RestateRejected");
        };
        let WorkflowError::RestateRejected { status, body } = err else {
            panic!("variante attendue RestateRejected : {err:?}");
        };
        assert_eq!(status, 404);
        assert_eq!(body, "{\"message\":\"awakeable not found\"}");

        let config = WorkflowConfig {
            admin_url: "http://127.0.0.1:80".to_string(),
            ingress_url: format!("http://127.0.0.1:{}", closed_port()),
            deployment_url: DEPLOYMENT_URL.to_string(),
        };
        let Err(err) = reject_awakeable(&config, &test_http(), "prom_1abc", "x").await else {
            panic!("un port refusant devait rendre RestateUnreachable");
        };
        assert!(
            matches!(err, WorkflowError::RestateUnreachable(_)),
            "variante attendue RestateUnreachable : {err:?}"
        );
    }

    /// `Must` : la config est validée avant la moindre requête — `ingress_url` vide →
    /// `InvalidConfig` nommant le champ, zéro octet envoyé (mêmes motifs que
    /// `register_deployment_url_admin_vide_refusee_sans_appel_reseau`).
    #[tokio::test]
    async fn resolve_awakeable_config_invalide_sans_requête() {
        let stub = StubServer::spawn(204, String::new());
        let config = WorkflowConfig {
            admin_url: stub.url(),
            ingress_url: String::new(),
            deployment_url: DEPLOYMENT_URL.to_string(),
        };
        let Err(err) = resolve_awakeable(&config, &test_http(), "prom_1abc", &serde_json::json!({})).await
        else {
            panic!("un ingress_url vide devait rendre Err(InvalidConfig)");
        };
        assert!(
            matches!(err, WorkflowError::InvalidConfig(_)),
            "variante attendue InvalidConfig : {err:?}"
        );
        assert!(err.to_string().contains("ingress_url"), "{err}");
        assert!(stub.requests().is_empty(), "aucune requête");
    }

    /// `Must` : un `2xx` sans le tableau `rows` attendu est une forme d'administration
    /// inattendue — une erreur typée, jamais un résultat vide qui la confondrait avec « aucune
    /// invocation » (variante existante la plus proche : `RestateRejected`, statut observé et
    /// corps verbatim — la variante `Transport` citée par l'amendement n'existe pas dans
    /// ./error.rs, arbitré « réutilisation »).
    #[tokio::test]
    async fn réponse_admin_sans_rows_est_une_erreur() {
        let stub = StubServer::spawn(200, "{\"trousers\":\"on fire\"}".to_string());
        let Err(err) = invocation_statuses(&stub.config(), &test_http(), &["inv_a1".to_string()]).await
        else {
            panic!("un 200 sans `rows` devait rendre une erreur, jamais un Vec");
        };
        let WorkflowError::RestateRejected { status, body } = err else {
            panic!("variante attendue RestateRejected : {err:?}");
        };
        assert_eq!(status, 200);
        assert!(body.contains("trousers"), "corps verbatim : {body}");
    }
}
