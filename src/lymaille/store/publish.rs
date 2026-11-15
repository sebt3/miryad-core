//! Publication du catalogue par l'hôte — contrat porté par `./publish.sdd` (issue #73).
//!
//! Chemin **système** distinct des stores : des fonctions libres sur une connexion, sans sujet,
//! sans principal, sans `check_write` — appelables au premier démarrage sur une base fraîche
//! (`CON-7`). Create-only : une ligne absente est insérée (`INSERT … ON CONFLICT DO NOTHING`
//! portable SQLite/PostgreSQL) avec sa marque de provenance ; une ligne existante n'est
//! remplacée que si elle est gérée par la même source **et** non modifiée depuis sa publication
//! (empreinte SHA-256 du contenu stocké, recalculée à la comparaison) **et** si la version
//! publiée est strictement supérieure (semver). Sinon `Skipped` — la ligne de l'administrateur
//! reste intacte. Les mêmes validations que les hooks des `../definitions/` s'appliquent avant
//! toute écriture (les fonctions `valider_et_deriver` rendues `pub(crate)`, jamais une copie).

use sea_orm::ActiveValue;
use sea_orm::ColumnTrait;
use sea_orm::ConnectionTrait;
use sea_orm::EntityTrait;
use sea_orm::QueryFilter;
use sea_orm::sea_query::Alias;
use sea_orm::sea_query::Expr;
use sea_orm::sea_query::ExprTrait;
use sea_orm::sea_query::OnConflict;
use sea_orm::sea_query::Query;
use sha2::Digest as _;
use sha2::Sha256;

use super::db_text;
use crate::lymaille::definitions::agent;
use crate::lymaille::definitions::catalog_context;
use crate::lymaille::definitions::context_type;
use crate::lymaille::error::LymailleError;
use crate::lymaille::host::ScriptGrants;

/// Table des agents du catalogue (`../migration.rs`), colonnes `managed_*` comprises.
const CATALOG_AGENTS_TABLE: &str = "miryad_lymaille_catalog_agents";

/// Table des types de contexte du catalogue (`../migration.rs`).
const CATALOG_CONTEXT_TYPES_TABLE: &str = "miryad_lymaille_catalog_context_types";

/// Table des contextes du catalogue (`../migration.rs`).
const CATALOG_CONTEXTS_TABLE: &str = "miryad_lymaille_catalog_contexts";

/// La marque de l'hôte : `source` (nom stable, non vide) et `version` de son catalogue (semver
/// valide), validées par les `ensure_*` avant toute écriture (`./publish.sdd`).
#[derive(Clone, Debug)]
pub struct Provenance {
    /// Nom stable de la source qui publie (jamais vide).
    pub source: String,
    /// Version semver du catalogue publié.
    pub version: String,
}

/// Issue d'une publication : la ligne créée, laissée inchangée, mise à jour, ou sautée avec la
/// raison du saut.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PublishOutcome {
    /// La ligne était absente : insérée avec sa provenance.
    Created,
    /// La ligne est gérée, non modifiée, et la version publiée n'est pas supérieure : rien
    /// n'est écrit — c'est le résultat d'un rejeu sans changement.
    Unchanged,
    /// La ligne est gérée, non modifiée, et la version publiée est supérieure : contenu et
    /// provenance remplacés.
    Updated,
    /// La ligne existe mais l'hôte ne doit pas l'écraser — voir [`SkipReason`].
    Skipped(SkipReason),
}

/// Raison d'un saut : la ligne n'est pas de cette source (ou pas gée du tout), ou elle a été
/// modifiée depuis sa publication par l'hôte.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkipReason {
    /// `managed_by` est `NULL` ou une autre source : la ligne n'est pas gérée par cet hôte.
    Unmanaged,
    /// L'empreinte recalculée du contenu stocké ne correspond pas à `managed_digest` : un
    /// administrateur a modifié la ligne depuis sa publication.
    Modified,
}

/// Choix interne issu de la comparaison de la ligne relue avec la publication en cours.
#[derive(Clone, Copy)]
enum Choix {
    NonGeree,
    Modifiee,
    Inchangee,
    Publier,
}

/// Valide la provenance (`Must` étape 1) et rend sa version déjà parsée pour la comparaison.
fn valider_provenance(provenance: &Provenance) -> Result<semver::Version, LymailleError> {
    if provenance.source.is_empty() {
        return Err(LymailleError::InvalidDefinition(
            "provenance source must not be empty".to_owned(),
        ));
    }
    semver::Version::parse(&provenance.version).map_err(|cause| {
        LymailleError::InvalidDefinition(format!("provenance version is not valid semver: {cause}"))
    })
}

/// Un refus des hooks (validation partagée) traverse en `InvalidDefinition`, message expurgé par
/// le contrat des hooks (jamais un contenu de définition ni un secret).
fn refus_hook(cause: crate::resource::HookError) -> LymailleError {
    LymailleError::InvalidDefinition(cause.message)
}

/// JSON canonique : sérialisation compacte à clés triées récursivement (le `Map` de
/// `serde_json` peut être à ordre d'insertion selon les features du graphe — la forme canonique
/// ne doit rien au hasard, motif de `./write_journal.rs`).
fn canonique(valeur: &serde_json::Value) -> String {
    match valeur {
        serde_json::Value::Object(carte) => {
            let mut cles: Vec<&String> = carte.keys().collect();
            cles.sort();
            let mut sortie = String::from("{");
            for (index, cle) in cles.iter().enumerate() {
                if index > 0 {
                    sortie.push(',');
                }
                sortie.push_str(&serde_json::Value::String((*cle).clone()).to_string());
                sortie.push(':');
                if let Some(entre) = carte.get(cle.as_str()) {
                    sortie.push_str(&canonique(entre));
                }
            }
            sortie.push('}');
            sortie
        }
        serde_json::Value::Array(elements) => {
            let mut sortie = String::from("[");
            for (index, element) in elements.iter().enumerate() {
                if index > 0 {
                    sortie.push(',');
                }
                sortie.push_str(&canonique(element));
            }
            sortie.push(']');
            sortie
        }
        autre => autre.to_string(),
    }
}

/// Empreinte : SHA-256 hexadécimal (minuscules) de la sérialisation canonique de l'objet des
/// colonnes de contenu **telles que stockées** (`Must`, recalculée à chaque comparaison, jamais
/// lue d'un cache). Une colonne absente (`None`) entre comme `null`.
fn empreinte(colonnes: &[(&str, Option<serde_json::Value>)]) -> String {
    let mut carte = serde_json::Map::new();
    for (nom, valeur) in colonnes {
        carte.insert(
            (*nom).to_owned(),
            valeur.clone().unwrap_or(serde_json::Value::Null),
        );
    }
    hex::encode(Sha256::digest(
        canonique(&serde_json::Value::Object(carte)).as_bytes(),
    ))
}

/// Comparaison du `Must` étape 4 : ligne gérée par cette source ? empreinte stockée égale à
/// l'empreinte recalculée du contenu actuel ? version publiée strictement supérieure ?
fn decider(
    par_cette_source: bool,
    digest_stocke: Option<&str>,
    digest_recalcule: &str,
    version_geree: Option<&str>,
    publiee: &semver::Version,
) -> Result<Choix, LymailleError> {
    if !par_cette_source {
        return Ok(Choix::NonGeree);
    }
    if digest_stocke != Some(digest_recalcule) {
        return Ok(Choix::Modifiee);
    }
    match version_geree {
        None => Ok(Choix::Publier),
        Some(stockee) => {
            let stockee = semver::Version::parse(stockee).map_err(|_| {
                LymailleError::Store("stored managed_version is not a valid version".to_owned())
            })?;
            if publiee > &stockee {
                Ok(Choix::Publier)
            } else {
                Ok(Choix::Inchangee)
            }
        }
    }
}

/// Rendu d'un choix qui ne déclenche aucune écriture.
fn issue_sans_ecriture(choix: Choix) -> Option<PublishOutcome> {
    match choix {
        Choix::NonGeree => Some(PublishOutcome::Skipped(SkipReason::Unmanaged)),
        Choix::Modifiee => Some(PublishOutcome::Skipped(SkipReason::Modified)),
        Choix::Inchangee => Some(PublishOutcome::Unchanged),
        Choix::Publier => None,
    }
}

/// Première valeur posée d'un `ActiveValue` (le hook pose tout ce qu'il dérive ; `NotSet` est
/// une anomalie rendue en `Store`, jamais une panic).
fn pose<T: Into<sea_orm::Value>>(valeur: ActiveValue<T>) -> Option<T> {
    match valeur {
        ActiveValue::Set(v) | ActiveValue::Unchanged(v) => Some(v),
        ActiveValue::NotSet => None,
    }
}

/// Trace `Must` : une `tracing::info!` par objet (table, identifiant, issue), jamais le contenu
/// de la définition ni une valeur de paramètre.
fn tracer(table: &str, id: &str, issue: PublishOutcome) {
    let nom = match issue {
        PublishOutcome::Created => "created",
        PublishOutcome::Unchanged => "unchanged",
        PublishOutcome::Updated => "updated",
        PublishOutcome::Skipped(SkipReason::Unmanaged) => "skipped-unmanaged",
        PublishOutcome::Skipped(SkipReason::Modified) => "skipped-modified",
    };
    tracing::info!(table, id, issue = nom, "catalog object published");
}

/// Les trois colonnes `managed_*` d'une ligne relue et l'empreinte recalculée de son contenu — la
/// donnée du `Must` étape 4, sous une forme indépendante de l'entité.
struct LigneGeree {
    par_cette_source: bool,
    version_geree: Option<String>,
    digest_stocke: Option<String>,
    digest_recalcule: String,
}

/// Cible d'une publication : table et clé lues, marque de l'hôte et empreinte du nouveau contenu.
struct Publication<'a> {
    table: &'a str,
    cle: &'a str,
    identifiant: &'a str,
    provenance: &'a Provenance,
    publiee: &'a semver::Version,
    digest_nouvelle: &'a str,
}

/// `INSERT … ON CONFLICT DO NOTHING` sur la colonne clé (portable SQLite et PostgreSQL,
/// motif `Expr::val` de `./catalog.rs`) : `true` si une ligne a été insérée, `false` si la clé
/// existait déjà (course avec une autre instance — la ligne est alors relue et traitée comme
/// existante).
async fn inserer<C: ConnectionTrait>(
    db: &C,
    table: &str,
    cle: &str,
    colonnes: &[(&str, Expr)],
) -> Result<bool, LymailleError> {
    let statement = Query::insert()
        .into_table(Alias::new(table))
        .columns(colonnes.iter().map(|(nom, _)| Alias::new(*nom)))
        .values(
            colonnes
                .iter()
                .map(|(_, valeur)| valeur.clone())
                .collect::<Vec<_>>(),
        )
        .map_err(|cause| LymailleError::Store(cause.to_string()))?
        .on_conflict(OnConflict::column(Alias::new(cle)).do_nothing().to_owned())
        .to_owned();
    let rendu = db
        .execute(&statement)
        .await
        .map_err(|cause| LymailleError::Store(db_text(&cause)))?;
    Ok(rendu.rows_affected() == 1)
}

/// Le chemin complet du `Must` étapes 3 et 4, commun aux trois `ensure_*` : lire la ligne par sa
/// clé ; absente → `INSERT … ON CONFLICT DO NOTHING` (`Created`, ou relecture traitée comme
/// existante en cas de course) ; présente → comparaison de [`decider`], puis `UPDATE` gardé par
/// l'empreinte lue quand il faut publier. `relire` est appelé deux fois au plus (lecture initiale,
/// relecture après conflit).
async fn publier<C>(
    db: &C,
    cible: &Publication<'_>,
    creation: &[(&str, Expr)],
    modifications: &[(&str, Expr)],
    relue: Result<Option<LigneGeree>, LymailleError>,
    relecture: impl std::future::Future<Output = Result<Option<LigneGeree>, LymailleError>>,
) -> Result<PublishOutcome, LymailleError>
where
    C: ConnectionTrait,
{
    if let Some(ligne) = relue? {
        return rendre_ligne(db, cible, ligne, modifications).await;
    }
    let mut colonnes = creation.to_vec();
    colonnes.push(("managed_by", Expr::val(cible.provenance.source.clone())));
    colonnes.push(("managed_version", Expr::val(cible.publiee.to_string())));
    colonnes.push(("managed_digest", Expr::val(cible.digest_nouvelle.to_owned())));
    if inserer(db, cible.table, cible.cle, &colonnes).await? {
        let issue = PublishOutcome::Created;
        tracer(cible.table, cible.identifiant, issue);
        return Ok(issue);
    }
    // Course avec une autre instance : la ligne est relue et traitée comme existante.
    let ligne = relecture
        .await?
        .ok_or_else(|| LymailleError::Store("insert conflicted but the row cannot be read".to_owned()))?;
    rendre_ligne(db, cible, ligne, modifications).await
}

/// Comparaison et suite sans écrasement aveugle du `Must` étape 4 sur une ligne relue.
async fn rendre_ligne<C: ConnectionTrait>(
    db: &C,
    cible: &Publication<'_>,
    ligne: LigneGeree,
    modifications: &[(&str, Expr)],
) -> Result<PublishOutcome, LymailleError> {
    let choix = decider(
        ligne.par_cette_source,
        ligne.digest_stocke.as_deref(),
        &ligne.digest_recalcule,
        ligne.version_geree.as_deref(),
        cible.publiee,
    )?;
    let issue = if let Some(issue) = issue_sans_ecriture(choix) {
        issue
    } else {
        let mut colonnes: Vec<(&str, Expr)> = modifications.to_vec();
        colonnes.push(("managed_by", Expr::val(cible.provenance.source.clone())));
        colonnes.push(("managed_version", Expr::val(cible.publiee.to_string())));
        colonnes.push(("managed_digest", Expr::val(cible.digest_nouvelle.to_owned())));
        let remplacee = remplacer(
            db,
            cible.table,
            cible.cle,
            cible.identifiant,
            &ligne.digest_recalcule,
            &colonnes,
        )
        .await?;
        if remplacee {
            PublishOutcome::Updated
        } else {
            PublishOutcome::Skipped(SkipReason::Modified)
        }
    };
    tracer(cible.table, cible.identifiant, issue);
    Ok(issue)
}

/// Les trois colonnes de provenance d'une ligne, sous la forme lue par `publier`.
fn marquee(
    par_cette_source: bool,
    managed_version: Option<String>,
    managed_digest: Option<String>,
    digest_recalcule: String,
) -> LigneGeree {
    LigneGeree {
        par_cette_source,
        version_geree: managed_version,
        digest_stocke: managed_digest,
        digest_recalcule,
    }
}

/// `UPDATE … WHERE cle = … AND managed_digest = <empreinte lue>` : le garde du `Must` étape 4,
/// qui rend l'écrasement aveugle impossible (`Must not`).
async fn remplacer<C: ConnectionTrait>(
    db: &C,
    table: &str,
    cle: &str,
    identifiant: &str,
    digest_lue: &str,
    colonnes: &[(&str, Expr)],
) -> Result<bool, LymailleError> {
    let mut update = Query::update();
    update.table(Alias::new(table));
    for (colonne, valeur) in colonnes {
        update.value(Alias::new(*colonne), valeur.clone());
    }
    update
        .and_where(Expr::col(Alias::new(cle)).eq(identifiant.to_owned()))
        .and_where(Expr::col(Alias::new("managed_digest")).eq(digest_lue.to_owned()));
    let rendu = db
        .execute(&update)
        .await
        .map_err(|cause| LymailleError::Store(db_text(&cause)))?;
    Ok(rendu.rows_affected() == 1)
}

/// Relit la ligne `catalog_agents` par sa clé, avec sa marque et l'empreinte recalculée de son
/// contenu (`definition` telle que stockée).
async fn relire_agent<C: ConnectionTrait>(
    db: &C,
    id: &str,
    source: &str,
) -> Result<Option<LigneGeree>, LymailleError> {
    let ligne = agent::Entity::find()
        .filter(agent::Column::AgentId.eq(id))
        .one(db)
        .await
        .map_err(|cause| LymailleError::Store(db_text(&cause)))?;
    Ok(ligne.map(|ligne| {
        marquee(
            ligne.managed_by.as_deref() == Some(source),
            ligne.managed_version,
            ligne.managed_digest,
            empreinte(&[("definition", Some(ligne.definition))]),
        )
    }))
}

/// Publie un agent du catalogue (`G_…`) : `catalog_agents`, validations du hook de
/// `../definitions/agent.rs` avant toute écriture.
///
/// # Errors
///
/// [`LymailleError::InvalidDefinition`] si la provenance est vide ou hors semver, ou si le hook
/// refuse la définition ; [`LymailleError::Store`] en cas d'échec de la base.
pub async fn ensure_agent<C: ConnectionTrait>(
    db: &C,
    provenance: &Provenance,
    definition: &::lymaille::AgentDefinition,
) -> Result<PublishOutcome, LymailleError> {
    let publiee = valider_provenance(provenance)?;
    let valeur = serde_json::to_value(definition)
        .map_err(|cause| LymailleError::Store(format!("agent definition is not serializable: {cause}")))?;
    let valide = agent::Entity::valider_et_deriver(
        agent::ActiveModel {
            definition: ActiveValue::Set(valeur),
            ..Default::default()
        },
        None,
    )
    .map_err(refus_hook)?;
    let id = pose(valide.agent_id)
        .ok_or_else(|| LymailleError::Store("agent hook did not derive agent_id".to_owned()))?;
    let kind =
        pose(valide.kind).ok_or_else(|| LymailleError::Store("agent hook did not derive kind".to_owned()))?;
    let profile = pose(valide.profile)
        .ok_or_else(|| LymailleError::Store("agent hook did not derive profile".to_owned()))?;
    let definition = pose(valide.definition)
        .ok_or_else(|| LymailleError::Store("agent hook lost its definition".to_owned()))?;
    let cree_le = pose(valide.created_at)
        .ok_or_else(|| LymailleError::Store("agent hook did not derive created_at".to_owned()))?;
    let cible = Publication {
        table: CATALOG_AGENTS_TABLE,
        cle: "agent_id",
        identifiant: &id,
        provenance,
        publiee: &publiee,
        digest_nouvelle: &empreinte(&[("definition", Some(definition.clone()))]),
    };
    publier(
        db,
        &cible,
        &[
            ("agent_id", Expr::val(id.clone())),
            ("kind", Expr::val(kind.clone())),
            ("profile", Expr::val(profile.clone())),
            ("definition", Expr::val(definition.clone())),
            ("created_at", Expr::val(cree_le)),
        ],
        &[
            ("kind", Expr::val(kind)),
            ("profile", Expr::val(profile)),
            ("definition", Expr::val(definition)),
        ],
        relire_agent(db, &id, provenance.source.as_str()).await,
        relire_agent(db, &id, provenance.source.as_str()),
    )
    .await
}

/// Relit la ligne `catalog_context_types` par sa clé, avec sa marque et l'empreinte recalculée
/// de son contenu (`active`, `allowed_agents`, `script_grants` tels que stockés — un `active`
/// `NULL` entre comme `null`).
async fn relire_contexte_type<C: ConnectionTrait>(
    db: &C,
    id: &str,
    source: &str,
) -> Result<Option<LigneGeree>, LymailleError> {
    let ligne = context_type::Entity::find()
        .filter(context_type::Column::Name.eq(id))
        .one(db)
        .await
        .map_err(|cause| LymailleError::Store(db_text(&cause)))?;
    Ok(ligne.map(|ligne| {
        marquee(
            ligne.managed_by.as_deref() == Some(source),
            ligne.managed_version,
            ligne.managed_digest,
            empreinte(&[
                ("active", ligne.active),
                ("allowed_agents", Some(ligne.allowed_agents)),
                ("script_grants", Some(ligne.script_grants)),
            ]),
        )
    }))
}

/// Publie un type de contexte du catalogue : `catalog_context_types`, validations du hook de
/// `../definitions/context_type.rs` avant toute écriture (y compris la relecture d'existence
/// des toolsets de `script_grants` par la connexion fournie).
///
/// # Errors
///
/// [`LymailleError::InvalidDefinition`] si la provenance est vide ou hors semver, ou si le hook
/// refuse le type, ses agents autorisés ou ses `script_grants` ; [`LymailleError::Store`] en cas
/// d'échec de la base.
pub async fn ensure_context_type<C: ConnectionTrait>(
    db: &C,
    provenance: &Provenance,
    definition: &::lymaille::ContextType,
    allowed_agents: &[String],
    script_grants: &ScriptGrants,
) -> Result<PublishOutcome, LymailleError> {
    let publiee = valider_provenance(provenance)?;
    let active = serde_json::to_value(definition)
        .map_err(|cause| LymailleError::Store(format!("context type is not serializable: {cause}")))?;
    let autorises = serde_json::to_value(allowed_agents)
        .map_err(|cause| LymailleError::Store(format!("allowed agents are not serializable: {cause}")))?;
    let grants = serde_json::json!({
        "toolsets": script_grants.toolsets,
        "modules": script_grants.modules,
    });
    let valide = context_type::Entity::valider_et_deriver(
        db,
        context_type::ActiveModel {
            active: ActiveValue::Set(Some(active)),
            draft: ActiveValue::Set(None),
            allowed_agents: ActiveValue::Set(autorises),
            script_grants: ActiveValue::Set(grants),
            ..Default::default()
        },
        None,
    )
    .await
    .map_err(refus_hook)?;
    let id = pose(valide.name)
        .ok_or_else(|| LymailleError::Store("context type hook did not derive name".to_owned()))?;
    let version = pose(valide.version)
        .ok_or_else(|| LymailleError::Store("context type hook did not derive version".to_owned()))?;
    let active = pose(valide.active)
        .and_then(|version| version)
        .ok_or_else(|| LymailleError::Store("context type hook lost its active version".to_owned()))?;
    let autorises = pose(valide.allowed_agents)
        .ok_or_else(|| LymailleError::Store("context type hook lost its allowed agents".to_owned()))?;
    let grants = pose(valide.script_grants)
        .ok_or_else(|| LymailleError::Store("context type hook lost its script grants".to_owned()))?;
    let cree_le = pose(valide.created_at)
        .ok_or_else(|| LymailleError::Store("context type hook did not derive created_at".to_owned()))?;
    let cible = Publication {
        table: CATALOG_CONTEXT_TYPES_TABLE,
        cle: "name",
        identifiant: &id,
        provenance,
        publiee: &publiee,
        digest_nouvelle: &empreinte(&[
            ("active", Some(active.clone())),
            ("allowed_agents", Some(autorises.clone())),
            ("script_grants", Some(grants.clone())),
        ]),
    };
    publier(
        db,
        &cible,
        &[
            ("name", Expr::val(id.clone())),
            ("version", Expr::val(version.clone())),
            ("allowed_agents", Expr::val(autorises.clone())),
            ("script_grants", Expr::val(grants.clone())),
            ("active", Expr::val(active.clone())),
            // `draft` n'est jamais écrit par la publication : sa valeur par défaut à l'insertion
            // est `NULL`, la colonne de brouillon appartient à la session de configuration.
            ("draft", Expr::null()),
            ("created_at", Expr::val(cree_le)),
        ],
        &[
            ("version", Expr::val(version)),
            ("allowed_agents", Expr::val(autorises)),
            ("script_grants", Expr::val(grants)),
            ("active", Expr::val(active)),
        ],
        relire_contexte_type(db, &id, provenance.source.as_str()).await,
        relire_contexte_type(db, &id, provenance.source.as_str()),
    )
    .await
}

/// Relit la ligne `catalog_contexts` par sa clé, avec sa marque et l'empreinte recalculée de son
/// contenu (`context` et `context_type` tels que stockés — une référence `NULL` comme `null`).
async fn relire_contexte<C: ConnectionTrait>(
    db: &C,
    id: &str,
    source: &str,
) -> Result<Option<LigneGeree>, LymailleError> {
    let ligne = catalog_context::Entity::find()
        .filter(catalog_context::Column::Name.eq(id))
        .one(db)
        .await
        .map_err(|cause| LymailleError::Store(db_text(&cause)))?;
    Ok(ligne.map(|ligne| {
        let reference = ligne
            .context_type
            .map(|nom| serde_json::Value::String(nom.clone()));
        marquee(
            ligne.managed_by.as_deref() == Some(source),
            ligne.managed_version,
            ligne.managed_digest,
            empreinte(&[("context", Some(ligne.context)), ("context_type", reference)]),
        )
    }))
}

/// Publie un contexte du catalogue (niveau `shared`) : `catalog_contexts`, validations du hook
/// de `../definitions/catalog_context.rs` avant toute écriture.
///
/// # Errors
///
/// [`LymailleError::InvalidDefinition`] si la provenance est vide ou hors semver, ou si le hook
/// refuse le contexte (dont le détecteur de secrets, sans écho de la valeur) ;
/// [`LymailleError::Store`] en cas d'échec de la base.
pub async fn ensure_catalog_context<C: ConnectionTrait>(
    db: &C,
    provenance: &Provenance,
    context: &::lymaille::ToolsetContext,
    context_type: Option<&str>,
) -> Result<PublishOutcome, LymailleError> {
    let publiee = valider_provenance(provenance)?;
    let valeur = serde_json::to_value(context)
        .map_err(|cause| LymailleError::Store(format!("catalog context is not serializable: {cause}")))?;
    let valide = catalog_context::Entity::valider_et_deriver(
        catalog_context::ActiveModel {
            context: ActiveValue::Set(valeur),
            context_type: ActiveValue::Set(context_type.map(str::to_owned)),
            ..Default::default()
        },
        None,
    )
    .map_err(refus_hook)?;
    let id = pose(valide.name)
        .ok_or_else(|| LymailleError::Store("catalog context hook did not derive name".to_owned()))?;
    let contexte = pose(valide.context)
        .ok_or_else(|| LymailleError::Store("catalog context hook lost its context".to_owned()))?;
    let type_ref = pose(valide.context_type)
        .ok_or_else(|| LymailleError::Store("catalog context hook lost its type reference".to_owned()))?;
    let cree_le = pose(valide.created_at)
        .ok_or_else(|| LymailleError::Store("catalog context hook did not derive created_at".to_owned()))?;
    let cible = Publication {
        table: CATALOG_CONTEXTS_TABLE,
        cle: "name",
        identifiant: &id,
        provenance,
        publiee: &publiee,
        digest_nouvelle: &empreinte(&[
            ("context", Some(contexte.clone())),
            (
                "context_type",
                type_ref
                    .as_ref()
                    .map(|nom| serde_json::Value::String(nom.clone())),
            ),
        ]),
    };
    publier(
        db,
        &cible,
        &[
            ("name", Expr::val(id.clone())),
            ("context", Expr::val(contexte.clone())),
            (
                "context_type",
                type_ref
                    .clone()
                    .map_or_else(Expr::null, |nom| Expr::val(nom.clone())),
            ),
            ("created_at", Expr::val(cree_le)),
        ],
        &[
            ("context", Expr::val(contexte)),
            ("context_type", type_ref.map_or_else(Expr::null, Expr::val)),
        ],
        relire_contexte(db, &id, provenance.source.as_str()).await,
        relire_contexte(db, &id, provenance.source.as_str()),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::CATALOG_AGENTS_TABLE;
    use super::CATALOG_CONTEXT_TYPES_TABLE;
    use super::CATALOG_CONTEXTS_TABLE;
    use super::Provenance;
    use super::PublishOutcome;
    use super::SkipReason;
    use crate::lymaille::host::ScriptGrants;
    use crate::lymaille::store::publish::{ensure_agent, ensure_catalog_context, ensure_context_type};
    use crate::lymaille::store::test_support::test_db;
    use ::lymaille::AgentDefinition;
    use ::lymaille::AgentKind;
    use ::lymaille::ContextLevel;
    use ::lymaille::ContextType;
    use ::lymaille::SecretDetector;
    use ::lymaille::ToolsetContext;
    use sea_orm::ActiveModelTrait as _;
    use sea_orm::ActiveValue;
    use sea_orm::ConnectionTrait as _;
    use sea_orm::DatabaseConnection;
    use sea_orm::sea_query::Alias;
    use sea_orm::sea_query::Expr;
    use sea_orm::sea_query::ExprTrait;
    use sea_orm::sea_query::Query;
    use serde_json::json;
    use vynil_core::Semver;

    fn provenance(source: &str, version: &str) -> Provenance {
        Provenance {
            source: source.to_owned(),
            version: version.to_owned(),
        }
    }

    fn semver(text: &str) -> Semver {
        Semver::parse(text).expect("la fixture porte une version valide")
    }

    fn agent_catalogue(id: &str, prompt: &str) -> AgentDefinition {
        AgentDefinition::new(id.to_owned(), AgentKind::Work, prompt.to_owned())
            .with_profile("qwen".to_owned())
    }

    fn type_catalogue(schema: serde_json::Value) -> ContextType {
        ContextType::new("G_home".to_owned(), semver("1.0.0"), schema)
            .expect("le type de fixture se construit")
    }

    fn contexte_catalogue(namespace: &str) -> ToolsetContext {
        ToolsetContext::new("G_home".to_owned(), ContextLevel::Shared)
            .expect("le contexte de fixture se construit")
            .with_parameter("namespace".to_owned(), json!(namespace))
            .expect("le paramètre de fixture n'est pas un secret")
    }

    /// Publie le trio du Scenario (« un type `G_home`, un contexte `G_home` de type `G_home` et
    /// un agent `G_triage` ») — rend `[type, contexte, agent]`.
    async fn publier_trio(
        db: &DatabaseConnection,
        prov: &Provenance,
        schema: serde_json::Value,
        namespace: &str,
        prompt: &str,
    ) -> Vec<PublishOutcome> {
        vec![
            ensure_context_type(
                db,
                prov,
                &type_catalogue(schema),
                &["G_triage".to_owned()],
                &ScriptGrants::default(),
            )
            .await
            .expect("la publication du type de contexte réussit"),
            ensure_catalog_context(db, prov, &contexte_catalogue(namespace), Some("G_home"))
                .await
                .expect("la publication du contexte réussit"),
            ensure_agent(db, prov, &agent_catalogue("G_triage", prompt))
                .await
                .expect("la publication de l'agent réussit"),
        ]
    }

    /// Les trois colonnes de provenance d'une ligne, lues en brut par leur clé.
    async fn lire_provenance(
        db: &DatabaseConnection,
        table: &str,
        id_col: &str,
        id: &str,
    ) -> (Option<String>, Option<String>, Option<String>) {
        let mut select = Query::select();
        select
            .columns([
                Alias::new("managed_by"),
                Alias::new("managed_version"),
                Alias::new("managed_digest"),
            ])
            .from(Alias::new(table))
            .and_where(Expr::col(Alias::new(id_col)).eq(id));
        let row = db
            .query_one(&select)
            .await
            .expect("la provenance se relit")
            .expect("la ligne existe");
        (
            row.try_get("", "managed_by")
                .expect("managed_by est une colonne TEXT nullable"),
            row.try_get("", "managed_version")
                .expect("managed_version est une colonne TEXT nullable"),
            row.try_get("", "managed_digest")
                .expect("managed_digest est une colonne TEXT nullable"),
        )
    }

    /// Colonne texte (ou JSON relu en texte) d'une ligne, pour comparer avant/après.
    async fn lire_colonne(
        db: &DatabaseConnection,
        table: &str,
        colonne: &str,
        id_col: &str,
        id: &str,
    ) -> Option<String> {
        let mut select = Query::select();
        select
            .column(Alias::new(colonne))
            .from(Alias::new(table))
            .and_where(Expr::col(Alias::new(id_col)).eq(id));
        db.query_one(&select)
            .await
            .expect("la colonne se relit")
            .and_then(|row| {
                row.try_get::<Option<String>>("", colonne)
                    .expect("la colonne se relit en texte")
            })
    }

    async fn compter(db: &DatabaseConnection, table: &str) -> i64 {
        let mut select = Query::select();
        select.expr(Expr::cust("COUNT(*)")).from(Alias::new(table));
        let row = db
            .query_one(&select)
            .await
            .expect("le comptage réussit")
            .expect("COUNT(*) rend une ligne");
        row.try_get::<i64>("", "COUNT(*)")
            .or_else(|_| row.try_get::<i32>("", "COUNT(*)").map(i64::from))
            .expect("le comptage est un entier")
    }

    /// Scenario « base fraîche, aucun sujet administrateur » : les trois objets sont publiés sur
    /// une base migrée sans aucun utilisateur, `Created` pour chacun, sans `UnknownSubject` ;
    /// les lignes portent la marque, la version et une empreinte non nulle.
    #[tokio::test]
    async fn base_fraiche_publie_sans_sujet_administrateur() {
        let db = test_db().await;
        let rendus = publier_trio(
            &db,
            &provenance("vanyline", "1.0.0"),
            json!({"type": "object"}),
            "prod",
            "trie",
        )
        .await;
        assert_eq!(
            rendus,
            vec![
                PublishOutcome::Created,
                PublishOutcome::Created,
                PublishOutcome::Created
            ],
            "les trois objets sont créés sur une base sans aucun compte"
        );
        for (table, id_col, id) in [
            (CATALOG_CONTEXT_TYPES_TABLE, "name", "G_home"),
            (CATALOG_CONTEXTS_TABLE, "name", "G_home"),
            (CATALOG_AGENTS_TABLE, "agent_id", "G_triage"),
        ] {
            let (source, version, digest) = lire_provenance(&db, table, id_col, id).await;
            assert_eq!(source.as_deref(), Some("vanyline"), "{table} : managed_by");
            assert_eq!(version.as_deref(), Some("1.0.0"), "{table} : managed_version");
            assert_eq!(
                digest.as_deref().map_or(0, str::len),
                64,
                "{table} : empreinte SHA-256 hex non nulle"
            );
        }
    }

    /// Scenario « rejouer la publication ne change rien » : même provenance, `Unchanged` partout
    /// et aucune ligne réécrite (contenu, horodatage et provenance identiques).
    #[tokio::test]
    async fn rejouer_la_publication_ne_change_rien() {
        let db = test_db().await;
        publier_trio(
            &db,
            &provenance("vanyline", "1.0.0"),
            json!({"type": "object"}),
            "prod",
            "trie",
        )
        .await;
        let avant = (
            lire_colonne(&db, CATALOG_CONTEXTS_TABLE, "context", "name", "G_home").await,
            lire_colonne(&db, CATALOG_AGENTS_TABLE, "created_at", "agent_id", "G_triage").await,
            lire_provenance(&db, CATALOG_AGENTS_TABLE, "agent_id", "G_triage").await,
        );
        let rendus = publier_trio(
            &db,
            &provenance("vanyline", "1.0.0"),
            json!({"type": "object"}),
            "prod",
            "trie",
        )
        .await;
        assert_eq!(
            rendus,
            vec![
                PublishOutcome::Unchanged,
                PublishOutcome::Unchanged,
                PublishOutcome::Unchanged
            ],
            "le rejeu sans changement ne publie rien"
        );
        let apres = (
            lire_colonne(&db, CATALOG_CONTEXTS_TABLE, "context", "name", "G_home").await,
            lire_colonne(&db, CATALOG_AGENTS_TABLE, "created_at", "agent_id", "G_triage").await,
            lire_provenance(&db, CATALOG_AGENTS_TABLE, "agent_id", "G_triage").await,
        );
        assert_eq!(avant, apres, "aucune ligne n'est réécrite");
    }

    /// Scenario « une ligne modifiée par un administrateur reste intacte » : le contexte modifié
    /// rend `Skipped(Modified)` et garde la modification de l'administrateur ; les lignes non
    /// modifiées sont mises à jour (`Updated`).
    #[tokio::test]
    async fn une_ligne_modifiee_par_un_administrateur_reste_intacte() {
        let db = test_db().await;
        publier_trio(
            &db,
            &provenance("vanyline", "1.0.0"),
            json!({"type": "object"}),
            "prod",
            "trie",
        )
        .await;
        // L'administrateur modifie le contexte `G_home` par le CRUD : les hooks préservent la
        // provenance (issue #73, `../definitions/catalog_context.rs`) mais le contenu change.
        db.execute_unprepared(&format!(
            "UPDATE {CATALOG_CONTEXTS_TABLE} SET context = \
             '{{\"name\":\"G_home\",\"level\":\"shared\",\"parameters\":{{\"namespace\":\"admin\"}}}}' \
             WHERE name = 'G_home'"
        ))
        .await
        .expect("la modification de l'administrateur s'applique");

        let rendus = publier_trio(
            &db,
            &provenance("vanyline", "1.1.0"),
            json!({"type": "object", "title": "G_home"}),
            "preprod",
            "trie affiné",
        )
        .await;
        assert_eq!(
            rendus,
            vec![
                PublishOutcome::Updated,
                PublishOutcome::Skipped(SkipReason::Modified),
                PublishOutcome::Updated,
            ],
            "le contexte modifié reste intact, les autres sont mis à jour"
        );
        let contenu = lire_colonne(&db, CATALOG_CONTEXTS_TABLE, "context", "name", "G_home").await;
        assert!(
            contenu.as_deref().is_some_and(|texte| texte.contains("admin")),
            "le contexte garde la modification de l'administrateur : {contenu:?}"
        );
        let (source, version, _) = lire_provenance(&db, CATALOG_CONTEXTS_TABLE, "name", "G_home").await;
        assert_eq!(source.as_deref(), Some("vanyline"));
        assert_eq!(
            version.as_deref(),
            Some("1.0.0"),
            "la ligne sautée garde sa provenance d'origine"
        );
    }

    /// Scenario « une ligne gérée et non modifiée est mise à jour par une version supérieure » :
    /// `Updated` puis `Unchanged` (version égale) puis `Unchanged` (version inférieure, aucune
    /// écriture).
    #[tokio::test]
    async fn une_ligne_geree_non_modifiee_est_mise_a_jour_par_une_version_superieure() {
        let db = test_db().await;
        let premier = ensure_agent(
            &db,
            &provenance("vanyline", "1.0.0"),
            &agent_catalogue("G_triage", "trie"),
        )
        .await
        .expect("la publication initiale réussit");
        assert_eq!(premier, PublishOutcome::Created);
        let digest_initial = lire_provenance(&db, CATALOG_AGENTS_TABLE, "agent_id", "G_triage")
            .await
            .2;

        let mis_a_jour = ensure_agent(
            &db,
            &provenance("vanyline", "1.1.0"),
            &agent_catalogue("G_triage", "trie affiné"),
        )
        .await
        .expect("la publication supérieure réussit");
        assert_eq!(mis_a_jour, PublishOutcome::Updated);
        let (source, version, digest) =
            lire_provenance(&db, CATALOG_AGENTS_TABLE, "agent_id", "G_triage").await;
        assert_eq!(source.as_deref(), Some("vanyline"));
        assert_eq!(
            version.as_deref(),
            Some("1.1.0"),
            "`managed_version` porte la version publiée"
        );
        assert_ne!(
            digest, digest_initial,
            "l'empreinte est recalculée sur le nouveau contenu"
        );

        let egal = ensure_agent(
            &db,
            &provenance("vanyline", "1.1.0"),
            &agent_catalogue("G_triage", "trie affiné"),
        )
        .await
        .expect("le rejeu à la même version réussit");
        assert_eq!(egal, PublishOutcome::Unchanged);

        let inferieure = ensure_agent(
            &db,
            &provenance("vanyline", "1.0.5"),
            &agent_catalogue("G_triage", "trie différent"),
        )
        .await
        .expect("la publication d'une version inférieure réussit");
        assert_eq!(
            inferieure,
            PublishOutcome::Unchanged,
            "une version inférieure n'écrit rien"
        );
        assert_eq!(
            lire_provenance(&db, CATALOG_AGENTS_TABLE, "agent_id", "G_triage")
                .await
                .1
                .as_deref(),
            Some("1.1.0"),
            "aucune écriture : `managed_version` reste à 1.1.0"
        );
    }

    /// Scenario « une ligne non gérée ou d'une autre source n'est pas touchée » : `Created` jam
    /// — les deux identifiants rendent `Skipped(Unmanaged)` et les lignes restent intactes.
    #[tokio::test]
    async fn une_ligne_non_geree_ou_d_une_autre_source_n_est_pas_touchee() {
        let db = test_db().await;
        // Un agent créé par un administrateur (provenance nulle) : écriture directe, hors CRUD.
        crate::lymaille::definitions::agent::ActiveModel {
            id: ActiveValue::NotSet,
            agent_id: ActiveValue::Set("G_triage".to_owned()),
            kind: ActiveValue::Set("work".to_owned()),
            profile: ActiveValue::Set("qwen".to_owned()),
            definition: ActiveValue::Set(
                serde_json::to_value(agent_catalogue("G_triage", "vu de l'admin"))
                    .expect("la fixture se sérialise"),
            ),
            created_at: ActiveValue::Set(chrono::Utc::now()),
            managed_by: ActiveValue::NotSet,
            managed_version: ActiveValue::NotSet,
            managed_digest: ActiveValue::NotSet,
        }
        .insert(&db)
        .await
        .expect("l'agent de l'administrateur est posé, provenance nulle");
        let rendu = ensure_agent(
            &db,
            &provenance("vanyline", "1.0.0"),
            &agent_catalogue("G_triage", "trie"),
        )
        .await
        .expect("la publication d'un agent non géré réussit en saut");
        assert_eq!(rendu, PublishOutcome::Skipped(SkipReason::Unmanaged));
        assert!(
            lire_colonne(&db, CATALOG_AGENTS_TABLE, "definition", "agent_id", "G_triage")
                .await
                .is_some_and(|texte| texte.contains("vu de l'admin")),
            "la ligne non gérée est intacte"
        );

        // Un contexte publié par une autre source, puis par `vanyline`.
        ensure_catalog_context(
            &db,
            &provenance("autre", "1.0.0"),
            &contexte_catalogue("prod"),
            Some("G_home"),
        )
        .await
        .expect("la publication par `autre` crée la ligne");
        let rendu = ensure_catalog_context(
            &db,
            &provenance("vanyline", "1.0.0"),
            &contexte_catalogue("prod"),
            Some("G_home"),
        )
        .await
        .expect("la publication par `vanyline` saute la ligne d'une autre source");
        assert_eq!(rendu, PublishOutcome::Skipped(SkipReason::Unmanaged));
        assert_eq!(
            lire_provenance(&db, CATALOG_CONTEXTS_TABLE, "name", "G_home")
                .await
                .0
                .as_deref(),
            Some("autre"),
            "`managed_by` reste à la source d'origine"
        );
    }

    /// Détecteur permissif : laisse entrer un paramètre `sk-…` dans le contexte construit, que
    /// le hook refusera à la désérialisation par le détecteur par défaut (motif de
    /// `../definitions/catalog_context.rs`).
    struct DetecteurPermissif;

    impl SecretDetector for DetecteurPermissif {
        fn looks_like_secret(&self, _texte: &str) -> bool {
            false
        }
    }

    /// Scenario « les mêmes validations que les hooks, avant toute écriture » : un agent `triage`
    /// (nom privé), un contexte dont un paramètre ressemble à un secret (sans écho de la valeur),
    /// une version `pas-semver` — les trois refus avant toute écriture.
    #[tokio::test]
    async fn les_memes_validations_que_les_hooks_avant_toute_ecriture() {
        let db = test_db().await;

        let refuse = ensure_agent(
            &db,
            &provenance("vanyline", "1.0.0"),
            &agent_catalogue("triage", "trie"),
        )
        .await
        .expect_err("un identifiant privé n'est pas du catalogue");
        let texte = refuse.to_string();
        assert!(
            texte.contains("MRD-LYMAILLE-004"),
            "le refus est un `InvalidDefinition` : {texte}"
        );
        assert_eq!(compter(&db, CATALOG_AGENTS_TABLE).await, 0, "aucune ligne écrite");

        let secret = ToolsetContext::new("G_home".to_owned(), ContextLevel::Shared)
            .expect("le contexte de fixture se construit")
            .with_secret_detector(std::sync::Arc::new(DetecteurPermissif))
            .with_parameter("api_key".to_owned(), json!("sk-abcdef"))
            .expect("le détecteur permissif laisse entrer la valeur");
        let refuse = ensure_catalog_context(&db, &provenance("vanyline", "1.0.0"), &secret, None)
            .await
            .expect_err("le détecteur de secrets du hook refuse à la désérialisation");
        let texte = refuse.to_string();
        assert!(
            texte.contains("MRD-LYMAILLE-004") && texte.contains("secret"),
            "le refus nomme la règle : {texte}"
        );
        assert!(
            !texte.contains("sk-abcdef"),
            "la valeur ne traverse jamais l'erreur : {texte}"
        );
        assert_eq!(
            compter(&db, CATALOG_CONTEXTS_TABLE).await,
            0,
            "aucune ligne écrite"
        );

        let refuse = ensure_agent(
            &db,
            &provenance("vanyline", "pas-semver"),
            &agent_catalogue("G_triage", "trie"),
        )
        .await
        .expect_err("une provenance dont la version n'est pas semver est refusée");
        let texte = refuse.to_string();
        assert!(
            texte.contains("MRD-LYMAILLE-004"),
            "la version est validée avant toute écriture : {texte}"
        );
        assert_eq!(compter(&db, CATALOG_AGENTS_TABLE).await, 0, "aucune ligne écrite");
    }

    /// Scenario « deux publications simultanées créent une seule ligne » : l'insertion gardee par
    /// `ON CONFLICT DO NOTHING` rend un `Created` et un `Unchanged` (relecture après conflit).
    #[tokio::test]
    async fn deux_publications_simultanees_creent_une_seule_ligne() {
        let db = test_db().await;
        let prov = provenance("vanyline", "1.0.0");
        let definition = agent_catalogue("G_triage", "trie");
        let (gauche, droite) = tokio::join!(
            ensure_agent(&db, &prov, &definition),
            ensure_agent(&db, &prov, &definition)
        );
        let rendus = [
            gauche.expect("la première publication concurrente réussit"),
            droite.expect("la seconde publication concurrente réussit"),
        ];
        assert!(
            rendus.contains(&PublishOutcome::Created) && rendus.contains(&PublishOutcome::Unchanged),
            "un appel crée, l'autre relit après le conflit : {rendus:?}"
        );
        assert_eq!(
            compter(&db, CATALOG_AGENTS_TABLE).await,
            1,
            "une seule ligne existe"
        );
    }

    /// Scenario « l'empreinte est recalculée, pas crue » : `managed_digest` écrasé en base par
    /// l'empreinte d'un autre contenu — l'empreinte recalculée du contenu actuel ne correspond
    /// plus et la ligne est tenue pour modifiée, malgré une version supérieure.
    #[tokio::test]
    async fn lempreinte_est_recalculee_pas_crue() {
        let db = test_db().await;
        ensure_agent(
            &db,
            &provenance("vanyline", "1.0.0"),
            &agent_catalogue("G_triage", "trie"),
        )
        .await
        .expect("la publication initiale crée la ligne");
        db.execute_unprepared(&format!(
            "UPDATE {CATALOG_AGENTS_TABLE} SET managed_digest = '{}' WHERE agent_id = 'G_triage'",
            "f".repeat(64)
        ))
        .await
        .expect("l'empreinte attendue d'un autre contenu est écrasée en base");

        let rendu = ensure_agent(
            &db,
            &provenance("vanyline", "1.1.0"),
            &agent_catalogue("G_triage", "trie affiné"),
        )
        .await
        .expect("la publication avec version supérieure réussit en saut");
        assert_eq!(
            rendu,
            PublishOutcome::Skipped(SkipReason::Modified),
            "l'empreinte stockée n'a pas été crue : elle est recalculée sur le contenu actuel"
        );
    }
}
