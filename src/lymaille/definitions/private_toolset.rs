//! Entité `lymaille_toolsets` (feature `lymaille`) — contrat porté par `./private_toolset.sdd`.
//!
//! Table `miryad_lymaille_toolsets` : les toolsets **privés** d'un utilisateur (seconde ressource
//! de la paire `DEC-19`). Comme au catalogue (`./toolset.rs`), chaque ligne porte la version
//! **active** et le **brouillon** d'un toolset (DSD `DEC-18`) ; l'**activation self-service**
//! (DSD `FR-9`, `DEC-8`) est l'écriture de `active` par le propriétaire — par l'entité ici, ou par
//! l'action d'activation de la session de configuration (`../interactive/activation.sdd`).
//! `OwnerOnly` : le moteur d'accès pose `owner_id` et cloisonne les lignes ; le `name` doit être
//! un identifiant `Private` au sens de `crate::lymaille::identity` (jamais `G_`/`g_`, DSD
//! `DEC-9`). Les contrôles de version sont le `check_toolset` partagé de ./toolset.rs.
//!
//! **Contrôle à la sauvegarde** (comme `./private_agent.rs`, amendement du 2026-11-11, DSD
//! `hooks-async` `DEC-7`) : les droits d'accord de modules du propriétaire, relu en base par la
//! connexion du hook, sont vérifiés ici pour chacune des versions posées — puis à l'activation
//! (`../interactive/activation.sdd`) et à l'appel, en défense en profondeur.

use sea_orm::ActiveValue;
use sea_orm::entity::prelude::*;
use serde::Deserialize;
use serde::Serialize;

use crate::auth::AuthPrincipal;
use crate::lymaille::identity;
use crate::resource::AccessPolicy;
use crate::resource::HookError;
use crate::resource::MiryadResource;

use super::toolset::check_toolset;

/// Toolset privé persisté — la table `miryad_lymaille_toolsets` posée par `../migration.rs`,
/// colonnes du contrat `Must` de `./private_toolset.sdd`.
#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "miryad_lymaille_toolsets")]
pub struct Model {
    /// Clé primaire entière auto-incrémentée (convention `miryad_*` de la crate).
    #[sea_orm(primary_key)]
    pub id: i32,
    /// Propriétaire de la ligne (`miryad_users.id`), posé par l'injection du propriétaire du
    /// moteur d'accès — jamais par le hook. Unicité `(owner_id, toolset_id)` par la migration.
    pub owner_id: i32,
    /// Identifiant du toolset (privé, jamais `G_`/`g_`), copie dérivée du `name` de ses versions
    /// par les hooks — libellé de la ressource, unique par propriétaire (contrainte de la
    /// migration).
    pub toolset_id: String,
    /// Version sémantique de la version active (à défaut du brouillon), copie dérivée par les
    /// hooks.
    pub version: String,
    /// La `lymaille::Toolset` active, colonne JSON nullable — son écriture par le propriétaire
    /// est l'activation self-service (DSD `FR-9`, `DEC-8`).
    pub active: Option<serde_json::Value>,
    /// La `lymaille::Toolset` en brouillon, colonne JSON nullable — jamais lue comme version
    /// active (DSD `DEC-18`).
    pub draft: Option<serde_json::Value>,
    /// Horodatage de création, posé par le hook de création.
    pub created_at: DateTimeUtc,
}

/// Aucune relation : les versions vivent dans la même ligne, la référence entre toolsets est un
/// nom, sans clé étrangère.
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

impl Entity {
    /// Étape d'identifiant du `Must` de `./private_toolset.sdd` : le `name` doit être `Private`
    /// au sens de `identity::classify` (DSD `DEC-9`). Distinction par préfixe alignée sur
    /// `./user_context.rs` (arbitrage du 2026-11-06) : `G_`/`g_` → « toolset id cannot use the
    /// catalog prefix » ; `H_`/`h_` (dont la casse réservée `h_` rend `ReservedPrefix` au sens de
    /// `classify`, DSD `DEC-27`) → « toolset id cannot use the host prefix » ; identifiant invalide
    /// au sens Rhai → « invalid toolset id ».
    fn verifier_identifiant(name: &str) -> Result<(), HookError> {
        match identity::classify(name) {
            Ok(identity::IdKind::Private) => Ok(()),
            Ok(identity::IdKind::Catalog) => Err(HookError::new("toolset id cannot use the catalog prefix")),
            Ok(identity::IdKind::Host) => Err(HookError::new("toolset id cannot use the host prefix")),
            Err(crate::lymaille::error::LymailleError::ReservedPrefix(identifiant)) => {
                if identifiant
                    .get(..2)
                    .is_some_and(|prefix| prefix.eq_ignore_ascii_case("g_"))
                {
                    Err(HookError::new("toolset id cannot use the catalog prefix"))
                } else {
                    Err(HookError::new("toolset id cannot use the host prefix"))
                }
            }
            Err(_) => Err(HookError::new("invalid toolset id")),
        }
    }

    /// Texte fixe du refus fermé : panne de lecture (`DbErr`) ou `subject` sans compte lors du
    /// contrôle d'autorité à la sauvegarde (`Must`, amendement du 2026-11-11, DSD `hooks-async`
    /// `DEC-7`) — aucun détail de l'erreur ne traverse.
    const PANNE_AUTORISATION: &'static str = "authorization check unavailable";

    /// Porte commune de `before_create` (`existing` absent) et `before_update`, `after` : les étapes
    /// de ./toolset.sdd dans le même ordre — au moins une version, chacune valide via le
    /// [`check_toolset`] partagé, les deux présentes portent le même `name` — sauf l'étape
    /// d'identifiant : ici `classify(name) = Private` ([`Self::verifier_identifiant`], à la place
    /// de l'étape `G_` du catalogue). Sur une mise à jour le `name` ne change pas. `toolset_id`
    /// et `version` (celle de l'active, à défaut du brouillon) sont toujours réécrits ; `owner_id`
    /// n'est jamais touché (injection du moteur d'accès). **Après** `check_toolset` (`Must not` de
    /// `./private_toolset.sdd`), le contrôle des droits d'accord de modules (`FR-9`, amendement du
    /// 2026-11-11, DSD `hooks-async` `DEC-7`) : le principal de l'appelant est reconstruit par
    /// [`identity::caller_of`] sur la **connexion du hook** — état admin relu, jamais mis en
    /// cache —, puis [`identity::check_grants`] tranche, pour **chacune des versions posées**
    /// (`active` et `draft`), les modules extraits de la forme JSON par la fonction partagée avec
    /// l'activation ([`crate::lymaille::interactive::activation::toolset_json_modules`]), contre
    /// la liste `reserved` injectée (la fonction publique passe `identity::ADMIN_ONLY_MODULES`, le
    /// test une liste non vide — précédent `activate_with`). Un module réservé refusé rend un
    /// `HookError` sans code nommant le module, jamais le source ; une panne de lecture ou un
    /// `subject` inconnu rendent le texte fixe [`Self::PANNE_AUTORISATION`] (échec fermé).
    async fn sauvegarde_avec<C: sea_orm::ConnectionTrait>(
        db: &C,
        active: ActiveModel,
        existing: Option<&Model>,
        principal: &AuthPrincipal,
        reserved: &[&str],
    ) -> Result<ActiveModel, HookError> {
        let active = Self::valider_et_deriver(active, existing)?;
        let caller = identity::caller_of(db, &principal.subject)
            .await
            .map_err(|_| HookError::new(Self::PANNE_AUTORISATION))?;
        for brute in [
            Self::version_presents(&active.active),
            Self::version_presents(&active.draft),
        ]
        .into_iter()
        .flatten()
        {
            let modules = crate::lymaille::interactive::activation::toolset_json_modules(brute);
            identity::check_grants(&caller, &modules, reserved).map_err(|cause| match cause {
                // Le message interne de `Forbidden` nomme le module ; son affichage porterait un
                // code `MRD-*` (`Must not`), seule la charge utile traverse.
                crate::lymaille::error::LymailleError::Forbidden(message) => HookError::new(message),
                _ => HookError::new(Self::PANNE_AUTORISATION),
            })?;
        }
        Ok(active)
    }

    /// Version réellement présente d'une colonne `Option<Json>` : posée (ou inchangée), non `None`,
    /// non `null` JSON — étape 1 des étapes de ./toolset.sdd.
    fn version_presents(valeur: &ActiveValue<Option<serde_json::Value>>) -> Option<&serde_json::Value> {
        match valeur {
            ActiveValue::Set(alhee) | ActiveValue::Unchanged(alhee) => {
                alhee.as_ref().filter(|version| !version.is_null())
            }
            ActiveValue::NotSet => None,
        }
    }

    /// Étape 2 des étapes de ./toolset.sdd : la version se désérialise en `lymaille::Toolset` et
    /// passe le [`check_toolset`] partagé de ./toolset.rs ; l'échec cite la cause, que la
    /// bibliothèque n'établit jamais depuis un fragment de source (`Must not`).
    fn charger(brute: &serde_json::Value) -> Result<::lymaille::Toolset, HookError> {
        let toolset: ::lymaille::Toolset = serde_json::from_value(brute.clone())
            .map_err(|cause| HookError::new(format!("invalid toolset: {cause}")))?;
        check_toolset(&toolset).map_err(|cause| HookError::new(format!("invalid toolset: {cause}")))?;
        Ok(toolset)
    }

    /// Porte commune de `before_create` (`existing` absent) et `before_update` : les étapes de
    /// ./toolset.sdd dans le même ordre — au moins une version, chacune valide via le
    /// [`check_toolset`] partagé, les deux présentes portent le même `name` — sauf l'étape
    /// d'identifiant : ici `classify(name) = Private` ([`Self::verifier_identifiant`], à la place
    /// de l'étape `G_` du catalogue). Sur une mise à jour le `name` ne change pas. `toolset_id`
    /// et `version` (celle de l'active, à défaut du brouillon) sont toujours réécrits ; `owner_id`
    /// n'est jamais touché (injection du moteur d'accès). Aucun `check_grants` : hook synchrone
    /// sans connexion (limite du `Purpose`), le contrôle est à l'activation et à l'appel.
    fn valider_et_deriver(
        mut active: ActiveModel,
        existing: Option<&Model>,
    ) -> Result<ActiveModel, HookError> {
        let brute_active = Self::version_presents(&active.active);
        let brute_draft = Self::version_presents(&active.draft);
        if brute_active.is_none() && brute_draft.is_none() {
            return Err(HookError::new("a toolset needs an active version or a draft"));
        }
        let active_chargee = brute_active.map(Self::charger).transpose()?;
        let draft_charge = brute_draft.map(Self::charger).transpose()?;
        if let (Some(version_active), Some(version_brouillon)) = (&active_chargee, &draft_charge)
            && version_active.name() != version_brouillon.name()
        {
            return Err(HookError::new(
                "the active and draft versions must carry the same name",
            ));
        }
        let reference = active_chargee
            .as_ref()
            .or(draft_charge.as_ref())
            .ok_or_else(|| HookError::new("a toolset needs an active version or a draft"))?;
        Self::verifier_identifiant(reference.name())?;
        if let Some(ligne) = existing
            && reference.name() != ligne.toolset_id.as_str()
        {
            return Err(HookError::new("toolset id cannot change"));
        }
        active.toolset_id = ActiveValue::Set(reference.name().to_owned());
        active.version = ActiveValue::Set(reference.version().to_string());
        if existing.is_none() && matches!(active.created_at, ActiveValue::NotSet) {
            active.created_at = ActiveValue::Set(chrono::Utc::now());
        }
        Ok(active)
    }
}

/// Métadonnées et hooks de la ressource `lymaille_toolsets`, lue à l'identique par REST et MCP.
/// Privé (DSD `DEC-19`) : lecture et écriture par le propriétaire seul, l'admin voyant tout par la
/// règle du moteur d'accès.
impl MiryadResource for Entity {
    fn resource_name() -> &'static str {
        "lymaille_toolsets"
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
        None
    }

    fn label_column() -> Option<<Self as EntityTrait>::Column> {
        Some(Column::ToolsetId)
    }

    /// Valide les versions, contrôle les modules accordés et dérive `toolset_id` et `version` —
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

    /// Mêmes étapes que [`Self::before_create`], plus l'identité : un `toolset_id` qui change est
    /// refusé.
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
    // du trait s'applique (./private_toolset.sdd).
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
    use ::lymaille::Toolset;
    use chrono::TimeZone as _;
    use chrono::Utc;
    use sea_orm::ActiveValue::NotSet;
    use sea_orm::ActiveValue::Set;
    use sea_orm::ColumnTrait as _;
    use sea_orm::EntityTrait as _;
    use sea_orm::QueryFilter as _;
    use sea_orm_migration::MigratorTrait;
    use vynil_core::Semver;

    // ── Fixtures — purement mémoire pour les hooks purs : aucune connexion, comme dans ./toolset.rs.
    // Les versions sont sérialisées depuis un `lymaille::Toolset` construit par la bibliothèque
    // elle-même, pour exercer la forme JSON réelle. `check_toolset` est celui de ./toolset.rs,
    // partagé (`Depends on` de ./private_toolset.sdd).

    /// Source Rhai de fixture : un module qui compile (le contrat est alors la fonction `ping`).
    const SOURCE_VALIDE: &str = "fn ping() { 1 }";

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

    fn toolset(name: &str, version: &str, source: &str) -> Toolset {
        Toolset::new(
            name.to_string(),
            "toolset de test".to_string(),
            Semver::parse(version).expect("la fixture porte une version valide"),
            source.to_string(),
        )
        .expect("la fixture est un toolset de forme valide")
    }

    fn version_json(name: &str, version: &str) -> serde_json::Value {
        serde_json::to_value(toolset(name, version, SOURCE_VALIDE)).expect("la fixture se sérialise")
    }

    /// `ActiveModel` dans la forme qui atteint le hook : colonnes dérivées fausses, versions
    /// voulues, clé, propriétaire et horodatage non posés. `owner_id` n'est jamais posé par le
    /// hook : c'est l'injection du propriétaire du moteur d'accès.
    fn active_avec(active: Option<serde_json::Value>, draft: Option<serde_json::Value>) -> ActiveModel {
        ActiveModel {
            id: NotSet,
            owner_id: NotSet,
            toolset_id: Set("fauc".to_string()),
            version: Set("0.0.0".to_string()),
            active: Set(active),
            draft: Set(draft),
            created_at: NotSet,
        }
    }

    /// Base migrée **sans aucun compte**.
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

    /// Retrait de toute appartenance de l'utilisateur (fixture de test — même motif que
    /// `../identity.rs`).
    async fn retirer_appartenances(db: &sea_orm::DatabaseConnection, user_id: i32) {
        crate::users::membership::Entity::delete_many()
            .filter(crate::users::membership::Column::UserId.eq(user_id))
            .exec(db)
            .await
            .expect("retrait des appartenances de fixture");
    }

    /// Version de `git` en `version` accordant les modules `modules` — la clé `modules` est une
    /// clé JSON additionnelle, ignorée par la désérialisation `Toolset` mais lue par l'extraction
    /// partagée (`../interactive/activation.rs`) — motif des fixtures de `activate_with`.
    fn version_avec_modules(version: &str, modules: &[&str]) -> serde_json::Value {
        let mut valeur = version_json("git", version);
        valeur["modules"] = serde_json::Value::Array(
            modules
                .iter()
                .map(|module| serde_json::Value::String((*module).to_string()))
                .collect(),
        );
        valeur
    }

    /// Verrou d'un refus de module réservé : `HookError` sans code, nommant le module, ne citant
    /// jamais le source des modules.
    fn assert_refus_module_reserve(erreur: &crate::resource::HookError) {
        assert!(
            erreur.code.is_none(),
            "aucun code `MRD-*` sur un `HookError` : {erreur:?}"
        );
        assert!(
            erreur.message.contains("secrets"),
            "le refus nomme le module réservé : {erreur:?}"
        );
        assert!(
            !erreur.message.contains("fn ping"),
            "le source des modules ne traverse jamais un `HookError` : {erreur:?}"
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
    fn modele(owner_id: i32, toolset_id: &str) -> Model {
        Model {
            id: 1,
            owner_id,
            toolset_id: toolset_id.to_string(),
            version: "1.0.0".to_string(),
            active: Some(version_json(toolset_id, "1.0.0")),
            draft: None,
            created_at: Utc.with_ymd_and_hms(2026, 11, 1, 12, 0, 0).unwrap(),
        }
    }

    /// Métadonnées `Must` : `OwnerOnly` en lecture comme en écriture, colonne propriétaire
    /// `owner_id`, aucun filtre, libellé `toolset_id`.
    #[test]
    fn politiques_de_la_ressource_privee() {
        assert_eq!(Entity::resource_name(), "lymaille_toolsets");
        assert_eq!(Entity::read_policy(), AccessPolicy::OwnerOnly);
        assert_eq!(Entity::write_policy(), AccessPolicy::OwnerOnly);
        assert!(matches!(Entity::owner_column(), Some(Column::OwnerId)));
        assert!(Entity::filter_column().is_none(), "aucune colonne de filtre");
        assert!(matches!(Entity::label_column(), Some(Column::ToolsetId)));
    }

    /// Scenario « un toolset privé avec brouillon seul est accepté » : `git` en `draft`, `active`
    /// nul, passe ; `toolset_id` et `version` sont dérivés du brouillon, `created_at` est posé.
    #[tokio::test]
    async fn before_create_accepte_un_brouillon_prive_seul_et_derive_les_colonnes() {
        let db = db_avec_compte("sujet-de-test").await;
        let rendu = Entity::before_create(
            &db,
            active_avec(None, Some(version_json("git", "1.0.0"))),
            &principal(),
        )
        .await
        .expect("un brouillon `git` à l'identifiant privé est accepté");
        assert_eq!(
            rendu.toolset_id,
            Set("git".to_string()),
            "`toolset_id` est la copie dérivée du `name` de la version"
        );
        assert_eq!(
            rendu.version,
            Set("1.0.0".to_string()),
            "`version` vient du brouillon faute d'active"
        );
        assert_eq!(rendu.active, Set(None), "`active` reste vide");
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

    /// `Must` (étape 1 des étapes de ./toolset.sdd) : `active` et `draft` tous deux absents ou
    /// `null` sont refusés avant toute désérialisation.
    #[tokio::test]
    async fn before_create_refuse_un_toolset_sans_version() {
        let rendus = vec![
            Entity::before_create(&hook_db(), active_avec(None, None), &principal()).await,
            Entity::before_create(
                &hook_db(),
                active_avec(Some(serde_json::Value::Null), Some(serde_json::Value::Null)),
                &principal(),
            )
            .await,
        ];
        for rendu in rendus {
            match rendu {
                Err(erreur) => {
                    assert!(erreur.code.is_none(), "aucun code sur un `HookError` de la crate");
                    assert_eq!(erreur.message, "a toolset needs an active version or a draft");
                }
                Ok(_) => panic!("un toolset sans aucune version doit être refusé"),
            }
        }
    }

    /// Scenario « le préfixe catalogue est refusé » : `G_git` (`Catalog`) puis `g_git`
    /// (`ReservedPrefix`) sont refusés par le même message, sans code `MRD-*` (`Must` :
    /// `classify(name)` doit rendre `Private`).
    #[tokio::test]
    async fn before_create_refuse_le_prefixe_catalogue() {
        for name in ["G_git", "g_git"] {
            let rendu = Entity::before_create(
                &hook_db(),
                active_avec(Some(version_json(name, "1.0.0")), None),
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
                        erreur.message, "toolset id cannot use the catalog prefix",
                        "le nom {name} n'est pas privé"
                    );
                }
                Ok(_) => panic!("le toolset `{name}` devait être refusé"),
            }
        }
    }

    /// Scenario « le préfixe hôte est refusé » (moitié majuscule, arbitré le 2026-11-06) :
    /// `H_git` (`classify` = `Host`, DSD `DEC-27`) est refusé sous le message contractualisé
    /// « toolset id cannot use the host prefix », sans code `MRD-*` (`Must`).
    #[tokio::test]
    async fn before_create_refuse_un_identifiant_hote() {
        let rendu = Entity::before_create(
            &hook_db(),
            active_avec(Some(version_json("H_git", "1.0.0")), None),
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
                    erreur.message, "toolset id cannot use the host prefix",
                    "`H_git` porte le préfixe hôte"
                );
            }
            Ok(_) => panic!("`H_git` est un identifiant résolu par l'hôte, jamais écrit ici"),
        }
    }

    /// Scenario « le préfixe hôte est refusé » (moitié minuscule, alignement arbitré le
    /// 2026-11-06 sur `./user_context.sdd`) : `h_git` — `ReservedPrefix` au sens de `classify`,
    /// préfixe `h_` — est refusé sous le **même** message que `H_git`, « toolset id cannot use
    /// the host prefix », et non sous « catalog prefix ».
    #[tokio::test]
    async fn before_create_refuse_le_prefixe_hote_minuscule() {
        let rendu = Entity::before_create(
            &hook_db(),
            active_avec(Some(version_json("h_git", "1.0.0")), None),
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
                    erreur.message, "toolset id cannot use the host prefix",
                    "`h_git` porte le préfixe hôte sous casse réservée, pas le préfixe \
                     catalogue"
                );
            }
            Ok(_) => panic!("`h_git` porte le préfixe hôte réservé, jamais écrit ici"),
        }
    }

    /// Scenario « un toolset invalide est refusé sans fuite de source » (`Must not` de
    /// `./private_toolset.sdd`) : un module qui ne compile pas est refusé via le `check_toolset`
    /// partagé, et le texte de la source ne traverse pas.
    #[tokio::test]
    async fn before_create_refuse_un_toolset_invalide_sans_fuir_le_source() {
        let marque = "SOURCE-A-NE-JAMAIS-CITER-7c21";
        let valeur = serde_json::to_value(toolset("git", "1.0.0", &format!("let x = {marque}(((")))
            .expect("la fixture se sérialise");
        match Entity::before_create(&hook_db(), active_avec(Some(valeur), None), &principal()).await {
            Err(erreur) => {
                assert!(
                    erreur.message.starts_with("invalid toolset:"),
                    "la cause est citée : {erreur:?}"
                );
                assert!(
                    !erreur.message.contains(marque),
                    "le source Rhai ne traverse jamais un `HookError` : {erreur:?}"
                );
            }
            Ok(_) => panic!("un module qui ne compile pas doit être refusé"),
        }
    }

    /// `Must` (`before_update`) : `toolset_id` ne change pas — la nouvelle version nommée `autre`
    /// est refusée sur la ligne `git`.
    #[tokio::test]
    async fn before_update_refuse_un_changement_d_identifiant() {
        let rendu = Entity::before_update(
            &hook_db(),
            active_avec(Some(version_json("autre", "2.0.0")), None),
            &modele(1, "git"),
            &principal(),
        )
        .await;
        match rendu {
            Err(erreur) => {
                assert!(erreur.code.is_none(), "aucun code sur un `HookError` de la crate");
                assert_eq!(erreur.message, "toolset id cannot change");
            }
            Ok(_) => panic!("renommer un toolset est refusé, comme pour les agents"),
        }
    }

    /// `Must` (`before_update`, étapes de ./toolset.sdd) : une mise à jour qui garde le nom
    /// dérive `version` depuis la nouvelle version. Comme au précédent ./toolset.rs, le hook ne
    /// touche pas `created_at` à la mise à jour : la valeur de la ligne existante traverse
    /// l'`ActiveModel` (posée par `mark_all_set` avant le hook) et le hook ne la réécrit pas.
    #[tokio::test]
    async fn before_update_derive_les_colonnes_et_ne_touche_pas_la_date_de_creation() {
        let db = db_avec_compte("sujet-de-test").await;
        let existing = modele(1, "git");
        let rendu = Entity::before_update(
            &db,
            active_avec(Some(version_json("git", "2.0.0")), None),
            &existing,
            &principal(),
        )
        .await
        .expect("une mise à jour qui garde le nom est acceptée");
        assert_eq!(rendu.toolset_id, Set("git".to_string()));
        assert_eq!(rendu.version, Set("2.0.0".to_string()));
        assert!(
            matches!(rendu.created_at, NotSet),
            "`created_at` n'est pas réécrit par le hook à la mise à jour, comme dans \
             ./toolset.rs : {:?}",
            rendu.created_at
        );
    }

    /// Scenario « un module réservé à l'admin, dans le brouillon ou la version active, est refusé
    /// à la sauvegarde » (amendement du 2026-11-11) : la fonction interne avec la liste
    /// `["secrets"]`, un propriétaire non-admin — chaque appel (création puis mise à jour, `draft`
    /// qui accorde `secrets`, puis `active` qui l'accorde) rend un `HookError` sans code nommant
    /// le module et ne citant pas le source.
    #[tokio::test]
    async fn sauvegarde_refuse_un_module_reserve_du_brouillon_ou_de_la_version_active() {
        let db = db_avec_compte("alice").await;
        let principal = principal_sujet("alice");
        for version_reservee in ["draft", "active"] {
            let accordee = version_avec_modules("2.0.0", &["secrets"]);
            let (active, draft) = if version_reservee == "active" {
                (Some(accordee.clone()), None)
            } else {
                (None, Some(accordee.clone()))
            };
            let rendu_creation = Entity::sauvegarde_avec(
                &db,
                active_avec(active.clone(), draft.clone()),
                None,
                &principal,
                &["secrets"],
            )
            .await;
            match rendu_creation {
                Err(erreur) => assert_refus_module_reserve(&erreur),
                Ok(_) => panic!("`secrets` réservé, posé en {version_reservee}, est refusé à la création"),
            }
            let rendu_mise_a_jour = Entity::sauvegarde_avec(
                &db,
                active_avec(active, draft),
                Some(&modele(1, "git")),
                &principal,
                &["secrets"],
            )
            .await;
            match rendu_mise_a_jour {
                Err(erreur) => assert_refus_module_reserve(&erreur),
                Ok(_) => panic!("`secrets` réservé, posé en {version_reservee}, est refusé à la mise à jour"),
            }
        }
    }

    /// Seconde moitié du Scenario « un module réservé à l'admin … est refusé à la sauvegarde » :
    /// un admin passe avec la même définition, et le droit est relu à chaque sauvegarde — retiré
    /// du groupe admin, le même appel est refusé.
    #[tokio::test]
    async fn sauvegarde_accepte_un_admin_et_relit_son_droit_a_chaque_salvegarde() {
        let db = db_migree_sans_compte().await;
        let alice = resolve_user(&db, "alice", None)
            .await
            .expect("compte local d'alice");
        crate::users::membership::sync_group_memberships(&db, alice.id, &["admin".to_string()])
            .await
            .expect("alice membre du groupe admin");
        let accordee = version_avec_modules("1.0.0", &["secrets"]);
        let principal = principal_sujet("alice");
        Entity::sauvegarde_avec(
            &db,
            active_avec(Some(accordee.clone()), None),
            None,
            &principal,
            &["secrets"],
        )
        .await
        .expect("la première sauvegarde, par un admin, est acceptée");
        retirer_appartenances(&db, alice.id).await;
        match Entity::sauvegarde_avec(
            &db,
            active_avec(Some(accordee), None),
            None,
            &principal,
            &["secrets"],
        )
        .await
        {
            Err(erreur) => assert_refus_module_reserve(&erreur),
            Ok(_) => panic!("retirée du groupe admin, alice doit être bridée : le droit est relu"),
        }
    }

    /// Scenario « une panne de lecture est un refus fermé sans détail » (amendement du
    /// 2026-11-11, DSD `hooks-async` `DEC-7`) : la lecture du compte échoue d'un `DbErr`
    /// détaillée — le `HookError` rendu, sans code, a pour message **exactement**
    /// `authorization check unavailable`, sans texte de la `DbErr` ni source des modules.
    #[tokio::test]
    async fn sauvegarde_refuse_en_ferme_sur_panne_de_lecture() {
        let rendu = Entity::before_create(
            &db_lecture_en_echec(),
            active_avec(None, Some(version_json("git", "1.0.0"))),
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
    }

    /// Scenario « cloisonnement par propriétaire » : `alice` et `bob` ont chacun un toolset `git`,
    /// et la liste de chacun ne rend que sa ligne (`OwnerOnly` + colonne propriétaire, tranchés
    /// par le moteur d'accès, pas par le hook — même motif que `./user_context.rs`).
    #[tokio::test]
    async fn chacun_ne_liste_que_ses_toolsets() {
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
                toolset_id: Set("git".to_string()),
                version: Set("1.0.0".to_string()),
                active: Set(Some(version_json("git", "1.0.0"))),
                draft: Set(None),
                created_at: Set(Utc::now()),
            })
            .exec(&db)
            .await
            .expect("le toolset `git` est inséré pour chaque propriétaire");
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
