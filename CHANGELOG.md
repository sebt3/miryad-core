# Changelog

Format inspiré de [Keep a Changelog](https://keepachangelog.com/fr/1.1.0/). L'API reste
instable avant `1.0` : un changement de version mineure peut être cassant.

## [0.3.0] — 2026-11-12

### Ruptures

- **`PrincipalSource::Agent`** : nouvelle variante (enum non `#[non_exhaustive]`) — un `match` exhaustif
  côté appelant doit être complété.
- **`workflow`** :
  - `RunInfo` gagne `vars` et `trigger` (un littéral de struct doit les renseigner) et perd `Eq` ;
  - `trigger_run` prend les `vars` du run, et le handler `run` reçoit un `RunInput` ;
  - nouvelle variante `ScriptTimeout` (`MRD-WORKFLOW-009`).
- **`MiryadResource`** : `before_create`, `before_update` et `before_delete` deviennent asynchrones et reçoivent la
  connexion de l'appel (`db: &C`, `C: sea_orm::ConnectionTrait`, connexion ou transaction) ; ils rendent un futur
  `Send`. Aucun trait de compatibilité : tout hook surchargé doit être réécrit (`async`, paramètre `db`).
  Corollaires : `server_for` et `caller_of` deviennent génériques sur `ConnectionTrait`.
- **Migrations** : les migrations `miryad_lymaille_*` (feature `lymaille`) s'ajoutent au `Migrator` ; elles
  n'existent pas en 0.2.0, aucune base existante n'est touchée sans la feature.

### Ajouts

- **Feature `lymaille`** (hors défaut, implique `db`) : hôte de la bibliothèque d'agents LLM `lymaille` 0.2.0.
  - Définitions (agents, toolsets, profils, providers, serveurs MCP, types de contexte, contextes) exposées en
    `MiryadResource` : REST, MCP et OpenAPI, avec leurs politiques de lecture et d'écriture. Providers réservés
    aux admins, références de secret seules (jamais de valeur).
  - Stores SeaORM cloisonnés par propriétaire (sessions, mémoires, brouillons, index de sessions, audit, journal
    d'écritures), SQLite et PostgreSQL ; rétention configurable et `purge_sessions`.
  - Définitions privées (agents et toolsets d'un utilisateur) et sessions de configuration : un non-admin
    écrit et active un sous-agent, sans pouvoir s'accorder un module réservé à l'admin.
  - Sessions interactives (nature `Work` ou `Config`) : hub d'événements, reprise après redémarrage, tour
    concurrent refusé, ouverture par contexte.
  - Surface REST + SSE de conversation, événements typés exportés en TypeScript (`ts-rs`).
  - Pont Matrix (`lymaille,matrix`) : salle ↔ session, annuaire mxid → principal, dédoublonnage, mise en forme des
    sorties longues.
  - `LymailleHost` : l'application fournit URL et jeton frais des serveurs MCP par rôle (identifiants `H_…`),
    repli sur la table `mcp_servers`.
  - Module Rhai `resources` (lecture et écritures idempotentes) avec parité sur les politiques REST et MCP.
  - `HostModule` et registre `HostModules` : l'application accorde ses propres modules Rhai à une session, sous une
    politique d'accès relue avant chaque appel ; un module réservé à l'admin ne peut être accordé par un non-admin.
  - `ScriptRunner` : exécution d'un script Rhai à droits bornés (`script_grants` du type de contexte), sans boucle
    d'inférence, sous l'identité du principal donné.
  - Contrôles d'autorité à la sauvegarde (agents privés, toolsets privés, contextes) par les hooks asynchrones.
  - Codes d'erreur `MRD-LYMAILLE-001` à `-012` ; variantes `ScriptTimeout`, `ScriptOperationLimit`, `ScriptResultTooLarge`, `ScriptFailed`.
- **`workflow`** :
  - step `subagent` (session déterministe reprise au rejeu, plafond d'application de durée, clients MCP du contexte
    lié montés en batch, clé masquée par le `Redactor`) ;
  - `SUBAGENT_CONTEXT_TYPE` (`"workflow_subagent"`) : type de contexte sous lequel l'utilisateur lie les serveurs MCP du step ;
  - handler `execute_long` aux options de durée relevées ;
  - `vars` de run lisibles et modifiables en Rhai, héritage du `trigger` par les runs enfants ;
  - moteur de gabarits et Rhai par liste blanche, `timeout_secs` et annulation coopérative, évaluation sur fil
    dédié à pile de 256 Mio.
- **CI** : seize combinaisons de test et de clippy (croisements `workflow,lymaille`, `lymaille,matrix`),
  contrôles `cargo tree` prouvant que `lymaille` et `ts-rs` sont absents du graphe par défaut.

### Limites connues

- Le step `subagent` ne masque la clé d'API que sur les échecs d'outil MCP (limite amont de `lymaille` 0.2.0).
- La preuve de reprise du step `subagent` après kill du service (`#[ignore]`, hors CI) valide le mécanisme Restate avec un journal de fixture,
  pas le `SubagentStep` réel.
- Le kind `pad` de workflow attend une release de `lymaille` (nature `Workflow`).
- `matrix` : la persistance des rotations de session contre un MAS réel n'a jamais été éprouvée.

## [0.2.0] — 2026-10-20

### Ruptures

- **GraphQL supprimé** : le module `graphql` et les dépendances `seaography` / `async-graphql`
  disparaissent. La surface d'API reste REST, MCP et OpenAPI.
- **Feature `db` optionnelle** : `sea-orm` et ses satellites sortent du graphe sans `db`. Le
  défaut (`static-frontend` + `db`) est inchangé, mais `--no-default-features` n'inclut plus la
  base. Les modules d'entités, l'état d'auth à comptes locaux et `rest` sont soumis à `db`.
  Les features `mcp`, `workflow` et `matrix` impliquent `db`.
- **Nouvelles variantes publiques** (enums non `#[non_exhaustive]`) : codes d'erreur d'auth
  `019` et `020`, `PrincipalSource::Bearer`. Un `match` exhaustif côté appelant doit être complété.
- **Migrations modifiées en place** (aucune production à ce jour) : `m20260923_000001` et
  `m20261006_000001` (`payload` sur `tracked_users`, table `rooms_pending_key_bundle`).
  Une base créée avec une version antérieure doit être recréée.

### Ajouts

- **Auth** : validation du JWT OIDC présenté en `Authorization: Bearer` (`verify_bearer`), avec
  dispatch JWT / token d'API. Sans `db`, la crate est un socle d'authentification et de
  publication d'API.
- **Templating central** et **client System One** (feature `systemone`), steps de workflow
  `systemone` (un gate refuse les clés inconnues).
- **Client Matrix de bot** (feature `matrix`) :
  - store de chiffrement E2EE SeaORM (`CryptoStore`, suite de conformité officielle, SQLite et
    PostgreSQL), attribution Apache-2.0 du code dérivé de `matrix-sdk-sqlite` ;
  - `MatrixClient` (connexion, import de session, `send`, `join_room`, `recent_messages`,
    `run_sync` avec auto-join) ;
  - login `device code` (RFC 8628) contre MAS, CA interne configurable (`ca_cert`),
    rafraîchissement automatique du jeton ;
  - step de workflow `matrix_send` (envoi idempotent par `txn_id` déterministe).
- **CI** : treize combinaisons de test et de clippy, job `no-db-graph` (prouve que `sea-orm`
  est absent du graphe sans `db`), audit RustSec informatif (`audit.yml`).

### Corrections

- `workflow` : un gate `systemone` refuse les clés inconnues.
- `matrix` : le device code retente ses requêtes hors interrogation (jetons déjà émis jamais perdus).

[0.2.0]: https://crates.io/crates/miryad-core/0.2.0
