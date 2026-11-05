//! Entité `lymaille_contexts` (feature `lymaille`) — contrat porté par
//! `./user_context.sdd`.
//!
//! Table `miryad_lymaille_user_contexts` : les contextes de toolset personnels (DSD `DEC-20`,
//! seconde ressource de la paire `DEC-19`), chaque utilisateur décrivant ses environnements
//! (paramètres, liaisons rôle → serveur MCP). `OwnerOnly` : l'utilisateur lit et écrit ses
//! lignes, un admin lit et écrit toutes les lignes (règle du moteur d'accès, `owner_id` posé
//! par l'injection du propriétaire dans `rest::core`). Le niveau `Session` d'un contexte est
//! interne, sans entité. Un contexte ne contient **aucun secret** : le constructeur de
//! `lymaille` applique son détecteur à la désérialisation. Les liaisons rôle → serveur ne sont
//! pas contrôlées ici (hook synchrone et sans connexion) : le droit d'usage est contrôlé à
//! l'assemblage et à l'appel (`../mcp_access.sdd`), et par `ContextStore::put_context` quand la
//! session de configuration écrit (`../store/toolset.sdd`).

use sea_orm::ActiveValue;
use sea_orm::entity::prelude::*;
use serde::Deserialize;
use serde::Serialize;

use crate::auth::AuthPrincipal;
use crate::lymaille::identity;
use crate::resource::AccessPolicy;
use crate::resource::HookError;
use crate::resource::MiryadResource;

/// Contexte de toolset personnel persisté — la table `miryad_lymaille_user_contexts` posée par
/// `../migration.rs` (`m20261104_000001_lymaille_lot2`), colonnes du contrat `Must` de
/// `./user_context.sdd`.
#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "miryad_lymaille_user_contexts")]
pub struct Model {
    /// Clé primaire entière auto-incrémentée (convention `miryad_*` de la crate).
    #[sea_orm(primary_key)]
    pub id: i32,
    /// Propriétaire de la ligne (`miryad_users.id`), posé par l'injection du propriétaire du
    /// moteur d'accès — jamais par le hook. Unicité `(owner_id, name)` par la migration.
    pub owner_id: i32,
    /// Identifiant du contexte, copie dérivée du `name` du contexte par les hooks — libellé de
    /// la ressource, unique par propriétaire (contrainte de la migration). Jamais un préfixe
    /// `G_`/`g_` du catalogue, jamais un `H_…` résolu par l'hôte (DSD `DEC-9`, `DEC-27`).
    pub name: String,
    /// Le `lymaille::ToolsetContext`, colonne JSON : la source de vérité, validée par
    /// `before_create` et `before_update`. Ne contient aucun secret (le constructeur de
    /// `lymaille` refuse une valeur qui en a l'air).
    pub context: serde_json::Value,
    /// Référence par `name` à un type de contexte du catalogue (`../definitions/context_type.rs`
    /// — arbitrage `DEC-28` du 2026-11-05) : la nature, l'agent par défaut et les agents autorisés
    /// des sessions ouvertes sur ce contexte en viennent. `None` : le contexte ne précise aucun
    /// type, l'ouverture de session le refuse (`../interactive/manager.sdd`) ; l'existence du type
    /// est vérifiée à l'ouverture, pas au hook (hook synchrone sans connexion) ; le hook ne
    /// contrôle que la **forme** de la référence, comme `./catalog_context.rs`.
    pub context_type: Option<String>,
    /// Horodatage de création, posé par le hook de création.
    pub created_at: DateTimeUtc,
}

/// Aucune relation : les liaisons rôle → serveur sont des noms portés par le JSON, sans clé
/// étrangère (contrôle à l'assemblage et à l'appel, `../mcp_access.sdd`).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

impl Entity {
    /// Jeton JSON (`snake_case`, lymaille 0.1.0) du seul niveau admis dans cette table.
    const NIVEAU_PRIVE: &'static str = "user";

    /// Nom lu dans la forme sérialisée : `ToolsetContext` ne rend pas son `name` par un
    /// accesseur public (lymaille 0.1.0, `pub(in crate::toolsets)`) — la colonne `name` est la
    /// copie de la clé de l'objet JSON, source de vérité, comme dans `./catalog_context.rs`. La
    /// désérialisation a réussi avant cet appel, la clé y est donc requise ; le refus n'est
    /// qu'un garde-fou sans `unwrap`.
    fn champ_texte(brute: &serde_json::Value, champ: &str) -> Result<String, HookError> {
        brute
            .get(champ)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| HookError::new(format!("invalid user context: no `{champ}` in the stored form")))
    }

    /// Le `context` se désérialise en `lymaille::ToolsetContext` — le constructeur de la
    /// bibliothèque applique son détecteur de secrets à chaque paramètre, rôle et surcharge
    /// (lymaille 0.1.0, `toolsets/toolset_context.rs`). Un refus pour secret détecté porte le
    /// message fixe du contrat, jamais la valeur ; les autres causes traversent en texte serde,
    /// qui ne cite que des noms de champ.
    fn charger(brute: &serde_json::Value) -> Result<(), HookError> {
        serde_json::from_value::<::lymaille::ToolsetContext>(brute.clone())
            .map(|_| ())
            .map_err(|cause| {
                let cause = cause.to_string();
                if cause.contains("looks like a secret") {
                    HookError::new("user context holds a secret-like value")
                } else {
                    HookError::new(format!("invalid user context: {cause}"))
                }
            })
    }

    /// Le nom doit être `Private` au sens de `classify` (DSD `DEC-9`) : jamais le préfixe
    /// `G_`/`g_` du catalogue, jamais l'identifiant `H_`/`h_` résolu par l'hôte que personne
    /// n'écrit (DSD `DEC-27`, réalignement R4 de `./user_context.sdd`, cohérent avec
    /// `identity::check_write`). Un identifiant invalide au sens Rhai est le même refus sous un
    /// message générique.
    fn verifier_nom(name: &str) -> Result<(), HookError> {
        match identity::classify(name) {
            Ok(identity::IdKind::Private) => Ok(()),
            Ok(identity::IdKind::Catalog) => {
                Err(HookError::new("user context name cannot use the catalog prefix"))
            }
            Ok(identity::IdKind::Host) => Err(HookError::new("user context name cannot use the host prefix")),
            Err(crate::lymaille::error::LymailleError::ReservedPrefix(identifiant)) => {
                if identifiant
                    .get(..2)
                    .is_some_and(|prefix| prefix.eq_ignore_ascii_case("g_"))
                {
                    Err(HookError::new("user context name cannot use the catalog prefix"))
                } else {
                    Err(HookError::new("user context name cannot use the host prefix"))
                }
            }
            Err(_) => Err(HookError::new("invalid user context name")),
        }
    }

    /// Porte commune de `before_create` (`existing` absent) et `before_update`, dans l'ordre du
    /// `Must` de `./user_context.sdd`, interrompue au premier échec : `context` objet JSON
    /// présent, désérialisable en `lymaille::ToolsetContext` (le constructeur refuse un secret,
    /// sans jamais le citer), `level` du contexte égal à `User`, `name` privé via `classify`,
    /// et sur une mise à jour `name` ne change pas. `name` est toujours réécrit depuis le
    /// contexte ; `created_at` est posé à la création s'il manque, et reste celui de la ligne
    /// existante à la mise à jour. `owner_id` n'est pas touché : il est posé par l'injection du
    /// propriétaire du moteur d'accès.
    fn valider_et_deriver(
        mut active: ActiveModel,
        existing: Option<&Model>,
    ) -> Result<ActiveModel, HookError> {
        let brute = match &active.context {
            ActiveValue::Set(brute) | ActiveValue::Unchanged(brute) if brute.is_object() => brute.clone(),
            ActiveValue::Set(_) | ActiveValue::Unchanged(_) | ActiveValue::NotSet => {
                return Err(HookError::new("user context is required"));
            }
        };
        Self::charger(&brute)?;
        if Self::champ_texte(&brute, "level")? != Self::NIVEAU_PRIVE {
            return Err(HookError::new("user contexts have level user"));
        }
        let name = Self::champ_texte(&brute, "name")?;
        Self::verifier_nom(&name)?;
        if let Some(ligne) = existing
            && name != ligne.name
        {
            return Err(HookError::new("user context name cannot change"));
        }
        let reference = match &active.context_type {
            ActiveValue::Set(reference) | ActiveValue::Unchanged(reference) => reference.as_deref(),
            ActiveValue::NotSet => None,
        };
        if reference
            .is_some_and(|reference| !matches!(identity::classify(reference), Ok(identity::IdKind::Private)))
        {
            // Symétrisée 2026-11-05 avec `./catalog_context.rs` (`DEC-28`) : la référence désigne
            // un nom de type du catalogue — la grammaire `lymaille`, jamais un identifiant
            // `G_`/`H_` (les noms de type n'ont pas de préfixe, arbitrage (a) du 2026-11-04).
            // L'existence n'est vérifiée qu'à l'ouverture : le hook ne juge que la forme.
            return Err(HookError::new("user context type reference is not a valid name"));
        }
        active.name = ActiveValue::Set(name);
        match existing {
            None => {
                if matches!(active.created_at, ActiveValue::NotSet) {
                    active.created_at = ActiveValue::Set(chrono::Utc::now());
                }
            }
            Some(ligne) => active.created_at = ActiveValue::Set(ligne.created_at),
        }
        Ok(active)
    }
}

/// Métadonnées de la ressource `lymaille_contexts`, lue à l'identique par REST et MCP. Privé
/// (DSD `DEC-19`, `DEC-20`) : lecture et écriture par le propriétaire seul, l'admin voyant tout
/// par la règle du moteur d'accès.
impl MiryadResource for Entity {
    fn resource_name() -> &'static str {
        "lymaille_contexts"
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
        Some(Column::Name)
    }

    /// Valide le contexte et réécrit `name` — voir [`Entity::valider_et_deriver`].
    fn before_create(
        active: Self::ActiveModel,
        _principal: &AuthPrincipal,
    ) -> Result<Self::ActiveModel, HookError> {
        Entity::valider_et_deriver(active, None)
    }

    /// Mêmes étapes que [`Self::before_create`], plus l'identité : un `name` qui change est
    /// refusé (`Must` de `./user_context.sdd`, précédent `./catalog_context.rs`).
    fn before_update(
        active: Self::ActiveModel,
        existing: &Self::Model,
        _principal: &AuthPrincipal,
    ) -> Result<Self::ActiveModel, HookError> {
        Entity::valider_et_deriver(active, Some(existing))
    }

    // `before_delete` volontairement non surdéclaré : aucun contrôle d'usage au lot 2, le défaut
    // identité du trait s'applique.
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
    use ::lymaille::ContextLevel;
    use ::lymaille::ToolsetContext;
    use chrono::TimeZone as _;
    use chrono::Utc;
    use sea_orm::ActiveValue::NotSet;
    use sea_orm::ActiveValue::Set;
    use sea_orm::EntityTrait as _;
    use sea_orm_migration::MigratorTrait;
    use serde_json::json;

    // ── Fixtures — purement mémoire pour les hooks purs : aucune connexion. La forme sérialisée
    // de base est produite par la bibliothèque elle-même (`ToolsetContext::new` puis
    // `to_value`). Comme au catalogue (`./catalog_context.rs`), `ToolsetContext::new` ne refuse
    // que le nom vide (lymaille 0.1.0, `toolsets/toolset_context.rs`) : un nom `dev` ou `G_x`
    // sort du constructeur et se désérialise ; c'est le hook qui tranche par `classify`.

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

    /// Forme sérialisée d'un contexte nommé `name`, de niveau `niveau`, paramètres `parameters`
    /// injectés directement — `with_parameter` refuse un secret, ce que la fixture de refus
    /// veut justement porter jusqu'au hook.
    fn contexte(name: &str, niveau: ContextLevel, parameters: serde_json::Value) -> serde_json::Value {
        let construit = ToolsetContext::new(name.to_string(), niveau).expect("la fixture se construit");
        let mut valeur = serde_json::to_value(construit).expect("la fixture se sérialise");
        let Some(map) = valeur.as_object_mut() else {
            panic!("la forme sérialisée d'un `ToolsetContext` est un objet");
        };
        map.insert("parameters".to_string(), parameters);
        valeur
    }

    /// `ActiveModel` dans la forme qui atteint le hook : colonne dérivée fausse (le hook doit la
    /// réécrire), `context` posé, clé, propriétaire et horodatage non posés.
    fn active_avec(context: serde_json::Value) -> ActiveModel {
        ActiveModel {
            id: NotSet,
            owner_id: NotSet,
            name: Set("faux-nom".to_string()),
            context: Set(context),
            context_type: NotSet,
            created_at: NotSet,
        }
    }

    /// Ligne existante des hooks `before_update`.
    fn modele(owner_id: i32, name: &str) -> Model {
        Model {
            id: 1,
            owner_id,
            name: name.to_string(),
            context: contexte(name, ContextLevel::User, json!({"namespace": "dev"})),
            context_type: None,
            created_at: Utc.with_ymd_and_hms(2026, 11, 1, 12, 0, 0).unwrap(),
        }
    }

    /// Scenario « chacun ne lit que ses contextes » : `alice` et `bob` ont chacun un contexte
    /// `dev`, et la liste de chacun ne rend que sa ligne (`OwnerOnly` + colonne propriétaire,
    /// tranchés par le moteur d'accès, pas par le hook).
    #[tokio::test]
    async fn chacun_ne_liste_que_ses_contextes() {
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
                name: Set("dev".to_string()),
                context: Set(contexte("dev", ContextLevel::User, json!({"namespace": "dev"}))),
                context_type: NotSet,
                created_at: Set(Utc::now()),
            })
            .exec(&db)
            .await
            .expect("le contexte `dev` est inséré");
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

    /// Métadonnées `Must` : `OwnerOnly` en lecture comme en écriture, colonne propriétaire
    /// `owner_id`, libellé `name`, aucune colonne de filtre.
    #[test]
    fn politiques_de_la_ressource_privee() {
        assert_eq!(Entity::resource_name(), "lymaille_contexts");
        assert_eq!(Entity::read_policy(), AccessPolicy::OwnerOnly);
        assert_eq!(Entity::write_policy(), AccessPolicy::OwnerOnly);
        assert!(matches!(Entity::owner_column(), Some(Column::OwnerId)));
        assert!(Entity::filter_column().is_none(), "aucune colonne de filtre");
        assert!(matches!(Entity::label_column(), Some(Column::Name)));
    }

    /// Le Given valide des Scenarios : `before_create` accepte un contexte `dev` de niveau
    /// `user` et réécrit `name` depuis le contexte ; `created_at` est posé quand il manque.
    #[test]
    fn before_create_accepte_un_contexte_valide_et_derive_le_nom() {
        let rendu = Entity::before_create(
            active_avec(contexte("dev", ContextLevel::User, json!({"namespace": "dev"}))),
            &principal(),
        )
        .expect("un contexte `dev` de niveau user est accepté");
        assert_eq!(
            rendu.name,
            Set("dev".to_string()),
            "`name` est réécrit depuis le contexte"
        );
        assert!(
            matches!(rendu.created_at, Set(_)),
            "`created_at` est posé par le hook de création quand il est absent"
        );
    }

    /// Scenario « le préfixe catalogue est refusé » : `G_x` (et `g_x`, préfixe réservé) ne
    /// commencent pas comme un nom privé.
    #[test]
    fn before_create_refuse_le_prefixe_catalogue() {
        for nom in ["G_x", "g_x"] {
            let rendu = Entity::before_create(
                active_avec(contexte(nom, ContextLevel::User, json!({}))),
                &principal(),
            );
            match rendu {
                Err(erreur) => {
                    assert!(
                        erreur.code.is_none(),
                        "aucun code `MRD-*` sur un `HookError` : {erreur:?}"
                    );
                    assert_eq!(erreur.message, "user context name cannot use the catalog prefix");
                }
                Ok(_) => panic!("`{nom}` n'est pas un identifiant privé"),
            }
        }
    }

    /// Scenario « un identifiant hôte n'est jamais écrit » (réalignement R4, DSD `DEC-27`) :
    /// `H_sandbox` (et `h_sandbox`, préfixe réservé) relèvent de l'hôte, pas de cette table.
    #[test]
    fn before_create_refuse_un_identifiant_hote() {
        for nom in ["H_sandbox", "h_sandbox"] {
            let rendu = Entity::before_create(
                active_avec(contexte(nom, ContextLevel::User, json!({}))),
                &principal(),
            );
            match rendu {
                Err(erreur) => {
                    assert!(
                        erreur.code.is_none(),
                        "aucun code `MRD-*` sur un `HookError` : {erreur:?}"
                    );
                    assert_eq!(erreur.message, "user context name cannot use the host prefix");
                }
                Ok(_) => panic!("`{nom}` est un identifiant résolu par l'hôte, jamais écrit ici"),
            }
        }
    }

    /// Scenario « un niveau shared est refusé dans la table privée » : `level` doit être `user`.
    #[test]
    fn before_create_refuse_un_niveau_shared() {
        let rendu = Entity::before_create(
            active_avec(contexte("dev", ContextLevel::Shared, json!({}))),
            &principal(),
        );
        match rendu {
            Err(erreur) => {
                assert!(erreur.code.is_none(), "aucun code `MRD-*` sur un `HookError`");
                assert_eq!(erreur.message, "user contexts have level user");
            }
            Ok(_) => panic!("un contexte de niveau shared n'a rien dans la table privée"),
        }
    }

    /// Scenario « un secret est refusé sans écho » : le message porte la règle, jamais la
    /// valeur (`Must not`).
    #[test]
    fn before_create_refuse_un_secret_sans_le_citer() {
        let rendu = Entity::before_create(
            active_avec(contexte(
                "dev",
                ContextLevel::User,
                json!({"api_key": "sk-abcdef"}),
            )),
            &principal(),
        );
        match rendu {
            Err(erreur) => {
                assert!(
                    erreur.code.is_none(),
                    "aucun code `MRD-*` sur un `HookError` : {erreur:?}"
                );
                assert!(
                    erreur.message.contains("secret-like value"),
                    "le refus nomme la règle : {erreur:?}"
                );
                assert!(
                    !erreur.message.contains("sk-abcdef"),
                    "la valeur ne traverse jamais un `HookError` : {erreur:?}"
                );
            }
            Ok(_) => panic!("un paramètre `sk-abcdef` devait être refusé"),
        }
    }

    /// `Must` : `context` absent ou non objet est refusé avant toute désérialisation.
    #[test]
    fn before_create_refuse_un_contexte_absent_ou_non_objet() {
        let sans = ActiveModel {
            context: NotSet,
            ..active_avec(serde_json::Value::Null)
        };
        match Entity::before_create(sans, &principal()) {
            Err(erreur) => assert_eq!(erreur.message, "user context is required"),
            Ok(_) => panic!("un `context` `NotSet` doit être refusé"),
        }
        match Entity::before_create(active_avec(json!(["pas", "un", "objet"])), &principal()) {
            Err(erreur) => assert_eq!(erreur.message, "user context is required"),
            Ok(_) => panic!("un `context` non objet doit être refusé"),
        }
    }

    /// `Must` (symétrisée 2026-11-05, même porte que `./catalog_context.rs`) : `context_type`
    /// posé doit être de forme identifiant valide — un nom de type du catalogue (grammaire
    /// `lymaille`, `classify` = `Private`), jamais un préfixe réservé (`G_`/`H_` et leurs casses
    /// réservées), non vide. L'existence du type n'est pas vérifiée ici (hook sans connexion).
    /// Le `HookError` ne porte aucun code `MRD-*`.
    #[test]
    fn before_create_refuse_un_context_type_de_forme_invalide() {
        for reference in ["", "G_cluster", "g_cluster", "h_cluster", "cluster-x"] {
            let active = ActiveModel {
                context_type: Set(Some(reference.to_string())),
                ..active_avec(contexte("dev", ContextLevel::User, json!({})))
            };
            match Entity::before_create(active, &principal()) {
                Err(erreur) => {
                    assert!(
                        erreur.code.is_none(),
                        "aucun code `MRD-*` sur un `HookError` : {erreur:?}"
                    );
                    assert!(
                        erreur.message.contains("context type"),
                        "le refus nomme la référence en cause : {erreur:?}"
                    );
                }
                Ok(_) => panic!("`{reference}` n'est pas une forme d'identifiant de type valide"),
            }
        }
    }

    /// `Must` : `context_type` absent (`None`) passe, et une référence de forme valide traverse
    /// le hook inchangée — l'existence est vérifiée à l'ouverture de session, pas ici.
    #[test]
    fn before_create_accepte_un_context_type_de_forme_valide() {
        let sans = Entity::before_create(
            active_avec(contexte("dev", ContextLevel::User, json!({}))),
            &principal(),
        )
        .expect("un contexte sans type est accepté par le hook");
        assert!(
            matches!(sans.context_type, NotSet),
            "`None` (ou absent) traverse le hook : {:?}",
            sans.context_type
        );
        let avec = Entity::before_create(
            ActiveModel {
                context_type: Set(Some("cluster".to_string())),
                ..active_avec(contexte("dev", ContextLevel::User, json!({})))
            },
            &principal(),
        )
        .expect("une référence de forme valide est acceptée sans contrôle d'existence");
        assert_eq!(
            avec.context_type,
            Set(Some("cluster".to_string())),
            "le hook ne réécrit pas la référence"
        );
    }

    /// Scenario (`before_update`) : `name` ne change pas.
    #[test]
    fn before_update_refuse_un_renommage() {
        let rendu = Entity::before_update(
            active_avec(contexte(
                "staging",
                ContextLevel::User,
                json!({"namespace": "staging"}),
            )),
            &modele(1, "dev"),
            &principal(),
        );
        match rendu {
            Err(erreur) => {
                assert!(erreur.code.is_none(), "aucun code `MRD-*` sur un `HookError`");
                assert_eq!(erreur.message, "user context name cannot change");
            }
            Ok(_) => panic!("renommer un contexte change son identité : la mise à jour est refusée"),
        }
    }

    /// `Must` (`before_update`) : une mise à jour qui garde le nom le réécrit depuis le contexte
    /// et laisse `created_at` à celui de la ligne existante.
    #[test]
    fn before_update_derive_le_nom_et_conserve_la_date_de_creation() {
        let existing = modele(1, "dev");
        let rendu = Entity::before_update(
            active_avec(contexte("dev", ContextLevel::User, json!({"namespace": "dev"}))),
            &existing,
            &principal(),
        )
        .expect("une mise à jour qui garde le nom est acceptée");
        assert_eq!(rendu.name, Set("dev".to_string()));
        assert_eq!(
            rendu.created_at,
            Set(existing.created_at),
            "`created_at` reste celui de la ligne existante"
        );
    }
}
