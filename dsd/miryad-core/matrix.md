# Solution Design: Client Matrix de bot

## Objective

Fournir les primitives pour bâtir des bots Matrix : se connecter, envoyer (dans le salon, dans un fil, en réponse),
éditer un message, lire (historique et flux), rejoindre une salle, avec chiffrement de bout en bout. Contexte : piloter l'application fille `vanyline` depuis un client Matrix, et
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

- **FR-9 - MUST:** Un envoi accepte une **relation** exprimée par un type propre à la crate (identifiants en `String`, `FR-1`) :
  soit un **fil** (identifiant de l'événement racine, `m.relates_to` de type `m.thread`, plus facultativement le dernier
  événement connu du fil pour le repli `m.in_reply_to` avec `is_falling_back: true` des clients sans fils), soit une
  **réponse** (identifiant de l'événement auquel répondre, `m.in_reply_to` sans fil). Les deux sont exclusives. Elles
  fonctionnent dans les salons chiffrés comme dans les autres ; `txn_id` garde la sémantique de `FR-8`.
    - **Source:** issue #87 (`sebt3/miryad-core`), « Proposition » 1, 2 et 4 ; user instruction, 2026-11-16
    - **Rationale:** un hôte (monitoring-bot, passerelle de notifications) regroupe les messages d'un sujet dans un fil ;
      il ne peut pas ajouter lui-même une relation puisque l'API n'expose aucun type `ruma` (`FR-1`).
    - **Realized by:** SpecDD /src/matrix/client.sdd
    - **Verified by:** `AC-7`

- **FR-10 - MUST:** `edit(room_id, event_id, body, html, txn_id)` publie un remplacement (`m.replace` avec `m.new_content`)
  de l'événement visé et rend l'identifiant de l'événement de **remplacement**. Même format de corps que `send`, même
  sémantique de `txn_id`.
    - **Source:** issue #87, « Proposition » 3
    - **Realized by:** TBD
    - **Verified by:** `AC-8`

- **FR-11 - MUST:** Le kind de workflow `matrix_send` accepte dans son `config` les mêmes champs de relation que `FR-9`, rendus par
  le gabarit comme les autres chaînes, et l'édition de `FR-10` est atteignable depuis un step (kind ou option, voir `OI-7`).
  La dérivation déterministe du `txn_id` de `FR-8` ne change pas.
    - **Source:** issue #87, « Proposition » 5
    - **Realized by:** TBD (`SpecDD /src/workflow/matrix_step.sdd`)
    - **Verified by:** `AC-7`, `AC-8`

- **FR-12 - MUST:** Le message remis au `MatrixEventHandler` porte l'identifiant de la **racine du fil** et celui de
  l'événement auquel il **répond**, quand ils existent. Le comportement de `FR-7` (éditions et messages du bot non remis)
  ne change pas.
    - **Source:** issue #87, « Proposition » 6
    - **Realized by:** TBD (`SpecDD /src/matrix/sync.sdd`)
    - **Verified by:** `AC-9`

- **FR-13 - MUST:** Un identifiant d'événement mal formé est `MatrixError::InvalidId` ; un événement visé inconnu (racine,
  réponse, édition) est une erreur typée **non retryable**.
    - **Source:** issue #87, « Proposition » 7
    - **Realized by:** TBD (`SpecDD /src/matrix/error.sdd`) ; faisabilité : voir `OI-7`
    - **Verified by:** `AC-10`

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

- **DEC-3 - `send` change de signature, sans compatibilité (2026-11-16)**
    - **Context:** `send(room_id, body, html, kind, txn_id)` est publique ; aucun hôte n'utilise la feature `matrix`.
    - **Decision:** la relation (fil ou réponse) devient un paramètre de `send` ; pas de second point d'entrée
      `send_with`. La cassure est faite maintenant, tant qu'elle ne coûte rien. `MatrixMessage` devient `#[non_exhaustive]`
      et porte `thread_root` et `in_reply_to` (`Option<String>`), sur la remise en direct comme dans `recent_messages`.
    - **Source:** user instruction, 2026-11-16 (arbitrage de `OI-7`, points (a) et (e))

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
- **AC-7:** Un message envoyé avec une racine porte une relation `m.thread` visant cette racine ; une réponse porte
  `m.in_reply_to` ; un rejeu avec le même `txn_id` ne crée pas de doublon.
- **AC-8:** Une édition remplace le corps affiché de l'événement visé.
- **AC-9:** Un événement entrant d'un fil expose l'identifiant de sa racine, une réponse l'identifiant de sa cible.
- **AC-10:** Un identifiant d'événement mal formé est refusé en `InvalidId` ; un événement visé inconnu rend une erreur
  non retryable.

## Open Issues

- **OI-1:** **Persistance des rotations de session contre un MAS réel** : jamais éprouvée. **TBD.**
- **OI-2:** **Cross-signing du bot** : Element affiche « chiffré par un appareil non vérifié par son propriétaire ».
- **OI-3:** **Provisionnement automatisé du compte et de la session de bot** (`kydah/box#454`).
- **OI-4:** **Envoi chiffré depuis un réplica qui ne synchronise pas** (hypothèse retenue, à éprouver avec deux
  processus) et **rattrapage des messages manqués pendant une coupure** (voie retenue : `/messages` depuis le dernier
  `event_id` traité, persisté en base). Voir `SpecDD /src/matrix/mod.sdd`.
- **OI-5:** **Identité Matrix → utilisateur miryad (RBAC)** : à trancher avec le harnais de chatbot, hors périmètre de ce
  lot et à venir.
- **OI-6:** Relever le plafond de `matrix-sdk` (CON-1) quand `sqlx` ou `matrix-sdk-sqlite` le permet. **Mesuré le
  2026-11-16 :** `matrix-sdk` 0.19.1 est publiée mais le conflit `links = "sqlite3"` persiste (`cargo update` refuse la
  résolution). Le relèvement corrigerait quatre advisories ouvertes (issues #40 `imbl-sized-chunks`, #41
  `matrix-sdk-crypto`, #68 et #89 `vodozemac`) ; il est aussi lié à RUSTSEC-2026-0459 (`libsqlite3-sys` ≥ 0.39, issue #66).
    - **Source (OI-1 à OI-5):** `roadmap@c62a184`, item 12 ; `arch@c62a184`, « Pas encore fait »
- **OI-7:** **Fils, réponses et éditions (issue #87)** — arbitrages attendus avant les specs de `FR-9` à `FR-13`. Les
  points (a) et (e) sont soldés (`DEC-3`) ; (b) est partiellement soldé (réponse sans fil, `edit` avec `kind`, voir
  `SpecDD /src/matrix/client.sdd`), la mesure de Palpo est faite (2026-11-17, voir (b)) ; (c) et (d) restent des recommandations non arbitrées.
    - **(a) Soldé le 2026-11-16** par `DEC-3` : la relation est un paramètre de `send`
      (`MessageRelation` : énumération `Thread { root, latest: Option<String> }` / `Reply { to }`, champs en `String`).
    - **(b) Construction de la relation.** Deux voies sous `matrix-sdk` 0.18 (lu dans `room/reply.rs` et `room/edit.rs`) :
      les assistants `Room::make_reply_event` / `make_edit_event` **récupèrent l'événement visé** sur le serveur
      (`/rooms/{id}/event`), ce qui donne gratuitement l'erreur typée de `FR-13` (`ReplyError::Fetch`, `EditError::Fetch`)
      et, pour l'édition, le refus d'éditer l'événement d'un autre (`EditError::NotAuthor`) ; mais l'assistant de fil prend
      pour repli l'événement visé, pas un « dernier événement » fourni par l'appelant. La construction manuelle de la
      relation honore `latest` et n'ajoute aucune requête, mais ne détecte pas un événement inconnu. **Recommandation :**
      assistants du SDK pour la réponse et l'édition ; construction manuelle pour le fil. **Mesuré le 2026-11-17** sur Palpo 0.1.4
      (banc live, requêtes `PUT …/send/m.room.message` directes) : une relation `m.thread`, `m.in_reply_to` ou `m.replace` vers un
      événement inconnu est refusée **immédiatement** (0,0 s) par un **HTTP 500** `M_UNKNOWN` « unknown db error » — pas un
      404 ni un 4xx — alors qu'un message plat répond 200. Le homeserver ne fournit donc pas l'erreur typée de `FR-13` pour
      le fil, et le SDK retente ce 500 (un `send` de fil vers une racine inconnue a duré plus de 300 s) : voir le `[?]` de
      `SpecDD /src/matrix/client.sdd` (vérifier la racine avant l'envoi, recommandé, ou borner la durée).
    - **(c) Édition.** `EditError::NotAuthor` est une cause nouvelle : `MatrixError` est `#[non_exhaustive]`, une variante
      (`MRD-MATRIX-009`, non retryable) s'ajoute sans casser les hôtes. Une édition ne reprend pas la relation de fil de
      l'original (le remplacement porte uniquement `m.replace`).
    - **(d) Kind d'édition.** Le `config` de `matrix_send` refuse les champs inconnus (`deny_unknown_fields`) et exige
      `body` ; l'édition exige en plus `event_id`. **Recommandation :** un kind distinct `matrix_edit`, plutôt qu'un champ
      optionnel qui change le sens des autres.
    - **(e) Soldé le 2026-11-16** par `DEC-3` : cassure assumée, `#[non_exhaustive]`, `recent_messages` porte les mêmes
      champs que la remise en direct.
