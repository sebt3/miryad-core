//! Identité de l'appelant et droits (`src/lymaille/identity.sdd`).
//!
//! La session ne persiste que le `subject` de son utilisateur : à **chaque appel** d'un module
//! de l'hôte, le principal est reconstruit depuis la table des utilisateurs — appartenance au
//! groupe admin à jour, jamais mise en cache (DSD `AC-5`). La nature de la session
//! (`Work`/`Config`) n'ajoute ni n'ôte aucun droit. Porte aussi la classification des
//! identifiants (`G_` = catalogue, `H_` = résolu par l'hôte, DSD `DEC-9`, `DEC-27`) et le contrôle
//! d'accord de modules d'une
//! définition (DSD `FR-9`).

use sea_orm::entity::prelude::*;

use crate::lymaille::store::db_text;
use crate::users::{group, user};

use super::error::LymailleError;

/// Principal d'un appel, reconstruit depuis la base à chaque appel de module (DSD `DEC-3`) :
/// valeur d'un instant, jamais persisté ni mis en cache entre deux appels — la session ne garde
/// que le `subject`.
#[derive(Clone, Debug)]
pub struct CallerPrincipal {
    /// `miryad_users.id` du compte local de l'appelant — propriétaire des lignes privées.
    pub user_id: i32,
    /// Claim `sub` de la session, verbatim.
    pub subject: String,
    /// Appartenance au groupe `admin` lue à cet instant — un retrait entre deux appels est
    /// effectif au second (DSD `AC-5`).
    pub is_admin: bool,
}

/// Porte de reconstruction du principal : un exemplaire par montage de l'état, sans autre état
/// que la connexion.
#[derive(Clone)]
pub struct Identity {
    db: DatabaseConnection,
}

impl Identity {
    /// Attache la porte d'identité à une connexion.
    #[must_use]
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }

    /// Reconstruit le principal du `subject` : lecture directe de `miryad_users` par `subject`
    /// (**sans création de compte**, à la différence de `resolve_user`) puis `is_admin` frais.
    ///
    /// # Errors
    ///
    /// - [`LymailleError::UnknownSubject`] : aucune ligne `miryad_users` pour ce `subject` —
    ///   jamais un principal vide ni par défaut (DSD `DEC-3`).
    /// - [`LymailleError::Store`] : `DbErr` d'une des deux lectures, texte expurgé.
    pub async fn resolve(&self, subject: &str) -> Result<CallerPrincipal, LymailleError> {
        let account = user::Entity::find()
            .filter(user::Column::Subject.eq(subject))
            .one(&self.db)
            .await
            .map_err(|err| LymailleError::Store(db_text(&err)))?
            .ok_or(LymailleError::UnknownSubject)?;
        let is_admin = group::is_admin(&self.db, account.id)
            .await
            .map_err(|err| LymailleError::Store(db_text(&err)))?;
        Ok(CallerPrincipal {
            user_id: account.id,
            subject: account.subject,
            is_admin,
        })
    }
}

/// Nature d'un identifiant de définition, tranchée par le préfixe seul (DSD `DEC-9`, `DEC-27`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdKind {
    /// Préfixe `G_` exact : le catalogue, commun à tous, en écriture réservé à l'admin.
    Catalog,
    /// Tout autre identifiant valide : le privé de son propriétaire.
    Private,
    /// Préfixe `H_` exact : un contexte résolu par l'hôte (`LymailleHost::resolve_context`,
    /// DSD `DEC-27`), jamais stocké en table, donc jamais écrit.
    Host,
}

/// Classe un identifiant (DSD `DEC-9`, `DEC-27`) : un identifiant est un identifiant Rhai valide.
///
/// # Errors
///
/// - [`LymailleError::InvalidDefinition`] : identifiant vide ou portant un caractère hors
///   `[A-Za-z0-9_]`.
/// - [`LymailleError::ReservedPrefix`] : préfixe `g_` ou `h_` sous une casse autre que `G_` ou
///   `H_` exacte — la casse est significative, `G_` seul désigne le catalogue, `H_` seul le
///   résolu par l'hôte.
pub fn classify(id: &str) -> Result<IdKind, LymailleError> {
    if id.is_empty()
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return Err(LymailleError::InvalidDefinition("invalid identifier".to_string()));
    }
    if id.starts_with("G_") {
        return Ok(IdKind::Catalog);
    }
    if id.starts_with("H_") {
        return Ok(IdKind::Host);
    }
    if id
        .get(..2)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("g_") || prefix.eq_ignore_ascii_case("h_"))
    {
        return Err(LymailleError::ReservedPrefix(id.to_string()));
    }
    Ok(IdKind::Private)
}

/// Contrôle d'écriture d'une définition : `Catalog` exige un admin, `Private` est accepté pour
/// tout appelant, `Host` est toujours refusé (DSD `DEC-27`) — aucune surface n'écrit un
/// identifiant `H_…`, admin compris.
///
/// # Errors
///
/// - [`LymailleError::Forbidden`] : écriture au catalogue par un non-admin, ou tentative
///   d'écriture d'un identifiant hôte.
/// - Les erreurs de [`classify`] (identifiant invalide ou préfixe réservé).
pub fn check_write(caller: &CallerPrincipal, id: &str) -> Result<IdKind, LymailleError> {
    match classify(id)? {
        IdKind::Host => Err(LymailleError::Forbidden(
            "host identifiers are not writable".to_string(),
        )),
        IdKind::Catalog if !caller.is_admin => Err(LymailleError::Forbidden(
            "catalog write requires admin".to_string(),
        )),
        kind => Ok(kind),
    }
}

/// Contrôle d'accord de modules d'une définition : une définition ne peut accorder que des
/// modules que son propriétaire pourrait utiliser lui-même (DSD `FR-9`).
///
/// # Errors
///
/// [`LymailleError::Forbidden`] : un module de `granted` figure dans `admin_only` et l'appelant
/// n'est pas admin.
pub fn check_grants(
    caller: &CallerPrincipal,
    granted: &[String],
    admin_only: &[&str],
) -> Result<(), LymailleError> {
    if caller.is_admin {
        return Ok(());
    }
    for module in granted {
        if admin_only.contains(&module.as_str()) {
            return Err(LymailleError::Forbidden(format!(
                "module reserved to admins: {module}"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migration::Migrator;
    use crate::users::membership::{self, sync_group_memberships};
    use crate::users::{resolve_user, user};
    use sea_orm::EntityTrait;
    use sea_orm_migration::MigratorTrait;

    async fn test_db() -> DatabaseConnection {
        let db = sea_orm::Database::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite connects");
        Migrator::up(&db, None).await.expect("migrations apply cleanly");
        db
    }

    /// `Scenario` : « un subject sans compte est refusé » — `Err(UnknownSubject)` et aucune
    /// ligne `miryad_users` créée (à la différence de `resolve_user`, get-or-create).
    #[tokio::test]
    async fn resolve_unknown_subject_is_refused_and_creates_no_row() {
        let db = test_db().await;
        let identity = Identity::new(db.clone());

        let result = identity.resolve("ghost").await;

        assert!(
            matches!(result, Err(LymailleError::UnknownSubject)),
            "`resolve` d'un subject sans compte doit refuser par `UnknownSubject`, jamais un \
             principal par défaut : {result:?}"
        );
        let rows = user::Entity::find()
            .all(&db)
            .await
            .expect("relecture de miryad_users");
        assert!(
            rows.is_empty(),
            "`resolve` ne crée aucune ligne `miryad_users` : {rows:?}"
        );
    }

    /// `Scenario` : « le droit admin est relu à chaque appel » (DSD `AC-5`) — un principal
    /// n'est jamais mis en cache : retirée du groupe admin entre deux appels, `alice` est
    /// non-admin au second.
    #[tokio::test]
    async fn resolve_re_reads_admin_right_on_every_call() {
        let db = test_db().await;
        let alice = resolve_user(&db, "alice", None)
            .await
            .expect("compte local d'alice");
        sync_group_memberships(&db, alice.id, &["admin".to_string()])
            .await
            .expect("alice membre du groupe admin seedé");
        let identity = Identity::new(db.clone());

        let first = identity.resolve("alice").await.expect("premier resolve d'alice");
        assert_eq!(first.user_id, alice.id, "`user_id` est l'id interne du compte");
        assert_eq!(first.subject, "alice", "`subject` est celui recherché");
        assert!(first.is_admin, "`alice` est membre du groupe admin");

        // Retrait du groupe admin : suppression de l'appartenance (fixture de test, le
        // chemin de production des appartenances est `sync_group_memberships`).
        membership::Entity::delete_many()
            .filter(membership::Column::UserId.eq(alice.id))
            .exec(&db)
            .await
            .expect("retrait des appartenances d'alice");

        let second = identity.resolve("alice").await.expect("second resolve d'alice");
        assert!(
            !second.is_admin,
            "un principal n'est jamais mis en cache : le droit admin est \
            relu à chaque appel"
        );
    }

    /// `Scenario` : « la classification des identifiants » — table complète verrouillée,
    /// amendement R5 (DSD `DEC-27`) : `H_sandbox` est `Host`, `h_sandbox` un préfixe réservé.
    #[test]
    fn classify_follows_the_classification_table() {
        let outcomes: Vec<String> = [
            "G_triage",
            "g_triage",
            "H_sandbox",
            "h_sandbox",
            "G-x",
            "triage",
            "mon_agent",
            "",
            "a b",
        ]
        .iter()
        .map(|id| match classify(id) {
            Ok(IdKind::Catalog) => "Catalog".to_string(),
            Ok(IdKind::Private) => "Private".to_string(),
            Ok(IdKind::Host) => "Host".to_string(),
            Err(LymailleError::ReservedPrefix(payload)) => {
                assert_eq!(
                    payload, *id,
                    "`ReservedPrefix` porte l'identifiant, jamais un secret"
                );
                "ReservedPrefix".to_string()
            }
            Err(LymailleError::InvalidDefinition(payload)) => {
                assert_eq!(payload, "invalid identifier", "`Must` règle 1");
                "InvalidDefinition".to_string()
            }
            Err(other) => format!("Unexpected({other})"),
        })
        .collect();
        assert_eq!(
            outcomes,
            [
                "Catalog",
                "ReservedPrefix",
                "Host",
                "ReservedPrefix",
                "InvalidDefinition",
                "Private",
                "Private",
                "InvalidDefinition",
                "InvalidDefinition"
            ],
            "table du Scenario « la classification des identifiants »"
        );
    }

    /// `Scenario` (réalignement R5, DSD `DEC-27`) : « un identifiant hôte n'est écrit par
    /// personne » — `H_…` est refusé par `check_write` admin compris, et `h_…` relève de
    /// `ReservedPrefix` avant toute décision d'écriture.
    #[test]
    fn check_write_refuses_host_identifiers_for_everyone() {
        let admin = CallerPrincipal {
            user_id: 1,
            subject: "admin".to_string(),
            is_admin: true,
        };
        let non_admin = CallerPrincipal {
            user_id: 2,
            subject: "alice".to_string(),
            is_admin: false,
        };

        for caller in [&admin, &non_admin] {
            let denied = check_write(caller, "H_sandbox");
            assert!(
                matches!(&denied, Err(LymailleError::Forbidden(message))
                    if message == "host identifiers are not writable"),
                "`H_sandbox` n'est écrit par personne (admin compris), ici {:?} : {denied:?}",
                caller.is_admin
            );
            let lowercase = check_write(caller, "h_sandbox");
            assert!(
                matches!(&lowercase, Err(LymailleError::ReservedPrefix(payload))
                    if payload == "h_sandbox"),
                "`h_sandbox` est un préfixe réservé avant toute décision d'écriture, \
                 ici {:?} : {lowercase:?}",
                caller.is_admin
            );
        }
    }

    /// `Scenario` : « seul un admin écrit au catalogue ».
    #[test]
    fn check_write_catalog_requires_admin() {
        let admin = CallerPrincipal {
            user_id: 1,
            subject: "admin".to_string(),
            is_admin: true,
        };
        let non_admin = CallerPrincipal {
            user_id: 2,
            subject: "alice".to_string(),
            is_admin: false,
        };

        let denied = check_write(&non_admin, "G_x");
        assert!(
            matches!(&denied, Err(LymailleError::Forbidden(message))
                if message == "catalog write requires admin"),
            "`G_x` exige un admin : {denied:?}"
        );

        let private = check_write(&non_admin, "x");
        assert!(
            matches!(private, Ok(IdKind::Private)),
            "un privé est accepté pour tout appelant : {private:?}"
        );

        let catalog = check_write(&admin, "G_x");
        assert!(
            matches!(catalog, Ok(IdKind::Catalog)),
            "`G_x` est accordé à un admin : {catalog:?}"
        );
    }

    /// `Scenario` : « un module réservé à l'admin n'est pas accordé à un non-admin » (DSD
    /// `FR-9`).
    #[test]
    fn check_grants_reserved_module_requires_admin() {
        let non_admin = CallerPrincipal {
            user_id: 2,
            subject: "alice".to_string(),
            is_admin: false,
        };
        let admin_only = ["secrets"];

        let granted_reserved = check_grants(
            &non_admin,
            &["resources".to_string(), "secrets".to_string()],
            &admin_only,
        );
        assert!(
            matches!(&granted_reserved, Err(LymailleError::Forbidden(message))
                if message == "module reserved to admins: secrets"),
            "`secrets` réservé à l'admin refusé à un non-admin : {granted_reserved:?}"
        );

        let granted_plain = check_grants(&non_admin, &["resources".to_string()], &admin_only);
        assert!(
            matches!(granted_plain, Ok(())),
            "aucun module réservé accordé : {granted_plain:?}"
        );
    }
}
