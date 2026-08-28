//! Gestion utilisateurs/groupes — résolution, synchronisation OIDC et comptes de service.

pub mod group;
pub mod membership;
pub mod service_account;
pub mod user;

pub use group::{ADMIN_GROUP_NAME, Group, is_admin, is_member};
pub use membership::{GroupMembership, sync_group_memberships};
pub use service_account::ensure_service_account;
pub use user::{User, resolve_user};
