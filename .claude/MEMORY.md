# Mémoire du projet

Ce fichier est maintenu par Claude au fil des sessions.
Les développeurs peuvent le lire, le corriger ou le compléter à tout moment.

---

## Contexte

miryad-core est né d'une session de bootstrap sur le projet **miryad** (template d'application
Rust/Vue "à la Debian", monorepo léger dans `$HOME/projets/kydah/miryad`, Gitea privé). En
discutant faisabilité, il est apparu que la mécanique (auth, RBAC, REST/GraphQL/MCP génériques,
workflow) devait vivre dans une crate séparée, publique, réutilisable — sur le modèle
`vynil`/`vynil-core`. miryad-core est cette crate : `$HOME/projets/miryad-core`, publique
(GitHub `sebt3/miryad-core` + crates.io), pattern nommage/emplacement calé sur `kuberest`/
`vanyline` (top-level sous `$HOME/projets/`, pas nested — contrairement à `vynil-core` qui est un
artefact historique d'extraction).

## Décisions d'architecture (2026-08-22)

- **Découpage par couche**, pas par mécanisme unique :
  - Composants UI (formulaires, tables) : scaffoldés dans le code consommateur via shadcn-vue —
    présentation pure, attendu que le développeur les retouche
  - Plomberie données (REST/GraphQL/MCP/RBAC/câblage front) : moteur générique dans miryad-core,
    piloté par un trait `MiryadResource` par entité SeaORM — jamais de boilerplate par entité
  - Raison : le développeur ne veut pas d'un générateur one-shot qui dérive du modèle avec le
    temps, ni d'une maintenance lourde côté applications produites ; il veut une forte intégration
    à des frameworks externes éprouvés plutôt que réinventer.
- **GraphQL : Seaography 2.0**, pas de resolvers écrits à la main. Seaography 2.0 génère un schéma
  GraphQL **dynamique à l'exécution** depuis les entités SeaORM (plus de régénération de code à
  chaque évolution du modèle depuis la 2.0) — validé par recherche web du 2026-08-22.
- **Workflow : apalis + apalis-postgres + apalis-workflow**, choisi explicitement pour rester dans
  le MVP (pas relégué en phase 2 — le développeur a été clair : "aucune application moderne
  n'arrive sans possibilité d'automatisation"). Raison technique du choix : support DAG natif,
  backend Postgres en pur client Rust (pas d'extension à installer côté cluster, contrairement à
  `pg_durable` de Microsoft) — compatible avec la contrainte "CNPG + authentik, rien d'autre à
  déployer".
- **Rhai (vynil-core) comme step-type natif du moteur de workflow** — sert le cas d'usage
  "fallback d'extraction défini par un admin en script" identifié dans l'exemple d'application
  cible (gestionnaire de recettes).
- miryad-core est **opinionated** (axum/SeaORM/Seaography/apalis imposés), contrairement à
  vynil-core qui reste générique sans framework imposé — assumé et documenté dans `AGENTS.md`
  pour éviter la confusion entre les deux philosophies.
- Licence : BSD 3-Clause, tranché le 2026-08-22 à la clôture du bootstrap — cohérence avec
  vynil/vynil-core. Fichier `LICENSE` en place.

## Frontière avec miryad (2026-08-22)

miryad-core = bibliothèque (crates.io), jamais de Dockerfile/Helm/déploiement ici. Tout ce qui
touche au build/déploiement de l'application reste dans `miryad` — corrigé après une première
version de la roadmap qui plaçait à tort un item "packaging production" ici.

## Scope MVP

Dans le MVP (pas de phase 2 pour aucun de ces points) : auth OIDC + tokens API, users/groupes/RBAC,
REST générique, GraphQL (Seaography), MCP (tools CRUD + sortie markdown), moteur de workflow
(apalis + step Rhai), frontend Vue générique (shadcn-vue) avec espace admin, CLI de scaffolding.

Hors-scope explicite pour l'instant : RBAC au niveau colonne (masquage de champs par rôle — au-delà
de lecture/écriture par entité), ownership multi-colonnes, UI de conception visuelle du DAG
(l'affichage du workflow dans l'UI suffit au MVP, pas forcément l'édition visuelle).

## Format du modèle de données (entrée du CLI de scaffolding)

**Pas encore défini.** C'est une question ouverte identifiée mais non résolue lors du bootstrap —
à trancher avant/pendant la feature qui touche au binaire `miryad`.

## Rôles

- Développeur principal : conception, décisions techniques, validation
- Claude : architecture, review
- Cadence (deepseek) : cadence l'implémentation d'une feature déjà designée, dispatche à `implement`
- Implement (Qwen3.6:35b-a3b, opencode) : implémentation guidée

## Mode de travail temporaire — Cadence/Implement indisponibles (2026-08-22)

Le workflow décrit dans `config.md` (Claude conçoit, Qwen/`implement` code, Cadence dispatche)
suppose ces deux agents disponibles. Ils ne le sont pas pour l'instant : les sessions en cours se
font en binôme direct développeur + Claude, Claude implémentant lui-même (fast-track), sans passer
par `.tasks/` ni par une copie de fichiers de référence — Claude lit directement les autres dépôts
locaux (`vanyline`, `vynil-core`, `kydah-mcp-template`, tous clonés sous `$HOME/projets/`) au
moment d'en porter un pattern, pas besoin de les dupliquer à l'avance. Le workflow `config.md`
reprendra tel quel dès que Cadence/Implement seront de nouveau disponibles.

## Clôture du bootstrap (2026-08-22)

Repo GitHub `sebt3/miryad-core` créé (public). Licence BSD-3-Clause tranchée à cette occasion (cf.
section Décisions d'architecture). `.claude/bootstrap.md` et `.claude/retrofit.md` (fichier mort,
jamais référencé par `CLAUDE.md`) supprimés. Le développeur préfère grouper plusieurs features
avant de pousser (`push ira à la fin des features`) plutôt que pousser à chaque commit — pas de
push après chaque feature, à faire quand demandé explicitement.

## Feature 7 (workflow) en standby — 2026-08-23

Après le déblocage de `vynil-core`, exploration comparative de moteurs de workflow pour la feature
7 : `apalis-workflow` (DAG typé Rust, pas piloté par la donnée — écarté), `Acts` (spike : le store
Postgres tronque sa table à chaque redémarrage, modèle déployé perdu — écarté), `Hatchet` (spike :
moteur serveur sain, mais le seul binding Rust disponible, `hatchet-sdk` non officiel, casse sur
`ctx.parent_output()` — le passage de données parent→enfant, cœur de tout DAG — contre toute
version de Hatchet self-hostable actuellement disponible ; bug isolé au binding, pas à la
plateforme), Temporal et Prefect écartés sur des critères d'architecture avant spike (empreinte de
déploiement pour Temporal ; flows intrinsèquement Python pour Prefect, incompatible avec
l'écosystème 100% Rust visé). Le fait-maison sur `apalis-postgres` reste l'option de secours mais
représente ~1200-1900 lignes de code distribué/concurrent (fan-in sous course, reprise sur crash)
à fiabiliser nous-mêmes — jugé disproportionné pour une brique dont la robustesse est justement
l'exigence de départ. Détail complet dans `docs/roadmap.md`, section 7.

Décision : la feature reste un pilier attendu (pas de dé-priorisation de fond), mais sans solution
d'implémentation saine identifiée pour l'instant — standby, à reprendre soit via un correctif
amont sur `hatchet-rust-sdk`, soit via une nouvelle option. Une nouvelle feature 7b (hooks métier
sur les 4 opérations CRUD génériques — absorbe une partie des cas d'usage simples que le workflow
aurait couverts) glisse dans le flow avant le frontend (8).

Le filtrage/tri étendu (au-delà de l'égalité exacte sur une seule colonne) identifié dans la même
discussion est un gap réel mais **pas MVP** — le développeur a explicitement recadré : scope déjà
large, à reprendre après le frontend (8) et un premier usage réel de miryad, pas avant. Devenu
feature 10 du roadmap. La CLI de scaffolding (feature 9) se glisse elle aussi avant le filtrage :
probablement le seul moyen de distribution automatisée d'un frontend généré — mais son périmètre
réel dans miryad-core (lib) vs. `miryad` (template applicatif) n'est pas tranché, à rediscuter
après l'implémentation de 7b. Leçon retenue : ne pas glisser dans le roadmap un gap que je repère
moi-même sans valider la priorité — cf. mémoire globale `feedback-roadmap-scope`.

## Frontière frontend/backend et réorganisation du roadmap — 2026-08-23

Discussion post-7b sur l'articulation front/back (feature 8) : le développeur principal a posé
deux problèmes de fond — (1) UI partagée en lib vs. template à cherry-picker (aucune des deux
options ne le satisfaisait), (2) comment produire une représentation intermédiaire du modèle de
données pour un scaffolding frontend, étant donné Rust "peu parseable" à ses yeux.

Sur (2) : faux problème — `rest/openapi.rs` (feature 4b) prouve déjà qu'une entité SeaORM
s'introspecte proprement (`#[derive(ToSchema)]`, compile-time). Rust reste golden source, l'IR en
est dérivée. Proposition initiale d'étendre `openapi.json` avec des extensions `x-miryad-*` —
**rejetée par le développeur** : un contrat public destiné à des consommateurs externes ne doit
pas porter des métadonnées internes de scaffolding (RBAC, owner_column). Deux publics, deux
artefacts. Retenu : un IR séparé (`resource_ir::<E>()`, feature 8), bâti directement sur
`EntityTrait::Column`/`ColumnType` (métadonnées SeaORM déjà obligatoires) plutôt que sur `utoipa` —
zéro annotation supplémentaire à ajouter par l'app.

Sur (1) : résolu en remarquant que shadcn-vue (déjà choisi au bootstrap) répond déjà à ce
dilemme — ce n'est pas une lib qu'on installe, c'est un CLI qui copie le code source dans le
projet consommateur. Même logique appliquée aux écrans CRUD : composables partagés et versionnés
(sans opinion produit, donc sans risque à partager) + écrans générés une fois dans le code de
l'app (comme shadcn-vue lui-même), jamais un template à cherry-picker. **Décision structurante** :
le générateur (TypeScript, consomme l'IR pour écrire les `.vue`) vit dans `miryad` (le template),
pas dans miryad-core — le développeur : "un projet miryad ne traite plus que le front, donc il est
naturel que son code d'utilitaires soit en TypeScript". miryad-core garde seulement `resource_ir`
et le service statique du frontend compilé (routeur générique, pas d'embarquement des assets dans
le binaire).

Conséquence sur le roadmap (réordonné par le développeur le 2026-08-23) : l'ancienne feature 9
(CLI de scaffolding générant des entités Rust depuis un modèle décrit à la main) **disparaît du
roadmap** — sa seule raison d'être (distribuer un frontend généré) n'existe plus, cette
responsabilité étant maintenant côté `miryad`. Nouvel ordre pour la suite : 8 (support frontend :
IR + service statique, design dans `docs/features/8-frontend-ir-static-serve.md`) → 9 (reprise du
moteur de workflow, ex-7, si une solution saine se présente) → 10 (filtrage/tri étendus, toujours
hors MVP). Détail dans `docs/roadmap.md`.

## Clôture de la feature 8 (support frontend : IR + service statique) — 2026-08-23

Trois corrections du développeur en cours de design, toutes intégrées avant implémentation :
(1) production du fichier IR — pas de binaire côté miryad-core ("un petit binaire xtask lie l'app
cible" était faux : miryad-core fournit `IrRegistry`, l'app écrit son propre `main()`, dédié ou
intégré à son binaire backend) ; (2) vocabulaire de types — repris d'OpenAPI (`type`/`format`),
pas un enum maison, sur suggestion du développeur reprenant le mérite (trancher la question) de
l'idée initiale (étendre `openapi.json`) sans en garder l'inconvénient (mélanger deux publics) ;
(3) `static-frontend` activée par défaut (`default = ["static-frontend"]`), contrairement à
`graphql`/`mcp`/`swagger-ui` — dépendance légère (`tower-http` seul), attendue par la quasi
totalité des apps miryad.

Implémentation : `src/ir.rs` (nouveau module — `FieldIr`/`EntityIr`/`resource_ir::<E>()`/
`IrRegistry`, type/format dérivés de `ColumnDef::get_column_type()`, PK détectée via
`PrimaryKey::iter()` + comparaison par nom puisque `Column` n'implémente pas `PartialEq`),
`src/frontend.rs` (nouveau module, `static_frontend_router` sur `tower_http::ServeDir` + fallback
SPA), `label_column()` ajoutée à `MiryadResource`, `AccessPolicy` gagne `Serialize`. Détail complet
dans `docs/architecture.md`, section "Support frontend (IR + service statique)".

## Clôture de la feature 7b (hooks métier CRUD) — 2026-08-23

Design mené en plusieurs allers-retours avec le développeur principal avant tout code : le
principe directeur ("un hook n'entre dans le scope que si les 3 surfaces peuvent l'honorer à
l'identique — un décalage fonctionnel entre endpoints est un no-go absolu") a mécaniquement réduit
le scope de départ (create/update/delete + before/after) à **`before_create` seul**, une fois
vérifié dans le source de `seaography` (pas la doc publique) que `before_active_model_save` ne se
déclenche que sur un insert et qu'aucun hook "after" avec accès aux données n'existe côté
Seaography. Choix d'API (méthode directe sur `MiryadResource` vs. trait compagnon vs. closure au
montage du routeur) tranché sur un comparatif objectif orienté ergonomie de l'app consommatrice,
pas sur une préférence de style — la méthode directe gagne parce qu'elle ne change rien aux points
d'enregistrement REST/GraphQL/MCP, contrairement aux deux autres options.

`HookError` est explicitement une erreur *applicative*, jamais un code `MRD-*` — distinction posée
par le développeur pour ne pas laisser croire qu'une règle métier rejetée est un bug de
miryad-core. Chaque surface la restitue sans lui imposer sa taxonomie interne (REST 422 JSON,
MCP `-32000`+`data`, GraphQL en concaténant `code`/`message` dans l'unique `String` que permet
`GuardAction::Block` — limite du mécanisme Seaography à cet endroit, pas un choix).

Implémentation : `resource.rs` (trait + `HookError`), `rest/core.rs`+`rest/error.rs` (REST+MCP
partagent `core::create`), `mcp/error.rs`+`mcp/protocol.rs`+`mcp/handler.rs` (champ `data` JSON-RPC
ajouté), `graphql/registry.rs` (dispatch par downcast de `&mut dyn Any` vers l'`ActiveModel`
concret, pointeur de fonction monomorphisé à l'enregistrement) + `graphql/hooks.rs`. Découverte en
cours d'implémentation : le hook GraphQL n'avait accès qu'au `GraphQlPrincipal` dérivé, pas à
l'`AuthPrincipal` d'origine — `graphql_handler` injecte désormais les deux dans les données de
requête. Détail complet dans `docs/architecture.md`, section "Hooks métier CRUD".

## Déblocage `vynil-core` et clôture de la feature 6 (MCP) — 2026-08-23

Les deux tickets amont sont résolus dans `vynil-core` v0.7.3 (2026-08-23) : #7 a introduit des
features Cargo indépendantes (`hbs`, `rhai`, `crypto`, toutes activées par défaut donc pas de
breaking change), #8 a vendorisé les helpers `json_*` de `handlebars_misc_helpers` directement
dans `vynil-core` (dépendant de `jmespath 0.5.0`, qui corrige le problème `Send`/`Sync` en amont)
plutôt que d'attendre un projet non maintenu depuis ~2 ans. `miryad-core` consomme désormais
`vynil-core = "0.7.3"` avec `default-features = false, features = ["hbs", "crypto"]`, gated
derrière la feature Cargo `mcp` (`dep:vynil-core`).

En reprenant le `src/mcp/` déjà écrit le 2026-08-22 (jamais committé faute de compiler, jamais
reviewé), la review a trouvé un bug réel dans `registry.rs` : `UpdateParams<E::Model>` utilisait
`#[serde(flatten)]` sur un champ `id` séparé, ce qui faisait perdre `id` (consommé par le champ
nommé avant que le flatten ne voie le reste) — `E::Model` le requiert comme PK non optionnelle,
donc `_update` échouait systématiquement en pratique, non détecté faute de test passant par ce
chemin de dispatch. Corrigé (désérialisation en deux passes) + test de régression avec DB SQLite
en mémoire. Feature 6 clôturée, migrée dans `docs/architecture.md` (section "Serveur MCP", statut
"bloqué" retiré). La feature 7 (workflow) profite du même déblocage (`vynil-core` feature `rhai`)
mais reste entièrement à designer — rien d'implémenté pour l'instant.
