//! Entité `lymaille_mcp_servers` (feature `lymaille`) — contrat porté par `./mcp_server.sdd`.
//!
//! Table `miryad_lymaille_mcp_servers` (`m20261104_000001_lymaille_lot2`, DSD `DEC-20`) : les
//! serveurs MCP que les agents peuvent appeler, déclarés par l'admin par REST et MCP sans
//! redéploiement — `lymaille` 0.1.0 ne fournit aucun store de ces définitions, cette table en est
//! la source. Le **secret n'est jamais en base** : `auth_ref` est une référence résolue par le
//! `@SecretResolver` (`../secret.sdd`, DSD `DEC-16`). Ressource `AdminOnly` en lecture comme en
//! écriture : l'URL peut porter un jeton en requête.

use sea_orm::ActiveValue;
use sea_orm::entity::prelude::*;
use serde::Deserialize;
use serde::Serialize;

use crate::auth::AuthPrincipal;
use crate::resource::AccessPolicy;
use crate::resource::HookError;
use crate::resource::MiryadResource;

/// Serveur MCP persisté — la table `miryad_lymaille_mcp_servers` posée par `../migration.rs`
/// (`m20261104_000001_lymaille_lot2`), colonnes du contrat `Must` de `./mcp_server.sdd`.
#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "miryad_lymaille_mcp_servers")]
pub struct Model {
    /// Clé primaire entière auto-incrémentée (convention `miryad_*` de la crate).
    #[sea_orm(primary_key)]
    pub id: i32,
    /// Nom du serveur, libellé de la ressource, unique (contrainte de la migration) : un
    /// identifiant Rhai (`[A-Za-z0-9_]+`), car c'est un rôle MCP des toolsets — il ne change pas
    /// à la mise à jour, les contextes y renvoient par nom.
    pub name: String,
    /// URL du serveur, schéma `http` ou `https` uniquement. Peut porter un jeton en requête :
    /// d'où la lecture `AdminOnly`, et jamais d'URL dans un message d'erreur ou une trace.
    pub url: String,
    /// Délai par requête, en secondes, borné à [1, 600] par les hooks.
    pub request_timeout_secs: i32,
    /// Délai de connexion, en secondes, borné à [1, 60] par les hooks.
    pub connect_timeout_secs: i32,
    /// Mode d'authentification : `none`, `static` (référence résolue par le `@SecretResolver`, DSD
    /// `DEC-16`) ou `forward` (jeton de l'appelant).
    pub auth_mode: String,
    /// Référence du secret d'authentification, jamais le secret lui-même (DSD `DEC-16`) : posée
    /// en `static` seule, absente en `none` et `forward`.
    pub auth_ref: Option<String>,
    /// Groupes autorisés à appeler ce serveur (DSD `DEC-20`, contrôle à l'usage :
    /// `../mcp_access.sdd`) : tableau JSON non vide de noms, `all` et `admin` exclusifs.
    pub allowed_groups: serde_json::Value,
    /// Horodatage de création, posé par le hook de création ; `before_update` le laisse à celui
    /// de la ligne existante (aucune entité de la crate ne porte d'`updated_at`).
    pub created_at: DateTimeUtc,
}

/// Aucune relation : les contextes renvoient au serveur par nom, sans clé étrangère.
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

impl Entity {
    /// Texte d'une colonne `String` réellement posée (ou inchangée) : `NotSet` compte comme
    /// absent, la porte refuse — le `NotSet` n'atteint jamais la base.
    fn lisible(valeur: &ActiveValue<String>) -> Option<&str> {
        match valeur {
            ActiveValue::Set(texte) | ActiveValue::Unchanged(texte) => Some(texte.as_str()),
            ActiveValue::NotSet => None,
        }
    }

    /// Référence d'une colonne `Option<String>` posée : `None` si absente ou effacée.
    fn reference(valeur: &ActiveValue<Option<String>>) -> Option<&str> {
        match valeur {
            ActiveValue::Set(reference) | ActiveValue::Unchanged(reference) => reference.as_deref(),
            ActiveValue::NotSet => None,
        }
    }

    /// Entier d'une colonne `Integer` réellement posée (ou inchangée).
    fn entier(valeur: &ActiveValue<i32>) -> Option<i32> {
        match valeur {
            ActiveValue::Set(entier) | ActiveValue::Unchanged(entier) => Some(*entier),
            ActiveValue::NotSet => None,
        }
    }

    /// Nom conforme à `[A-Za-z0-9_]+` : un nom de rôle MCP est un identifiant Rhai.
    fn est_identifiant(nom: &str) -> bool {
        !nom.is_empty() && nom.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
    }

    /// Nom de groupe conforme à `[A-Za-z0-9_.-]+`.
    fn est_groupe(nom: &str) -> bool {
        !nom.is_empty()
            && nom
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.' | b'-'))
    }

    /// Schéma de l'URL : `http` ou `https` (insensible à la casse, RFC 3986), lu par simple
    /// préfixe — aucune dépendance d'analyse d'URL de plus (`Depends on` de `./mcp_server.sdd`).
    fn schema_est_http(url: &str) -> bool {
        url.find("://")
            .and_then(|separateur| url.get(..separateur))
            .is_some_and(|schema| matches!(schema.to_ascii_lowercase().as_str(), "http" | "https"))
    }

    /// Porte commune de `before_create` (`existing` absent) et `before_update`, dans l'ordre du
    /// `Must` de `./mcp_server.sdd`, interrompue au premier échec : nom d'identifiant Rhai, schéma
    /// `http`/`https`, délais bornés, mode d'authentification et cohérence de sa référence,
    /// groupes autorisés ; sur une mise à jour le nom ne change pas et `created_at` reste celui
    /// de la ligne existante. Les messages sont fixes : `Must not` — ni l'URL ni `auth_ref` n'y
    /// figurent. L'existence d'un groupe n'est pas vérifiée en base : un hook est synchrone et
    /// sans connexion, le droit est relu à l'usage (`../mcp_access.sdd`).
    fn valider(active: &mut ActiveModel, existing: Option<&Model>) -> Result<(), HookError> {
        if !Self::lisible(&active.name).is_some_and(Self::est_identifiant) {
            return Err(HookError::new(
                "mcp server name must be a Rhai identifier ([A-Za-z0-9_]+)",
            ));
        }
        if !Self::lisible(&active.url).is_some_and(Self::schema_est_http) {
            return Err(HookError::new("mcp server url must be http or https"));
        }
        if !Self::entier(&active.request_timeout_secs).is_some_and(|delai| (1..=600).contains(&delai)) {
            return Err(HookError::new(
                "mcp server request timeout must be between 1 and 600 seconds",
            ));
        }
        if !Self::entier(&active.connect_timeout_secs).is_some_and(|delai| (1..=60).contains(&delai)) {
            return Err(HookError::new(
                "mcp server connect timeout must be between 1 and 60 seconds",
            ));
        }
        let mode = Self::lisible(&active.auth_mode).unwrap_or_default();
        if !matches!(mode, "none" | "static" | "forward") {
            return Err(HookError::new("invalid mcp server auth mode"));
        }
        let reference = Self::reference(&active.auth_ref);
        match mode {
            "static" if reference.is_none_or(str::is_empty) => {
                return Err(HookError::new(
                    "static mcp server auth requires a non-empty auth_ref",
                ));
            }
            "none" | "forward" if reference.is_some() => {
                return Err(HookError::new(
                    "mcp server auth_ref requires the static auth mode",
                ));
            }
            _ => {}
        }
        let noms = Self::groupes(&active.allowed_groups)?;
        let exclusif = noms.iter().any(|nom| matches!(*nom, "all" | "admin"));
        if exclusif && noms.len() > 1 {
            return Err(HookError::new(
                "mcp server allowed groups cannot combine all or admin with other groups",
            ));
        }
        if let Some(ligne) = existing
            && Self::lisible(&active.name) != Some(ligne.name.as_str())
        {
            return Err(HookError::new("mcp server name cannot change"));
        }
        match existing {
            Some(ligne) => active.created_at = ActiveValue::Set(ligne.created_at),
            None if matches!(active.created_at, ActiveValue::NotSet) => {
                active.created_at = ActiveValue::Set(chrono::Utc::now());
            }
            None => {}
        }
        Ok(())
    }

    /// Étape 5 : tableau JSON non vide de noms de groupe — la **liste vide** est refusée (une
    /// ligne sans groupe n'est utilisable par personne, on le dit à la sauvegarde plutôt qu'en
    /// silence). Rendu des noms validés contre `[A-Za-z0-9_.-]+` (dont `all` et `admin`, gérés
    /// par la règle d'exclusivité du demandeur).
    fn groupes(valeur: &ActiveValue<serde_json::Value>) -> Result<Vec<&str>, HookError> {
        let forme = HookError::new("mcp server allowed groups must be a non-empty array of group names");
        let tableau = match valeur {
            ActiveValue::Set(groupes) | ActiveValue::Unchanged(groupes) => Some(groupes),
            ActiveValue::NotSet => None,
        }
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| forme.clone())?;
        let noms = tableau
            .iter()
            .map(serde_json::Value::as_str)
            .collect::<Option<Vec<&str>>>()
            .ok_or_else(|| forme.clone())?;
        if noms.is_empty() {
            return Err(forme);
        }
        if !noms.iter().all(|nom| Self::est_groupe(nom)) {
            return Err(HookError::new("invalid mcp server allowed group name"));
        }
        Ok(noms)
    }
}

/// Métadonnées de la ressource `lymaille_mcp_servers`, lue à l'identique par REST et MCP :
/// lecture et écriture réservées aux admins (`./mcp_server.sdd`) — l'URL peut porter un jeton en
/// requête.
impl MiryadResource for Entity {
    fn resource_name() -> &'static str {
        "lymaille_mcp_servers"
    }

    fn read_policy() -> AccessPolicy {
        AccessPolicy::AdminOnly
    }

    fn write_policy() -> AccessPolicy {
        AccessPolicy::AdminOnly
    }

    fn owner_column() -> Option<<Self as EntityTrait>::Column> {
        None
    }

    fn filter_column() -> Option<<Self as EntityTrait>::Column> {
        Some(Column::AuthMode)
    }

    fn label_column() -> Option<<Self as EntityTrait>::Column> {
        Some(Column::Name)
    }

    /// Applique les cinq premières portes du `Must` et pose `created_at` — voir
    /// `Entity::valider`.
    fn before_create<C>(
        db: &C,
        mut active: Self::ActiveModel,
        _principal: &AuthPrincipal,
    ) -> impl std::future::Future<Output = Result<Self::ActiveModel, HookError>> + Send
    where
        C: sea_orm::ConnectionTrait,
    {
        let _ = db;
        async move {
            Entity::valider(&mut active, None)?;
            Ok(active)
        }
    }

    /// Mêmes portes que [`Self::before_create`], plus l'identité : renommer un serveur est
    /// refusé car les contextes y renvoient par nom ; `created_at` reste celui de la ligne
    /// existante.
    fn before_update<C>(
        db: &C,
        mut active: Self::ActiveModel,
        existing: &Self::Model,
        _principal: &AuthPrincipal,
    ) -> impl std::future::Future<Output = Result<Self::ActiveModel, HookError>> + Send
    where
        C: sea_orm::ConnectionTrait,
    {
        let _ = db;
        async move {
            Entity::valider(&mut active, Some(existing))?;
            Ok(active)
        }
    }

    // `before_delete` volontairement non surdéclaré : le défaut identité du trait s'applique
    // (./mcp_server.sdd).
}

/// Construit la configuration du connecteur `lymaille` pour ce serveur — pure : l'URL et les
/// deux délais, rien d'autre. Le secret n'y figure jamais : il est résolu à l'usage par
/// `../secret.sdd` (DSD `DEC-16`). Les délais ont été bornés par les hooks ; une valeur hors
/// plage n'atteint jamais cet appel, le repli `u64` n'est qu'un garde-fou sans `unwrap`.
#[must_use]
pub fn client_config(modele: &Model) -> ::lymaille::McpClientConfig {
    ::lymaille::McpClientConfig::new(&modele.url)
        .with_request_timeout(std::time::Duration::from_secs(
            u64::try_from(modele.request_timeout_secs).unwrap_or_default(),
        ))
        .with_connect_timeout(std::time::Duration::from_secs(
            u64::try_from(modele.connect_timeout_secs).unwrap_or_default(),
        ))
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
    use crate::resource::AccessPolicy;
    use crate::resource::MiryadResource;
    use ::lymaille::McpClientConfig;
    use chrono::TimeZone as _;
    use chrono::Utc;
    use sea_orm::ActiveValue::NotSet;
    use sea_orm::ActiveValue::Set;
    use serde_json::json;
    use std::time::Duration;

    // ── Fixtures — purement mémoire : les hooks de ./mcp_server.sdd sont des fonctions pures sur
    // l'`ActiveModel` (position contractuelle après `rest::core::mark_all_set`, avant toute
    // écriture, aucune connexion).

    fn principal() -> AuthPrincipal {
        AuthPrincipal {
            subject: "sujet-de-test".to_string(),
            email: None,
            preferred_username: None,
            source: PrincipalSource::ApiToken { token_id: 1 },
        }
    }

    /// `ActiveModel` dans la forme qui atteint le hook : clé et horodatage non posés, délais dans
    /// les bornes hautes-basses valides (30 s / 5 s) sauf override du test.
    fn serveur(
        name: &str,
        url: &str,
        request_timeout_secs: i32,
        connect_timeout_secs: i32,
        auth_mode: &str,
        auth_ref: Option<&str>,
        allowed_groups: serde_json::Value,
    ) -> ActiveModel {
        ActiveModel {
            id: NotSet,
            name: Set(name.to_string()),
            url: Set(url.to_string()),
            request_timeout_secs: Set(request_timeout_secs),
            connect_timeout_secs: Set(connect_timeout_secs),
            auth_mode: Set(auth_mode.to_string()),
            auth_ref: Set(auth_ref.map(str::to_owned)),
            allowed_groups: Set(allowed_groups),
            created_at: NotSet,
        }
    }

    /// Le serveur valide de référence — le Given du premier Scenario : `kubectl`,
    /// `https://mcp.example/`, `static` + `KUBECTL_TOKEN`, groupes `["ops"]`.
    fn serveur_kubectl() -> ActiveModel {
        serveur(
            "kubectl",
            "https://mcp.example/",
            30,
            5,
            "static",
            Some("KUBECTL_TOKEN"),
            json!(["ops"]),
        )
    }

    /// Ligne existante du hook `before_update`.
    fn modele(nom: &str) -> Model {
        Model {
            id: 1,
            name: nom.to_string(),
            url: "https://mcp.example/".to_string(),
            request_timeout_secs: 30,
            connect_timeout_secs: 5,
            auth_mode: "static".to_string(),
            auth_ref: Some("KUBECTL_TOKEN".to_string()),
            allowed_groups: json!(["ops"]),
            created_at: Utc.with_ymd_and_hms(2026, 11, 1, 12, 0, 0).unwrap(),
        }
    }

    /// Scenario « un serveur statique valide est accepté » : le Given complet passe
    /// `before_create`, et l'horodatage de création est posé par le hook (`created_at` arrivait
    /// `NotSet`).
    #[tokio::test]
    async fn before_create_accepte_un_serveur_statique_valide() {
        let rendu = Entity::before_create(&hook_db(), serveur_kubectl(), &principal())
            .await
            .expect("le serveur `kubectl` statique du Scenario est accepté");
        assert!(
            matches!(rendu.created_at, Set(_)),
            "`created_at` est posé par le hook de création quand il est absent"
        );
    }

    /// Scenario « static sans référence, none avec référence » : deux refus, `HookError` sans
    /// code `MRD-*`, et `auth_ref` jamais cité dans le message (`Must not`).
    #[tokio::test]
    async fn before_create_refuse_static_sans_reference_et_none_avec_reference() {
        let sans_reference = serveur(
            "kubectl",
            "https://mcp.example/",
            30,
            5,
            "static",
            None,
            json!(["ops"]),
        );
        match Entity::before_create(&hook_db(), sans_reference, &principal()).await {
            Err(erreur) => {
                assert!(
                    erreur.code.is_none(),
                    "aucun code `MRD-*` sur un `HookError` : {erreur:?}"
                );
                assert_eq!(
                    erreur.message, "static mcp server auth requires a non-empty auth_ref",
                    "static sans référence est refusé"
                );
            }
            Ok(_) => panic!("`static` sans `auth_ref` doit être refusé"),
        }
        let avec_reference = serveur(
            "kubectl",
            "https://mcp.example/",
            30,
            5,
            "none",
            Some("KUBECTL_TOKEN"),
            json!(["ops"]),
        );
        match Entity::before_create(&hook_db(), avec_reference, &principal()).await {
            Err(erreur) => {
                assert!(
                    erreur.code.is_none(),
                    "aucun code `MRD-*` sur un `HookError` : {erreur:?}"
                );
                assert_eq!(
                    erreur.message, "mcp server auth_ref requires the static auth mode",
                    "`none` avec `auth_ref` est refusé"
                );
                assert!(
                    !erreur.message.contains("KUBECTL_TOKEN"),
                    "la référence n'est jamais citée : {erreur:?}"
                );
            }
            Ok(_) => panic!("`none` avec `auth_ref` doit être refusé"),
        }
    }

    /// Scenario « un schéma non http est refusé sans citer l'URL » : le message du `Must`
    /// exactement, sans la moindre trace de l'URL refusée.
    #[tokio::test]
    async fn before_create_refuse_un_schema_non_http_sans_citer_l_url() {
        let url = "file:///etc/passwd";
        let rendu = serveur("kubectl", url, 30, 5, "none", None, json!(["ops"]));
        match Entity::before_create(&hook_db(), rendu, &principal()).await {
            Err(erreur) => {
                assert!(
                    erreur.code.is_none(),
                    "aucun code `MRD-*` sur un `HookError` : {erreur:?}"
                );
                assert_eq!(erreur.message, "mcp server url must be http or https");
                assert!(
                    !erreur.message.contains(url) && !erreur.message.contains("passwd"),
                    "l'URL refusée ne traverse jamais le message : {erreur:?}"
                );
            }
            Ok(_) => panic!("le schéma `file` doit être refusé"),
        }
    }

    /// Scenario « groupes autorisés » : liste vide refusée (refus par défaut, dit à la
    /// sauvegarde), `["all", "ops"]` refusé (`all` exclusif), `["admin"]` seul accepté.
    #[tokio::test]
    async fn before_create_refuse_les_groupes_vides_et_all_combine_puis_accepte_admin() {
        let vide = serveur("kubectl", "https://mcp.example/", 30, 5, "none", None, json!([]));
        match Entity::before_create(&hook_db(), vide, &principal()).await {
            Err(erreur) => {
                assert!(
                    erreur.code.is_none(),
                    "aucun code `MRD-*` sur un `HookError` : {erreur:?}"
                );
                assert!(
                    !erreur.message.is_empty(),
                    "la liste vide est refusée, et le refus se dit : {erreur:?}"
                );
            }
            Ok(_) => panic!("une liste vide de groupes doit être refusée"),
        }
        let combine = serveur(
            "kubectl",
            "https://mcp.example/",
            30,
            5,
            "none",
            None,
            json!(["all", "ops"]),
        );
        match Entity::before_create(&hook_db(), combine, &principal()).await {
            Err(erreur) => {
                assert!(
                    erreur.code.is_none(),
                    "aucun code `MRD-*` sur un `HookError` : {erreur:?}"
                );
                assert!(
                    !erreur.message.is_empty(),
                    "`all` ne se combine avec aucun autre groupe : {erreur:?}"
                );
            }
            Ok(_) => panic!("`[\"all\", \"ops\"]` doit être refusé"),
        }
        let admin = serveur(
            "kubectl",
            "https://mcp.example/",
            30,
            5,
            "none",
            None,
            json!(["admin"]),
        );
        Entity::before_create(&hook_db(), admin, &principal())
            .await
            .expect("`[\"admin\"]` seul est un tableau de groupes valide");
    }

    /// Scenario « le nom ne change pas » : les contextes renvoient au serveur par nom.
    #[tokio::test]
    async fn before_update_refuse_un_renommage() {
        let rendu = serveur(
            "autre",
            "https://mcp.example/",
            30,
            5,
            "static",
            Some("KUBECTL_TOKEN"),
            json!(["ops"]),
        );
        match Entity::before_update(&hook_db(), rendu, &modele("kubectl"), &principal()).await {
            Err(erreur) => {
                assert!(
                    erreur.code.is_none(),
                    "aucun code `MRD-*` sur un `HookError` : {erreur:?}"
                );
                assert_eq!(erreur.message, "mcp server name cannot change");
            }
            Ok(_) => panic!("renommer un serveur MCP est refusé, comme pour les providers"),
        }
    }

    /// `Must` (`before_update`, étape 6) : une mise à jour qui garde le nom passe et `created_at`
    /// reste celui de la ligne existante, même si le corps en apporte un autre.
    #[tokio::test]
    async fn before_update_garde_le_nom_et_conserve_la_date_de_creation() {
        let existing = modele("kubectl");
        let mut rendu_serveur = serveur_kubectl();
        rendu_serveur.created_at = Set(Utc.with_ymd_and_hms(2020, 1, 1, 0, 0, 0).unwrap());
        let rendu = Entity::before_update(&hook_db(), rendu_serveur, &existing, &principal())
            .await
            .expect("une mise à jour qui garde le nom est acceptée");
        assert_eq!(
            rendu.created_at,
            Set(existing.created_at),
            "`created_at` reste celui de la ligne existante"
        );
    }

    /// Scenario « la ressource n'est lisible que par un admin » : les métadonnées du `Must`,
    /// exactes — le RBAC du moteur (`/src/resource.sdd`, `rbac`) traduit ces deux `AdminOnly` en
    /// deux refus pour un non-admin qui liste puis crée ; la parité REST/MCP vient de ce que les
    /// deux surfaces lisent la même déclaration.
    #[test]
    fn politiques_reservees_aux_admins() {
        assert_eq!(Entity::resource_name(), "lymaille_mcp_servers");
        assert_eq!(Entity::read_policy(), AccessPolicy::AdminOnly);
        assert_eq!(Entity::write_policy(), AccessPolicy::AdminOnly);
        assert!(Entity::owner_column().is_none(), "aucun propriétaire");
        assert!(
            matches!(Entity::filter_column(), Some(Column::AuthMode)),
            "le filtre de liste est le mode d'authentification"
        );
        assert!(matches!(Entity::label_column(), Some(Column::Name)));
    }

    /// `Must` étape 1 : `name` est un identifiant Rhai `[A-Za-z0-9_]+` — vide, espace ou tiret
    /// sont refusés.
    #[tokio::test]
    async fn before_create_refuse_un_nom_hors_identifiant_rhai() {
        for nom in ["", "mon serveur", "kubectl-v2"] {
            let rendu = serveur(nom, "https://mcp.example/", 30, 5, "none", None, json!(["ops"]));
            match Entity::before_create(&hook_db(), rendu, &principal()).await {
                Err(erreur) => {
                    assert!(
                        erreur.code.is_none(),
                        "aucun code `MRD-*` sur un `HookError` : {erreur:?}"
                    );
                    assert_eq!(
                        erreur.message, "mcp server name must be a Rhai identifier ([A-Za-z0-9_]+)",
                        "le nom {nom:?} n'est pas un identifiant Rhai"
                    );
                }
                Ok(_) => panic!("le nom {nom:?} doit être refusé"),
            }
        }
    }

    /// `Must` étape 3 : délais bornés — `request_timeout_secs` ∈ [1, 600],
    /// `connect_timeout_secs` ∈ [1, 60].
    #[tokio::test]
    async fn before_create_refuse_les_delais_hors_plage() {
        for (request, connect) in [(0, 5), (601, 5), (30, 0), (30, 61)] {
            let rendu = serveur(
                "kubectl",
                "https://mcp.example/",
                request,
                connect,
                "none",
                None,
                json!(["ops"]),
            );
            match Entity::before_create(&hook_db(), rendu, &principal()).await {
                Err(erreur) => {
                    assert!(
                        erreur.code.is_none(),
                        "aucun code `MRD-*` sur un `HookError` : {erreur:?}"
                    );
                    assert!(
                        !erreur.message.is_empty(),
                        "délais ({request}, {connect}) hors plage : {erreur:?}"
                    );
                }
                Ok(_) => panic!("les délais ({request}, {connect}) doivent être refusés"),
            }
        }
    }

    /// `Must` étape 4 : `auth_mode` hors `{none, static, forward}` est refusé ; `forward` avec
    /// référence est refusé comme `none`.
    #[tokio::test]
    async fn before_create_refuse_un_auth_mode_inconnu_et_forward_avec_reference() {
        let inconnu = serveur(
            "kubectl",
            "https://mcp.example/",
            30,
            5,
            "oauth",
            None,
            json!(["ops"]),
        );
        match Entity::before_create(&hook_db(), inconnu, &principal()).await {
            Err(erreur) => assert_eq!(erreur.message, "invalid mcp server auth mode"),
            Ok(_) => panic!("`oauth` n'est pas un mode d'authentification du contrat"),
        }
        let forward = serveur(
            "kubectl",
            "https://mcp.example/",
            30,
            5,
            "forward",
            Some("KUBECTL_TOKEN"),
            json!(["ops"]),
        );
        match Entity::before_create(&hook_db(), forward, &principal()).await {
            Err(erreur) => assert_eq!(
                erreur.message,
                "mcp server auth_ref requires the static auth mode"
            ),
            Ok(_) => panic!("`forward` n'a rien à résoudre : la référence est refusée"),
        }
    }

    /// `Must` étape 5 : `allowed_groups` est un tableau JSON de noms `[A-Za-z0-9_.-]+` (ou
    /// `all`/`admin`) — non-tableau, élément non texte et nom avec espace ou caractère interdit
    /// sont refusés.
    #[tokio::test]
    async fn before_create_refuse_des_groupes_de_forme_invalide() {
        for groupes in [
            json!({"groupe": "ops"}),
            json!([1]),
            json!(["oops group"]),
            json!(["op$s"]),
        ] {
            let rendu = serveur(
                "kubectl",
                "https://mcp.example/",
                30,
                5,
                "none",
                None,
                groupes.clone(),
            );
            match Entity::before_create(&hook_db(), rendu, &principal()).await {
                Err(erreur) => {
                    assert!(
                        erreur.code.is_none(),
                        "aucun code `MRD-*` sur un `HookError` : {erreur:?}"
                    );
                    assert!(
                        !erreur.message.is_empty(),
                        "{groupes} n'est pas un tableau de noms de groupe : {erreur:?}"
                    );
                }
                Ok(_) => panic!("{groupes} doit être refusé"),
            }
        }
    }

    /// `Must` (`client_config`, pure) : les deux délais du modèle passent dans la
    /// `McpClientConfig`. Le contrôle se fait par `Debug` : celui de `lymaille` n'affiche
    /// volontairement **pas** l'URL (elle peut porter un jeton) — ce test prouve donc aussi le
    /// `Must not` « jamais l'URL dans une trace ».
    #[test]
    fn client_config_rend_les_deux_delais_sans_l_url() {
        let mut ligne = modele("kubectl");
        ligne.request_timeout_secs = 60;
        ligne.connect_timeout_secs = 3;
        let rendu = super::client_config(&ligne);
        let attendu = McpClientConfig::new("https://url-non-citee.example/")
            .with_request_timeout(Duration::from_secs(60))
            .with_connect_timeout(Duration::from_secs(3));
        assert_eq!(format!("{rendu:?}"), format!("{attendu:?}"));
        assert!(
            !format!("{rendu:?}").contains("mcp.example"),
            "`McpClientConfig` n'affiche jamais l'URL : {rendu:?}"
        );
    }
}
