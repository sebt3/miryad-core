# Solution Design: Utilisateurs, groupes et RBAC

## Objective

Décider, pour chaque requête et chaque entité, ce qu'un principal peut lire, écrire et créer, à partir de groupes dont
l'IdP est la seule source de vérité.

## Scope and Non-goals

Sous la feature `db` (modules `users` et `rbac`). La Solution couvre la synchronisation des appartenances, l'évaluation
des politiques et deux routes de lecture (`GET /api/v1/users`, `GET /api/v1/me`).

Hors Solution : aucune API d'assignation manuelle de groupe, aucune route de gestion d'utilisateur.

## Applicable Domain Requirements

`miryad-core#CON-7` (fonctions génériques sur `ConnectionTrait`, car le seed de migration les appelle).

## Functional Requirements

- **FR-1 - MUST:** À chaque login OIDC réussi, `resolve_user` (get-or-create par `subject`) puis
  `sync_group_memberships` réconcilient **entièrement** les appartenances locales depuis le claim `groups` : ajout des
  groupes présents (création à la volée d'un `Group` jamais vu), retrait de ceux qui ne le sont plus.
    - **Source:** `arch@c62a184`, « Utilisateurs & Groupes / RBAC »
    - **Realized by:** SpecDD /src/users/membership.sdd, SpecDD /src/users/user.sdd, SpecDD /src/users/group.sdd
    - **Verified by:** `AC-1`

- **FR-2 - MUST:** Le groupe `admin` est créé par migration (toujours présent, vide au départ) et n'a rien de spécial
  au niveau du schéma : c'est une convention lue par `is_admin`.
    - **Source:** `arch@c62a184`, « Utilisateurs & Groupes / RBAC »
    - **Realized by:** SpecDD /src/users/group.sdd, SpecDD /src/rbac.sdd
    - **Verified by:** `AC-1`

- **FR-3 - MUST:** `can_read` et `can_write` évaluent un enregistrement déjà chargé. `OwnerOnly` compare via la
  réflexion générique de `ModelTrait` sur `owner_column()`. L'admin gagne sur toutes les politiques sauf `Public`.
    - **Source:** `arch@c62a184`, « Utilisateurs & Groupes / RBAC »
    - **Realized by:** SpecDD /src/rbac.sdd
    - **Verified by:** `AC-2`

- **FR-4 - MUST:** `OwnerOnly` sans `owner_column()` refuse (fail-closed).
    - **Source:** `arch@c62a184`, « Utilisateurs & Groupes / RBAC »
    - **Realized by:** SpecDD /src/rbac.sdd
    - **Verified by:** `AC-2`

- **FR-5 - MUST:** `can_create` n'a pas de record à comparer : `Public` et `OwnerOnly` autorisent toujours (le créateur
  devient propriétaire), `Group` et `AdminOnly` vérifient l'appartenance.
    - **Source:** `arch@c62a184`, « Utilisateurs & Groupes / RBAC »
    - **Realized by:** SpecDD /src/rbac.sdd
    - **Verified by:** `AC-2`

- **FR-6 - MUST:** `list_access` produit une condition de filtrage (`Unrestricted`, `FilterByOwner`, `Forbidden`) plutôt
  qu'un booléen, avec refus complet avant la construction de la requête pour `Group` ou `AdminOnly` sans appartenance.
    - **Source:** `arch@c62a184`, « Utilisateurs & Groupes / RBAC »
    - **Realized by:** SpecDD /src/rbac.sdd
    - **Verified by:** `AC-3`

- **FR-7 - MUST:** `GET /api/v1/users` (issue #4) est réservé aux admins, paginé, en lecture seule, et rend
  `{ id, subject, email, groups }` en deux requêtes (memberships puis groupes) pour éviter le N+1.
    - **Source:** `arch@c62a184`, « Liste admin des utilisateurs (issue #4) »
    - **Realized by:** SpecDD /src/rest/admin.sdd
    - **Verified by:** `AC-4`

- **FR-8 - MUST:** `GET /api/v1/me` (issue #24) est ouvert à tout principal authentifié, jamais `AdminOnly`, toujours
  restreint à son propre compte, et rend `{ subject, email, groups }`. Il réutilise `groups_by_user` (pas de second
  mécanisme de résolution de groupes).
    - **Source:** `arch@c62a184`, « Identité self-service (issue #24) »
    - **Realized by:** SpecDD /src/rest/me.sdd
    - **Verified by:** `AC-4`

## Constraints

- **CON-1 - MUST NOT:** La crate ne fournit aucune API d'assignation manuelle de groupe ni route de gestion
  d'utilisateur ; `User`, `Group` et `GroupMembership` n'implémentent pas `MiryadResource` (pas d'owner, jamais
  d'écriture) et ne sont donc exposés par aucune route REST ou MCP générique.
    - **Source:** `arch@c62a184`, « Utilisateurs & Groupes / RBAC »
    - **Realized by:** SpecDD /src/users/user.sdd, SpecDD /src/rest/admin.sdd

## Design Decisions

- **DEC-1 - Synchronisation, pas gestion**
    - **Context:** deux sources de vérité des appartenances divergeraient.
    - **Decision:** Authentik est la seule source de vérité pour l'appartenance aux groupes, `admin` compris.
    - **Consequences:** la liste admin est en lecture seule ; une révocation de groupe ne prend effet sur les tokens
      API existants qu'au prochain login navigateur de la personne (limite acceptée).
    - **Source:** décision structurante, `arch@c62a184`, « Utilisateurs & Groupes / RBAC »

- **DEC-2 - Fonctions génériques sur `ConnectionTrait`**
    - **Decision:** `resolve_user`, `is_admin`, `is_member` n'acceptent pas `&DatabaseConnection` en dur.
    - **Consequences:** le seed de migration les appelle via `manager.get_connection()`.
    - **Source:** `arch@c62a184`, « Utilisateurs & Groupes / RBAC »

## SpecDD Realization

`SpecDD /src/rbac.sdd`, `SpecDD /src/users/user.sdd`, `SpecDD /src/users/group.sdd`,
`SpecDD /src/users/membership.sdd`, `SpecDD /src/rest/admin.sdd`, `SpecDD /src/rest/me.sdd`.

## Acceptance Criteria

**ASSUMPTION :** correspondance exacte avec les `Scenario` des specs non vérifiée dans la migration DSD.

- **AC-1:** Un utilisateur dont le claim `groups` perd `editors` n'est plus membre de `editors` après son login suivant ;
  un groupe jamais vu est créé.
- **AC-2:** Un non-admin ne lit pas l'enregistrement d'autrui en `OwnerOnly` ; l'admin le lit ; `OwnerOnly` sans
  `owner_column` est refusé.
- **AC-3:** Un non-admin qui filtre par catégorie ne voit jamais les enregistrements d'autrui (le filtre est combiné en
  `AND` avec la condition RBAC).
- **AC-4:** `GET /api/v1/users` rend 403 à un non-admin ; `GET /api/v1/me` ne rend jamais le compte d'un autre.

## Open Issues

- **OI-1:** Un principal issu d'un token API reflète le dernier login navigateur de son `subject`. Voir
  `miryad-core/auth#OI-1`.
- **OI-2:** L'identité Matrix → utilisateur miryad (RBAC) n'est pas tranchée ; elle le sera avec le harnais de chatbot.
  Voir `miryad-core/matrix#OI-5`.
