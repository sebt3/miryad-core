//! Provisionnement idempotent des comptes de service : ligne utilisateur, appartenances de
//! groupes et secret `Bearer` fourni par l'appelant — pensé pour l'automatisation de
//! déploiement, rejouable à chaque démarrage sans dupliquer ni compte, ni groupes, ni token.

use sea_orm::prelude::DateTimeUtc;
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};

use crate::auth::AuthError;
use crate::auth::ensure_token;
use crate::users::membership::sync_group_memberships;
use crate::users::user;
use crate::users::user::resolve_user;

/// Garantit l'existence d'un compte de service (jamais de login OIDC — pensé pour
/// l'automatisation de déploiement, ex. kuberest) et d'un token le référençant, avec une valeur
/// de token **fournie par l'appelant** (pas générée aléatoirement comme `issue_token`) —
/// typiquement lue d'une variable d'environnement, pour que l'automatisation de déploiement
/// connaisse le secret à l'avance sans devoir le récupérer après coup.
///
/// # Espace des `subject` — à la charge de l'appelant (arbitré 2026-09-29)
///
/// La crate ne garde ni charset, ni préfixe, ni namespace : `subject`, `token` et `token_name`
/// sont consommés verbatim (aucun trim, aucun contrôle de format, de longueur, de préfixe
/// `mrd_` ou de casse). Le préfixe `system:` est recommandé pour séparer les comptes machines
/// des `sub` OIDC humains. Risque documenté, non gardé : une collision entre un `subject` de
/// service et un `sub` OIDC humain fusionne les deux sur la même ligne (contrainte `UNIQUE` de
/// `miryad_users.subject`) ; à rouvrir en cas de multi-IdP — un marqueur de type de compte
/// passerait par une migration, jamais par un filtre ici.
///
/// `pepper` : poivre HMAC des empreintes (cf. `auth::token`) — en pratique
/// `MiryadAuthState::token_pepper`, transmis à `ensure_token` (arbitrage 2026-09-27).
///
/// Pensée pour être appelée par l'app cible à son démarrage, après ses migrations, uniquement si
/// elle le décide. Idempotent : rejouable à chaque démarrage sans dupliquer ni le compte, ni ses
/// appartenances de groupe, ni le token. La création du compte émet une trace `tracing` `info`
/// (subject et groupes, jamais le secret ni son empreinte) ; le rejeu idempotent reste muet
/// (arbitré 2026-09-29).
///
/// # Errors
///
/// `AuthError` (arbitré 2026-09-29) — aucun code `MRD-*` inventé ici, la fonction ne fait que
/// propager :
/// - tout `sea_orm::DbErr` des trois étapes (lecture d'audit, `resolve_user`,
///   `sync_group_memberships`, `ensure_token`) passe par le `From` et ressort
///   `AuthError::Database` (`MRD-AUTH-016`) ;
/// - `AuthError::TokenHashConflict` (`MRD-AUTH-017`) : l'empreinte du secret est déjà posée
///   sous un autre `subject` — remontée telle quelle, plus d'aplatissement en `DbErr::Custom` ;
/// - `AuthError::Internal` (`MRD-AUTH-018`) : refus du poivre par `hash_token`, chemin
///   infaillible en pratique, propagé par `ensure_token`.
pub async fn ensure_service_account(
    db: &DatabaseConnection,
    subject: &str,
    token: &str,
    token_name: &str,
    groups: &[String],
    expires_at: Option<DateTimeUtc>,
    pepper: &str,
) -> Result<(), AuthError> {
    // Sonde d'audit (trace à la création, arbitré 2026-09-29) : lecture préalable pure qui sert
    // uniquement à savoir si cet appel crée le compte — le get-or-create reste la prérogative
    // de `resolve_user`, rien n'est ici réimplémenté. Sous course de deux premières répliques,
    // la création peut être tracée deux fois : le contrat porte sur les écritures (qui
    // convergent), pas sur la course de la trace.
    let existing = user::Entity::find()
        .filter(user::Column::Subject.eq(subject))
        .one(db)
        .await?;

    let account = resolve_user(db, subject, None).await?;
    if existing.is_none() {
        tracing::info!(subject, groups = ?groups, "service account created");
    }
    sync_group_memberships(db, account.id, groups).await?;
    ensure_token(db, subject, token_name, token, expires_at, pepper).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::token::{Column as TokenColumn, Entity as TokenEntity};
    use crate::auth::validate_token;
    use crate::migration::Migrator;
    use crate::users::group;
    use crate::users::group::{is_admin, is_member};
    use crate::users::membership::trace_capture::capture_traces;
    use crate::users::membership::{Column as MembershipColumn, Entity as MembershipEntity};
    use crate::users::user::{Column as UserColumn, Entity as UserEntity};
    use chrono::Utc;
    use sea_orm::entity::prelude::*;
    use sea_orm::{ColumnTrait, DbBackend, QueryFilter, Statement};
    use sea_orm_migration::MigratorTrait;

    const PEPPER: &str = "test-pepper";

    async fn test_db() -> DatabaseConnection {
        let db = sea_orm::Database::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite connects");
        Migrator::up(&db, None).await.expect("migrations apply cleanly");
        db
    }

    /// Horodatage à la seconde entière (patron de `auth/token.rs`) — évite les troncatures de
    /// précision au round-trip `SQLite` quand on compare un `expires_at` relus.
    fn whole_second(offset: i64) -> DateTimeUtc {
        DateTimeUtc::from_timestamp(Utc::now().timestamp() + offset, 0).expect("valid timestamp")
    }

    async fn token_rows(db: &DatabaseConnection, subject: &str) -> Vec<crate::auth::token::Model> {
        TokenEntity::find()
            .filter(TokenColumn::Subject.eq(subject))
            .all(db)
            .await
            .expect("query succeeds")
    }

    async fn membership_count(db: &DatabaseConnection, user_id: i32) -> usize {
        MembershipEntity::find()
            .filter(MembershipColumn::UserId.eq(user_id))
            .all(db)
            .await
            .expect("query succeeds")
            .len()
    }

    /// `Scenario` : « Premier provisionnement : l'utilisateur, les appartenances et le token d'un
    /// seul appel ».
    #[tokio::test]
    async fn creates_account_groups_and_token() {
        let db = test_db().await;
        ensure_service_account(
            &db,
            "system:kuberest",
            "mrd_bootstrap-secret",
            "bootstrap",
            &["admin".to_string()],
            None,
            PEPPER,
        )
        .await
        .expect("provisioning succeeds");

        let principal = validate_token(&db, "mrd_bootstrap-secret", PEPPER)
            .await
            .expect("token authenticates");
        assert_eq!(principal.subject, "system:kuberest");

        let user = resolve_user(&db, "system:kuberest", None)
            .await
            .expect("resolve succeeds");
        assert!(is_admin(&db, user.id).await.expect("query succeeds"));
    }

    /// `Scenario` : « Rejeu identique : zéro écart entre deux démarrages » — étendu (tâche
    /// « Convertir ») : `@id` et `created_at` de l'utilisateur inchangés, exactement une ligne de
    /// token avec son `created_at` d'origine.
    #[tokio::test]
    async fn is_idempotent_across_restarts() {
        let db = test_db().await;
        ensure_service_account(
            &db,
            "system:kuberest",
            "mrd_bootstrap-secret",
            "bootstrap",
            &["admin".to_string()],
            None,
            PEPPER,
        )
        .await
        .expect("first provisioning succeeds");
        let user_before = resolve_user(&db, "system:kuberest", None)
            .await
            .expect("resolve succeeds");
        let tokens_before = token_rows(&db, "system:kuberest").await;
        assert_eq!(
            tokens_before.len(),
            1,
            "une ligne de token après le premier appel"
        );

        ensure_service_account(
            &db,
            "system:kuberest",
            "mrd_bootstrap-secret",
            "bootstrap",
            &["admin".to_string()],
            None,
            PEPPER,
        )
        .await
        .expect("second provisioning succeeds");

        let user_after = resolve_user(&db, "system:kuberest", None)
            .await
            .expect("resolve succeeds");
        assert_eq!(
            user_after.id, user_before.id,
            "même ligne `miryad_users` au rejeu"
        );
        assert_eq!(
            user_after.created_at, user_before.created_at,
            "`created_at` de l'utilisateur inchangé au rejeu"
        );

        let tokens_after = token_rows(&db, "system:kuberest").await;
        assert_eq!(
            tokens_after.len(),
            1,
            "exactement une ligne de token par empreinte"
        );
        assert_eq!(
            tokens_after[0].created_at, tokens_before[0].created_at,
            "le rejeu ne rafraîchit jamais `created_at`"
        );

        assert_eq!(
            membership_count(&db, user_after.id).await,
            1,
            "une seule appartenance, aucun doublon de `admin`"
        );
    }

    /// `Scenario` : « Rotation de secret sur le même `subject` : ajout sans révocation » — étendu
    /// (tâche « Convertir ») : aucune ligne de token supprimée par ce provisionnement.
    #[tokio::test]
    async fn rotating_the_secret_adds_a_new_token_without_removing_the_old_one() {
        let db = test_db().await;
        ensure_service_account(
            &db,
            "system:kuberest",
            "mrd_first-secret",
            "bootstrap",
            &[],
            None,
            PEPPER,
        )
        .await
        .expect("first provisioning succeeds");
        ensure_service_account(
            &db,
            "system:kuberest",
            "mrd_second-secret",
            "bootstrap",
            &[],
            None,
            PEPPER,
        )
        .await
        .expect("second provisioning succeeds");

        assert!(validate_token(&db, "mrd_first-secret", PEPPER).await.is_ok());
        assert!(validate_token(&db, "mrd_second-secret", PEPPER).await.is_ok());
        assert_eq!(
            token_rows(&db, "system:kuberest").await.len(),
            2,
            "rotation = ajout pur : l'ancienne ligne de token n'est pas supprimée"
        );
    }

    /// `Scenario` : « Rejeu d'une valeur expirée : elle ne peut pas être ranimée ».
    #[tokio::test]
    async fn rejeu_of_lapsed_value_cannot_revive_it() {
        let db = test_db().await;
        let past = whole_second(-86_400);
        ensure_service_account(
            &db,
            "system:lapsed",
            "mrd_lapsed-secret",
            "bootstrap",
            &[],
            Some(past),
            PEPPER,
        )
        .await
        .expect("premier provisionnement Ok");

        ensure_service_account(
            &db,
            "system:lapsed",
            "mrd_lapsed-secret",
            "bootstrap",
            &[],
            None,
            PEPPER,
        )
        .await
        .expect("rejeu sans expiration rendu Ok par le no-op global sur l'empreinte");

        let result = validate_token(&db, "mrd_lapsed-secret", PEPPER).await;
        assert!(
            matches!(result, Err(AuthError::TokenExpired)),
            "la date d'origine survit au rejeu — `expires_at` n'est jamais rafraîchi : {result:?}"
        );
    }

    /// `Scenario` : « Secret partagé entre deux `subject` : le second n'obtient pas le secret ».
    /// Le `Then` « `Ok(())` est rendu » de la spec est antérieur à l'arbitrage 2026-09-27
    /// d'`ensure_token` (rejet explicite `MRD-AUTH-017`, cf. `auth/token.sdd`) — c'est le
    /// `Scenario` « empreinte déjà détenue… » de la présente spec, postérieur et arbitré, qui
    /// chiffre le rendu : `Err(AuthError::TokenHashConflict)`. Les `But` du scénario (ligne
    /// d'utilisateur et appartenance de `svc-b` posées avant l'échec du token, secret resté à
    /// `svc-a`) sont intouchés et verrouillés ici. Écart de texte consigné au rapport B2a.
    #[tokio::test]
    async fn shared_secret_under_second_subject_authenticates_first() {
        let db = test_db().await;
        ensure_service_account(&db, "svc-a", "mrd_shared-secret", "bootstrap", &[], None, PEPPER)
            .await
            .expect("provisionnement de svc-a Ok");

        let result = ensure_service_account(
            &db,
            "svc-b",
            "mrd_shared-secret",
            "bootstrap",
            &["ops".to_string()],
            None,
            PEPPER,
        )
        .await;
        assert!(
            matches!(result, Err(AuthError::TokenHashConflict)),
            "l'empreinte est une ressource déjà allouée : rejet explicite, pas de no-op silencieux : {result:?}"
        );

        assert!(
            token_rows(&db, "svc-b").await.is_empty(),
            "aucune ligne de token ne porte l'empreinte de X avec subject svc-b"
        );
        let principal = validate_token(&db, "mrd_shared-secret", PEPPER)
            .await
            .expect("le secret reste authentifiable");
        assert_eq!(
            principal.subject, "svc-a",
            "le secret reste la propriété du premier titulaire"
        );

        let svc_b = resolve_user(&db, "svc-b", None)
            .await
            .expect("la ligne miryad_users de svc-b a bien été créée");
        assert!(
            is_member(&db, svc_b.id, "ops").await.expect("query succeeds"),
            "les deux premières étapes ont précédé le rejet du token : l'appartenance ops est appliquée"
        );
    }

    /// `Scenario` : « Groupes en réconciliation : une liste plus courte que la dernière fois
    /// retire ».
    #[tokio::test]
    async fn shorter_group_list_provisioning_removes_stale_membership() {
        let db = test_db().await;
        ensure_service_account(
            &db,
            "system:reconcile",
            "mrd_reconcile-secret",
            "bootstrap",
            &["admin".to_string(), "ops".to_string()],
            None,
            PEPPER,
        )
        .await
        .expect("provisionnement complet Ok");

        ensure_service_account(
            &db,
            "system:reconcile",
            "mrd_reconcile-secret",
            "bootstrap",
            &["ops".to_string()],
            None,
            PEPPER,
        )
        .await
        .expect("rejeu avec liste plus courte Ok");

        let user = resolve_user(&db, "system:reconcile", None)
            .await
            .expect("resolve succeeds");
        assert!(
            !is_admin(&db, user.id).await.expect("query succeeds"),
            "`admin` a été retiré par la réconciliation"
        );
        assert!(is_member(&db, user.id, "ops").await.expect("query succeeds"));
    }

    /// `Scenario` : « Liste vide : les appartenances s'effacent, le secret survit ».
    #[tokio::test]
    async fn empty_group_list_clears_memberships_but_keeps_the_secret() {
        let db = test_db().await;
        ensure_service_account(
            &db,
            "system:clear",
            "mrd_clear-secret",
            "bootstrap",
            &["ops".to_string()],
            None,
            PEPPER,
        )
        .await
        .expect("provisionnement avec ops Ok");

        ensure_service_account(
            &db,
            "system:clear",
            "mrd_clear-secret",
            "bootstrap",
            &[],
            None,
            PEPPER,
        )
        .await
        .expect("rejeu à liste vide Ok");

        let user = resolve_user(&db, "system:clear", None)
            .await
            .expect("resolve succeeds");
        assert_eq!(
            membership_count(&db, user.id).await,
            0,
            "les appartenances se sont effacées"
        );

        let principal = validate_token(&db, "mrd_clear-secret", PEPPER)
            .await
            .expect("vider les groupes ne touche pas au token");
        assert_eq!(principal.subject, "system:clear");
    }

    /// `Scenario` : « Groupe inconnu de la liste : créé à la volée par le provisionnement ».
    #[tokio::test]
    async fn unknown_group_in_list_becomes_a_membership_through_provisioning() {
        let db = test_db().await;
        ensure_service_account(
            &db,
            "system:creates",
            "mrd_create-group-secret",
            "bootstrap",
            &["ops".to_string()],
            None,
            PEPPER,
        )
        .await
        .expect("provisionnement Ok");

        let user = resolve_user(&db, "system:creates", None)
            .await
            .expect("resolve succeeds");
        assert!(is_member(&db, user.id, "ops").await.expect("query succeeds"));
        assert!(
            group::Entity::find()
                .filter(group::Column::Name.eq("ops"))
                .one(&db)
                .await
                .expect("query succeeds")
                .is_some(),
            "`ensure_group` a posé la ligne de groupe, sans registre préalable des noms"
        );
    }

    /// `Scenario` : « Ligne machine née sans email ni nom d'affichage ».
    #[tokio::test]
    async fn provisioned_user_row_has_no_email_nor_display_name() {
        let db = test_db().await;
        ensure_service_account(
            &db,
            "system:deploy",
            "mrd_deploy-secret",
            "bootstrap",
            &[],
            None,
            PEPPER,
        )
        .await
        .expect("provisionnement Ok");

        let row = UserEntity::find()
            .filter(UserColumn::Subject.eq("system:deploy"))
            .one(&db)
            .await
            .expect("query succeeds")
            .expect("row exists");
        assert!(
            row.email.is_none(),
            "le `None` passé à `resolve_user` est la seule source d'email de ce chemin"
        );
        assert!(row.display_name.is_none());

        ensure_service_account(
            &db,
            "system:deploy",
            "mrd_deploy-secret",
            "bootstrap",
            &[],
            None,
            PEPPER,
        )
        .await
        .expect("rejeu Ok");
        let row_after = UserEntity::find()
            .filter(UserColumn::Subject.eq("system:deploy"))
            .one(&db)
            .await
            .expect("query succeeds")
            .expect("row exists");
        assert!(
            row_after.email.is_none() && row_after.display_name.is_none(),
            "un rejeu ne remplit pas ces deux champs"
        );
    }

    /// `Scenario` : « `subject` et `token_name` vides acceptés verbatim, sans garde de format »
    /// — verrouille aussi la tâche « Espace des `subject` à la charge de l'appelant » : la crate
    /// ne normalise rien.
    #[tokio::test]
    async fn empty_subject_and_name_are_provisioned_verbatim() {
        let db = test_db().await;
        ensure_service_account(&db, "", "court", "", &[], None, PEPPER)
            .await
            .expect("vide et hors format sont des entrées contractuelles, telles quelles");

        let principal = validate_token(&db, "court", PEPPER)
            .await
            .expect("token authenticates");
        assert_eq!(principal.subject, "");

        let rows = TokenEntity::find().all(&db).await.expect("query succeeds");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "", "`token_name` vide écrit verbatim dans `name`");
    }

    /// Verrou de non-normalisation du chantier « espace des `subject` » (arbitré 2026-09-29) :
    /// casse et blancs préservés, aucun équivalent normalisé créé en parallèle.
    #[tokio::test]
    async fn subject_is_consumed_verbatim_without_normalization() {
        let db = test_db().await;
        let subject = " SysTème:Mixte ";
        ensure_service_account(
            &db,
            subject,
            "mrd_verbatim-secret",
            "My Token Name",
            &["Ops".to_string()],
            None,
            PEPPER,
        )
        .await
        .expect("provisionnement Ok");

        let row = UserEntity::find()
            .filter(UserColumn::Subject.eq(subject))
            .one(&db)
            .await
            .expect("query succeeds")
            .expect("row exists");
        assert_eq!(
            row.subject, subject,
            "aucun trim, aucune casse, aucun préfixe imposé — l'espace est à la charge de l'appelant"
        );
        assert_eq!(
            UserEntity::find().all(&db).await.expect("query succeeds").len(),
            1,
            "aucune variante normalisée du subject n'a été créée"
        );

        let principal = validate_token(&db, "mrd_verbatim-secret", PEPPER)
            .await
            .expect("token authenticates");
        assert_eq!(principal.subject, subject);
        assert!(
            is_member(&db, row.id, "Ops").await.expect("query succeeds"),
            "les noms de groupes non blancs traversent intacts"
        );
    }

    /// `Scenario` : « Expiration déjà passée : provisionnement l'accepte, usage la refuse ».
    #[tokio::test]
    async fn past_expiry_secret_is_provisioned_then_denied() {
        let db = test_db().await;
        let past = whole_second(-86_400);
        ensure_service_account(
            &db,
            "system:backdated",
            "mrd_backdated-secret",
            "bootstrap",
            &[],
            Some(past),
            PEPPER,
        )
        .await
        .expect("provisionnement avec date passée acceptée à l'écriture");

        let row = TokenEntity::find()
            .filter(TokenColumn::Subject.eq("system:backdated"))
            .one(&db)
            .await
            .expect("query succeeds")
            .expect("row exists");
        assert_eq!(
            row.expires_at,
            Some(past),
            "la date passée est stockée verbatim, jamais redressée"
        );

        let result = validate_token(&db, "mrd_backdated-secret", PEPPER).await;
        assert!(
            matches!(result, Err(AuthError::TokenExpired)),
            "la garde d'expiration vit dans `validate_token`, jamais à l'écriture : {result:?}"
        );
    }

    /// `Scenario` : « Base non migrée : erreur à la première requête, rien n'est créé » — rendu
    /// sous la signature arbitrée 2026-09-29 : le `DbErr` nu de la première requête passe par le
    /// `From` et ressort `AuthError::Database` (`MRD-AUTH-016`), les étapes groupes et token ne
    /// sont jamais atteintes.
    #[tokio::test]
    async fn missing_tables_fail_before_any_write() {
        let db = sea_orm::Database::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite connects (base non migrée)");

        let result = ensure_service_account(
            &db,
            "system:none",
            "mrd_missing-tables-secret",
            "bootstrap",
            &["ops".to_string()],
            None,
            PEPPER,
        )
        .await;
        let err = result.expect_err("la base sans tables fait échouer la première requête");
        assert!(
            matches!(err, AuthError::Database(_)),
            "faute de base, pas faute du client : {err:?}"
        );
        assert!(
            err.to_string().starts_with("MRD-AUTH-016: database error: "),
            "le `?` emballe le DbErr nu dans le wrapper Database (littéral possédé par `auth/error.sdd`) : {err}"
        );

        let tables = db
            .query_all_raw(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT name FROM sqlite_master WHERE type = 'table' AND name LIKE 'miryad_%'".to_string(),
            ))
            .await
            .expect("sqlite_master reads");
        assert!(
            tables.is_empty(),
            "aucun appel n'a créé de table, et le secret n'a été écrit nulle part"
        );
    }

    /// `Scenario` : « empreinte déjà détenue par un autre sujet remonte en
    /// `AuthError::TokenHashConflict` » (arbitré 2026-09-29, tâche « Sortie en `AuthError` ») —
    /// testable de bout en bout depuis que la signature rend `Result<(), AuthError>` : plus
    /// d'aplatissement en `DbErr::Custom`. Étendu (tâche B2b) au `Scenario` amendé « Secret
    /// partagé entre deux `subject` : rejection en `MRD-AUTH-017`, écritures antérieures non
    /// rollées » : l'`Err` ne revient pas les étapes antérieures (pas de transaction
    /// d'entourage, seule la réconciliation de `./membership.rs` est transactionnelle) — la
    /// ligne `miryad_users` du second `subject`, le groupe et l'appartenance créés avant le
    /// rejet du token survivent. Comportement gelé, verrouillé ici comme au test
    /// `shared_secret_under_second_subject_authenticates_first`.
    #[tokio::test]
    async fn provisioning_existing_hash_under_other_subject_yields_token_hash_conflict() {
        let db = test_db().await;
        ensure_service_account(
            &db,
            "subject-a",
            "mrd_taken-secret",
            "bootstrap",
            &[],
            None,
            PEPPER,
        )
        .await
        .expect("premier provisionnement Ok");

        let result = ensure_service_account(
            &db,
            "subject-b",
            "mrd_taken-secret",
            "bootstrap",
            &["ops".to_string()],
            None,
            PEPPER,
        )
        .await;
        assert!(
            matches!(result, Err(AuthError::TokenHashConflict)),
            "MRD-AUTH-017 sort tel quel, non aplati en chaîne : {result:?}"
        );
        assert_eq!(
            result.expect_err("une Err").to_string(),
            "MRD-AUTH-017: token hash already provisioned for another subject",
            "le littéral @Display de `auth/error.sdd` traverse intact"
        );

        // `Scenario` amendé « écritures antérieures non rollées » : les deux premières étapes
        // (`resolve_user`, `sync_group_memberships`) ont précédé le rejet du token.
        let subject_b = UserEntity::find()
            .filter(UserColumn::Subject.eq("subject-b"))
            .one(&db)
            .await
            .expect("query succeeds")
            .expect("la ligne miryad_users de subject-b a bien été créée avant le rejet");
        assert!(
            group::Entity::find()
                .filter(group::Column::Name.eq("ops"))
                .one(&db)
                .await
                .expect("query succeeds")
                .is_some(),
            "le groupe `ops` créé par la réconciliation survit à l'`Err` du token"
        );
        assert!(
            is_member(&db, subject_b.id, "ops").await.expect("query succeeds"),
            "l'appartenance `ops` appliquée avant le rejet n'est pas rollée : \
             `ensure_service_account` n'enveloppe pas ses étapes d'une transaction d'entourage"
        );
    }

    /// `Scenario` « création d'un compte machine trace un `info` sans secret » (tâche « Trace
    /// d'audit », arbitré 2026-09-29) : une seule trace `info` à la création, portant le `subject`
    /// et les groupes, jamais le secret ni son empreinte ; le rejeu ne retrace pas la création.
    #[tokio::test]
    async fn creation_emits_info_audit_trace_replay_silent() {
        use hmac::{Hmac, Mac};
        use sha2::Sha256;

        fn hmac_hex(secret: &str, pepper: &str) -> String {
            let mut mac =
                Hmac::<Sha256>::new_from_slice(pepper.as_bytes()).expect("HMAC accepts any key length");
            mac.update(secret.as_bytes());
            hex::encode(mac.finalize().into_bytes())
        }

        let db = test_db().await;
        let secret = "mrd_audit-secret";
        let hash = hmac_hex(secret, PEPPER);

        let lines = {
            let (_guard, traces) = capture_traces();
            ensure_service_account(
                &db,
                "system:audited",
                secret,
                "bootstrap",
                &["admin".to_string()],
                None,
                PEPPER,
            )
            .await
            .expect("provisioning succeeds");
            traces.lines()
        };
        let infos: Vec<&String> = lines
            .iter()
            .filter(|(level, _)| *level == tracing::Level::INFO)
            .map(|(_, line)| line)
            .collect();
        assert_eq!(
            infos.len(),
            1,
            "`Must` (arbitré 2026-09-29) : une trace `info` à la création, une seule : {lines:?}"
        );
        let line = infos.first().expect("une ligne `info`");
        assert!(
            line.contains("system:audited"),
            "la trace porte le `subject` : {line}"
        );
        assert!(
            line.contains("admin"),
            "la trace porte la liste des groupes : {line}"
        );
        assert!(!line.contains(secret), "jamais le secret : {line}");
        assert!(!line.contains(&hash), "jamais l'empreinte du secret : {line}");

        let replay_lines = {
            let (_guard, traces) = capture_traces();
            ensure_service_account(
                &db,
                "system:audited",
                secret,
                "bootstrap",
                &["admin".to_string()],
                None,
                PEPPER,
            )
            .await
            .expect("replay succeeds");
            traces.lines()
        };
        assert!(
            !replay_lines
                .iter()
                .any(|(_, line)| line.contains("system:audited")),
            "`Must` : le rejeu idempotent reste muet sur la création : {replay_lines:?}"
        );
    }
}
