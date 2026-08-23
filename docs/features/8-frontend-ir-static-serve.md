# Feature 8 — Support frontend (IR + service statique)

## Ce que la feature fait

Deux briques indépendantes, strictement backend, pour que le générateur TypeScript vivant dans le
template `miryad` (décision du 2026-08-23, cf. `docs/roadmap.md`) puisse produire des écrans CRUD
et que le serveur puisse ensuite les distribuer :

1. **`resource_ir::<E>()`** — fonction pure exposant une représentation intermédiaire (IR) par
   entité : champs + types, RBAC, `owner_column`, `filter_column`. Dérivée directement des
   métadonnées SeaORM déjà obligatoires pour toute `MiryadResource` (`EntityTrait::Column` →
   `ColumnDef` → `ColumnType`) — aucune annotation supplémentaire à ajouter par l'app, contrairement
   à l'OpenAPI actuel qui exige `#[derive(ToSchema)]`.
2. **Service statique** — routeur générique qui sert un répertoire de fichiers (les assets Vue déjà
   compilés) avec fallback SPA (toute route non capturée par l'API renvoie `index.html`).

## Ce qu'elle ne fait pas

- **Ne génère aucun fichier `.vue`/TS.** Le templating et l'écriture des composants vivent dans
  `miryad`, pas ici — cf. discussion du 2026-08-23.
- **Ne touche pas `/openapi.json`.** L'IR est un artefact séparé, jamais fusionné au document
  OpenAPI (feature 4b) — deux publics différents (consommateurs externes de l'API vs. outillage
  interne de scaffolding), donc deux contrats différents, chacun libre d'évoluer sans casser
  l'autre.
- **N'embarque pas les assets dans le binaire.** Le service statique sert un répertoire externe
  (chemin fourni par l'app au montage) — pas de `rust-embed`/`include_dir!` ici. Si une app veut
  embarquer ses assets dans son propre binaire, elle le fait à son niveau ; miryad-core reste
  agnostique de la provenance des fichiers.
- **Ne prescrit pas de framework JS.** L'IR est un JSON générique ; rien n'empêche un autre
  générateur que celui de `miryad` de le consommer un jour.

## Interfaces clés et modules touchés

### `src/ir.rs` (nouveau module, top-level — pas sous `rest/`, pas spécifique à une couche API)

```rust
#[derive(Debug, Clone, Serialize)]
pub enum FieldKind {
    String, Integer, Float, Boolean, DateTime, Json, Other,
}
// Mapping depuis sea_orm::ColumnType — volontairement plus restreint et stable que l'enum SeaORM
// brut (qui peut gagner des variantes), pour ne pas exposer un détail d'implémentation SeaORM
// comme contrat au générateur TS.

#[derive(Debug, Clone, Serialize)]
pub struct FieldIr {
    pub name: String,
    pub kind: FieldKind,
    pub nullable: bool,
    pub is_primary_key: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct EntityIr {
    pub resource_name: String,
    pub fields: Vec<FieldIr>,
    pub read_policy: AccessPolicy,   // AccessPolicy passe en #[derive(Serialize)]
    pub write_policy: AccessPolicy,
    pub owner_column: Option<String>,
    pub filter_column: Option<String>,
}

pub fn resource_ir<E: MiryadResource>() -> EntityIr;
```

`AccessPolicy` (`src/resource.rs`) gagne `#[derive(Serialize)]` — pas de raison de dupliquer
l'enum, il n'y a rien de sensible à cacher au générateur.

Production concrète du fichier IR : un petit binaire (le futur `miryad`, ou un `xtask`) lie l'app
cible, appelle `resource_ir::<E>()` pour chaque entité montée, sérialise en JSON — pas besoin de
lancer le serveur, `resource_ir` est une fonction pure comme `resource_openapi`.

### `src/frontend.rs` (nouveau module, feature Cargo optionnelle — proposition : `static-frontend`)

```rust
pub fn static_frontend_router<S>(assets_dir: impl Into<PathBuf>) -> axum::Router<S>
where S: Clone + Send + Sync + 'static;
// sert `assets_dir` (tower_http::services::ServeDir), fallback vers `assets_dir/index.html`
// pour toute route non capturée par l'API (routing côté client, SPA Vue Router)
```

Nouvelle dépendance : `tower-http` (feature `fs`) — actuellement seulement présent en
dev-dependency (`features = ["util"]`, pour les tests). À promouvoir en dépendance normale,
optionnelle, derrière la feature Cargo dédiée — même pattern que `swagger-ui`/`graphql`/`mcp`.

## Risques identifiés et questions ouvertes

1. **Mapping `ColumnType` → `FieldKind` : liste exacte des variantes à couvrir.** SeaORM a des
   dizaines de variantes (`Char`, `TinyInteger` .. `BigInteger`, `Decimal`, `Timestamp`, `Uuid`,
   `Json`, `Binary`, ...). La proposition ci-dessus regroupe large (String/Integer/Float/Boolean/
   DateTime/Json/Other) — à valider : est-ce suffisant pour que le générateur choisisse le bon
   widget de formulaire, ou faut-il plus de granularité (ex. distinguer `Integer` d'un `Decimal`
   pour ne pas arrondir un montant) ?

2. **Faut-il une notion de "label" (quel champ afficher dans une liste/un select) ?** Absente de
   `MiryadResource` aujourd'hui. Deux options : nouvelle méthode `label_column()` (défaut : PK),
   ou le générateur devine (première colonne `String`) sans rien ajouter au trait maintenant.

3. **Nom de la feature Cargo pour le service statique** (`static-frontend` proposé, à confirmer)
   et **chemin de configuration** — argument de fonction simple (`impl Into<PathBuf>`) suffit-il,
   ou faut-il l'intégrer à `MiryadAuthState`/un état de config plus large comme les autres
   routeurs ?

4. **Emplacement du binaire qui produit l'IR.** Proposé ci-dessus comme "le futur `miryad`", mais
   `miryad` (binaire) est une feature de miryad-core distincte (`src/bin/miryad.rs`, jamais
   commencée) dont le périmètre réel par rapport à ce besoin précis n'est pas tranché — pourrait
   aussi être un simple `xtask`/exemple documenté, sans attendre le binaire `miryad` complet.
