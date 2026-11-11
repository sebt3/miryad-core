//! Activation self-service des définitions du propriétaire (`src/lymaille/interactive/activation.sdd`).
//!
//! La session de configuration **propose**, l'hôte **exécute** (`DEC-8`, `FR-9`, `AC-8`) : cette
//! fonction promeut le brouillon d'un toolset en version `active` sur demande explicite de
//! l'utilisateur (route REST ou commande Matrix), jamais par un appel de l'agent `Config`.
//! `check_grants` est fait **ici**, état admin relu en base ; la publication au catalogue reste
//! réservée aux admins.

use sea_orm::sea_query::{Alias, Expr, ExprTrait, Query, UpdateStatement};
use sea_orm::{ConnectionTrait, DatabaseConnection, TransactionTrait};

use ::lymaille::{DraftKind, DraftStore, Toolset};

use crate::lymaille::definitions::toolset::check_toolset;
use crate::lymaille::error::LymailleError;
use crate::lymaille::identity::{
    ADMIN_ONLY_MODULES, CallerPrincipal, IdKind, Identity, check_grants, classify,
};
use crate::lymaille::store::SeaDraftStore;
use crate::lymaille::store::db_text;

/// Tables de la promotion (../migration.sdd, colonnes de ./activation.sdd).
const PRIVATE_TOOLSETS_TABLE: &str = "miryad_lymaille_toolsets";
const CATALOG_TOOLSETS_TABLE: &str = "miryad_lymaille_catalog_toolsets";

/// Texte unique du refus de publication au catalogue (`Must` de la spec).
const CATALOG_NEEDS_ADMIN: &str = "catalog publication requires admin";

/// Résultat d'une activation : la définition promue et ses versions avant/après.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActivationReport {
    /// Identifiant de la définition activée (privé de l'appelant ou `G_…`).
    pub name: String,
    /// Version devenue active (dérivée du brouillon).
    pub version: String,
    /// Version active précédente, `None` si aucune n'était en service.
    pub previous_version: Option<String>,
}

/// Promeut le brouillon du toolset `name` de l'appelant en version active
/// (`Scenario` « le propriétaire active son brouillon »).
///
/// # Errors
///
/// - `LymailleError::Forbidden` : `G_…` par un non-admin, identifiant `H_…`, ou module réservé
///   accordé par un non-admin (`check_grants`).
/// - `LymailleError::NotFound` : aucune ligne de l'appelant, ou ligne sans brouillon.
/// - `LymailleError::InvalidDefinition` : brouillon illisible ou invalide (`check_toolset`).
/// - `LymailleError::Store` : panne de la base, transaction annulée.
pub async fn activate_toolset(
    db: &DatabaseConnection,
    caller: &CallerPrincipal,
    name: &str,
) -> Result<ActivationReport, LymailleError> {
    activate_with(db, caller, name, ADMIN_ONLY_MODULES, false).await
}

/// Jette le brouillon `kind` nommé `name` (`Must` de la spec) : réutilise `SeaDraftStore`, même
/// table, mêmes contrôles que `DraftStore::delete_draft` (`../store/draft.sdd`) — pas de logique
/// de jet dupliquée ici.
///
/// # Errors
///
/// - `LymailleError::Store` : panne de la base, ou refus des stores (identifiant `H_…`, catalogue
///   sans admin), rendu avec son code `MRD-*`.
pub async fn discard_draft(
    db: &DatabaseConnection,
    caller: &CallerPrincipal,
    kind: DraftKind,
    name: &str,
) -> Result<bool, LymailleError> {
    let store = SeaDraftStore::new(db.clone(), Identity::new(db.clone()), caller.subject.clone());
    DraftStore::delete_draft(&store, kind, name)
        .await
        .map_err(from_toolsets)
}

/// Cœur de `activate_toolset` avec la liste de modules réservés et un point de panne injectés —
/// séparateurs des `Scenario` « module réservé » et « atomicité » ; la fonction publique passe la
/// liste vide et `false`.
async fn activate_with(
    db: &DatabaseConnection,
    caller: &CallerPrincipal,
    name: &str,
    reserved: &[&str],
    fail_after_active: bool,
) -> Result<ActivationReport, LymailleError> {
    // Étape 1 : le préfixe tranche la table (`../identity.sdd`) ; `H_…` n'est pas activable,
    // `G_…` exige un admin sous le texte de la spec (un admin l'active comme équivalent de son
    // écriture de `active` par l'entité).
    let kind = classify(name)?;
    let table = match kind {
        IdKind::Host => {
            return Err(LymailleError::Forbidden(
                "host identifiers are not activable".to_string(),
            ));
        }
        IdKind::Catalog if !caller.is_admin => {
            return Err(LymailleError::Forbidden(CATALOG_NEEDS_ADMIN.to_string()));
        }
        IdKind::Catalog => CATALOG_TOOLSETS_TABLE,
        IdKind::Private => PRIVATE_TOOLSETS_TABLE,
    };

    // Étape 2 : la ligne de l'appelant ; pas de ligne ou pas de brouillon → même `NotFound`.
    let row = read_row(db, table, kind, caller, name).await?;
    let Some((draft, version)) = row else {
        return Err(LymailleError::NotFound("no draft to activate".to_string()));
    };
    let Some(draft) = draft else {
        return Err(LymailleError::NotFound("no draft to activate".to_string()));
    };

    // Étape 3 : mêmes contrôles que `put_draft`, puis `check_grants` sur les modules que le
    // brouillon accorde — refus nommant le module, jamais le source.
    let toolset: Toolset = serde_json::from_value(draft.clone()).map_err(|_cause| {
        // La cause serde est abandonnée : le texte d'un toolset porte son source.
        LymailleError::InvalidDefinition(format!("stored toolset {name} is unreadable"))
    })?;
    check_toolset(&toolset).map_err(from_toolsets)?;
    let modules: Vec<String> = draft
        .get("modules")
        .and_then(serde_json::Value::as_array)
        .map_or_else(Vec::new, |entries| {
            entries
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_owned)
                .collect()
        });
    check_grants(caller, &modules, reserved)?;

    // Version dérivée du brouillon ; la colonne `version` rend la précédente (`DEC-17`).
    let version_new = draft
        .get("version")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            LymailleError::InvalidDefinition(format!("draft of toolset {name} carries no version"))
        })?
        .to_owned();
    let previous_version = if version.is_empty() { None } else { Some(version) };

    // Étape 4 : une transaction — `active ← draft`, `version` dérivée, puis `draft ← NULL`. Une
    // panne entre les deux écritures annule la transaction : la ligne garde son état initial.
    let transaction = db
        .begin()
        .await
        .map_err(|erreur| LymailleError::Store(db_text(&erreur)))?;
    update_promote(&transaction, table, kind, caller, name, &draft, &version_new).await?;
    if fail_after_active {
        // Point de panne du `Scenario` « atomicité » : la transaction est abandonnée sans
        // commit, la ligne reste dans son état initial. Jamais atteint en production.
        return Err(LymailleError::Store(
            "simulated failure after promotion".to_string(),
        ));
    }
    clear_draft(&transaction, table, kind, caller, name).await?;
    transaction
        .commit()
        .await
        .map_err(|erreur| LymailleError::Store(db_text(&erreur)))?;
    Ok(ActivationReport {
        name: name.to_owned(),
        version: version_new,
        previous_version,
    })
}

/// Conditions de la ligne ciblée : `toolset_id`, plus `owner_id` pour le privé (jamais la ligne
/// d'un autre).
fn conditions(kind: IdKind, caller: &CallerPrincipal, name: &str) -> Vec<(&'static str, Expr)> {
    let mut conditions = vec![("toolset_id", Expr::val(name))];
    if kind == IdKind::Private {
        conditions.push(("owner_id", Expr::val(caller.user_id)));
    }
    conditions
}

/// Lecture de la ligne : `(draft, version)`, `None` si aucune ligne.
async fn read_row(
    db: &DatabaseConnection,
    table: &str,
    kind: IdKind,
    caller: &CallerPrincipal,
    name: &str,
) -> Result<Option<(Option<serde_json::Value>, String)>, LymailleError> {
    let mut select = Query::select();
    select
        .columns([Alias::new("draft"), Alias::new("version")])
        .from(Alias::new(table));
    for (column, value) in conditions(kind, caller, name) {
        select.and_where(Expr::col(Alias::new(column)).eq(value));
    }
    let row = db
        .query_one(&select)
        .await
        .map_err(|erreur| LymailleError::Store(db_text(&erreur)))?;
    row.map(|row| {
        let draft = row
            .try_get::<Option<serde_json::Value>>("", "draft")
            .map_err(|erreur| LymailleError::Store(db_text(&erreur)))?;
        let version = row
            .try_get::<String>("", "version")
            .map_err(|erreur| LymailleError::Store(db_text(&erreur)))?;
        Ok((draft, version))
    })
    .transpose()
}

/// Première écriture de la promotion : `active ← draft`, `version` dérivée. Garde : la ligne doit
/// encore porter exactement le brouillon relu — une promotion concurrente fait échouer la
/// transaction plutôt que promouvoir un brouillon non contrôlé.
async fn update_promote(
    transaction: &sea_orm::DatabaseTransaction,
    table: &str,
    kind: IdKind,
    caller: &CallerPrincipal,
    name: &str,
    draft: &serde_json::Value,
    version_new: &str,
) -> Result<(), LymailleError> {
    let mut update = Query::update();
    update
        .table(Alias::new(table))
        .value(Alias::new("active"), Expr::val(draft.clone()))
        .value(Alias::new("version"), Expr::val(version_new))
        .and_where(Expr::col(Alias::new("draft")).eq(draft.clone()));
    apply_conditions(&mut update, kind, caller, name);
    let affected = transaction
        .execute(&update)
        .await
        .map_err(|erreur| LymailleError::Store(db_text(&erreur)))?
        .rows_affected();
    if affected == 0 {
        return Err(LymailleError::Store(format!(
            "toolset {name} changed while activating"
        )));
    }
    Ok(())
}

/// Seconde écriture : `draft ← NULL`, garde `draft IS NOT NULL` — la ligne ne peut pas avoir
/// perdu son brouillon en cours de transaction.
async fn clear_draft(
    transaction: &sea_orm::DatabaseTransaction,
    table: &str,
    kind: IdKind,
    caller: &CallerPrincipal,
    name: &str,
) -> Result<(), LymailleError> {
    let mut update = Query::update();
    update
        .table(Alias::new(table))
        .value(Alias::new("draft"), Expr::val(None::<serde_json::Value>))
        .and_where(Expr::col(Alias::new("draft")).is_not_null());
    apply_conditions(&mut update, kind, caller, name);
    let affected = transaction
        .execute(&update)
        .await
        .map_err(|erreur| LymailleError::Store(db_text(&erreur)))?
        .rows_affected();
    if affected == 0 {
        return Err(LymailleError::Store(format!(
            "draft of toolset {name} vanished mid-transaction"
        )));
    }
    Ok(())
}

/// Applique les conditions de clé à une `UPDATE`.
fn apply_conditions(update: &mut UpdateStatement, kind: IdKind, caller: &CallerPrincipal, name: &str) {
    for (column, value) in conditions(kind, caller, name) {
        update.and_where(Expr::col(Alias::new(column)).eq(value));
    }
}

/// Traduction des erreurs de `lymaille` (`ToolsetsError`) : une panne de stockage est `Store`,
/// tout refus de contrôle est `InvalidDefinition` — le texte ne cite jamais le source (les
/// détails de `ToolsetsError` sont des phrases fixes).
fn from_toolsets(error: ::lymaille::ToolsetsError) -> LymailleError {
    match error {
        ::lymaille::ToolsetsError::Store(text) => LymailleError::Store(text),
        other => LymailleError::InvalidDefinition(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lymaille::store::test_support::test_db;
    use crate::users::resolve_user;
    use serde_json::json;
    use vynil_core::Semver;

    /// Base neuve migrée (SQLite mémoire, ou PostgreSQL si `MIRYAD_TEST_PG_URL`).
    async fn db_test() -> DatabaseConnection {
        test_db().await
    }

    /// Compte local de test et principal non-admin correspondant.
    async fn compte(db: &DatabaseConnection, subject: &str) -> CallerPrincipal {
        let account = resolve_user(db, subject, None)
            .await
            .expect("compte local de test cree");
        CallerPrincipal {
            user_id: account.id,
            subject: subject.to_owned(),
            is_admin: false,
        }
    }

    /// Compte local et principal administrateur déclaré.
    async fn compte_admin(db: &DatabaseConnection, subject: &str) -> CallerPrincipal {
        let mut caller = compte(db, subject).await;
        caller.is_admin = true;
        caller
    }

    /// Forme sérialisée d'un toolset nommé `name` en version `version`.
    fn toolset_json(name: &str, version: &str) -> serde_json::Value {
        let toolset = Toolset::new(
            name.to_owned(),
            format!("the toolset {name}"),
            Semver::parse(version).expect("la version de fixture se parse"),
            "fn ping() { 1 }".to_owned(),
        )
        .expect("le toolset de fixture se construit");
        serde_json::to_value(toolset).expect("le toolset de fixture se sérialise")
    }

    /// Insertion brute d'une ligne de toolsets (la table privée n2019a pas d2019entit00e9 `SeaORM`).
    async fn inserer_ligne(
        db: &DatabaseConnection,
        table: &str,
        owner_id: Option<i32>,
        name: &str,
        version: &str,
        active: Option<serde_json::Value>,
        draft: Option<serde_json::Value>,
    ) {
        let mut insert = Query::insert();
        insert.into_table(Alias::new(table));
        let mut columns: Vec<&str> = Vec::new();
        let mut values: Vec<Expr> = Vec::new();
        if let Some(owner_id) = owner_id {
            columns.push("owner_id");
            values.push(Expr::val(owner_id));
        }
        columns.extend(["toolset_id", "version", "active", "draft", "created_at"]);
        values.push(Expr::val(name));
        values.push(Expr::val(version));
        values.push(Expr::val(active));
        values.push(Expr::val(draft));
        values.push(Expr::val(chrono::Utc::now()));
        insert
            .columns(columns.iter().copied().map(Alias::new))
            .values(values)
            .expect("les colonnes et valeurs de fixture correspondent");
        db.execute(&insert).await.expect("ligne de toolset inseree");
    }

    /// Relecture brute `(version, active, draft)` d'une ligne.
    async fn ligne(
        db: &DatabaseConnection,
        table: &str,
        owner_id: Option<i32>,
        name: &str,
    ) -> (String, Option<serde_json::Value>, Option<serde_json::Value>) {
        let mut select = Query::select();
        select
            .columns([Alias::new("version"), Alias::new("active"), Alias::new("draft")])
            .from(Alias::new(table));
        if let Some(owner_id) = owner_id {
            select.and_where(Expr::col(Alias::new("owner_id")).eq(owner_id));
        }
        select.and_where(Expr::col(Alias::new("toolset_id")).eq(name));
        let row = db
            .query_one(&select)
            .await
            .expect("relecture de fixture")
            .expect("la ligne existe");
        (
            row.try_get::<String>("", "version").expect("version lue"),
            row.try_get::<Option<serde_json::Value>>("", "active")
                .expect("active lue"),
            row.try_get::<Option<serde_json::Value>>("", "draft")
                .expect("draft lu"),
        )
    }

    /// Scenario « le propriétaire active son brouillon » : `active` = v2, `draft` nul,
    /// `previous_version = v1`.
    #[tokio::test(flavor = "multi_thread")]
    async fn le_proprietaire_active_son_brouillon() {
        let db = db_test().await;
        let alice = compte(&db, "alice").await;
        let v1 = toolset_json("mon_toolset", "1.0.0");
        let v2 = toolset_json("mon_toolset", "2.0.0");
        inserer_ligne(
            &db,
            PRIVATE_TOOLSETS_TABLE,
            Some(alice.user_id),
            "mon_toolset",
            "1.0.0",
            Some(v1),
            Some(v2.clone()),
        )
        .await;

        let report = activate_toolset(&db, &alice, "mon_toolset")
            .await
            .expect("le proprietaire active son brouillon");
        assert_eq!(report.name, "mon_toolset");
        assert_eq!(report.version, "2.0.0", "la version derivee du brouillon");
        assert_eq!(
            report.previous_version.as_deref(),
            Some("1.0.0"),
            "la version active precedente est signalee"
        );
        let (version, active, draft) =
            ligne(&db, PRIVATE_TOOLSETS_TABLE, Some(alice.user_id), "mon_toolset").await;
        assert_eq!(version, "2.0.0", "la colonne version suit l'active");
        assert_eq!(active.as_ref(), Some(&v2), "active est le brouillon promu");
        assert!(draft.is_none(), "le brouillon est remis a NULL");
    }

    /// Scenario « un module réservé à l'admin bloque l'activation d'un non-admin » : `Forbidden`
    /// nommant le module, ligne inchangée. La liste réservée est le séparateur de test — la liste
    /// de production est vide.
    #[tokio::test(flavor = "multi_thread")]
    async fn un_module_reserve_bloque_lactivation_dun_non_admin() {
        let db = db_test().await;
        let alice = compte(&db, "alice").await;
        let v1 = toolset_json("mon_toolset", "1.0.0");
        let mut brouillon = toolset_json("mon_toolset", "2.0.0");
        brouillon["modules"] = json!(["admin_only"]);
        inserer_ligne(
            &db,
            PRIVATE_TOOLSETS_TABLE,
            Some(alice.user_id),
            "mon_toolset",
            "1.0.0",
            Some(v1.clone()),
            Some(brouillon.clone()),
        )
        .await;

        let refus = activate_with(&db, &alice, "mon_toolset", &["admin_only"], false)
            .await
            .expect_err("un module reserve bloque un non-admin");
        match &refus {
            LymailleError::Forbidden(message) => {
                assert!(
                    message.contains("admin_only"),
                    "le module reserve est nomme : {message}"
                );
                assert!(
                    !message.contains("fn ping"),
                    "le source du toolset n'est jamais cite : {message}"
                );
            }
            autre => panic!("`Forbidden` attendue, obtenu : {autre:?}"),
        }
        let (version, active, draft) =
            ligne(&db, PRIVATE_TOOLSETS_TABLE, Some(alice.user_id), "mon_toolset").await;
        assert_eq!(version, "1.0.0", "la ligne est inchangee");
        assert_eq!(active.as_ref(), Some(&v1), "l'active n'a pas bouge");
        assert_eq!(draft.as_ref(), Some(&brouillon), "le brouillon est conserve");
    }

    /// Scenario « pas de brouillon » : `NotFound("no draft to activate")`.
    #[tokio::test(flavor = "multi_thread")]
    async fn pas_de_brouillon() {
        let db = db_test().await;
        let alice = compte(&db, "alice").await;
        let v1 = toolset_json("mon_toolset", "1.0.0");
        inserer_ligne(
            &db,
            PRIVATE_TOOLSETS_TABLE,
            Some(alice.user_id),
            "mon_toolset",
            "1.0.0",
            Some(v1),
            None,
        )
        .await;

        let refus = activate_toolset(&db, &alice, "mon_toolset")
            .await
            .expect_err("sans brouillon, rien a activer");
        match &refus {
            LymailleError::NotFound(message) => assert_eq!(message, "no draft to activate"),
            autre => panic!("`NotFound` attendue, obtenu : {autre:?}"),
        }
    }

    /// Scenario « on n'active pas le toolset d'un autre » : `bob` → `NotFound`, la ligne
    /// d'`alice` intacte.
    #[tokio::test(flavor = "multi_thread")]
    async fn on_nactive_pas_le_toolset_dun_autre() {
        let db = db_test().await;
        let alice = compte(&db, "alice").await;
        let bob = compte(&db, "bob").await;
        let v1 = toolset_json("mon_toolset", "1.0.0");
        let v2 = toolset_json("mon_toolset", "2.0.0");
        inserer_ligne(
            &db,
            PRIVATE_TOOLSETS_TABLE,
            Some(alice.user_id),
            "mon_toolset",
            "1.0.0",
            Some(v1.clone()),
            Some(v2),
        )
        .await;

        let refus = activate_toolset(&db, &bob, "mon_toolset")
            .await
            .expect_err("bob n'a pas de ligne sous ce nom");
        assert!(
            matches!(&refus, LymailleError::NotFound(message) if message == "no draft to activate"),
            "la ligne d'autrui est introuvable pour bob : {refus:?}"
        );
        let (_version, active, draft) =
            ligne(&db, PRIVATE_TOOLSETS_TABLE, Some(alice.user_id), "mon_toolset").await;
        assert_eq!(active.as_ref(), Some(&v1), "la ligne d'alice est inchangee");
        assert!(draft.is_some(), "le brouillon d'alice est conserve");
    }

    /// Scenario « le catalogue exige un admin » : `Forbidden` pour un non-admin, succès pour un
    /// admin (équivalent de l'écriture de `active` par l'entité).
    #[tokio::test(flavor = "multi_thread")]
    async fn le_catalogue_exige_un_admin() {
        let db = db_test().await;
        let bob = compte(&db, "bob").await;
        let admin = compte_admin(&db, "root").await;
        let v1 = toolset_json("G_git", "1.0.0");
        let v2 = toolset_json("G_git", "2.0.0");
        inserer_ligne(
            &db,
            CATALOG_TOOLSETS_TABLE,
            None,
            "G_git",
            "1.0.0",
            Some(v1),
            Some(v2),
        )
        .await;

        let refus = activate_toolset(&db, &bob, "G_git")
            .await
            .expect_err("un non-admin ne publie pas au catalogue");
        match &refus {
            LymailleError::Forbidden(message) => {
                assert_eq!(message, CATALOG_NEEDS_ADMIN);
            }
            autre => panic!("`Forbidden` attendue, obtenu : {autre:?}"),
        }
        let report = activate_toolset(&db, &admin, "G_git")
            .await
            .expect("l'admin active l'equivalent d'une ecriture de l'entite");
        assert_eq!(report.version, "2.0.0");
        assert_eq!(report.previous_version.as_deref(), Some("1.0.0"));
    }

    /// Scenario « atomicité » : panne simulée entre les deux écritures, la ligne est restée dans
    /// son état initial (transaction abandonnée).
    #[tokio::test(flavor = "multi_thread")]
    async fn atomicite() {
        let db = db_test().await;
        let alice = compte(&db, "alice").await;
        let v1 = toolset_json("mon_toolset", "1.0.0");
        let v2 = toolset_json("mon_toolset", "2.0.0");
        inserer_ligne(
            &db,
            PRIVATE_TOOLSETS_TABLE,
            Some(alice.user_id),
            "mon_toolset",
            "1.0.0",
            Some(v1.clone()),
            Some(v2.clone()),
        )
        .await;

        let panne = activate_with(&db, &alice, "mon_toolset", &[], true)
            .await
            .expect_err("la panne simpee interrompt la promotion");
        assert!(
            matches!(panne, LymailleError::Store(_)),
            "la panne est une erreur de transaction : {panne:?}"
        );
        let (version, active, draft) =
            ligne(&db, PRIVATE_TOOLSETS_TABLE, Some(alice.user_id), "mon_toolset").await;
        assert_eq!(version, "1.0.0", "la ligne est restee dans son etat initial");
        assert_eq!(active.as_ref(), Some(&v1), "l'active n'a pas bouge");
        assert_eq!(draft.as_ref(), Some(&v2), "le brouillon est conserve");
    }

    /// `discard_draft` jette le brouillon (vrai), puis rend faux quand il n'y en a plus ; la
    /// version active est ménagée (`../store/draft.sdd`).
    #[tokio::test(flavor = "multi_thread")]
    async fn discard_jette_le_brouillon() {
        let db = db_test().await;
        let alice = compte(&db, "alice").await;
        let v1 = toolset_json("mon_toolset", "1.0.0");
        let v2 = toolset_json("mon_toolset", "2.0.0");
        inserer_ligne(
            &db,
            PRIVATE_TOOLSETS_TABLE,
            Some(alice.user_id),
            "mon_toolset",
            "1.0.0",
            Some(v1.clone()),
            Some(v2),
        )
        .await;

        let jete = discard_draft(&db, &alice, DraftKind::Toolset, "mon_toolset")
            .await
            .expect("le brouillon se jette");
        assert!(jete, "un brouillon a ete supprime");
        let (_version, active, draft) =
            ligne(&db, PRIVATE_TOOLSETS_TABLE, Some(alice.user_id), "mon_toolset").await;
        assert!(draft.is_none(), "le brouillon a disparu");
        assert_eq!(active.as_ref(), Some(&v1), "l'active est menagee");

        let second = discard_draft(&db, &alice, DraftKind::Toolset, "mon_toolset")
            .await
            .expect("le second jet ne panique pas");
        assert!(!second, "plus rien a jeter rend faux");
    }

    /// Un identifiant `H_…` n'est pas activable : `Forbidden`, aucune table lue.
    #[tokio::test(flavor = "multi_thread")]
    async fn un_identifiant_hote_nest_pas_activable() {
        let db = db_test().await;
        let alice = compte(&db, "alice").await;
        let refus = activate_toolset(&db, &alice, "H_sandbox")
            .await
            .expect_err("un nom d'hote ne s'active pas");
        assert!(
            matches!(&refus, LymailleError::Forbidden(_)),
            "le refus est un `Forbidden` : {refus:?}"
        );
    }

    /// Un brouillon illisible est un `InvalidDefinition` ; sa cause serde, qui pourrait citer le
    /// source, n'est pas propagée (`Must not` de la spec).
    #[tokio::test(flavor = "multi_thread")]
    async fn un_brouillon_illisible_est_refuse() {
        let db = db_test().await;
        let alice = compte(&db, "alice").await;
        inserer_ligne(
            &db,
            PRIVATE_TOOLSETS_TABLE,
            Some(alice.user_id),
            "mon_toolset",
            "1.0.0",
            None,
            Some(json!({"name": "mon_toolset"})),
        )
        .await;
        let refus = activate_toolset(&db, &alice, "mon_toolset")
            .await
            .expect_err("un brouillon sans forme de toolset est refuse");
        match &refus {
            LymailleError::InvalidDefinition(message) => {
                assert!(
                    message.contains("mon_toolset") && !message.contains("missing field"),
                    "le refus nomme le toolset, pas la cause serde : {message}"
                );
            }
            autre => panic!("`InvalidDefinition` attendue, obtenu : {autre:?}"),
        }
    }

    /// Le rapport est comparable et son `Debug` ne porte aucun source.
    #[test]
    fn le_rapport_est_comparable() {
        let rapport = ActivationReport {
            name: "t".to_owned(),
            version: "2.0.0".to_owned(),
            previous_version: Some("1.0.0".to_owned()),
        };
        assert_eq!(rapport, rapport.clone());
        let rendu = format!("{rapport:?}");
        assert!(
            rendu.contains("2.0.0") && !rendu.contains("fn "),
            "le Debug nomme, jamais le source : {rendu}"
        );
    }
}
