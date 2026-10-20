# Changelog

Format inspiré de [Keep a Changelog](https://keepachangelog.com/fr/1.1.0/). L'API reste
instable avant `1.0` : un changement de version mineure peut être cassant.

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
