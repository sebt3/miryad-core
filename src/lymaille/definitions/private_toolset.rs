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
//! **Limite assumée** (comme `./private_agent.rs`) : les droits d'accord de modules du
//! propriétaire exigent l'état admin relu en base et ne sont pas vérifiés dans le hook (synchrone,
//! sans connexion) mais à l'activation (`../interactive/activation.sdd`) et à l'appel.

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

    /// Valide les versions présentes et dérive `toolset_id` et `version` — voir
    /// [`Entity::valider_et_deriver`].
    fn before_create(
        active: Self::ActiveModel,
        _principal: &AuthPrincipal,
    ) -> Result<Self::ActiveModel, HookError> {
        Entity::valider_et_deriver(active, None)
    }

    /// Mêmes étapes que [`Self::before_create`], plus l'identité : un `toolset_id` qui change est
    /// refusé.
    fn before_update(
        active: Self::ActiveModel,
        existing: &Self::Model,
        _principal: &AuthPrincipal,
    ) -> Result<Self::ActiveModel, HookError> {
        Entity::valider_et_deriver(active, Some(existing))
    }

    // `before_delete` volontairement non surdéclaré : aucun contrôle d'usage, le défaut identité
    // du trait s'applique (./private_toolset.sdd).
}

#[cfg(test)]
mod tests {
    use super::ActiveModel;
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
    use sea_orm::EntityTrait as _;
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
    #[test]
    fn before_create_accepte_un_brouillon_prive_seul_et_derive_les_colonnes() {
        let rendu = Entity::before_create(
            active_avec(None, Some(version_json("git", "1.0.0"))),
            &principal(),
        )
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
    #[test]
    fn before_create_refuse_un_toolset_sans_version() {
        let rendus = vec![
            Entity::before_create(active_avec(None, None), &principal()),
            Entity::before_create(
                active_avec(Some(serde_json::Value::Null), Some(serde_json::Value::Null)),
                &principal(),
            ),
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
    #[test]
    fn before_create_refuse_le_prefixe_catalogue() {
        for name in ["G_git", "g_git"] {
            let rendu =
                Entity::before_create(active_avec(Some(version_json(name, "1.0.0")), None), &principal());
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
    #[test]
    fn before_create_refuse_un_identifiant_hote() {
        let rendu = Entity::before_create(
            active_avec(Some(version_json("H_git", "1.0.0")), None),
            &principal(),
        );
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
    #[test]
    fn before_create_refuse_le_prefixe_hote_minuscule() {
        let rendu = Entity::before_create(
            active_avec(Some(version_json("h_git", "1.0.0")), None),
            &principal(),
        );
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
    #[test]
    fn before_create_refuse_un_toolset_invalide_sans_fuir_le_source() {
        let marque = "SOURCE-A-NE-JAMAIS-CITER-7c21";
        let valeur = serde_json::to_value(toolset("git", "1.0.0", &format!("let x = {marque}(((")))
            .expect("la fixture se sérialise");
        match Entity::before_create(active_avec(Some(valeur), None), &principal()) {
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
    #[test]
    fn before_update_refuse_un_changement_d_identifiant() {
        let rendu = Entity::before_update(
            active_avec(Some(version_json("autre", "2.0.0")), None),
            &modele(1, "git"),
            &principal(),
        );
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
    #[test]
    fn before_update_derive_les_colonnes_et_ne_touche_pas_la_date_de_creation() {
        let existing = modele(1, "git");
        let rendu = Entity::before_update(
            active_avec(Some(version_json("git", "2.0.0")), None),
            &existing,
            &principal(),
        )
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
