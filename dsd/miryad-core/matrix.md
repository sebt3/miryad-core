# Solution Design: Client Matrix de bot

## Objective

Fournir les primitives pour bâtir des bots Matrix : se connecter, envoyer, lire (historique et flux), rejoindre une
salle, avec chiffrement de bout en bout. Contexte : piloter l'application fille `vanyline` depuis un client Matrix, et
servir de base à un nouveau `kydah-monitoring-agents` bâti sur miryad-core (arbitré le 2026-10-06).

## Scope and Non-goals

Feature `matrix`, **hors défaut**, qui implique `db`. Le homeserver visé est Palpo derrière MAS (authentification déléguée
à Authentik, mots de passe désactivés).

Hors Solution : aucune logique de conversation. Le futur harnais de chatbot (outillage standardisé issu de `vanyline`,
dont la remontée de code et de spec n'est pas prête) consommera `MatrixEventHandler`.

## Applicable Domain Requirements

`miryad-core#CON-3` (codes `MRD-MATRIX-*` et `MRD-MXSTORE-*`), `miryad-core#CON-6` (préfixe `miryad_matrix_`),
`miryad-core#CON-5` (exception de migration du 2026-10-17).

## Functional Requirements

- **FR-1 - MUST:** L'API publique n'expose aucun type de `matrix-sdk` ni de `ruma` (identifiants en `String`).
    - **Source:** `arch@c62a184`, « Client Matrix de bot »
    - **Rationale:** la montée de version de `matrix-sdk` ne casse pas les applications.
    - **Realized by:** SpecDD /src/matrix/mod.sdd
    - **Verified by:** `AC-1`

- **FR-2 - MUST:** Le store de chiffrement (`matrix::store`) implémente `CryptoStore` sur SeaORM dans la base de
  l'application : seize tables préfixées `miryad_matrix_`, une clé de compte dans chacune, les valeurs sensibles
  scellées par le `StoreCipher` du compte (protégé par la passphrase de la config).
    - **Source:** `arch@c62a184`, « Client Matrix de bot »
    - **Realized by:** SpecDD /src/matrix/store/mod.sdd
    - **Verified by:** `AC-2`

- **FR-3 - MUST:** La session est stockée dans le store, scellée, jamais en clair et jamais dans un fichier. Le client
  écoute `subscribe_to_session_changes` et réécrit la session à chaque rotation du jeton de rafraîchissement.
    - **Source:** `arch@c62a184`, « Client Matrix de bot » (le jeton d'accès de MAS vit 300 s)
    - **Realized by:** SpecDD /src/matrix/mod.sdd
    - **Verified by:** `AC-3`

- **FR-4 - MUST:** Deux voies de connexion : `login_with_device_code` (RFC 8628, nominal, un opérateur approuve une fois)
  et `import_session` (jeton importé, repli et voie automatisable).
    - **Source:** `arch@c62a184`, « Client Matrix de bot »
    - **Realized by:** SpecDD /src/matrix/mod.sdd
    - **Verified by:** `AC-4`

- **FR-5 - MUST:** `MatrixConfig::ca_cert` (contenu PEM) **s'ajoute** aux racines webpki des deux clients HTTP (device
  code et `matrix-sdk`). Le client du device code désactive les redirections.
    - **Source:** `arch@c62a184`, « Client Matrix de bot » (CA interne)
    - **Realized by:** SpecDD /src/matrix/mod.sdd

- **FR-6 - MUST:** `run_sync` n'a **qu'un seul synchronisateur par compte** à la fois, arbitré par un bail dans le store
  (`miryad.matrix.sync`, durée `2 × sync_timeout + 15 s`, renouvelé à chaque réponse, libéré à l'arrêt) ; les autres
  réplicas attendent.
    - **Source:** `arch@c62a184`, « Client Matrix de bot » (synchronisation)
    - **Rationale:** deux appareils qui déchiffrent en parallèle corrompraient les sessions Olm.
    - **Realized by:** SpecDD /src/matrix/mod.sdd
    - **Verified by:** `AC-5`

- **FR-7 - MUST:** La livraison est « au moins une fois, sans rattrapage ». Un message peut être remis deux fois (bascule
  de bail) : le handler doit être idempotent, clé naturelle `event_id`. Les messages du bot, les `m.notice` et les
  éditions ne sont jamais remis (anti-boucle). L'auto-join n'accepte que les inviteurs listés dans `auto_join_from`.
    - **Source:** `arch@c62a184`, « Client Matrix de bot » (synchronisation)
    - **Realized by:** SpecDD /src/matrix/mod.sdd
    - **Verified by:** `AC-5`

- **FR-8 - MUST:** `send` accepte un `txn_id` ; un rejeu avec le même identifiant est dédupliqué par le serveur. Le kind
  `matrix_send` (sous `workflow` **et** `matrix`, enregistré par l'application) dérive un `txn_id` déterministe de
  `(run_key, step_id)` : un retry Restate ne produit pas de doublon. Une erreur retryable du client le reste ;
  `RoomUnavailable` et les erreurs de configuration ne le sont pas.
    - **Source:** `arch@c62a184`, « Client Matrix de bot » (envoi)
    - **Realized by:** SpecDD /src/workflow/matrix_step.sdd
    - **Verified by:** `AC-6`

## Constraints

- **CON-1 - MUST:** `matrix-sdk` est épinglé en `0.18`, sans la feature `sqlite`.
    - **Source:** `arch@c62a184`, « Client Matrix de bot »
    - **Rationale:** `matrix-sdk` ≥ 0.19 impose `libsqlite3-sys` 0.38 et casse le graphe avec le `sqlx-sqlite` de SeaORM
      (`links = "sqlite3"`). Critère de sortie : relever le plafond `sqlx` ou `matrix-sdk-sqlite`.
    - **Realized by:** SpecDD /src/matrix/mod.sdd

- **CON-2 - MUST:** La logique dérivée de `matrix-sdk-sqlite` (schéma, formats, sémantique ; six fichiers) porte une
  attribution Apache-2.0 en tête de fichier, `NOTICE` et `LICENSE-APACHE`.
    - **Source:** `arch@c62a184`, « Client Matrix de bot »
    - **Realized by:** SpecDD /src/matrix/store/mod.sdd

- **CON-3 - MUST:** `matrix::store` est isolé : aucun accès à `matrix-sdk-sqlite`, ses seules dépendances sont SeaORM et
  `matrix-sdk-crypto`.
    - **Source:** `arch@c62a184`, « Client Matrix de bot »
    - **Rationale:** le module est conçu pour être extrait en crate dédiée.
    - **Verified by:** `tests/matrix_store_isolation.rs`

- **CON-4 - MUST:** `device_code.rs` reste un fichier isolé qui implémente le protocole en HTTP puis remet les jetons à
  `matrix-sdk` (`restore_session`).
    - **Source:** `arch@c62a184`, « Client Matrix de bot »
    - **Rationale:** `matrix-sdk` 0.18 n'expose pas le device code à l'appelant ; l'isolement permet de le remplacer par
      un appel natif ≥ 0.19.
    - **Realized by:** SpecDD /src/matrix/mod.sdd

## Design Decisions

- **DEC-1 - Store de chiffrement dans la base de l'application**
    - **Decision:** `CryptoStore` implémenté sur SeaORM plutôt qu'avec `matrix-sdk-sqlite`.
    - **Consequences:** pas de second moteur SQL ; PostgreSQL éprouvé le 2026-10-18 (suite entière, conformité
      officielle de `matrix-sdk-crypto` comprise, 73 verts sur PostgreSQL 17 comme sur SQLite). Le module est conçu
      pour être extrait ; `MIRYAD_TEST_PG_URL` bascule les tests sur une base réelle, un schéma neuf par test.
    - **Source:** `arch@c62a184`, « Client Matrix de bot »

- **DEC-2 - Pas d'automatisation du compte par scraping**
    - **Context:** le consentement et la création de compte de MAS sont des pages humaines.
    - **Decision:** l'automatisation passe par l'API d'administration de MAS (`kydah/box#454`), jamais par du scraping
      HTML.
    - **Source:** `arch@c62a184`, « Client Matrix de bot » (spike du 2026-10-18)

## Operations and Resilience

Constats du spike du 2026-10-18 contre Palpo 0.4.0 et MAS 1.26.0 : enregistrement dynamique accepté, portée MSC2967
`device:{id}` honorée, introspection par Palpo, approbation possible par un compte de service Authentik (jeton
`app_password`).

Pièges constatés :

- MAS répond `authorization_pending` en **HTTP 403** : les erreurs OAuth se lisent dans le corps JSON, pas dans le statut.
- MAS ne fournit pas de `verification_uri_complete`.
- Au démarrage, le sync initial est sans jeton : ses messages ne sont pas remis (trace `info`).

Source : `arch@c62a184`, « Client Matrix de bot ».

## SpecDD Realization

`SpecDD /src/matrix/mod.sdd`, `SpecDD /src/matrix/store/mod.sdd` et leurs enfants ; `SpecDD /src/workflow/matrix_step.sdd`.

## Acceptance Criteria

**ASSUMPTION :** `AC-1` à `AC-6` sont formulés d'après les exigences ; correspondance exacte avec les `Scenario`
non vérifiée dans la migration DSD.

- **AC-1:** L'API publique de la crate n'expose aucun type de `matrix-sdk` ni de `ruma`.
- **AC-2:** La suite de conformité officielle du store passe sur SQLite et sur PostgreSQL.
- **AC-3:** Une rotation de session est réécrite dans le store ; sans réécriture, la session devient invalide au
  redémarrage.
- **AC-4:** Le device code aboutit contre Palpo + MAS (login, auto-join, écho).
    - **Verification:** banc live `tests/matrix_live.rs` (`#[ignore]`, variables `MIRYAD_MATRIX_*`), test interactif
      `device_code_login_then_echo` exécuté le 2026-10-18. Ne tourne ni dans la batterie ni en CI.
- **AC-5:** Un seul réplica synchronise à la fois pour un compte.
- **AC-6:** Un retry Restate du kind `matrix_send` ne produit pas de doublon.

## Open Issues

- **OI-1:** **Persistance des rotations de session contre un MAS réel** : jamais éprouvée. **TBD.**
- **OI-2:** **Cross-signing du bot** : Element affiche « chiffré par un appareil non vérifié par son propriétaire ».
- **OI-3:** **Provisionnement automatisé du compte et de la session de bot** (`kydah/box#454`).
- **OI-4:** **Envoi chiffré depuis un réplica qui ne synchronise pas** (hypothèse retenue, à éprouver avec deux
  processus) et **rattrapage des messages manqués pendant une coupure** (voie retenue : `/messages` depuis le dernier
  `event_id` traité, persisté en base). Voir `SpecDD /src/matrix/mod.sdd`.
- **OI-5:** **Identité Matrix → utilisateur miryad (RBAC)** : à trancher avec le harnais de chatbot, hors périmètre de ce
  lot et à venir.
- **OI-6:** Relever le plafond de `matrix-sdk` (CON-1) quand `sqlx` ou `matrix-sdk-sqlite` le permet.
    - **Source (OI-1 à OI-5):** `roadmap@c62a184`, item 12 ; `arch@c62a184`, « Pas encore fait »
