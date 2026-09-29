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
