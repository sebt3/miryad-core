//! `SeaAgentStore`, `SeaProfileStore`, `SeaProviderStore` — les traits de catalogue de
//! `lymaille` sur `SeaORM` (`src/lymaille/store/catalog.sdd`).
//!
//! Les agents suivent les règles de définitions de ./mod.sdd : cloisonnement par propriétaire,
//! catalogue `G_`, droit relu à l'appel. Profils et providers sont **globaux** (configuration
//! administrée, sans propriétaire) : leur écriture est réservée aux admins, leur lecture n'exige
//! aucun contrôle admin côté store (le moteur de `lymaille` lit les providers pour tourner — le
//! droit de lire la *liste* vit dans la politique `AdminOnly` de l'entité,
//! `../definitions/provider.sdd`). Les objets sont reconstruits depuis la colonne JSON, source
//! de vérité (DSD `DEC-17`).

use sea_orm::ConnectionTrait;
use sea_orm::DatabaseConnection;
use sea_orm::DbErr;
use sea_orm::sea_query::Alias;
use sea_orm::sea_query::Expr;
use sea_orm::sea_query::ExprTrait;
use sea_orm::sea_query::OnConflict;
use sea_orm::sea_query::Query;
use sea_orm::sea_query::SelectStatement;

use ::lymaille::AgentDefinition;
use ::lymaille::AgentStore;
use ::lymaille::AgentsError;
use ::lymaille::AgentsResult;
use ::lymaille::ModelProfile;
use ::lymaille::ProfileStore;
use ::lymaille::Provider;
use ::lymaille::ProviderStore;

use super::db_text;
use crate::lymaille::error::LymailleError;
use crate::lymaille::identity::CallerPrincipal;
use crate::lymaille::identity::IdKind;
use crate::lymaille::identity::Identity;
use crate::lymaille::identity::check_write;
use crate::lymaille::identity::classify;

/// Tables, noms complets (../migration.sdd).
const CATALOG_AGENTS_TABLE: &str = "miryad_lymaille_catalog_agents";
const PRIVATE_AGENTS_TABLE: &str = "miryad_lymaille_agents";
const PROFILES_TABLE: &str = "miryad_lymaille_profiles";
const PROVIDERS_TABLE: &str = "miryad_lymaille_providers";

/// Refus de l'hôte rendu à `lymaille` par la variante `Store` de son erreur, code `MRD-*`
/// compris (./mod.sdd règle 5).
fn deny(error: &LymailleError) -> AgentsError {
    AgentsError::Store(error.to_string())
}

/// Principal de l'appel, reconstruit à chaque opération (./mod.sdd règle 2, DSD `DEC-3`).
async fn principal(identity: &Identity, subject: &str) -> Result<CallerPrincipal, AgentsError> {
    identity.resolve(subject).await.map_err(|err| deny(&err))
}

/// Vérifie que l'appelant est admin à cet instant — la configuration de modèle et les providers
/// sont administrés (`./catalog.sdd`).
async fn require_admin(identity: &Identity, subject: &str, what: &str) -> AgentsResult<()> {
    let caller = principal(identity, subject).await?;
    if caller.is_admin {
        Ok(())
    } else {
        Err(deny(&LymailleError::Forbidden(format!("{what} requires admin"))))
    }
}

/// Première valeur JSON d'une requête : `None` si aucune ligne ou si la colonne est `NULL`.
async fn first_json(
    db: &DatabaseConnection,
    select: &SelectStatement,
    column: &str,
) -> Result<Option<serde_json::Value>, DbErr> {
    let row = db.query_one(select).await?;
    row.map(|row| row.try_get::<Option<serde_json::Value>>("", column))
        .transpose()
        .map(Option::flatten)
}

/// Upsert : `INSERT` sur `columns`/`values`, conflit sur `keys` → mise à jour de `updates`.
/// `created_at` n'est mis à jour par personne : posée à l'insertion, conservée à la mise à jour
/// (./mod.sdd règle 6, comme les hooks de `../definitions/`).
async fn upsert(
    db: &DatabaseConnection,
    table: &str,
    columns: &[&str],
    values: Vec<Expr>,
    keys: &[&str],
    updates: &[&str],
) -> Result<(), DbErr> {
    let statement = Query::insert()
        .into_table(Alias::new(table))
        .columns(columns.iter().copied().map(Alias::new))
        .values(values)
        .map_err(|err| DbErr::Custom(err.to_string()))?
        .on_conflict(
            OnConflict::columns(keys.iter().copied().map(Alias::new))
                .update_columns(updates.iter().copied().map(Alias::new))
                .to_owned(),
        )
        .to_owned();
    db.execute(&statement).await.map(|_| ())
}

/// Store des définitions d'agent — catalogue (`G_…`) et privé de chaque propriétaire
/// (`./catalog.sdd`).
#[derive(Clone)]
pub struct SeaAgentStore {
    db: DatabaseConnection,
    identity: Identity,
    subject: String,
}

/// Store des profils de modèle, table globale (`./catalog.sdd`).
#[derive(Clone)]
pub struct SeaProfileStore {
    db: DatabaseConnection,
    identity: Identity,
    subject: String,
}

/// Store des providers, table globale, références seules sans secret (`./catalog.sdd`).
#[derive(Clone)]
pub struct SeaProviderStore {
    db: DatabaseConnection,
    identity: Identity,
    subject: String,
}

impl SeaAgentStore {
    pub(crate) fn new(db: DatabaseConnection, identity: Identity, subject: String) -> Self {
        Self {
            db,
            identity,
            subject,
        }
    }
}

impl SeaProfileStore {
    pub(crate) fn new(db: DatabaseConnection, identity: Identity, subject: String) -> Self {
        Self {
            db,
            identity,
            subject,
        }
    }
}

impl SeaProviderStore {
    pub(crate) fn new(db: DatabaseConnection, identity: Identity, subject: String) -> Self {
        Self {
            db,
            identity,
            subject,
        }
    }
}

impl AgentStore for SeaAgentStore {
    async fn get_agent(&self, id: &str) -> AgentsResult<Option<AgentDefinition>> {
        // Règle 3 : la classification tranche — préfixe réservé ou identifiant invalide à la
        // lecture rendent `Ok(None)`, jamais une erreur, et sans requête.
        let Ok(kind) = classify(id) else {
            return Ok(None);
        };
        // DEC-27 (amendement à venir dans `./catalog.sdd`) : un identifiant `H_…` est résolu
        // par l'hôte, jamais stocké en table — lecture `Ok(None)` sans requête, avant même la
        // reconstruction du principal.
        if kind == IdKind::Host {
            return Ok(None);
        }
        let caller = principal(&self.identity, &self.subject).await?;
        let mut select = Query::select();
        select.column(Alias::new("definition"));
        match kind {
            IdKind::Catalog => {
                select
                    .from(Alias::new(CATALOG_AGENTS_TABLE))
                    .and_where(Expr::col(Alias::new("agent_id")).eq(id));
            }
            IdKind::Private => {
                select
                    .from(Alias::new(PRIVATE_AGENTS_TABLE))
                    .and_where(Expr::col(Alias::new("owner_id")).eq(caller.user_id))
                    .and_where(Expr::col(Alias::new("agent_id")).eq(id));
            }
            // Garde de couverture : `Host` rend déjà `Ok(None)` sans requête plus haut.
            IdKind::Host => return Ok(None),
        }
        let stored = first_json(&self.db, &select, "definition")
            .await
            .map_err(|err| AgentsError::Store(db_text(&err)))?;
        stored
            .map(|value| {
                serde_json::from_value::<AgentDefinition>(value).map_err(|_cause| {
                    // Le texte de la cause serde est abandonné : il peut citer un fragment de
                    // la définition (prompt compris). La lecture désigne la ligne, pas son corps.
                    AgentsError::Store(format!("stored agent definition {id} is unreadable"))
                })
            })
            .transpose()
    }

    async fn put_agent(&self, definition: &AgentDefinition) -> AgentsResult<()> {
        definition.validate()?;
        let caller = principal(&self.identity, &self.subject).await?;
        let kind = check_write(&caller, definition.id()).map_err(|err| deny(&err))?;
        let value = serde_json::to_value(definition)
            .map_err(|_cause| AgentsError::Store("agent definition could not be serialized".to_owned()))?;
        let nature = serde_json::to_value(definition.kind())
            .ok()
            .and_then(|value| value.as_str().map(str::to_owned))
            .ok_or_else(|| AgentsError::Store("agent kind is not a text".to_owned()))?;
        let now = Expr::val(chrono::Utc::now());
        let outcome = match kind {
            IdKind::Catalog => {
                upsert(
                    &self.db,
                    CATALOG_AGENTS_TABLE,
                    &["agent_id", "kind", "profile", "definition", "created_at"],
                    vec![
                        Expr::val(definition.id()),
                        Expr::val(nature),
                        Expr::val(definition.profile()),
                        Expr::val(value),
                        now,
                    ],
                    &["agent_id"],
                    &["kind", "profile", "definition"],
                )
                .await
            }
            IdKind::Private => {
                upsert(
                    &self.db,
                    PRIVATE_AGENTS_TABLE,
                    &[
                        "owner_id",
                        "agent_id",
                        "kind",
                        "profile",
                        "definition",
                        "created_at",
                    ],
                    vec![
                        Expr::val(caller.user_id),
                        Expr::val(definition.id()),
                        Expr::val(nature),
                        Expr::val(definition.profile()),
                        Expr::val(value),
                        now,
                    ],
                    &["owner_id", "agent_id"],
                    &["kind", "profile", "definition"],
                )
                .await
            }
            // DEC-27 (amendement à venir dans `./catalog.sdd`) : `check_write` refuse déjà
            // `H_…` en amont — aucun accès table ; branche de couverture au même refus.
            IdKind::Host => {
                return Err(deny(&LymailleError::Forbidden(
                    "host identifiers are not writable".to_string(),
                )));
            }
        };
        outcome.map_err(|err| AgentsError::Store(db_text(&err)))
    }
}

impl ProfileStore for SeaProfileStore {
    async fn get_profile(&self, name: &str) -> AgentsResult<Option<ModelProfile>> {
        let select = Query::select()
            .columns([Alias::new("profile")])
            .from(Alias::new(PROFILES_TABLE))
            .and_where(Expr::col(Alias::new("name")).eq(name))
            .to_owned();
        let stored = first_json(&self.db, &select, "profile")
            .await
            .map_err(|err| AgentsError::Store(db_text(&err)))?;
        stored
            .map(|value| {
                serde_json::from_value::<ModelProfile>(value).map_err(|_cause| {
                    AgentsError::Store(format!("stored model profile {name} is unreadable"))
                })
            })
            .transpose()
    }

    async fn put_profile(&self, profile: &ModelProfile) -> AgentsResult<()> {
        // La configuration de modèle est administrée : écriture réservée aux admins.
        require_admin(&self.identity, &self.subject, "model profile write").await?;
        let value = serde_json::to_value(profile)
            .map_err(|_cause| AgentsError::Store("model profile could not be serialized".to_owned()))?;
        upsert(
            &self.db,
            PROFILES_TABLE,
            &["name", "profile", "created_at"],
            vec![
                Expr::val(profile.name()),
                Expr::val(value),
                Expr::val(chrono::Utc::now()),
            ],
            &["name"],
            &["profile"],
        )
        .await
        .map_err(|err| AgentsError::Store(db_text(&err)))
    }
    // `resolve_profile` : l'implémentation par défaut du trait est conservée (`./catalog.sdd`).
}

impl ProviderStore for SeaProviderStore {
    async fn get_provider(&self, name: &str) -> AgentsResult<Option<Provider>> {
        // Lecture sans contrôle admin : le moteur de `lymaille` lit les providers pour tourner ;
        // le droit de lire la liste vit dans la politique `AdminOnly` de l'entité.
        let select = Query::select()
            .columns([Alias::new("provider")])
            .from(Alias::new(PROVIDERS_TABLE))
            .and_where(Expr::col(Alias::new("name")).eq(name))
            .to_owned();
        let stored = first_json(&self.db, &select, "provider")
            .await
            .map_err(|err| AgentsError::Store(db_text(&err)))?;
        stored
            .map(|value| {
                serde_json::from_value::<Provider>(value)
                    .map_err(|_cause| AgentsError::Store(format!("stored provider {name} is unreadable")))
            })
            .transpose()
    }

    async fn put_provider(&self, provider: &Provider) -> AgentsResult<()> {
        require_admin(&self.identity, &self.subject, "provider write").await?;
        // Un `Provider` ne porte que des références — aucune écriture de secret possible ici ;
        // sa `Deserialize` a appliqué `Provider::new` à la construction de l'objet.
        let value = serde_json::to_value(provider)
            .map_err(|_cause| AgentsError::Store("provider could not be serialized".to_owned()))?;
        let level = serde_json::to_value(provider.level())
            .ok()
            .and_then(|value| value.as_str().map(str::to_owned))
            .ok_or_else(|| AgentsError::Store("provider level is not a text".to_owned()))?;
        upsert(
            &self.db,
            PROVIDERS_TABLE,
            &["name", "kind", "level", "provider", "created_at"],
            vec![
                Expr::val(provider.name()),
                Expr::val(provider.kind()),
                Expr::val(level),
                Expr::val(value),
                Expr::val(chrono::Utc::now()),
            ],
            &["name"],
            &["kind", "level", "provider"],
        )
        .await
        .map_err(|err| AgentsError::Store(db_text(&err)))
    }
}

#[cfg(test)]
mod tests {
    use super::{SeaAgentStore, SeaProfileStore, SeaProviderStore};
    use crate::lymaille::identity::Identity;
    use crate::lymaille::store::test_support::test_db;
    use crate::users::membership::sync_group_memberships;
    use crate::users::resolve_user;
    use ::lymaille::AgentDefinition;
    use ::lymaille::AgentKind;
    use ::lymaille::AgentStore as _;
    use ::lymaille::AgentsError;
    use ::lymaille::DetectedModel;
    use ::lymaille::ModelProfile;
    use ::lymaille::ProfileStore as _;
    use ::lymaille::Provider;
    use ::lymaille::ProviderLevel;
    use ::lymaille::ProviderStore as _;
    use sea_orm::ConnectionTrait as _;
    use sea_orm::DatabaseConnection;
    use sea_orm::sea_query::Alias;
    use sea_orm::sea_query::Query;

    /// Compte local de test, admin ou non.
    async fn user(db: &DatabaseConnection, subject: &str, admin: bool) {
        let account = resolve_user(db, subject, None)
            .await
            .expect("compte local de test créé");
        if admin {
            sync_group_memberships(db, account.id, &["admin".to_string()])
                .await
                .expect("appartenance au groupe admin posée");
        }
    }

    fn agent(id: &str) -> AgentDefinition {
        AgentDefinition::new(
            id.to_string(),
            AgentKind::Work,
            "prompt de test".to_string(),
            "qwen".to_string(),
        )
    }

    /// Définition que `AgentDefinition::validate` refuse : un sous-agent délégable.
    fn invalid_agent() -> AgentDefinition {
        AgentDefinition::new(
            "reviewer".to_string(),
            AgentKind::Sub,
            "relire".to_string(),
            "qwen".to_string(),
        )
        .with_delegable(vec!["other".to_string()])
    }

    fn profile(name: &str) -> ModelProfile {
        ModelProfile::new(name.to_string(), "local".to_string(), "qwen3.8-flash".to_string())
    }

    fn provider() -> Provider {
        Provider::new(
            "local".to_string(),
            "openai".to_string(),
            "http://localhost:8000/v1".to_string(),
            Some("LLM_API_KEY".to_string()),
            ProviderLevel::User,
        )
        .expect("des références simples font un provider")
    }

    async fn row_count(db: &DatabaseConnection, table: &str) -> usize {
        let select = Query::select()
            .column(Alias::new("name"))
            .from(Alias::new(table))
            .to_owned();
        db.query_all(&select).await.expect("comptage des lignes").len()
    }

    /// `Scenario` « un agent privé reste privé, le catalogue est partagé ».
    #[tokio::test]
    async fn a_private_agent_stays_private_and_the_catalog_is_shared() {
        let db = test_db().await;
        user(&db, "alice", false).await;
        user(&db, "bob", false).await;
        user(&db, "root", true).await;

        SeaAgentStore::new(db.clone(), Identity::new(db.clone()), "alice".to_string())
            .put_agent(&agent("x"))
            .await
            .expect("alice écrit son agent privé `x`");
        SeaAgentStore::new(db.clone(), Identity::new(db.clone()), "root".to_string())
            .put_agent(&agent("G_y"))
            .await
            .expect("l'admin écrit `G_y`");

        let bob = SeaAgentStore::new(db.clone(), Identity::new(db.clone()), "bob".to_string());
        assert!(
            matches!(bob.get_agent("x").await, Ok(None)),
            "`x` est privé à alice : {:?}",
            bob.get_agent("x").await
        );
        assert_eq!(
            bob.get_agent("G_y").await.expect("lecture du catalogue"),
            Some(agent("G_y")),
            "`G_y` est partagé"
        );
    }

    /// `Scenario` « une définition invalide est refusée avant l'écriture » :
    /// `AgentsError::InvalidDefinition` et aucune ligne écrite, ni au catalogue ni en privé.
    #[tokio::test]
    async fn an_invalid_definition_is_refused_before_the_write() {
        let db = test_db().await;
        user(&db, "alice", false).await;
        let store = SeaAgentStore::new(db.clone(), Identity::new(db.clone()), "alice".to_string());

        let refused = store
            .put_agent(&invalid_agent())
            .await
            .expect_err("la définition refusée par `validate` est rejetée");
        assert!(
            matches!(refused, AgentsError::InvalidDefinition { .. }),
            "la variante attendue est `InvalidDefinition` : {refused:?}"
        );
        for table in ["miryad_lymaille_catalog_agents", "miryad_lymaille_agents"] {
            let select = Query::select()
                .column(Alias::new("agent_id"))
                .from(Alias::new(table))
                .to_owned();
            let rows = db.query_all(&select).await.expect("comptage des agents");
            assert!(rows.is_empty(), "aucune ligne écrite dans {table}");
        }
    }

    /// `Scenario` « un non-admin n'écrit pas un profil ni un provider » : deux erreurs `Store`
    /// citant `MRD-LYMAILLE-002`, aucune ligne écrite.
    #[tokio::test]
    async fn a_non_admin_writes_neither_a_profile_nor_a_provider() {
        let db = test_db().await;
        user(&db, "bob", false).await;
        let identity = Identity::new(db.clone());

        let refused_profile = SeaProfileStore::new(db.clone(), identity.clone(), "bob".to_string())
            .put_profile(&profile("qwen"))
            .await
            .expect_err("un non-admin n'écrit pas de profil");
        assert!(
            refused_profile.to_string().contains("MRD-LYMAILLE-002"),
            "le refus du profil cite le code : {refused_profile}"
        );
        let refused_provider = SeaProviderStore::new(db.clone(), identity, "bob".to_string())
            .put_provider(&provider())
            .await
            .expect_err("un non-admin n'écrit pas de provider");
        assert!(
            refused_provider.to_string().contains("MRD-LYMAILLE-002"),
            "le refus du provider cite le code : {refused_provider}"
        );
        assert_eq!(row_count(&db, "miryad_lymaille_profiles").await, 0);
        assert_eq!(row_count(&db, "miryad_lymaille_providers").await, 0);
    }

    /// `Scenario` « profil et provider font l'aller-retour » — égalité des objets relus, et
    /// `resolve_profile` (défaut du trait) applique l'écrasement du profil sur la détection.
    #[tokio::test]
    async fn profile_and_provider_round_trip() {
        let db = test_db().await;
        user(&db, "root", true).await;
        let identity = Identity::new(db.clone());
        let stored_profile = profile("qwen").with_context_size(262_144);

        SeaProfileStore::new(db.clone(), identity.clone(), "root".to_string())
            .put_profile(&stored_profile)
            .await
            .expect("l'admin écrit le profil");
        SeaProviderStore::new(db.clone(), identity.clone(), "root".to_string())
            .put_provider(&provider())
            .await
            .expect("l'admin écrit le provider");

        assert_eq!(
            SeaProfileStore::new(db.clone(), identity.clone(), "root".to_string())
                .get_profile("qwen")
                .await
                .expect("lecture du profil"),
            Some(stored_profile),
            "le profil relu égale l'original"
        );
        assert_eq!(
            SeaProviderStore::new(db.clone(), identity.clone(), "n'importe-qui".to_string())
                .get_provider("local")
                .await
                .expect("la lecture d'un provider n'exige pas l'admin"),
            Some(provider()),
            "le provider relu égale l'original"
        );

        let resolved = SeaProfileStore::new(db.clone(), identity, "root".to_string())
            .resolve_profile("qwen", Some(DetectedModel::new(Some(8192), None, None)))
            .await
            .expect("le profil stocké se résout");
        assert_eq!(
            resolved.context_size(),
            262_144,
            "l'écrasement du profil l'emporte sur la détection"
        );
    }

    /// `Scenario` « un nom absent est None » : les trois lectures sans aucune définition en
    /// base. Le `subject` du store a un compte — la règle 2 de ./mod.sdd résout le principal à
    /// chaque opération des stores de définitions, ce qui n'est pas une définition.
    #[tokio::test]
    async fn an_absent_name_is_none() {
        let db = test_db().await;
        user(&db, "alice", false).await;
        let identity = Identity::new(db.clone());
        assert!(
            matches!(
                SeaProfileStore::new(db.clone(), identity.clone(), "alice".to_string())
                    .get_profile("zzz")
                    .await,
                Ok(None)
            ),
            "profil absent → None"
        );
        assert!(
            matches!(
                SeaProviderStore::new(db.clone(), identity.clone(), "alice".to_string())
                    .get_provider("zzz")
                    .await,
                Ok(None)
            ),
            "provider absent → None"
        );
        assert!(
            matches!(
                SeaAgentStore::new(db.clone(), identity, "alice".to_string())
                    .get_agent("zzz")
                    .await,
                Ok(None)
            ),
            "agent absent → None"
        );
    }
}
