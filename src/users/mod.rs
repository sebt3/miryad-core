//! Gestion utilisateurs/groupes — résolution, synchronisation OIDC et comptes de service.

/// Groupes (`miryad_groups`) : entité `SeaORM`, `ensure_group` en get-or-create par nom exact et
/// lectures d'appartenance `is_admin`/`is_member`.
pub mod group;

/// Table d'association `miryad_group_memberships` et sa réconciliation exclusive depuis le
/// claim groupes de l'`OIDC` — aucun chemin d'écriture manuel.
pub mod membership;

/// Provisionnement idempotent des comptes de service : ligne utilisateur, appartenances de
/// groupes et secret `Bearer` fourni par l'appelant.
pub mod service_account;

/// Entité `SeaORM` `miryad_users` et get-or-create `resolve_user` : lien du claim `sub` de
/// l'`OIDC` vers un `id` interne `i32` stable.
pub mod user;

pub use group::{ADMIN_GROUP_NAME, Group, is_admin, is_member};
pub use membership::{GroupMembership, sync_group_memberships};
pub use service_account::ensure_service_account;
pub use user::{User, resolve_user};

#[cfg(test)]
mod tests {
    // Le cinquième `Scenario` de `./mod.sdd` (« le module compile et expose sa surface sans
    // aucune feature activée ») n'a pas de test unitaire ici, conformément à sa note et à la
    // tâche « Convertir » : sa preuve est la combinaison `--no-default-features` de la batterie
    // de `/tooling.sdd` (lib et tests du module users compilés sans `static-frontend`,
    // `swagger-ui`, `graphql`, `graphiql` ni `mcp`), l'exercice de features débordant du
    // fichier. Les quatre autres `Scenario` ont chacun un test nommé distinct ci-dessous.

    /// `Scenario` : « les neuf chemins plats résolvent sans passer par un enfant ».
    /// Compilation des neufs chemins par le nom plat (`crate::users::…`, celui du consommateur —
    /// la cible unitaire ne résout pas le nom propre de la crate, cf. le test jumeau de
    /// `../workflow/mod.rs`) plus lecture runtime de la constante. Aucun des chemins ci-dessous
    /// ne nomme `group::`, `membership::`, `service_account::` ou `user::`.
    #[test]
    fn nine_flat_paths_resolve_and_admin_constant_reads_admin() {
        // Les trois alias plats en position de type, sans instanciation (mêmes raisons de
        // harnais que le test de `../workflow/mod.rs` : pas de liaison `_…` sous pedantic).
        fn witness_alias_types(
            _: Option<crate::users::Group>,
            _: Option<crate::users::User>,
            _: Option<crate::users::GroupMembership>,
        ) {
        }
        // Les cinq fonctions plates référencées en valeur, instanciées sur la connexion concrète
        // de `sea_orm`, jamais appelées — la résolution du chemin plat suffit à preuve.
        std::hint::black_box((
            crate::users::is_admin::<sea_orm::DatabaseConnection>,
            crate::users::is_member::<sea_orm::DatabaseConnection>,
            crate::users::resolve_user::<sea_orm::DatabaseConnection>,
            crate::users::sync_group_memberships::<sea_orm::DatabaseConnection>,
            crate::users::ensure_service_account,
        ));
        witness_alias_types(None, None, None);
        assert_eq!(crate::users::ADMIN_GROUP_NAME, "admin");
    }

    /// `Scenario` : « les trois alias plats sont les Entity enfants eux-mêmes ». La définition
    /// même des trois fonctions identité (paramètre plat, retour enfant — ou l'inverse par
    /// l'annotation) ne compile que si l'alias et l'`Entity` sont le même type, pas deux types
    /// jumeaux. La seconde partie verrouille `is_admin`/`is_member` utilisables par le chemin
    /// plat avec une clause `C: ConnectionTrait` au point d'appel.
    #[test]
    fn flat_aliases_are_the_child_entities_themselves() {
        fn identity_group(alias: crate::users::Group) -> crate::users::group::Entity {
            alias
        }
        fn identity_user(alias: crate::users::User) -> crate::users::user::Entity {
            alias
        }
        fn identity_membership(alias: crate::users::GroupMembership) -> crate::users::membership::Entity {
            alias
        }
        // Point d'appel générique `C: ConnectionTrait` sur les chemins plats — compilation
        // seule, les futurs construits ne sont jamais pollués.
        fn call_site_with_connection_clause<C: sea_orm::ConnectionTrait>(
            db: &C,
        ) -> impl std::future::Future<Output = (bool, bool)> {
            let admin = crate::users::is_admin(db, 1);
            let member = crate::users::is_member(db, 1, crate::users::ADMIN_GROUP_NAME);
            async move { (admin.await.unwrap_or_default(), member.await.unwrap_or_default()) }
        }
        std::hint::black_box((identity_group, identity_user, identity_membership));
        std::hint::black_box(call_site_with_connection_clause::<sea_orm::DatabaseConnection>);
    }

    /// `Scenario` : « modules et items à plat coexistent dans un même list d'imports ». Les
    /// treize noms (quatre modules, neuf items) tiennent dans un seul `use` sans collision ;
    /// `user::resolve_user` et `resolve_user` sont le même item, prouvé en type : deux `fn item`
    /// distincts n'unifient jamais un même paramètre générique `T`.
    #[test]
    fn modules_and_flat_items_coexist_in_one_use_list() {
        use crate::users::{
            ADMIN_GROUP_NAME, Group, GroupMembership, User, ensure_service_account, group, is_admin,
            is_member, membership, resolve_user, service_account, sync_group_memberships, user,
        };

        // Chaque nom importé est consommé — les modules par leurs enfants nommés dans les
        // signatures, les items par position de type ou en valeur.
        fn witness_modules(
            _: Option<Group>,
            _: Option<User>,
            _: Option<GroupMembership>,
            _: Option<user::Model>,
            _: Option<group::Model>,
            _: Option<membership::Model>,
        ) {
        }
        fn same_item_proof<T>(_both_paths: (T, T)) {}
        witness_modules(None, None, None, None, None, None);
        std::hint::black_box((
            ADMIN_GROUP_NAME,
            is_admin::<sea_orm::DatabaseConnection>,
            is_member::<sea_orm::DatabaseConnection>,
            sync_group_memberships::<sea_orm::DatabaseConnection>,
        ));
        same_item_proof((
            resolve_user::<sea_orm::DatabaseConnection>,
            user::resolve_user::<sea_orm::DatabaseConnection>,
        ));
        same_item_proof((ensure_service_account, service_account::ensure_service_account));
    }

    /// `Scenario` : « les chemins enfants restent l'autre voie valable pour le non-mis-à-plat ».
    /// Les six noms non aplatis (`group::ensure_group` et les items générés de `user` et
    /// `membership`) résolvent par le chemin de l'enfant — compilation seule. L'absence de ces
    /// six noms sous le chemin plat est vérifiée par relecture des quatre `pub use` (pas de
    /// glob, neuf items exactement), cf. `./mod.sdd` : aucun outil de compile-failure n'existe
    /// dans le dépôt.
    #[test]
    fn child_paths_keep_the_non_flattened_surface_reachable() {
        fn ensure_group_via_child_path<C: sea_orm::ConnectionTrait>(
            db: &C,
            name: &str,
        ) -> impl std::future::Future<Output = Result<i32, sea_orm::DbErr>> {
            crate::users::group::ensure_group(db, name)
        }
        fn witness_generated_child_items(
            _: Option<crate::users::user::Model>,
            _: crate::users::user::Column,
            _: crate::users::user::ActiveModel,
            _: crate::users::membership::Column,
            _: crate::users::membership::ActiveModel,
        ) {
        }
        std::hint::black_box(ensure_group_via_child_path::<sea_orm::DatabaseConnection>);
        witness_generated_child_items(
            None,
            crate::users::user::Column::Subject,
            crate::users::user::ActiveModel::default(),
            crate::users::membership::Column::UserId,
            crate::users::membership::ActiveModel::default(),
        );
    }
}
