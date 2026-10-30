# Solution Design: Authentification

## Objective

Authentifier une requête de trois façons (session navigateur OIDC, JWT OIDC en Bearer, token API) et en tirer un
`AuthPrincipal` unique que REST et MCP consomment, avec ou sans base de données.

## Scope and Non-goals

La Solution couvre le flux Authorization Code, le cookie de session, la validation d'un JWT Bearer, les tokens API, le
self-service des tokens et les comptes de service. L'appartenance aux groupes et le RBAC sont dans
`miryad-core/access-control`.

Hors Solution : pas de rafraîchissement automatique du JWKS, pas de gestion de mots de passe, pas de comptes locaux.

## Applicable Domain Requirements

`miryad-core#BO-2` (profil sans base), `miryad-core#CON-3` (codes `MRD-AUTH-NNN`), `miryad-core#NFR-1`.

## Functional Requirements

- **FR-1 - MUST:** `MiryadAuthState` est l'état minimal requis, composé dans l'`AppState` de l'app par `FromRef` ;
  `auth_router<S>()` monte `/auth/login`, `/auth/callback` et `/auth/logout` (préfixe `/auth` figé). L'app n'a pas
  d'`AppState` concret imposé.
    - **Source:** `arch@c62a184`, « Authentification — OIDC + session cookie »
    - **Realized by:** SpecDD /src/auth/state.sdd, SpecDD /src/auth/mod.sdd
    - **Verified by:** `AC-1`

- **FR-2 - MUST:** Le cookie de session (`miryad_session`) porte un payload JSON chiffré (`id_token`, `subject`, `email`,
  `preferred_username`), sans délimiteur. Le cookie transitoire CSRF/nonce (`miryad_oidc_pending`) est chiffré avec un
  `Max-Age` de 300 s.
    - **Source:** `arch@c62a184`, « Authentification — OIDC + session cookie »
    - **Realized by:** SpecDD /src/auth/cookie.sdd
    - **Verified by:** `AC-1`

- **FR-3 - MUST:** Le claim `groups` est éphémère : consommé une seule fois par la synchronisation de groupes au
  callback, jamais persisté dans le cookie.
    - **Source:** `arch@c62a184`, « Authentification — OIDC + session cookie »
    - **Realized by:** SpecDD /src/auth/oidc.sdd
    - **Verified by:** `AC-1`

- **FR-4 - MUST:** `preferred_username` est propagé de l'`id_token` jusqu'à `AuthPrincipal` (session) ; le chemin token
  API le fixe à `None`. Les cookies posés avant ce claim se relisent `None` sans invalidation de session.
    - **Source:** décision du développeur principal, 2026-09-27 (`arch@c62a184`, « Tokens API + dual-auth »)
    - **Realized by:** SpecDD /src/auth/principal.sdd, SpecDD /src/auth/cookie.sdd
    - **Verified by:** `AC-1`

- **FR-5 - MUST:** `Authorization: Bearer <matériau>` est un choix définitif du client : aucun repli sur le cookie, quel
  que soit le résultat. Le matériau est un JWT si et seulement s'il contient exactement deux `.` (règle sans parse :
  un token API `mrd_…` n'en contient jamais).
    - **Source:** `arch@c62a184`, « Validation d'un JWT OIDC en Bearer », « Tokens API + dual-auth »
    - **Realized by:** SpecDD /src/auth/dual.sdd, SpecDD /src/auth/middleware.sdd
    - **Verified by:** `AC-2`

- **FR-6 - MUST:** Un JWT Bearer est vérifié sans E/S par `verify_bearer` : signature contre le JWKS du snapshot de
  discovery, `iss` égal à l'issuer, `aud` contenant le `client_id`, `exp`. Aucun nonce. Un échec rend `MRD-AUTH-020`
  si le JWT est valide mais échu, `MRD-AUTH-019` sinon, et le JWT n'entre jamais dans l'erreur.
    - **Source:** `arch@c62a184`, « Validation d'un JWT OIDC en Bearer »
    - **Realized by:** SpecDD /src/auth/oidc.sdd, SpecDD /src/auth/error.sdd
    - **Verified by:** `AC-2`

- **FR-7 - MUST:** Sans la feature `db`, le callback vérifie le `state`, échange le code, pose le cookie et redirige sans
  aucune écriture, et un Bearer non-JWT rend `MRD-AUTH-019` sans E/S. Les routes `/auth/*` sont identiques dans les
  deux profils.
    - **Source:** `arch@c62a184`, « Feature `db` et profil sans base »
    - **Realized by:** SpecDD /src/auth/mod.sdd, SpecDD /src/auth/state.sdd
    - **Verified by:** `AC-3`

- **FR-8 - MUST:** Un token API émis a la forme `mrd_<43 caractères base64url>` (32 octets aléatoires) et n'est stocké
  que sous forme de HMAC-SHA256 hexadécimal avec le poivre `token_pepper`. Il n'est jamais stocké ni journalisé en
  clair.
    - **Source:** `arch@c62a184`, « Tokens API + dual-auth »
    - **Realized by:** SpecDD /src/auth/token.sdd
    - **Verified by:** `AC-4`

- **FR-9 - MUST:** `ensure_token` reçoit la valeur du token de l'appelant et est idempotent (aucun doublon si un token de
  ce hash existe pour ce `subject`). Un secret changé **ajoute** un nouveau token sans supprimer l'ancien, valide
  jusqu'à révocation explicite.
    - **Source:** `arch@c62a184`, « Tokens API + dual-auth »
    - **Realized by:** SpecDD /src/auth/token.sdd, SpecDD /src/users/service_account.sdd
    - **Verified by:** `AC-4`

- **FR-10 - MUST:** `ensure_service_account` garantit l'existence d'un compte « machine » (jamais de login OIDC),
  membre de groupes donnés, authentifiable par un secret connu à l'avance. L'app décide si et quand l'appeler ; la
  crate n'automatise rien.
    - **Source:** `arch@c62a184`, « Tokens API + dual-auth » ; roadmap 2c
    - **Realized by:** SpecDD /src/users/service_account.sdd
    - **Verified by:** `AC-4`

- **FR-11 - MUST:** `GET/POST /api/v1/tokens` et `DELETE /api/v1/tokens/{id}` (self-service, issue #5) sont ouverts à
  tout principal authentifié, toujours restreints à son propre `subject`. `GET` ne renvoie jamais la valeur en clair ;
  `POST` la renvoie une seule fois ; `DELETE` vérifie l'appartenance (403 sinon), car `revoke_token` ne la vérifie pas.
    - **Source:** `arch@c62a184`, « Tokens API + dual-auth » ; issue #5
    - **Realized by:** SpecDD /src/rest/tokens.sdd
    - **Verified by:** `AC-5`

## Constraints

- **CON-1 - MUST NOT:** `OidcIdentity` et le `Debug` de `PrincipalSource::Session` et `Bearer` ne divulguent jamais
  l'`id_token` ou le JWT : le `Debug` manuel les remplace par `"[REDACTED]"`, et `OidcIdentity` ne se formate jamais
  avec `{:?}`.
    - **Source:** `arch@c62a184`, « Limites assumées », « Tokens API + dual-auth »
    - **Realized by:** SpecDD /src/auth/principal.sdd, SpecDD /src/auth/oidc.sdd

- **CON-2 - MUST:** Le claim `groups` est extrait à la main du payload JWT déjà vérifié.
    - **Source:** `arch@c62a184`, « Authentification — OIDC + session cookie »
    - **Rationale:** `groups` est spécifique à Authentik, non standard OIDC ; reconfigurer `CoreClient` avec des
      `AdditionalClaims` génériques pour un seul champ n'en valait pas le coût.

- **CON-3 - MUST:** Un en-tête `Bearer` présent mais invalide ne retombe pas sur le cookie : son échec est final.
    - **Source:** `arch@c62a184`, « Tokens API + dual-auth »
    - **Rationale:** pas de repli silencieux vers un mode plus faible.

## Design Decisions

- **DEC-1 - Détection du JWT sans parse**
    - **Decision:** deux `.` exactement.
    - **Consequences:** un token API (base64url) n'est jamais pris pour un JWT, sans coût de parsing.
    - **Source:** `arch@c62a184`, « Validation d'un JWT OIDC en Bearer »

- **DEC-2 - HMAC avec poivre plutôt qu'un KDF lent pour les tokens**
    - **Context:** le secret est déjà de haute entropie, ce n'est pas un mot de passe humain.
    - **Decision:** HMAC-SHA256 avec poivre.
    - **Source:** `arch@c62a184`, « Tokens API + dual-auth »

- **DEC-3 - Session non rafraîchie ; profil sans base de premier rang**
    - **Decision:** `MiryadAuthState` compte cinq champs sans `db` et sept avec ; le profil sans base est un cas
      d'usage prévu (API qui relaie l'`id_token` à l'API server Kubernetes, UI Kubernetes générique).
    - **Source:** `arch@c62a184`, « Profil sans base de données »

## Operations and Security

Limites assumées du Bearer OIDC :

- Pas de rafraîchissement automatique du JWKS : une rotation de clés côté fournisseur impose un redémarrage. Une
  signature sans clé correspondante arme `OidcClient::jwks_rotation_needed()` et la requête est rejetée en
  `MRD-AUTH-019` sans re-fetch.
- Pas de groupes pour un sujet jamais connecté par le navigateur : `PrincipalSource::Bearer` ne porte aucun groupe.
- `aud` est testé par « contient », pas par égalité ; `azp` n'est pas contrôlé (la vérification est commentée dans la
  source d'`openidconnect`) ; `iat` n'est pas contraint ; un `sub` vide est accepté, comme au login.
- Un `id_token` volé reste valable jusqu'à son `exp`.
- Un principal issu d'un token API reflète le dernier login navigateur de son `subject`, pas l'état Authentik en temps
  réel.

Source : `arch@c62a184`, « Limites assumées », « Authentification » et « Utilisateurs & Groupes / RBAC ».

## SpecDD Realization

Les specs de `src/auth/` (`state`, `cookie`, `oidc`, `principal`, `dual`, `middleware`, `token`, `error`, `mod`),
`SpecDD /src/users/service_account.sdd` et `SpecDD /src/rest/tokens.sdd`. Les mappings fins sont portés par les champs
`Realized by` ci-dessus.

## Acceptance Criteria

**ASSUMPTION :** correspondance exacte avec les `Scenario` des specs non vérifiée dans la migration DSD.

- **AC-1:** Un login OIDC complet pose un cookie chiffré, et le principal résolu porte `subject`, `email` et
  `preferred_username` ; les groupes ne figurent pas dans le cookie.
- **AC-2:** Un JWT valide donne un principal `Bearer` ; un JWT échu rend `MRD-AUTH-020`, tout autre rejet `MRD-AUTH-019` ;
  un en-tête `Bearer` invalide avec un cookie valide reste refusé.
- **AC-3:** Sans `db`, le callback ne fait aucune écriture et un Bearer non-JWT est refusé immédiatement.
- **AC-4:** Un token émis n'apparaît nulle part en clair ; `ensure_token` appelé deux fois avec la même valeur ne crée
  qu'un token.
- **AC-5:** Un utilisateur ne peut ni lister ni révoquer le token d'un autre (403).

## Open Issues

- **OI-1:** Le claim `groups` est lu uniquement au callback. Un compte qui n'utilise que des tokens ne voit jamais sa
  révocation de groupe prise en compte avant son prochain login navigateur. Limite acceptée ; **TBD :** décider si
  elle reste acceptable pour les comptes de service.
- **OI-2:** Les fragments OpenAPI des routes de compte (`/api/v1/tokens`) sont absents du document : voir
  `miryad-core/rest-api#OI-1`.
