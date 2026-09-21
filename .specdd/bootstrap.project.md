# SpecDD project specific overrides

## Flux de développement : SpecDD + test-first (TTD)

Unité de travail : une spec `.sdd`. Règle absolue : pas de code sans spec approuvée par
Sébastien, pas d'implémentation avant les tests dérivés de la spec.

1. **Spec** — la spec de la cible existe et est revue : rédigée en session avec l'agent
   `spec-dd`, ou produite en reverse-engineering par `spec-reverse` puis relue attentivement
   par Sébastien (le comportement accidentel du code existant ne devient un contrat qu'après
   cette relecture).
2. **Test d'abord** — l'agent `implementer` convertit d'abord chaque `Scenario` de la spec en
   test, et les fait compiler et échouer pour la bonne raison (comportement manquant, pas
   erreur de construction). Il ne touche pas encore au comportement de production.
3. **Implémentation** — le minimum de code qui fait passer les tests, strictement dans le
   périmètre `Owns` / `Can modify` de la spec.
4. **Validation** — l'agent `validator` relit spec ↔ code ↔ tests puis relance la batterie
   complète (tests et harnais sur chaque combinaison de features, fmt — voir `./tooling.sdd`)
   et produit une synthèse.
5. **Clôture** — les `Tasks` passent à `[x]` seulement après synthèse verte ; spec, code et
   tests avancent ensemble, sans `[x]` décoratif.

## Règle : une spec par fichier source

- Un fichier de `src/` = une spec `.sdd` du même basename dans le même répertoire
  (`src/foo.rs` ↔ `src/foo.sdd`), couvrant **tout** son comportement observable : `Purpose`,
  `Exposes`, `Accepts`, `Returns`, `Raises`, `Handles`, `Must`, `Must not`, `Depends on`,
  `Scenario`, `Done when`. Une description partielle n'est pas une spec.
- Un `mod.rs` a sa spec `mod.sdd` dans le répertoire du module : contrat d'agrégation
  (déclarations de sous-modules, gating `#[cfg(feature = ...)]`, ré-exports, erreurs et états
  partagés). Elle n'owne que son `mod.rs` — chaque enfant garde sa propre spec.
- Une spec par fichier de migration (`src/migration/m20260822_000001_*.sdd`, etc.) : une
  migration committée est un contrat de schéma immuable, corrigé par la migration suivante et
  jamais réécrit. Sa spec décrit `up`/`down`, les tables/colonnes/index posés, le préfixe
  `miryad_*` et le comportement en remontée partielle.
- Regrouper 2 à 3 fichiers dans une seule spec n'est admis que si ils forment **un seul
  contrat** (le fichier regroupé n'a pas de comportement indépendant de son hôte). La
  justification du regroupement va dans `Purpose` de la spec. Par défaut : une spec par fichier.
- Les tests inline `#[cfg(test)] mod tests` vivent dans le fichier source et sont régis par sa
  spec. Un fichier de `tests/` (crate d'intégration séparée) n'a pas de spec propre : il est
  rattaché par `Owns` ou `Can modify` à la spec du contrat inter-surface qu'il exerce
  (`tests/resource.rs` est rattaché à `./src/resource.sdd`).
- Hors `src/` : spec racine `miryad-core.sdd`, harnais de toolchain `tooling.sdd` (rustfmt +
  `[lints]`), CI `.github/workflows/workflows.sdd`. Une spec sans fichier source
  correspondant est légitime pour ces artefacts de configuration ; l'inverse (source sans
  spec) est interdit : on écrit la spec d'abord, jamais le code d'abord.

## Où vit le contexte

- La spec `.sdd` est le **contrat** : ce qui est dû, ce qui est interdit, ce qui est prouvé
  par un test. C'est l'autorité pour implémenter et pour vérifier.
- `./docs/architecture.md` est le **récit d'architecture** (le pourquoi, les décisions, les
  pièges de dépendances constatés dans le source des amont). Il se met à jour à la clôture
  d'une feature ; il ne remplace jamais un contrat et ne se substitue pas à une spec.
- `./docs/roadmap.md` est la **priorisation grosse maille**. Un gap repéré par un agent n'y
  entre pas sans décision explicite de Sébastien sur sa priorité.
- Il n'y a plus de design de feature dans `docs/features/` : le design se rédige dans la spec
  `.sdd` de la cible et dans ses `Tasks`.

## Contrats transverses (imposés à toute spec de `src/`)

- Erreurs : code unique `MRD-<DOMAINE>-NNN` (`MRD-AUTH-001`, `MRD-REST-012`, `MRD-MCP-004`, …),
  décliné dans la `Raises`/`Handles` de la spec du module. Exception de fond : @HookError est
  une erreur **applicative** de l'application consommatrice — elle ne porte jamais de code
  `MRD-*` et chaque surface la restitue sans lui imposer sa taxonomie.
- Logging : `tracing` uniquement. Jamais `println!` / `eprintln!` / `dbg!`.
- Gating de feature : chaque spec dit explicitement sous quelle(s) feature(s) son fichier
  compile (`static-frontend` — default, `swagger-ui`, `graphql`, `graphiql`, `mcp`) et ce que
  ce gating impose à la batterie de vérification.
- Une entité déclarée par @MiryadResource est lue **à l'identique** par REST, GraphQL et MCP :
  tout comportement ajouté sur une surface doit être honoré sur les deux autres. Un décalage
  fonctionnel entre surfaces est un no-go, pas une dette.
- Frontière de dépôt : miryad-core est une bibliothèque publiée sur crates.io, pas un
  déployable. Dockerfile, chart Helm, doc de déploiement CNPG/Authentik et générateur
  frontend TypeScript vivent dans `miryad` (le template), pas dans ce dépôt.

## Harnais clippy (guidage des agents)

- La source de vérité du harnais est la table `[lints]` de `Cargo.toml`, contractualisée par
  `./tooling.sdd` : `unsafe_code` interdit, `missing_docs` warn, `pedantic` et `cargo` en
  `deny`, famille stricte (`unwrap_used`, `expect_used`, `panic`, `unreachable`, `dbg_macro`,
  `todo`, `unimplemented`, `print_stdout`, `print_stderr`, `arithmetic_side_effects`,
  `indexing_slicing`, `unwrap_in_result`, `panic_in_result_fn`) en `deny`.
- Dans tout fichier touché par une tâche : **zéro warning** clippy (strict + pedantic + cargo)
  et `missing_docs` sur ce que la tâche expose. La dette préexistante ailleurs est tracée dans
  `./tooling.sdd`, ne doit pas grossir, et se purge module par module.
- Code de production : jamais `unwrap()` / `expect()` / `panic!` / `todo!` /
  `unimplemented!` / `dbg!` / `println!` — propager l'erreur du module ; indexation directe par
  `[]` → `get()` ou branche explicite ; arithmétique sans side-effect silencieux.
- `#[allow(...)]` des lints du harnais : uniquement sous `cfg(test)` (exemption portée par
  `src/lib.rs` pour la lib et par l'en-tête de chaque crate de `tests/`) ou avec commentaire
  de justification d'une ligne citant la spec concernée.

## Correction de bug

Un bug se traite par le contrat, pas par une rustine hors spec :

1. Qualifier le domaine : une spec existante est fausse/incomplète (le contrat n'a pas vu le cas),
   ou le code ne respecte pas sa spec (le contrat est bon).
2. Si le contrat est fautif : la spec est corrigée d'abord, avec le `Scenario` qui décrit le cas
   réel, et la correction est validée par Sébastien — une correction de bug change un contrat, elle
   n'esquive pas sa relecture.
3. Si le contrat est bon : l'`implementer` écrit d'abord le test du `Scenario` manquant, le voit
   échouer sur le code existant, puis corrige. Le test reste comme test de régression.
4. Le fix part sur une branche `fix/<description_courte>` depuis `origin/main` sauf s'il appartient
   à la feature en cours ; message de commit `fix: <description courte>` suivi de deux lignes,
   l'origine du bug puis le correctif.

## Rôle de Sébastien

Sébastien est la source de vérité sur l'intention. Toute ambiguïté de contrat, de sécurité, de
frontière, de priorité de roadmap ou de permission d'édition : on s'arrête et on demande, on ne
suppose pas. Un comportement de code existant qui ressemble à un bug est une question `[?]`,
jamais un contrat implicite.

Git : pas de `Co-Authored-By` dans les messages de commit, et `git push` uniquement sur demande
explicite — le push est une décision de Sébastien, pas une étape de clôture.
