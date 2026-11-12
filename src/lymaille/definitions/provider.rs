//! Entité `lymaille_providers` (feature `lymaille`) — contrat porté par `./provider.sdd`.
//!
//! Table `miryad_lymaille_providers` : les moteurs d'inférence, références seules — un
//! `lymaille::Provider` porte un nom, un type, une adresse et une référence d'authentification
//! (nom de variable d'environnement), jamais de secret (DSD `DEC-16`). Administré par l'admin
//! seul, en lecture comme en écriture : un utilisateur n'a aucune raison de lire l'adresse d'un
//! moteur.

use sea_orm::ActiveValue;
use sea_orm::entity::prelude::*;
use serde::Deserialize;
use serde::Serialize;

use crate::auth::AuthPrincipal;
use crate::resource::AccessPolicy;
use crate::resource::HookError;
use crate::resource::MiryadResource;

/// Provider d'inférence persisté — la table `miryad_lymaille_providers` posée par
/// `../migration.rs`, colonnes du contrat `Must` de ./provider.sdd.
#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "miryad_lymaille_providers")]
pub struct Model {
    /// Clé primaire entière auto-incrémentée (convention `miryad_*` de la crate).
    #[sea_orm(primary_key)]
    pub id: i32,
    /// Nom du provider, copie dérivée de `provider.name` par les hooks — libellé de la ressource,
    /// unique (contrainte de la migration).
    pub name: String,
    /// Type du provider (par exemple `openai`), copie dérivée par les hooks.
    pub kind: String,
    /// Niveau du provider (`user` ou `project`), copie dérivée par les hooks.
    pub level: String,
    /// Le `lymaille::Provider` complet, colonne JSON : la source de vérité. Ses références sont
    /// résolues à l'exécution par `../secret.rs` ; la désérialisation rejette déjà tout secret
    /// en clair.
    pub provider: serde_json::Value,
    /// Horodatage de création, posé par le hook de création.
    pub created_at: DateTimeUtc,
}

/// Aucune relation : la référence du profil vers le provider est un nom, sans clé étrangère.
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

impl Entity {
    /// Porte commune de `before_create` et `before_update`, dans l'ordre du `Must` de
    /// ./provider.sdd : `provider` est un objet JSON qui se désérialise en `lymaille::Provider` —
    /// la `Deserialize` de la bibliothèque applique `Provider::new`, qui refuse nom ou type vide,
    /// adresse ou référence avec blanc, identifiants dans une URL et forme de jeton. Tout échec
    /// rend le même message fixe : la cause serde est **abandonnée**, ses textes pourraient citer
    /// une valeur (`Must not`). `name`, `kind` et `level` sont les copies dérivées ; `name` ne
    /// change pas sur une mise à jour.
    fn valider_et_deriver(
        mut active: ActiveModel,
        existing: Option<&Model>,
    ) -> Result<ActiveModel, HookError> {
        let brute = match &active.provider {
            ActiveValue::Set(valeur) | ActiveValue::Unchanged(valeur) if valeur.is_object() => Some(valeur),
            ActiveValue::Set(_) | ActiveValue::Unchanged(_) | ActiveValue::NotSet => None,
        };
        let Some(brute) = brute else {
            return Err(HookError::new("provider is required"));
        };
        let moteur: ::lymaille::Provider =
            serde_json::from_value(brute.clone()).map_err(|_cause| HookError::new("invalid provider"))?;
        let Ok(serde_json::Value::String(niveau)) = serde_json::to_value(moteur.level()) else {
            return Err(HookError::new("invalid provider"));
        };
        if let Some(ligne) = existing
            && moteur.name() != ligne.name.as_str()
        {
            return Err(HookError::new("provider name cannot change"));
        }
        active.name = ActiveValue::Set(moteur.name().to_owned());
        active.kind = ActiveValue::Set(moteur.kind().to_owned());
        active.level = ActiveValue::Set(niveau);
        if existing.is_none() && matches!(active.created_at, ActiveValue::NotSet) {
            active.created_at = ActiveValue::Set(chrono::Utc::now());
        }
        Ok(active)
    }
}

/// Métadonnées et hooks de la ressource `lymaille_providers`, lue à l'identique par REST et MCP :
/// lecture et écriture réservées aux admins (DSD `DEC-16`).
impl MiryadResource for Entity {
    fn resource_name() -> &'static str {
        "lymaille_providers"
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
        None
    }

    fn label_column() -> Option<<Self as EntityTrait>::Column> {
        Some(Column::Name)
    }

    /// Valide le provider et dérive `name`, `kind` et `level` — voir `Entity::valider_et_deriver`.
    fn before_create<C>(
        db: &C,
        active: Self::ActiveModel,
        _principal: &AuthPrincipal,
    ) -> impl std::future::Future<Output = Result<Self::ActiveModel, HookError>> + Send
    where
        C: sea_orm::ConnectionTrait,
    {
        let _ = db;
        async move { Entity::valider_et_deriver(active, None) }
    }

    /// Mêmes étapes que [`Self::before_create`], plus le nom : renommer un provider est refusé.
    fn before_update<C>(
        db: &C,
        active: Self::ActiveModel,
        existing: &Self::Model,
        _principal: &AuthPrincipal,
    ) -> impl std::future::Future<Output = Result<Self::ActiveModel, HookError>> + Send
    where
        C: sea_orm::ConnectionTrait,
    {
        let _ = db;
        async move { Entity::valider_et_deriver(active, Some(existing)) }
    }

    // `before_delete` volontairement non surdéclaré : le défaut identité du trait s'applique
    // (./provider.sdd).
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
    use ::lymaille::Provider;
    use ::lymaille::ProviderLevel;
    use chrono::TimeZone as _;
    use sea_orm::ActiveValue::NotSet;
    use sea_orm::ActiveValue::Set;
    use serde_json::json;

    // ── Fixtures — purement mémoire : hooks purs, aucune connexion.

    fn principal() -> AuthPrincipal {
        AuthPrincipal {
            subject: "sujet-de-test".to_string(),
            email: None,
            preferred_username: None,
            source: PrincipalSource::ApiToken { token_id: 1 },
        }
    }

    /// `Provider` sérialisé : la fixture passe par le constructeur de la bibliothèque, donc par
    /// sa validation des références.
    fn provider_json(name: &str, kind: &str, address: &str, auth: &str) -> serde_json::Value {
        let construit = Provider::new(
            name.to_string(),
            kind.to_string(),
            address.to_string(),
            Some(auth.to_string()),
            ProviderLevel::User,
        )
        .expect("la fixture est un provider de forme valide");
        serde_json::to_value(construit).expect("la fixture se sérialise")
    }

    /// `ActiveModel` dans la forme qui atteint le hook : colonnes dérivées fausses.
    fn active_avec(provider: serde_json::Value) -> ActiveModel {
        ActiveModel {
            id: NotSet,
            name: Set("faux-nom".to_string()),
            kind: Set("faux-type".to_string()),
            level: Set("faux-niveau".to_string()),
            provider: Set(provider),
            created_at: NotSet,
        }
    }

    fn modele(nom: &str) -> Model {
        Model {
            id: 1,
            name: nom.to_string(),
            kind: "openai".to_string(),
            level: "user".to_string(),
            provider: provider_json(nom, "openai", "http://llm:8080/v1", "LLM_API_KEY"),
            created_at: chrono::Utc.with_ymd_and_hms(2026, 11, 1, 12, 0, 0).unwrap(),
        }
    }

    /// Scenario « un provider valide dérive nom, type et niveau » : les colonnes fausses de
    /// l'entrée sont réécrites depuis le provider, l'horodatage de création est posé.
    #[tokio::test]
    async fn before_create_accepte_un_provider_valide_et_derive_ses_colonnes() {
        let rendu = Entity::before_create(
            &hook_db(),
            active_avec(provider_json(
                "local",
                "openai",
                "http://llm:8080/v1",
                "LLM_API_KEY",
            )),
            &principal(),
        )
        .await
        .expect("un provider de références valides est accepté");
        assert_eq!(rendu.name, Set("local".to_string()));
        assert_eq!(rendu.kind, Set("openai".to_string()));
        assert_eq!(rendu.level, Set("user".to_string()));
        assert!(
            matches!(rendu.created_at, Set(_)),
            "`created_at` est posé par le hook de création"
        );
    }

    /// Scenario « un secret en clair est refusé sans être cité » : une référence d'authentification
    /// en forme de jeton est rejetée par la désérialisation (`Provider::new`), et le message
    /// n'offre ni l'adresse, ni la référence, ni la valeur.
    #[tokio::test]
    async fn before_create_refuse_un_secret_en_clair_sans_le_citer() {
        let jeton = "sk-abc123supersecretpasaciter";
        let valeur = json!({
            "name": "local",
            "kind": "openai",
            "address": "http://llm:8080/v1",
            "auth": jeton,
            "level": "user",
        });
        match Entity::before_create(&hook_db(), active_avec(valeur), &principal()).await {
            Err(erreur) => {
                assert!(erreur.code.is_none(), "aucun code sur un `HookError` de la crate");
                assert_eq!(erreur.message, "invalid provider");
                assert!(
                    !erreur.message.contains(jeton) && !erreur.message.contains("llm:8080"),
                    "aucune valeur ne traverse : {erreur:?}"
                );
            }
            Ok(_) => panic!("une référence en forme de jeton doit être refusée"),
        }
    }

    /// `Must` : `provider` absent (`NotSet`) ou non objet est refusé avant toute désérialisation.
    #[tokio::test]
    async fn before_create_refuse_un_provider_absent_ou_non_objet() {
        let sans = ActiveModel {
            provider: NotSet,
            ..active_avec(serde_json::Value::Null)
        };
        match Entity::before_create(&hook_db(), sans, &principal()).await {
            Err(erreur) => assert_eq!(erreur.message, "provider is required"),
            Ok(_) => panic!("un provider `NotSet` doit être refusé"),
        }
        match Entity::before_create(&hook_db(), active_avec(json!(["local"])), &principal()).await {
            Err(erreur) => assert_eq!(erreur.message, "provider is required"),
            Ok(_) => panic!("un provider non objet doit être refusé"),
        }
    }

    /// `Must` (`before_update`) : `name` inchangé.
    #[tokio::test]
    async fn before_update_refuse_un_renommage() {
        let rendu = Entity::before_update(
            &hook_db(),
            active_avec(provider_json(
                "autre",
                "openai",
                "http://llm:8080/v1",
                "LLM_API_KEY",
            )),
            &modele("local"),
            &principal(),
        )
        .await;
        match rendu {
            Err(erreur) => {
                assert!(erreur.code.is_none(), "aucun code sur un `HookError` de la crate");
                assert_eq!(erreur.message, "provider name cannot change");
            }
            Ok(_) => panic!("renommer un provider est refusé, comme pour les profils"),
        }
    }

    /// Scenario « lecture et écriture réservées aux admins » : les métadonnées du `Must`, exactes.
    #[test]
    fn politiques_reservees_aux_admins() {
        assert_eq!(Entity::resource_name(), "lymaille_providers");
        assert_eq!(Entity::read_policy(), AccessPolicy::AdminOnly);
        assert_eq!(Entity::write_policy(), AccessPolicy::AdminOnly);
        assert!(Entity::owner_column().is_none(), "aucun propriétaire");
        assert!(Entity::filter_column().is_none());
        assert!(matches!(Entity::label_column(), Some(Column::Name)));
    }
}
