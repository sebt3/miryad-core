//! Contrat central [`MiryadResource`](crate::resource::MiryadResource) — une implémentation par entité SeaORM,
//! lue telle quelle par REST, GraphQL et MCP.
//!
//! Voir la doc crate pour un exemple complet et `docs/architecture.md` pour les détails.

use sea_orm::EntityTrait;
use serde::Serialize;

use crate::auth::AuthPrincipal;

/// Erreur métier retournée par un hook applicatif (`before_create`, `before_update`,
/// `before_delete`) — jamais une
/// erreur *de* miryad-core, donc jamais de code `MRD-XXX-NNN` (cette convention identifie un
/// problème dans le framework, pas une règle métier qui rejette une requête). Le code est libre,
/// à la charge de l'app ; `None` si elle n'en a pas.
#[derive(Debug, Clone)]
pub struct HookError {
    /// Code applicatif libre, à la charge de l'app — jamais un code `MRD-*` de la crate ;
    /// `None` quand l'app n'a pas de code.
    pub code: Option<String>,
    /// Message décrivant la règle métier qui rejette l'opération.
    pub message: String,
}

impl HookError {
    /// Construit une erreur sans code (`code` à `None`) — le message accepte indifféremment
    /// `&str` et `String`.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            code: None,
            message: message.into(),
        }
    }

    /// Construit une erreur avec un code applicatif (`code` à `Some`) — code libre, jamais un
    /// code `MRD-*` de la crate ; les deux paramètres acceptent indifféremment `&str` et
    /// `String`.
    pub fn with_code(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: Some(code.into()),
            message: message.into(),
        }
    }
}

/// Politique d'accès à une entité exposée par miryad-core.
/// Read et write sont évalués séparément — une entité peut être publique en
/// lecture et restreinte en écriture (cas "recettes partagées, modifiables
/// par leur auteur uniquement").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum AccessPolicy {
    /// Tout utilisateur authentifié (JWT ou token API valide)
    Public,
    /// Uniquement l'utilisateur référencé par `owner_column` (+ les membres
    /// du groupe admin). Sans `owner_column` déclarée, c'est une déclaration
    /// invalide — voir [`MiryadResource::owner_column`].
    OwnerOnly,
    /// Membres du groupe nommé (+ admin)
    Group(&'static str),
    /// Membres du groupe admin uniquement
    AdminOnly,
}

/// Contrat qu'implémente toute entité `SeaORM` exposée par miryad-core.
/// Une seule implémentation par entité — REST, GraphQL et MCP la lisent
/// telle quelle, aucune n'a sa propre déclaration de politique.
pub trait MiryadResource: EntityTrait {
    /// Nom exposé côté API (ex: "recipes") — utilisé pour les chemins REST et le nom des tools
    /// MCP.
    ///
    /// Côté GraphQL, ce nom n'est **pas** le type d'objet exposé dans le schéma : `Seaography`
    /// `2.0.0-rc.9` construit le nom d'objet et l'identifiant passé à ses hooks depuis le nom de
    /// table SQL en `CamelCase` (`EntityObjectBuilder::type_name`, vérifié source amont), jamais
    /// depuis `resource_name`. `graphql::PolicyRegistry` indexe pourtant par `resource_name` et
    /// `graphql::hooks` retombe sur `GuardAction::Allow` quand aucune politique ne correspond —
    /// fail-open documenté tel quel, bug `[!]` #19 de `resource.sdd`, correctif différé tant que
    /// le pont GraphQL reste un prototype sans consommateur. Une entité dont la table SQL et le
    /// `resource_name` ne s'alignent pas (ex: table `tags` pour le nom `recipe-tags`) voit ses
    /// politiques déclarées inappliquées sur cette surface.
    fn resource_name() -> &'static str;

    /// Politique de lecture déclarée — déclaration indépendante de `write_policy` (aucune
    /// fusion implicite : une entité peut être `Public` en lecture et `OwnerOnly` en écriture),
    /// fonction statique sans récepteur ni contexte.
    fn read_policy() -> AccessPolicy;
    /// Politique d'écriture déclarée — deuxième déclaration indépendante du couple, évaluée
    /// séparément de `read_policy` par le `RBAC` sur chaque opération.
    fn write_policy() -> AccessPolicy;

    /// Colonne portant l'identifiant du propriétaire. `None` si l'entité
    /// n'a pas de notion de propriétaire (ex: référentiel partagé comme la
    /// liste des ingrédients dans l'exemple recette).
    /// Doit désigner une colonne de type entier `i32` (une colonne `Option<i32>`, nullable, est
    /// acceptée), comme `users::user::Model::id` (arbitré 2026-09-29) : `rest::resource_router`
    /// refuse de monter l'entité dont la colonne propriétaire a un autre type (`panic` au
    /// montage, `rest/mod.sdd`). Une ligne dont la colonne vaut `None` n'a aucun propriétaire —
    /// refusée hors admin (`rbac.sdd`), contrat assumé. Ce fichier ne vérifie toujours rien
    /// lui-même.
    /// Être `None` alors que `read_policy()` ou `write_policy()` retourne `AccessPolicy::OwnerOnly`
    /// est une **déclaration invalide** (arbitré 2026-09-27), jamais un choix runtime à arbitrer
    /// par requête : `rest::mod` refuse de monter le routeur d'une telle entité (panic au montage,
    /// même mécanisme que la collision de `resource_name`) et `rbac::can_create` refuse par
    /// cohérence défensive avec `can_read`/`can_write`. Ce fichier ne vérifie rien lui-même à la
    /// compilation — l'invalidité se prouve en aval, le comportement est documenté ici.
    fn owner_column() -> Option<<Self as EntityTrait>::Column>;

    /// Colonne texte sur laquelle la liste peut être filtrée côté REST et MCP seulement
    /// (`?filter=valeur`, égalité exacte) — feature 4. GraphQL ne lit jamais cette colonne :
    /// le pont GraphQL ne construit que la clause propriétaire, le filtrage y passe par les
    /// inputs natifs `seaography` (arbitré 2026-09-27). `None` par défaut : pas
    /// de filtre pour cette entité. Une entité qui veut un filtre de liste
    /// (ex. "recettes par catégorie") le déclare explicitement.
    #[must_use]
    fn filter_column() -> Option<<Self as EntityTrait>::Column> {
        None
    }

    /// Colonne à afficher comme libellé humain de l'entité (liste, select) — feature 8, IR
    /// frontend. Contrat du template `miryad`, pas de miryad-core (arbitré 2026-09-29) :
    /// miryad-core ne déclare que la colonne (ou `None`) ; le générateur du template retombe sur
    /// la clé primaire quand elle vaut `None` — exigence adressée au générateur `TypeScript` du
    /// template, hors de ce dépôt et invérifiable ici.
    #[must_use]
    fn label_column() -> Option<<Self as EntityTrait>::Column> {
        None
    }

    /// Hook métier exécuté après RBAC (`can_create`), avant l'insertion — peut muter
    /// l'`ActiveModel` (champ dérivé, valeur calculée) ou rejeter l'opération avec une erreur
    /// métier. Miroir direct de `before_active_model_save` (Seaography, feature 5) : create only,
    /// car Seaography ne déclenche ce hook que sur un insert pour l'instant — un hook qui ne se
    /// comporterait pas à l'identique sur les 3 surfaces (REST/GraphQL/MCP) n'a pas sa place ici.
    /// Défaut : no-op.
    ///
    /// # Errors
    ///
    /// Un `Err(HookError)` remonté par l'override de l'application consommatrice (code applicatif
    /// libre, jamais un code `MRD-*` de la crate). Le défaut ne rejette jamais.
    fn before_create(
        active: Self::ActiveModel,
        principal: &AuthPrincipal,
    ) -> Result<Self::ActiveModel, HookError> {
        let _ = principal;
        Ok(active)
    }

    /// Hook métier de la mise à jour — position symétrique de [`before_create`](Self::before_create)
    /// sur la création : après RBAC (`can_write`, évalué sur la ligne relue) et après
    /// `rest::core::mark_all_set`, avant les deux invariants de `rest::core::update` (forçage de la
    /// clé primaire à l'id du chemin, reconduction de `owner_column` depuis `existing`) — un hook
    /// hostile est défait par le même ordre que sur la création. `existing` (le `Model` relu avant
    /// mise à jour, la même ligne qu'a évaluée `can_write`) est prêté en lecture seule : comparer
    /// avant/après (ex. valider une transition d'état) ne demande pas de relire la base soi-même.
    /// Déclenché sur REST et MCP seulement, **jamais sur GraphQL** — Seaography `2.0.0-rc.9` ne
    /// pilote son hook équivalent (`before_active_model_save`) qu'à l'insertion ; asymétrie actée
    /// par l'amendement `resource.sdd` du 2026-09-23 à la règle de parité. Défaut : identité —
    /// `Ok(active)`, `existing` et `principal` ignorés.
    ///
    /// # Errors
    ///
    /// Un `Err(HookError)` remonté par l'override de l'application consommatrice (code applicatif
    /// libre, jamais un code `MRD-*` de la crate). Le défaut ne rejette jamais.
    fn before_update(
        active: Self::ActiveModel,
        existing: &Self::Model,
        principal: &AuthPrincipal,
    ) -> Result<Self::ActiveModel, HookError> {
        let _ = existing;
        let _ = principal;
        Ok(active)
    }

    /// Hook métier de la suppression : exécuté après RBAC (`can_write`, évalué sur la ligne
    /// relue), avant l'exécution du `DELETE` — un `Err` interrompt avant toute requête. Aucun
    /// `ActiveModel` n'existe pour une suppression, seul `existing` (le `Model` relu) est prêté,
    /// en lecture seule. Déclenché sur REST et MCP seulement, **jamais sur GraphQL** (même
    /// asymétrie actée que [`before_update`](Self::before_update), amendement `resource.sdd` du
    /// 2026-09-23). Défaut : `Ok(())`, `existing` et `principal` ignorés.
    ///
    /// # Errors
    ///
    /// Un `Err(HookError)` remonté par l'override de l'application consommatrice (code applicatif
    /// libre, jamais un code `MRD-*` de la crate) — il interrompt avant toute requête `DELETE`.
    /// Le défaut ne rejette jamais.
    fn before_delete(existing: &Self::Model, principal: &AuthPrincipal) -> Result<(), HookError> {
        let _ = existing;
        let _ = principal;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{AccessPolicy, HookError, MiryadResource};
    use crate::auth::{AuthPrincipal, PrincipalSource};
    use sea_orm::ActiveValue::Set;
    use sea_orm::Iden;

    /// Entité avec propriétaire — lecture `Public`, écriture `OwnerOnly`, aucune surdéclaration
    /// de `filter_column`/`label_column`/`before_create` (défauts du trait exercés par ce
    /// fixture).
    mod recipes {
        use super::{AccessPolicy, MiryadResource};
        use sea_orm::entity::prelude::*;

        #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
        #[sea_orm(table_name = "recipes")]
        pub struct Model {
            #[sea_orm(primary_key)]
            pub id: i32,
            pub title: String,
            pub owner_id: i32,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {}

        impl ActiveModelBehavior for ActiveModel {}

        impl MiryadResource for Entity {
            fn resource_name() -> &'static str {
                "recipes"
            }
            fn read_policy() -> AccessPolicy {
                AccessPolicy::Public
            }
            fn write_policy() -> AccessPolicy {
                AccessPolicy::OwnerOnly
            }
            fn owner_column() -> Option<Column> {
                Some(Column::OwnerId)
            }
        }
    }

    /// Entité sans propriétaire — référentiel partagé `AdminOnly` dans les deux sens, sans
    /// surdéclaration de `label_column`.
    mod ingredients {
        use super::{AccessPolicy, MiryadResource};
        use sea_orm::entity::prelude::*;

        #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
        #[sea_orm(table_name = "ingredients")]
        pub struct Model {
            #[sea_orm(primary_key)]
            pub id: i32,
            pub name: String,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {}

        impl ActiveModelBehavior for ActiveModel {}

        impl MiryadResource for Entity {
            fn resource_name() -> &'static str {
                "ingredients"
            }
            fn read_policy() -> AccessPolicy {
                AccessPolicy::AdminOnly
            }
            fn write_policy() -> AccessPolicy {
                AccessPolicy::AdminOnly
            }
            fn owner_column() -> Option<Column> {
                None
            }
        }
    }

    /// Entité à politiques dissociées (`Group("editors")` en lecture, `AdminOnly` en écriture)
    /// et `before_create` surdéclaré : majuscule le libellé, rejette le libellé vide d'un
    /// `HookError` de code libre `WIDGET-001`.
    mod widgets {
        use super::{AccessPolicy, AuthPrincipal, HookError, MiryadResource, Set};
        use sea_orm::entity::prelude::*;

        #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
        #[sea_orm(table_name = "widgets")]
        pub struct Model {
            #[sea_orm(primary_key)]
            pub id: i32,
            pub label: String,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {}

        impl ActiveModelBehavior for ActiveModel {}

        impl MiryadResource for Entity {
            fn resource_name() -> &'static str {
                "widgets"
            }
            fn read_policy() -> AccessPolicy {
                AccessPolicy::Group("editors")
            }
            fn write_policy() -> AccessPolicy {
                AccessPolicy::AdminOnly
            }
            fn owner_column() -> Option<Column> {
                None
            }

            fn before_create(
                active: ActiveModel,
                _principal: &AuthPrincipal,
            ) -> Result<ActiveModel, HookError> {
                let label = match &active.label {
                    sea_orm::ActiveValue::Set(v) | sea_orm::ActiveValue::Unchanged(v) => v.clone(),
                    sea_orm::ActiveValue::NotSet => String::new(),
                };
                if label.is_empty() {
                    return Err(HookError::with_code("WIDGET-001", "label must not be empty"));
                }
                let mut active = active;
                active.label = Set(label.to_uppercase());
                Ok(active)
            }
        }
    }

    /// Entité surdéclarant `filter_column` (`category`) et `label_column` (`title`).
    mod filterables {
        use super::{AccessPolicy, MiryadResource};
        use sea_orm::entity::prelude::*;

        #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
        #[sea_orm(table_name = "filterables")]
        pub struct Model {
            #[sea_orm(primary_key)]
            pub id: i32,
            pub title: String,
            pub category: String,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {}

        impl ActiveModelBehavior for ActiveModel {}

        impl MiryadResource for Entity {
            fn resource_name() -> &'static str {
                "filterables"
            }
            fn read_policy() -> AccessPolicy {
                AccessPolicy::Public
            }
            fn write_policy() -> AccessPolicy {
                AccessPolicy::AdminOnly
            }
            fn owner_column() -> Option<Column> {
                None
            }
            fn filter_column() -> Option<Column> {
                Some(Column::Category)
            }
            fn label_column() -> Option<Column> {
                Some(Column::Title)
            }
        }
    }

    /// Déclaration invalide assumée : écriture `OwnerOnly` avec `owner_column` à `None`. Se
    /// compile — le refus effectif vit en aval (montage REST, `rbac::can_create`), jamais ici.
    mod ownerless {
        use super::{AccessPolicy, MiryadResource};
        use sea_orm::entity::prelude::*;

        #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
        #[sea_orm(table_name = "ownerless")]
        pub struct Model {
            #[sea_orm(primary_key)]
            pub id: i32,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {}

        impl ActiveModelBehavior for ActiveModel {}

        impl MiryadResource for Entity {
            fn resource_name() -> &'static str {
                "ownerless"
            }
            fn read_policy() -> AccessPolicy {
                AccessPolicy::Public
            }
            fn write_policy() -> AccessPolicy {
                AccessPolicy::OwnerOnly
            }
            fn owner_column() -> Option<Column> {
                None
            }
        }
    }

    /// Entité dont `before_update` et `before_delete` rejettent systématiquement en comptant
    /// leurs appels — espions du verrou GraphQL (sans feature, ce module est absent, et le harnais
    /// ne compte personne).
    #[cfg(feature = "graphql")]
    mod locked_pair {
        use super::{AccessPolicy, AuthPrincipal, HookError, MiryadResource};
        use sea_orm::entity::prelude::*;
        use std::sync::atomic::{AtomicUsize, Ordering};

        pub static UPDATE_CALLS: AtomicUsize = AtomicUsize::new(0);
        pub static DELETE_CALLS: AtomicUsize = AtomicUsize::new(0);

        #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
        #[sea_orm(table_name = "locked_pairs")]
        pub struct Model {
            #[sea_orm(primary_key)]
            pub id: i32,
            pub label: String,
        }

        #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
        pub enum Relation {}

        impl ActiveModelBehavior for ActiveModel {}

        impl MiryadResource for Entity {
            fn resource_name() -> &'static str {
                "locked-pair"
            }
            fn read_policy() -> AccessPolicy {
                AccessPolicy::Public
            }
            fn write_policy() -> AccessPolicy {
                AccessPolicy::AdminOnly
            }
            fn owner_column() -> Option<Column> {
                None
            }

            fn before_update(
                active: ActiveModel,
                existing: &Self::Model,
                principal: &AuthPrincipal,
            ) -> Result<ActiveModel, HookError> {
                let _ = (active, existing, principal);
                UPDATE_CALLS.fetch_add(1, Ordering::Relaxed);
                Err(HookError::new("update must never reach the GraphQL bridge"))
            }

            fn before_delete(existing: &Self::Model, principal: &AuthPrincipal) -> Result<(), HookError> {
                let _ = (existing, principal);
                DELETE_CALLS.fetch_add(1, Ordering::Relaxed);
                Err(HookError::new("delete must never reach the GraphQL bridge"))
            }
        }
    }

    fn api_principal() -> AuthPrincipal {
        AuthPrincipal {
            subject: "svc-1".to_string(),
            email: None,
            preferred_username: None,
            source: PrincipalSource::ApiToken { token_id: 42 },
        }
    }

    fn session_principal() -> AuthPrincipal {
        AuthPrincipal {
            subject: "user-1".to_string(),
            email: Some("user-1@example.test".to_string()),
            preferred_username: Some("user-1".to_string()),
            source: PrincipalSource::Session {
                id_token: "id-token".to_string(),
            },
        }
    }

    /// `Scenario` « construction de `HookError` sans code » — message fourni en `&str` puis en
    /// `String` : `code` reste `None`, `message` porte la chaîne exacte.
    #[test]
    fn hook_error_new_leaves_code_none() {
        let from_str = HookError::new("label must not be empty");
        let from_string = HookError::new("label must not be empty".to_string());
        assert!(from_str.code.is_none());
        assert!(from_string.code.is_none());
        assert_eq!(from_str.message, "label must not be empty");
        assert_eq!(from_string.message, "label must not be empty");
    }

    /// `Scenario` « code applicatif libre porté par `HookError` » — `WIDGET-001` et le message
    /// traversent intacts depuis `&str` comme depuis `String`. Le littéral de structure exhaustif
    /// prouve que le type n'expose que `code` et `message` : aucune place pour un code `MRD-*`.
    #[test]
    fn hook_error_with_code_preserves_code_and_message() {
        let from_str = HookError::with_code("WIDGET-001", "label must not be empty");
        let from_string =
            HookError::with_code("WIDGET-001".to_string(), "label must not be empty".to_string());
        assert_eq!(from_str.code.as_deref(), Some("WIDGET-001"));
        assert_eq!(from_str.message, "label must not be empty");
        assert_eq!(from_string.code.as_deref(), Some("WIDGET-001"));
        assert_eq!(from_string.message, "label must not be empty");

        let literal = HookError {
            code: Some("WIDGET-001".to_string()),
            message: "label must not be empty".to_string(),
        };
        assert_eq!(literal.code.as_deref(), Some("WIDGET-001"));
    }

    /// `Scenario` « copie et rendu `Debug` de `HookError` » — le clone conserve les deux champs
    /// (le type ne dérive pas `PartialEq`, la comparaison est champ par champ) et le `Debug`
    /// nommé porte le nom du type et les deux valeurs.
    #[test]
    fn hook_error_clone_preserves_debug_fields() {
        let original = HookError::with_code("WIDGET-001", "label must not be empty");
        let clone = original.clone();
        assert_eq!(clone.code.as_deref(), original.code.as_deref());
        assert_eq!(clone.message, original.message);

        let rendered = format!("{clone:?}");
        assert!(rendered.contains("HookError"), "{rendered}");
        assert!(rendered.contains("WIDGET-001"), "{rendered}");
        assert!(rendered.contains("label must not be empty"), "{rendered}");
    }

    /// `Scenario` « variantes unitaires d'`AccessPolicy` en chaînes JSON » — noms bruts des
    /// variantes, aucun attribut de renommage.
    #[test]
    fn access_policy_unit_variants_serialize_to_their_names() {
        assert_eq!(
            serde_json::to_string(&AccessPolicy::Public).expect("json"),
            "\"Public\""
        );
        assert_eq!(
            serde_json::to_string(&AccessPolicy::OwnerOnly).expect("json"),
            "\"OwnerOnly\""
        );
        assert_eq!(
            serde_json::to_string(&AccessPolicy::AdminOnly).expect("json"),
            "\"AdminOnly\""
        );
    }

    /// `Scenario` « `Group` sérialisé en objet JSON tagué » — représentation externe par défaut
    /// de `serde`, en clé unique.
    #[test]
    fn access_policy_group_serializes_as_single_key_object() {
        assert_eq!(
            serde_json::to_string(&AccessPolicy::Group("editors")).expect("json"),
            "{\"Group\":\"editors\"}"
        );
    }

    /// `Scenario` « politique `Copy` et comparable » — l'affectation duplique sans `clone` et la
    /// valeur reste utilisable ; chaque variante est égale à elle-même et distincte des autres,
    /// `Group` avec un autre nom aussi.
    #[test]
    fn access_policy_is_copy_and_comparable() {
        let owner = AccessPolicy::OwnerOnly;
        let copy_a = owner;
        let copy_b = owner;
        assert_eq!(copy_a, AccessPolicy::OwnerOnly);
        assert_eq!(copy_b, AccessPolicy::OwnerOnly);
        assert_eq!(owner, AccessPolicy::OwnerOnly);

        assert_ne!(owner, AccessPolicy::Public);
        assert_ne!(owner, AccessPolicy::AdminOnly);
        assert_ne!(owner, AccessPolicy::Group("editors"));
        assert_ne!(AccessPolicy::Group("editors"), AccessPolicy::Group("admins"));
        assert_eq!(
            AccessPolicy::Group("editors"),
            AccessPolicy::Group("editors"),
            "deux `Group` du même nom sont égaux"
        );
    }

    /// `Scenario` « politiques dissociées avec `Group` extrait » — lecture `Group("editors")`
    /// (nom extrait en durée `'static`), écriture `AdminOnly`, relues dans les deux ordres sans
    /// interférence.
    #[test]
    fn group_and_admin_only_policies_are_read_back_independently() {
        let write_first = widgets::Entity::write_policy();
        let read = widgets::Entity::read_policy();
        assert_eq!(write_first, AccessPolicy::AdminOnly);

        let AccessPolicy::Group(name) = read else {
            panic!("attendu une politique de lecture `Group`, obtenu {read:?}");
        };
        let name: &'static str = name;
        assert_eq!(name, "editors");

        assert_eq!(widgets::Entity::read_policy(), AccessPolicy::Group("editors"));
        assert_eq!(widgets::Entity::write_policy(), AccessPolicy::AdminOnly);
    }

    /// `Scenario` « incohérence `OwnerOnly` sans colonne propriétaire acceptée à la compilation »
    /// — le fixture `ownerless` suffit à exister pour prouver la compilation ; les valeurs relues
    /// sont exactement celles déclarées. Le refus effectif est un contrat d'aval : panic au
    /// montage REST (`rest/mod.rs`) et `false` défensif de `rbac::can_create` (`rbac.rs`) — ce
    /// fichier reste un contrat de compilation, pas d'exécution.
    #[test]
    fn owner_only_without_owner_column_still_compiles() {
        assert_eq!(ownerless::Entity::read_policy(), AccessPolicy::Public);
        assert_eq!(ownerless::Entity::write_policy(), AccessPolicy::OwnerOnly);
        assert!(ownerless::Entity::owner_column().is_none());
    }

    /// `Scenario` « `filter_column` par défaut sans surdéclaration » — `None` pour `recipes`.
    #[test]
    fn filter_column_defaults_to_none() {
        assert!(recipes::Entity::filter_column().is_none());
    }

    /// `Scenario` « `label_column` par défaut sans surdéclaration » — `None` pour `ingredients` ;
    /// le repli sur la clé primaire est une promesse du template `miryad` (doc de
    /// `label_column`, hors de ce dépôt).
    #[test]
    fn label_column_defaults_to_none() {
        assert!(ingredients::Entity::label_column().is_none());
    }

    /// `Scenario` « colonnes de filtre et de libellé surdéclarées » — chacune retourne l'`Option`
    /// exactement de la colonne déclarée, jamais une chaîne.
    #[test]
    fn filter_and_label_column_overrides_return_declared_values() {
        assert!(matches!(
            filterables::Entity::filter_column(),
            Some(filterables::Column::Category)
        ));
        assert!(matches!(
            filterables::Entity::label_column(),
            Some(filterables::Column::Title)
        ));
    }

    /// `Scenario` « `before_create` par défaut est une identité sans égard au principal » —
    /// principal de token API puis de session, `ActiveModel` à valeurs posées conservé champ par
    /// champ, sans panic ni comportement conditionnel.
    #[test]
    fn default_before_create_is_identity_on_any_principal() {
        let active = recipes::ActiveModel {
            id: Set(1),
            title: Set("beetroot soup".to_string()),
            owner_id: Set(7),
        };
        for principal in [api_principal(), session_principal()] {
            let outcome = recipes::Entity::before_create(active.clone(), &principal)
                .expect("le défaut ne rejette jamais");
            assert_eq!(outcome, active, "le défaut conserve l'ActiveModel inchangé");
        }
    }

    /// `Scenario` « mutation de l'`ActiveModel` par un override de `before_create` » — `gadget`
    /// ressort `GADGET`, l'override remplace bien l'`ActiveModel` transmis.
    #[test]
    fn before_create_override_mutates_the_active_model() {
        let active = widgets::ActiveModel {
            id: Set(1),
            label: Set("gadget".to_string()),
        };
        let outcome =
            widgets::Entity::before_create(active, &api_principal()).expect("un libellé non vide passe");
        assert!(
            matches!(&outcome.label, Set(label) if label == "GADGET"),
            "{outcome:?}"
        );
    }

    /// `Scenario` « rejet par un override de `before_create` avec code libre » — libellé vide :
    /// `Err` avec `code` `WIDGET-001` et `message` porté ; ni le code ni le message ne portent le
    /// préfixe `MRD-`.
    #[test]
    fn before_create_override_rejects_with_free_form_error() {
        let active = widgets::ActiveModel {
            id: Set(2),
            label: Set(String::new()),
        };
        let error =
            widgets::Entity::before_create(active, &api_principal()).expect_err("un libellé vide est rejeté");
        assert_eq!(error.code.as_deref(), Some("WIDGET-001"));
        assert_eq!(error.message, "label must not be empty");
        assert!(!error.code.expect("code vérifié ci-dessus").starts_with("MRD-"));
        assert!(!error.message.contains("MRD-"));
    }

    /// Lit les métadonnées par le seul bornage `MiryadResource`, sans nommer d'entité concrète —
    /// comme le font les surfaces.
    fn read_contract<E: MiryadResource>() -> (String, Option<String>) {
        (
            E::resource_name().to_string(),
            E::owner_column().map(|column| column.to_string()),
        )
    }

    /// `Scenario` « accès générique au contrat via le supertrait `EntityTrait` » — la fonction
    /// bornée ci-dessus compile et rend les valeurs déclarées, instanciée avec `recipes`.
    #[test]
    fn entity_columns_are_reachable_through_generic_bound() {
        let (name, owner_column) = read_contract::<recipes::Entity>();
        assert_eq!(name, "recipes");
        assert_eq!(owner_column.as_deref(), Some("owner_id"));
    }

    /// `Scenario` « surface disponible sans aucune feature » — test ordinaire sans `cfg`, exécuté
    /// sur chaque combinaison de la batterie de `tooling.sdd` : c'est le run
    /// `--no-default-features` de cette batterie qui prouve l'absence de gating du fichier (ce
    /// fichier ne contient aucun `#[cfg(feature = ...)]` hors fixtures marquées, elles aussi
    /// testables sur toute la matrice sans la feature concernée).
    #[test]
    fn resource_surface_available_without_default_features() {
        let error = HookError::new("app rule");
        assert!(error.code.is_none());
        assert_eq!(recipes::Entity::read_policy(), AccessPolicy::Public);
        assert_eq!(
            serde_json::to_string(&AccessPolicy::OwnerOnly).expect("json"),
            "\"OwnerOnly\""
        );
    }

    /// `Scenario` « métadonnées identiques au seuil GraphQL » — compilé sous la feature
    /// `graphql` : `PolicyRegistry` relit exactement les `AccessPolicy` déclarés et la colonne
    /// propriétaire stringifiée, sans retraduction. But du `Scenario` : l'alignement entre la clé
    /// `resource_name` et l'identifiant que `seaography` passe réellement à ses hooks n'est pas
    /// prouvé ici — bug connu `[!]` #19, différé tant que le pont reste un prototype sans
    /// consommateur (exception `Must` de `/miryad-core.sdd`).
    #[cfg(feature = "graphql")]
    #[test]
    fn graphql_policy_registry_reads_trait_metadata() {
        let mut registry = crate::graphql::PolicyRegistry::new();
        registry.register::<recipes::Entity>();

        let policy = registry.get("recipes").expect("entité enregistrée retrouvée");
        assert_eq!(policy.read, AccessPolicy::Public);
        assert_eq!(policy.write, AccessPolicy::OwnerOnly);
        assert_eq!(policy.owner_column.as_deref(), Some("owner_id"));
    }

    /// `Scenario` « `before_update` et `before_delete` ne se déclenchent jamais sur GraphQL » —
    /// limitation consignée (tâche `[ ]` de `resource.sdd`, signalé pour `[?]`) : aucune exécution
    /// de mutations `seaography` n'est harnaisable dans ce dépôt (pas de schéma de builder monté
    /// sur base en mémoire dans les tests) ; le harnais GraphQL neuf ou la preuve par lecture
    /// reste à arbitrer. Ce test verrouille la conséquence observable du mécanisme, pas les
    /// mutations exécutées : le miroir `graphql::EntityPolicy` ne porte qu'un slot de hook —
    /// `before_create` —, aucun chemin du pont ne peut joindre `before_update`/`before_delete` ;
    /// et appeler le seul point d'entrée de hook que le pont détienne sur une entité dont les
    /// deux hooks rejettent systématiquement laisse les deux espions à zéro. La lecture de la
    /// source vendue `seaography-2.0.0-rc.9` (`before_active_model_save` appelé uniquement par les
    /// mutations de création) reste la seconde preuve, consignée par la tâche.
    #[cfg(feature = "graphql")]
    #[test]
    fn graphql_mutations_skip_before_update_and_before_delete() {
        use std::sync::atomic::Ordering;

        let mut registry = crate::graphql::PolicyRegistry::new();
        registry.register::<locked_pair::Entity>();
        let policy = registry.get("locked-pair").expect("entité enregistrée retrouvée");

        let mut active = locked_pair::ActiveModel {
            id: sea_orm::ActiveValue::NotSet,
            label: sea_orm::ActiveValue::NotSet,
        };
        (policy.before_create)(&mut active, &api_principal())
            .expect("le défaut d'identité de before_create passe");

        assert_eq!(
            locked_pair::UPDATE_CALLS.load(Ordering::Relaxed),
            0,
            "before_update jamais appelé par le pont GraphQL"
        );
        assert_eq!(
            locked_pair::DELETE_CALLS.load(Ordering::Relaxed),
            0,
            "before_delete jamais appelé par le pont GraphQL"
        );
    }
}
