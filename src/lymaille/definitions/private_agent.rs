//! Entité `lymaille_agents` (feature `lymaille`) — contrat porté par `./private_agent.sdd`.
//!
//! Table `miryad_lymaille_agents` : les définitions d'agent **privées** d'un utilisateur (seconde
//! ressource de la paire `DEC-19`), qu'il crée, lit, modifie et supprime par REST et MCP hors
//! session de configuration. `OwnerOnly` : le moteur d'accès pose `owner_id` et cloisonne les
//! lignes ; l'identifiant est un identifiant `Private` au sens de `crate::lymaille::identity`
//! (jamais `G_`/`g_`, DSD `DEC-9`). Mêmes colonnes dérivées que `./agent.rs` : la colonne JSON
//! `definition` (une `lymaille::AgentDefinition` complète) est la source de vérité (DSD
//! `DEC-17`), `agent_id`, `kind` et `profile` en sont des copies posées par les hooks.
//!
//! **Contrôle à la sauvegarde** (DSD `FR-9`, amendement du 2026-11-11, DSD `hooks-async`
//! `DEC-7`) : le hook asynchrone reçoit la connexion de l'appel — `check_grants` est fait **ici**,
//! l'état admin reconstruit par `identity::caller_of` à chaque sauvegarde (jamais mis en cache),
//! puis par `put_agent` (chemin des stores et de la session de configuration,
//! `../store/catalog.sdd`) et, en défense en profondeur, à l'appel d'un module (`../identity.sdd`).
//! Le contrôle est tranché sous l'**appelant** du hook : un admin qui écrit la ligne d'un autre
//! utilisateur n'est pas bridé à la sauvegarde, l'appel relit alors le propriétaire.

use sea_orm::ActiveValue;
use sea_orm::entity::prelude::*;
use serde::Deserialize;
use serde::Serialize;

use crate::auth::AuthPrincipal;
use crate::lymaille::identity;
use crate::resource::AccessPolicy;
use crate::resource::HookError;
use crate::resource::MiryadResource;

/// Définition d'agent privée persistée — la table `miryad_lymaille_agents` posée par
/// `../migration.rs`, colonnes du contrat `Must` de `./private_agent.sdd`.
#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "miryad_lymaille_agents")]
pub struct Model {
    /// Clé primaire entière auto-incrémentée (convention `miryad_*` de la crate).
    #[sea_orm(primary_key)]
    pub id: i32,
    /// Propriétaire de la ligne (`miryad_users.id`), posé par l'injection du propriétaire du
    /// moteur d'accès — jamais par le hook. Unicité `(owner_id, agent_id)` par la migration.
    pub owner_id: i32,
    /// Identifiant de l'agent (privé, jamais `G_`/`g_`), copie dérivée de `definition.id` par les
    /// hooks — libellé de la ressource, unique par propriétaire (contrainte de la migration).
    pub agent_id: String,
    /// Nature de l'agent (`work`, `config` ou `sub`), copie dérivée de `definition` par les
    /// hooks — colonne de filtre de la ressource.
    pub kind: String,
    /// Nom du profil de modèle désigné, copie dérivée de `definition` par les hooks.
    pub profile: String,
    /// L'`lymaille::AgentDefinition` complète, colonne JSON : la source de vérité (DSD
    /// `DEC-17`), validée par `before_create` et `before_update`.
    pub definition: serde_json::Value,
    /// Horodatage de création, posé par le hook de création (aucune entité de la crate ne porte
    /// d'`updated_at`).
    pub created_at: DateTimeUtc,
}

/// Aucune relation : la référence au profil est un nom, résolu à l'exécution, sans clé étrangère
/// (couplage lâche, comme `owner_id` des lignes privées).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

impl Entity {
    /// Étape 4 du `Must` de `./private_agent.sdd` : l'identifiant doit être `Private` au sens de
    /// `identity::classify` (DSD `DEC-9`). Distinction par préfixe alignée sur
    /// `./user_context.rs` (arbitrage du 2026-11-06) : `G_`/`g_` → « agent id cannot use the
    /// catalog prefix » ; `H_`/`h_` (dont la casse réservée `h_` rend `ReservedPrefix` au sens de
    /// `classify`, DSD `DEC-27`) → « agent id cannot use the host prefix » ; identifiant invalide
    /// au sens Rhai → « invalid agent id ».
    fn verifier_identifiant(id: &str) -> Result<(), HookError> {
        match identity::classify(id) {
            Ok(identity::IdKind::Private) => Ok(()),
            Ok(identity::IdKind::Catalog) => Err(HookError::new("agent id cannot use the catalog prefix")),
            Ok(identity::IdKind::Host) => Err(HookError::new("agent id cannot use the host prefix")),
            Err(crate::lymaille::error::LymailleError::ReservedPrefix(identifiant)) => {
                if identifiant
                    .get(..2)
                    .is_some_and(|prefix| prefix.eq_ignore_ascii_case("g_"))
                {
                    Err(HookError::new("agent id cannot use the catalog prefix"))
                } else {
                    Err(HookError::new("agent id cannot use the host prefix"))
                }
            }
            Err(_) => Err(HookError::new("invalid agent id")),
        }
    }

    /// Texte fixe du refus fermé : panne de lecture (`DbErr`) ou `subject` sans compte lors du
    /// contrôle d'autorité à la sauvegarde (`Must`, amendement du 2026-11-11, DSD `hooks-async`
    /// `DEC-7`) — aucun détail de l'erreur ne traverse.
    const PANNE_AUTORISATION: &'static str = "authorization check unavailable";

    /// Porte commune de `before_create` (`existing` absent) et `before_update` : les étapes 1 à 5
    /// de ./agent.rs dans le même ordre, interrompues au premier échec — `definition` objet JSON
    /// présent, désérialisable en `lymaille::AgentDefinition` et validée par sa `validate` (la
    /// cause traverse, jamais le prompt), puis ici `classify = Private`
    /// ([`Self::verifier_identifiant`], à la place de l'étape `G_` du catalogue) ; sur une mise à jour
    /// l'identifiant ne change pas et `created_at` reste celui de la ligne existante. Les colonnes
    /// dérivées sont toujours réécrites ; `owner_id` n'est jamais touché (injection du moteur
    /// d'accès). **Après** cette validation (`Must not` de `./private_agent.sdd`), le contrôle des
    /// droits d'accord de modules (`FR-9`, amendement du 2026-11-11, DSD `hooks-async` `DEC-7`) :
    /// le principal de l'appelant est reconstruit par [`identity::caller_of`] sur la **connexion
    /// du hook** — état admin relu, jamais mis en cache —, puis [`identity::check_grants`] tranche
    /// les modules de la définition contre la liste `reserved` injectée (la fonction publique
    /// passe `identity::ADMIN_ONLY_MODULES`, le test une liste non vide — précédent
    /// `activate_with`). Un module réservé refusé rend un `HookError` sans code nommant le module,
    /// jamais le prompt ; une panne de lecture ou un `subject` inconnu rendent le texte fixe
    /// [`Self::PANNE_AUTORISATION`] (échec fermé).
    async fn sauvegarde_avec<C: sea_orm::ConnectionTrait>(
        db: &C,
        active: ActiveModel,
        existing: Option<&Model>,
        principal: &AuthPrincipal,
        reserved: &[&str],
    ) -> Result<ActiveModel, HookError> {
        let (active, definition) = Self::valider_et_deriver(active, existing)?;
        let caller = identity::caller_of(db, &principal.subject)
            .await
            .map_err(|_| HookError::new(Self::PANNE_AUTORISATION))?;
        identity::check_grants(&caller, definition.modules(), reserved).map_err(|cause| match cause {
            // Le message interne de `Forbidden` nomme le module ; son affichage porterait un
            // code `MRD-*` (`Must not`), seule la charge utile traverse.
            crate::lymaille::error::LymailleError::Forbidden(message) => HookError::new(message),
            _ => HookError::new(Self::PANNE_AUTORISATION),
        })?;
        Ok(active)
    }

    /// Validation des étapes de ./agent.rs, puis dérivation des colonnes : rend l'`ActiveModel`
    /// dérivé **et** la définition désérialisée, dont [`Self::sauvegarde_avec`] lit les modules
    /// accordés pour le contrôle `FR-9`.
    fn valider_et_deriver(
        mut active: ActiveModel,
        existing: Option<&Model>,
    ) -> Result<(ActiveModel, ::lymaille::AgentDefinition), HookError> {
        let brute = match &active.definition {
            ActiveValue::Set(valeur) | ActiveValue::Unchanged(valeur) if valeur.is_object() => Some(valeur),
            ActiveValue::Set(_) | ActiveValue::Unchanged(_) | ActiveValue::NotSet => None,
        };
        let Some(brute) = brute else {
            return Err(HookError::new("agent definition is required"));
        };
        let definition: ::lymaille::AgentDefinition = serde_json::from_value(brute.clone())
            .map_err(|cause| HookError::new(format!("invalid agent definition: {cause}")))?;
        definition
            .validate()
            .map_err(|cause| HookError::new(format!("invalid agent definition: {cause}")))?;
        Self::verifier_identifiant(definition.id())?;
        if let Some(ligne) = existing
            && definition.id() != ligne.agent_id.as_str()
        {
            return Err(HookError::new("agent id cannot change"));
        }
        let Ok(serde_json::Value::String(nature)) = serde_json::to_value(definition.kind()) else {
            return Err(HookError::new("invalid agent definition"));
        };
        active.agent_id = ActiveValue::Set(definition.id().to_owned());
        active.kind = ActiveValue::Set(nature);
        active.profile = ActiveValue::Set(definition.profile().to_owned());
        match existing {
            Some(ligne) => active.created_at = ActiveValue::Set(ligne.created_at),
            None if matches!(active.created_at, ActiveValue::NotSet) => {
                active.created_at = ActiveValue::Set(chrono::Utc::now());
            }
            None => {}
        }
        Ok((active, definition))
    }
}

/// Métadonnées et hooks de la ressource `lymaille_agents`, lue à l'identique par REST et MCP.
/// Privé (DSD `DEC-19`) : lecture et écriture par le propriétaire seul, l'admin voyant tout par la
/// règle du moteur d'accès.
impl MiryadResource for Entity {
    fn resource_name() -> &'static str {
        "lymaille_agents"
    }

    fn read_policy() -> AccessPolicy {
        AccessPolicy::OwnerOnly
    }

    fn write_policy() -> AccessPolicy {
        AccessPolicy::OwnerOnly
    }

    fn owner_column() -> Option<<Self as EntityTrait>::Column> {
        Some(Column::OwnerId)
    }

    fn filter_column() -> Option<<Self as EntityTrait>::Column> {
        Some(Column::Kind)
    }

    fn label_column() -> Option<<Self as EntityTrait>::Column> {
        Some(Column::AgentId)
    }

    /// Valide la définition, contrôle les modules accordés et réécrit les colonnes dérivées —
    /// voir `Entity::sauvegarde_avec`.
    fn before_create<C>(
        db: &C,
        active: Self::ActiveModel,
        principal: &AuthPrincipal,
    ) -> impl std::future::Future<Output = Result<Self::ActiveModel, HookError>> + Send
    where
        C: sea_orm::ConnectionTrait,
    {
        Entity::sauvegarde_avec(db, active, None, principal, identity::ADMIN_ONLY_MODULES)
    }

    /// Mêmes étapes que [`Self::before_create`] sur la nouvelle définition, plus l'identité :
    /// renommer un agent change son identité et est refusé ; `created_at` reste celui de la ligne
    /// existante.
    fn before_update<C>(
        db: &C,
        active: Self::ActiveModel,
        existing: &Self::Model,
        principal: &AuthPrincipal,
    ) -> impl std::future::Future<Output = Result<Self::ActiveModel, HookError>> + Send
    where
        C: sea_orm::ConnectionTrait,
    {
        Entity::sauvegarde_avec(
            db,
            active,
            Some(existing),
            principal,
            identity::ADMIN_ONLY_MODULES,
        )
    }

    // `before_delete` volontairement non surdéclaré : aucun contrôle d'usage, le défaut identité
    // du trait s'applique (./private_agent.sdd).
}

#[cfg(test)]
mod tests {
    use super::ActiveModel;

    /// Connexion simulée pour appeler les hooks depuis les tests : la validation de ce module
    /// est pure et n'émet aucune requête ; une connexion à file vide suffit.
    fn hook_db() -> sea_orm::DatabaseConnection {
        sea_orm::MockDatabase::new(sea_orm::DbBackend::Sqlite).into_connection()
    }
    use super::Column;
    use super::Entity;
    use super::Model;
    use crate::auth::AuthPrincipal;
    use crate::auth::PrincipalSource;
    use crate::migration::Migrator;
    use crate::resource::AccessPolicy;
    use crate::resource::MiryadResource;
    use crate::users::resolve_user;
    use ::lymaille::AgentDefinition;
    use ::lymaille::AgentKind;
    use chrono::TimeZone as _;
    use chrono::Utc;
    use sea_orm::ActiveValue::NotSet;
    use sea_orm::ActiveValue::Set;
    use sea_orm::ColumnTrait as _;
    use sea_orm::EntityTrait as _;
    use sea_orm::QueryFilter as _;
    use sea_orm_migration::MigratorTrait;
    use serde_json::json;

    // ── Fixtures — purement mémoire pour les hooks purs : aucune connexion (position contractuelle
    // après `rest::core::mark_all_set`, avant toute écriture). Les définitions sont sérialisées
    // depuis un `lymaille::AgentDefinition` construit par la bibliothèque elle-même, comme dans
    // `./agent.rs`, pour que le test prouve l'aller-retour de sa forme réelle.

    fn principal() -> AuthPrincipal {
        AuthPrincipal {
            subject: "sujet-de-test".to_string(),
            email: None,
            preferred_username: None,
            source: PrincipalSource::ApiToken { token_id: 1 },
        }
    }

    fn principal_sujet(subject: &str) -> AuthPrincipal {
        AuthPrincipal {
            subject: subject.to_string(),
            ..principal()
        }
    }

    /// `AgentDefinition` sérialisée — la colonne `definition` est construite par la bibliothèque
    /// elle-même, jamais à la main.
    fn definition(id: &str, kind: AgentKind, prompt: &str, profile: &str) -> serde_json::Value {
        serde_json::to_value(AgentDefinition::new(
            id.to_string(),
            kind,
            prompt.to_string(),
            profile.to_string(),
        ))
        .expect("la fixture se sérialise")
    }

    /// `ActiveModel` dans la forme qui atteint le hook : colonnes dérivées fausses (le hook doit
    /// les réécrire), `definition` posée, clé, propriétaire et horodatage non posés. `owner_id`
    /// n'est jamais posé par le hook : c'est l'injection du propriétaire du moteur d'accès.
    fn active_avec(definition: serde_json::Value) -> ActiveModel {
        ActiveModel {
            id: NotSet,
            owner_id: NotSet,
            agent_id: Set("faux-agent".to_string()),
            kind: Set("faux-kind".to_string()),
            profile: Set("faux-profil".to_string()),
            definition: Set(definition),
            created_at: NotSet,
        }
    }

    /// Base migrée **sans aucun compte** : le hook qui consulte la connexion bute sur un sujet
    /// inconnu — cas « `subject` sans compte » du Scenario panne.
    async fn db_migree_sans_compte() -> sea_orm::DatabaseConnection {
        let db = sea_orm::Database::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite connects");
        Migrator::up(&db, None).await.expect("migrations apply cleanly");
        db
    }

    /// Base migrée portant un compte local pour `subject` : les hooks qui reconstruisent le
    /// principal par `caller_of` (amendement du 2026-11-11, DSD `hooks-async` `DEC-7`) exigent
    /// la ligne réelle — motif des tests de `../identity.rs`.
    async fn db_avec_compte(subject: &str) -> sea_orm::DatabaseConnection {
        let db = db_migree_sans_compte().await;
        resolve_user(&db, subject, None)
            .await
            .expect("compte local de test");
        db
    }

    /// Compte local mis dans le groupe `admin` (par la réconciliation OIDC, seul chemin de
    /// production des appartenances — motif de `../identity.rs`).
    async fn compte_admin(db: &sea_orm::DatabaseConnection, subject: &str) {
        let compte = resolve_user(db, subject, None)
            .await
            .expect("compte local de test");
        crate::users::membership::sync_group_memberships(db, compte.id, &["admin".to_string()])
            .await
            .expect("compte membre du groupe admin seedé");
    }

    /// Retrait de toute appartenance de l'utilisateur (fixture de test — le Scenario « le droit
    /// admin est relu » est muet sur le chemin de retrait, `../identity.rs` fait de même).
    async fn retirer_appartenances(db: &sea_orm::DatabaseConnection, user_id: i32) {
        crate::users::membership::Entity::delete_many()
            .filter(crate::users::membership::Column::UserId.eq(user_id))
            .exec(db)
            .await
            .expect("retrait des appartenances de fixture");
    }

    /// Définition valide accordant les modules `modules` — la colonne `definition` construite par
    /// la bibliothèque elle-même, comme la fixture [`definition`].
    fn definition_avec_modules(id: &str, prompt: &str, profile: &str, modules: &[&str]) -> serde_json::Value {
        serde_json::to_value(
            AgentDefinition::new(
                id.to_string(),
                AgentKind::Sub,
                prompt.to_string(),
                profile.to_string(),
            )
            .with_modules(modules.iter().map(|module| (*module).to_string()).collect()),
        )
        .expect("la fixture se sérialise")
    }

    /// Verrou d'un refus de module réservé : `HookError` sans code, nommant le module, ne citant
    /// jamais le prompt.
    fn assert_refus_module_reserve(erreur: &crate::resource::HookError, prompt: &str) {
        assert!(
            erreur.code.is_none(),
            "aucun code `MRD-*` sur un `HookError` : {erreur:?}"
        );
        assert!(
            erreur.message.contains("secrets"),
            "le refus nomme le module réservé : {erreur:?}"
        );
        assert!(
            !erreur.message.contains(prompt),
            "le prompt ne traverse jamais un `HookError` : {erreur:?}"
        );
    }

    /// Connexion simulée dont **toute lecture** échoue d'un `DbErr` porteur d'une marque — le
    /// test verrouille que cette marque ne traverse jamais le `HookError` (motif des pannes de
    /// `src/users/group.rs`).
    fn db_lecture_en_echec() -> sea_orm::DatabaseConnection {
        sea_orm::MockDatabase::new(sea_orm::DbBackend::Sqlite)
            .append_query_errors([sea_orm::DbErr::Query(sea_orm::RuntimeErr::Internal(
                "PANNE-DE-LECTURE-marquee-4e1a".to_string(),
            ))])
            .into_connection()
    }

    /// Ligne existante des hooks `before_update`.
    fn modele(owner_id: i32, agent_id: &str) -> Model {
        Model {
            id: 1,
            owner_id,
            agent_id: agent_id.to_string(),
            kind: "sub".to_string(),
            profile: "qwen".to_string(),
            definition: definition(agent_id, AgentKind::Sub, "prompt d'origine", "qwen"),
            created_at: Utc.with_ymd_and_hms(2026, 11, 1, 12, 0, 0).unwrap(),
        }
    }

    /// Métadonnées `Must` : `OwnerOnly` en lecture comme en écriture, colonne propriétaire
    /// `owner_id`, filtre sur `kind`, libellé `agent_id`.
    #[test]
    fn politiques_de_la_ressource_privee() {
        assert_eq!(Entity::resource_name(), "lymaille_agents");
        assert_eq!(Entity::read_policy(), AccessPolicy::OwnerOnly);
        assert_eq!(Entity::write_policy(), AccessPolicy::OwnerOnly);
        assert!(matches!(Entity::owner_column(), Some(Column::OwnerId)));
        assert!(matches!(Entity::filter_column(), Some(Column::Kind)));
        assert!(matches!(Entity::label_column(), Some(Column::AgentId)));
    }

    /// Scenario « un agent privé valide est accepté » : la définition `triage` (id privé) passe,
    /// les colonnes fausses de l'entrée sont réécrites depuis la définition, et `created_at` est
    /// posé quand il manque.
    #[tokio::test]
    async fn before_create_accepte_un_agent_prive_valide_et_derive_les_colonnes() {
        let db = db_avec_compte("sujet-de-test").await;
        let rendu = Entity::before_create(
            &db,
            active_avec(definition("triage", AgentKind::Sub, "trie", "qwen")),
            &principal(),
        )
        .await
        .expect("une définition `triage` à l'identifiant privé est acceptée");
        assert_eq!(
            rendu.agent_id,
            Set("triage".to_string()),
            "`agent_id` est la copie dérivée de `definition.id`"
        );
        assert_eq!(
            rendu.kind,
            Set("sub".to_string()),
            "`kind` est la copie dérivée de la nature de la définition"
        );
        assert_eq!(
            rendu.profile,
            Set("qwen".to_string()),
            "`profile` est la copie dérivée du profil désigné"
        );
        assert!(
            matches!(rendu.owner_id, NotSet),
            "`owner_id` n'est jamais touché par le hook : {:?}",
            rendu.owner_id
        );
        assert!(
            matches!(rendu.created_at, Set(_)),
            "`created_at` est posé par le hook de création quand il est absent"
        );
    }

    /// Scenario « le préfixe catalogue est refusé » : `G_triage` (`Catalog`) puis `g_triage`
    /// (`ReservedPrefix`) sont refusés par le même message, sans code `MRD-*` (`Must` étape 4 :
    /// `classify` doit rendre `Private`).
    #[tokio::test]
    async fn before_create_refuse_le_prefixe_catalogue() {
        for id in ["G_triage", "g_triage"] {
            let rendu = Entity::before_create(
                &hook_db(),
                active_avec(definition(id, AgentKind::Sub, "trie", "qwen")),
                &principal(),
            )
            .await;
            match rendu {
                Err(erreur) => {
                    assert!(
                        erreur.code.is_none(),
                        "un `HookError` de la crate ne porte aucun code : {erreur:?}"
                    );
                    assert_eq!(
                        erreur.message, "agent id cannot use the catalog prefix",
                        "l'identifiant {id} n'est pas privé"
                    );
                }
                Ok(_) => panic!("la définition d'id {id} devait être refusée"),
            }
        }
    }

    /// Scenario « le préfixe hôte est refusé » (moitié majuscule, arbitré le 2026-11-06) :
    /// `H_triage` (`classify` = `Host`, DSD `DEC-27`) est refusé sous le message contractualisé
    /// « agent id cannot use the host prefix », sans code `MRD-*` (`Must` étape 4).
    #[tokio::test]
    async fn before_create_refuse_un_identifiant_hote() {
        let rendu = Entity::before_create(
            &hook_db(),
            active_avec(definition("H_triage", AgentKind::Sub, "trie", "qwen")),
            &principal(),
        )
        .await;
        match rendu {
            Err(erreur) => {
                assert!(
                    erreur.code.is_none(),
                    "aucun code `MRD-*` sur un `HookError` : {erreur:?}"
                );
                assert_eq!(
                    erreur.message, "agent id cannot use the host prefix",
                    "`H_triage` porte le préfixe hôte"
                );
            }
            Ok(_) => panic!("`H_triage` est un identifiant résolu par l'hôte, jamais écrit ici"),
        }
    }

    /// Scenario « le préfixe hôte est refusé » (moitié minuscule, alignement arbitré le
    /// 2026-11-06 sur `./user_context.sdd`) : `h_triage` — `ReservedPrefix` au sens de `classify`,
    /// préfixe `h_` — est refusé sous le **même** message que `H_triage`, « agent id cannot use
    /// the host prefix », et non sous « catalog prefix ».
    #[tokio::test]
    async fn before_create_refuse_le_prefixe_hote_minuscule() {
        let rendu = Entity::before_create(
            &hook_db(),
            active_avec(definition("h_triage", AgentKind::Sub, "trie", "qwen")),
            &principal(),
        )
        .await;
        match rendu {
            Err(erreur) => {
                assert!(
                    erreur.code.is_none(),
                    "aucun code `MRD-*` sur un `HookError` : {erreur:?}"
                );
                assert_eq!(
                    erreur.message, "agent id cannot use the host prefix",
                    "`h_triage` porte le préfixe hôte sous casse réservée, pas le préfixe \
                     catalogue"
                );
            }
            Ok(_) => panic!("`h_triage` porte le préfixe hôte réservé, jamais écrit ici"),
        }
    }

    /// `Must` (étape 1 des étapes de ./agent.sdd) : `definition` absente ou non objet est refusée
    /// avant toute désérialisation.
    #[tokio::test]
    async fn before_create_refuse_une_definition_absente_ou_non_objet() {
        let sans = ActiveModel {
            definition: NotSet,
            ..active_avec(serde_json::Value::Null)
        };
        match Entity::before_create(&hook_db(), sans, &principal()).await {
            Err(erreur) => assert_eq!(erreur.message, "agent definition is required"),
            Ok(_) => panic!("une `definition` `NotSet` doit être refusée"),
        }
        match Entity::before_create(
            &hook_db(),
            active_avec(json!(["pas", "un", "objet"])),
            &principal(),
        )
        .await
        {
            Err(erreur) => assert_eq!(erreur.message, "agent definition is required"),
            Ok(_) => panic!("une `definition` non objet doit être refusée"),
        }
    }

    /// `Must` (étape 3 des étapes de ./agent.sdd) / `Must not` : une définition que
    /// `AgentDefinition::validate` refuse est rejetée avec la cause de lymaille, et le prompt ne
    /// traverse jamais le message.
    #[tokio::test]
    async fn before_create_refuse_une_definition_invalide_sans_citer_le_prompt() {
        let prompt = "prompt-a-ne-jamais-citer-9f3c";
        let refusee = AgentDefinition::new(
            "triage".to_string(),
            AgentKind::Sub,
            prompt.to_string(),
            "qwen".to_string(),
        )
        .with_delegable(vec!["sous".to_string()]);
        // `validate` refuse : un sous-agent ne peut déclarer de sous-agent délégable.
        assert!(
            refusee.validate().is_err(),
            "précondition : la fixture est bien refusée par `AgentDefinition::validate`"
        );
        let valeur = serde_json::to_value(refusee).expect("la fixture se sérialise");
        match Entity::before_create(&hook_db(), active_avec(valeur), &principal()).await {
            Err(erreur) => {
                assert!(
                    erreur.message.starts_with("invalid agent definition:"),
                    "la cause de lymaille est citée : {erreur:?}"
                );
                assert!(
                    !erreur.message.contains(prompt),
                    "le prompt ne traverse jamais un `HookError` : {erreur:?}"
                );
            }
            Ok(_) => panic!("la définition refusée par `validate` devait être rejetée"),
        }
    }

    /// Scenario « renommer est refusé » : la nouvelle définition d'id `y` ne doit pas être
    /// acceptée sur la ligne `x`.
    #[tokio::test]
    async fn before_update_refuse_un_renommage() {
        let rendu = Entity::before_update(
            &hook_db(),
            active_avec(definition("y", AgentKind::Sub, "trie", "qwen")),
            &modele(1, "x"),
            &principal(),
        )
        .await;
        match rendu {
            Err(erreur) => {
                assert!(erreur.code.is_none(), "aucun code `MRD-*` sur un `HookError`");
                assert_eq!(erreur.message, "agent id cannot change");
            }
            Ok(_) => panic!("renommer un agent change son identité : la mise à jour est refusée"),
        }
    }

    /// `Must` (`before_update`, étapes de ./agent.sdd) : une mise à jour qui garde l'id derive
    /// les colonnes et laisse `created_at` à celui de la ligne existante.
    #[tokio::test]
    async fn before_update_derive_les_colonnes_et_conserve_la_date_de_creation() {
        let db = db_avec_compte("sujet-de-test").await;
        let existing = modele(1, "x");
        let rendu = Entity::before_update(
            &db,
            active_avec(definition("x", AgentKind::Sub, "trie affiné", "qwen")),
            &existing,
            &principal(),
        )
        .await
        .expect("une mise à jour qui garde l'id est acceptée");
        assert_eq!(rendu.agent_id, Set("x".to_string()));
        assert_eq!(rendu.kind, Set("sub".to_string()));
        assert_eq!(rendu.profile, Set("qwen".to_string()));
        assert_eq!(
            rendu.created_at,
            Set(existing.created_at),
            "`created_at` reste celui de la ligne existante"
        );
    }

    /// Scenario « un accord de module réservé à l'admin est refusé à la sauvegarde » (amendement
    /// du 2026-11-11) : la fonction interne avec la liste `["secrets"]`, un propriétaire non-admin
    /// et une définition valide qui accorde `secrets` — le hook de création puis celui de mise à
    /// jour rendent un `HookError` sans code nommant le module réservé, sans citer le prompt,
    /// avant toute écriture.
    #[tokio::test]
    async fn sauvegarde_refuse_un_accord_de_module_reserve_a_la_creation_et_a_la_mise_a_jour() {
        let db = db_avec_compte("alice").await;
        let marque = "PROMPT-A-NE-JAMAIS-CITER-b7d2";
        let accordee = definition_avec_modules("triage", marque, "qwen", &["resources", "secrets"]);
        let principal = principal_sujet("alice");
        let rendu_creation =
            Entity::sauvegarde_avec(&db, active_avec(accordee.clone()), None, &principal, &["secrets"]).await;
        match rendu_creation {
            Err(erreur) => assert_refus_module_reserve(&erreur, marque),
            Ok(_) => panic!("`secrets` réservé à l'admin est refusé à un non-admin à la création"),
        }
        let rendu_mise_a_jour = Entity::sauvegarde_avec(
            &db,
            active_avec(accordee),
            Some(&modele(1, "triage")),
            &principal,
            &["secrets"],
        )
        .await;
        match rendu_mise_a_jour {
            Err(erreur) => assert_refus_module_reserve(&erreur, marque),
            Ok(_) => panic!("`secrets` réservé à l'admin est refusé à un non-admin à la mise à jour"),
        }
    }

    /// Scenario « un admin, ou une définition sans module réservé, est accepté » (amendement du
    /// 2026-11-11) : même liste `["secrets"]`, un appelant admin avec la définition qui accorde
    /// `secrets`, puis un non-admin avec une définition qui n'accorde que `resources` — `Ok` dans
    /// les deux cas, l'`ActiveModel` inchangé hors colonnes dérivées.
    #[tokio::test]
    async fn sauvegarde_accepte_un_admin_ou_une_definition_sans_module_reserve() {
        let db = db_migree_sans_compte().await;
        compte_admin(&db, "root").await;
        let marque = "PROMPT-A-NE-JAMAIS-CITER-b7d2";
        let accordee = definition_avec_modules("triage", marque, "qwen", &["secrets"]);
        let rendu_admin = Entity::sauvegarde_avec(
            &db,
            active_avec(accordee.clone()),
            None,
            &principal_sujet("root"),
            &["secrets"],
        )
        .await
        .expect("un admin sauve une définition accordant `secrets`");
        assert_eq!(rendu_admin.agent_id, Set("triage".to_string()));
        assert_eq!(rendu_admin.kind, Set("sub".to_string()));
        assert_eq!(rendu_admin.profile, Set("qwen".to_string()));
        assert!(
            matches!(rendu_admin.owner_id, NotSet),
            "`owner_id` n'est jamais touché par le hook : {:?}",
            rendu_admin.owner_id
        );
        assert!(
            matches!(rendu_admin.created_at, Set(_)),
            "`created_at` est posé à la création : {:?}",
            rendu_admin.created_at
        );
        assert_eq!(
            rendu_admin.definition,
            Set(accordee),
            "la colonne `definition` traverse inchangée hors colonnes dérivées"
        );
        // Un non-admin avec une définition qui n'accorde que `resources` passe aussi.
        resolve_user(&db, "alice", None)
            .await
            .expect("compte local d'alice");
        let rendue = definition_avec_modules("triage", marque, "qwen", &["resources"]);
        Entity::sauvegarde_avec(
            &db,
            active_avec(rendue),
            None,
            &principal_sujet("alice"),
            &["secrets"],
        )
        .await
        .expect("aucun module réservé accordé : la sauvegarde passe");
    }

    /// Scenario « le droit admin est relu à chaque sauvegarde » (amendement du 2026-11-11) : un
    /// admin qui sauve une définition accordant `secrets`, retiré du groupe admin, se la voit
    /// refuser à la seconde sauvegarde — le principal n'est jamais mis en cache.
    #[tokio::test]
    async fn sauvegarde_relit_le_droit_admin_a_chaque_salvegarde() {
        let db = db_migree_sans_compte().await;
        let alice = resolve_user(&db, "alice", None)
            .await
            .expect("compte local d'alice");
        crate::users::membership::sync_group_memberships(&db, alice.id, &["admin".to_string()])
            .await
            .expect("alice membre du groupe admin");
        let marque = "PROMPT-A-NE-JAMAIS-CITER-b7d2";
        let accordee = definition_avec_modules("triage", marque, "qwen", &["secrets"]);
        let principal = principal_sujet("alice");
        Entity::sauvegarde_avec(&db, active_avec(accordee.clone()), None, &principal, &["secrets"])
            .await
            .expect("la première sauvegarde, par un admin, est acceptée");
        retirer_appartenances(&db, alice.id).await;
        match Entity::sauvegarde_avec(&db, active_avec(accordee), None, &principal, &["secrets"]).await {
            Err(erreur) => assert_refus_module_reserve(&erreur, marque),
            Ok(_) => panic!("retirée du groupe admin, alice n'est plus bridée ? le droit doit être relu"),
        }
    }

    /// Scenario « une panne de lecture est un refus fermé sans détail » (amendement du
    /// 2026-11-11, DSD `hooks-async` `DEC-7`) : la lecture du compte échoue d'un `DbErr`
    /// détaillée, puis le `subject` n'a aucun compte — les deux rendent le `HookError` sans code
    /// de message **exactement** `authorization check unavailable`, sans texte de la `DbErr`.
    #[tokio::test]
    async fn sauvegarde_refuse_en_ferme_sur_panne_de_lecture() {
        let definition = definition("triage", AgentKind::Sub, "prompt-a-ne-jamais-citer-9f3c", "qwen");
        // Cas 1 : la lecture du compte échoue d'un `DbErr` porteur d'une marque.
        let rendu = Entity::before_create(
            &db_lecture_en_echec(),
            active_avec(definition.clone()),
            &principal(),
        )
        .await;
        match rendu {
            Err(erreur) => {
                assert!(erreur.code.is_none(), "aucun code `MRD-*` : {erreur:?}");
                assert_eq!(
                    erreur.message, "authorization check unavailable",
                    "la panne de lecture rend le texte fixe du `Must`"
                );
                assert!(
                    !erreur.message.contains("PANNE-DE-LECTURE-marquee-4e1a"),
                    "le texte de la `DbErr` ne traverse jamais : {erreur:?}"
                );
            }
            Ok(_) => panic!("une panne de lecture du compte devait refuser la sauvegarde"),
        }
        // Cas 2 : un `subject` sans compte sur une base migrée.
        let rendu = Entity::before_create(
            &db_migree_sans_compte().await,
            active_avec(definition),
            &principal(),
        )
        .await;
        match rendu {
            Err(erreur) => {
                assert!(erreur.code.is_none(), "aucun code `MRD-*` : {erreur:?}");
                assert_eq!(
                    erreur.message, "authorization check unavailable",
                    "un `subject` inconnu est le même refus fermé"
                );
            }
            Ok(_) => panic!("un `subject` sans compte devait refuser la sauvegarde"),
        }
    }

    /// Scenario « chacun ne voit que ses agents » : `alice` et `bob` ont chacun un agent `x`, et
    /// la liste de chacun ne rend que sa ligne (`OwnerOnly` + colonne propriétaire, tranchés par
    /// le moteur d'accès, pas par le hook — même motif que `./user_context.rs`).
    #[tokio::test]
    async fn chacun_ne_liste_que_ses_agents() {
        let db = sea_orm::Database::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite connects");
        Migrator::up(&db, None).await.expect("migrations apply cleanly");
        let alice = resolve_user(&db, "alice", None)
            .await
            .expect("compte local d'alice");
        let bob = resolve_user(&db, "bob", None).await.expect("compte local de bob");
        for compte in [&alice, &bob] {
            Entity::insert(ActiveModel {
                id: NotSet,
                owner_id: Set(compte.id),
                agent_id: Set("x".to_string()),
                kind: Set("sub".to_string()),
                profile: Set("qwen".to_string()),
                definition: Set(definition("x", AgentKind::Sub, "trie", "qwen")),
                created_at: Set(Utc::now()),
            })
            .exec(&db)
            .await
            .expect("l'agent `x` est inséré pour chaque propriétaire");
        }
        for (appelee, proprietaire) in [("alice", alice.id), ("bob", bob.id)] {
            let vue =
                crate::rest::core::list::<Entity>(&db, &principal_sujet(appelee), Some(1), Some(50), None)
                    .await
                    .expect("la liste de l'appelante aboutit");
            assert_eq!(vue.total_items, 1, "`{appelee}` ne voit qu'une ligne");
            assert!(
                vue.items.iter().all(|ligne| ligne.owner_id == proprietaire),
                "`{appelee}` ne voit que ses lignes"
            );
        }
    }
}
