//! Modules Rhai de l'hôte, registre `HostModules` et enrobage d'autorité (`src/lymaille/
//! host_module.sdd`, DSD `scripts-hote#FR-1` à `FR-3`, `DEC-4`). L'application déclare un
//! [`HostModule`] : nom, politique d'accès fermée ([`HostAccess`]) et fonctions ([`HostFunction`]).
//! [`mount_host_modules`] monte le module et **enrobe chaque fonction** d'un contrôle central,
//! exécuté avant chaque appel sur un principal relu en base (`lymaille#FR-4`, précédent :
//! `resource_module`) : un module n'a pas à réimplémenter l'autorité. Le registre [`HostModules`]
//! est une valeur du montage (précédent : `ResourceRegistry`), distincte de `LymailleHost` : une
//! collision de nom échoue au démarrage ([`HostModules::register`]). Aucun principal n'est gardé
//! entre deux appels, la connexion n'est jamais passée au module. Compile sous la feature
//! `lymaille` (gating porté par `../mod.rs`, lui-même gated dans `src/lib.rs`).

use std::any::TypeId;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use rhai::plugin::{PluginFunc, RhaiFunc};
use rhai::{Dynamic, EvalAltResult, FuncRegistration, Module, NativeCallContext, Position};
use sea_orm::DatabaseConnection;
use vynil_core::rt::block_on;

use super::error::LymailleError;
use crate::lymaille::identity::caller_of;

/// Noms de modules Rhai réservés par la crate : les modules déjà montés par l'assemblage
/// (`resources` de `resource_module`, les modules natifs de `lymaille` et de l'inspection des
/// sessions). Un [`HostModule`] d'hôte ne peut pas les usurper ([`HostModules::register`],
/// DSD `scripts-hote#FR-1`).
pub const RESERVED_MODULE_NAMES: &[&str] = &["resources", "result", "contexts", "inspect", "sessions"];

/// Politique d'accès d'un [`HostModule`] — énumération fermée (DSD `scripts-hote#OI-1`) : pas de
/// décision de l'hôte par appel, la politique est relue par l'enrobage central avant chaque appel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostAccess {
    /// Tout compte local résolu par `subject` accède ; un `subject` sans compte est refusé.
    Authenticated,
    /// Seul un membre du groupe `admin` (relu à l'appel) accède.
    AdminOnly,
}

/// Échec applicatif d'une fonction d'hôte : texte rendu tel quel au script, sans contenu secret
/// (responsabilité de l'hôte — la crate n'y ajoute ni code `MRD-*` ni détail technique).
#[derive(Debug, Clone)]
pub struct HostCallError(String);

impl HostCallError {
    /// Un échec portant le texte applicatif rendu au script.
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        Self(text.into())
    }

    /// Le texte porté, rendu tel quel par l'enrobage.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.0
    }
}

/// Le corps d'une [`HostFunction`] : les arguments Rhai déjà déballés en [`Dynamic`] en entrée,
/// la valeur rendue ou un [`HostCallError`] en sortie.
type HostCall = Arc<dyn Fn(&mut [Dynamic]) -> Result<Dynamic, HostCallError> + Send + Sync>;

/// Une fonction publiée par un [`HostModule`] : `call` reçoit les arguments Rhai déjà déballés
/// en [`Dynamic`] (taille `params`) et rend une valeur Rhai ou un [`HostCallError`].
pub struct HostFunction {
    /// Nom de la fonction — identifiant Rhai valide, appelée par `module::nom(...)`.
    pub name: &'static str,
    /// Arité contractuelle : un appel d'une arité différente est une fonction inconnue pour Rhai.
    pub params: usize,
    /// Le corps, appelé par l'enrobage seulement la politique satisfaite.
    pub call: HostCall,
}

/// Un module d'hôte : nom, politique d'accès et fabrique de ses fonctions pour un `subject`
/// (DSD `scripts-hote#FR-1`). La politique n'est jamais redécidée par le module : l'enrobage
/// central la vérifie avant chaque appel (`FR-2`).
pub trait HostModule: Send + Sync {
    /// Nom du module — un identifiant Rhai valide, ni réservé ni déjà pris (contrôlé à
    /// l'enregistrement, [`HostModules::register`]).
    fn name(&self) -> &'static str;

    /// Politique d'accès fermée du module.
    fn access(&self) -> HostAccess;

    /// Les fonctions publiées pour ce `subject`, appelées au montage uniquement.
    fn functions(&self, subject: &str) -> Vec<HostFunction>;
}

/// Choix des modules à monter : [`HostSelect::All`] pour une session interactive ou un step
/// batch (`FR-3`, `subject` = celui du déclencheur en batch, `lymaille#FR-10`) ;
/// [`HostSelect::Named`] pour un script, restreint à `script_grants.modules` (DSD `DEC-2`).
#[derive(Debug, Clone, Copy)]
pub enum HostSelect<'a> {
    /// Tous les modules enregistrés.
    All,
    /// Seulement ceux nommés dans l'ensemble ; un nom inconnu du registre est ignoré.
    Named(&'a BTreeSet<String>),
}

/// Valeur du montage : les modules d'hôte enregistrés par l'application, triés par nom
/// (précédent : `ResourceRegistry`, DSD `scripts-hote#DEC-4`). `Default` est un registre vide.
#[derive(Default)]
pub struct HostModules {
    modules: BTreeMap<&'static str, Arc<dyn HostModule>>,
}

impl HostModules {
    /// Registre vide.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Enregistre un module, rend `&mut Self` pour l'enchaînement.
    ///
    /// # Errors
    ///
    /// [`LymailleError::Config`] nommant le module si son nom est réservé
    /// ([`RESERVED_MODULE_NAMES`]), déjà enregistré, ou n'est pas un identifiant Rhai valide —
    /// une collision échoue au démarrage (DSD `scripts-hote#FR-1`).
    pub fn register(&mut self, module: Arc<dyn HostModule>) -> Result<&mut Self, LymailleError> {
        let name = module.name();
        if RESERVED_MODULE_NAMES.contains(&name)
            || self.modules.contains_key(name)
            || !is_valid_rhai_identifier(name)
        {
            return Err(LymailleError::Config(format!(
                "host module name reserved or taken: {name}"
            )));
        }
        self.modules.insert(name, module);
        Ok(self)
    }

    /// Le module enregistré sous `name`, ou `None`.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Arc<dyn HostModule>> {
        self.modules.get(name)
    }

    /// Les noms enregistrés, triés (`BTreeMap`).
    #[must_use]
    pub fn names(&self) -> Vec<&'static str> {
        self.modules.keys().copied().collect()
    }

    /// Les noms des modules `AdminOnly` — lus par le contrôle d'accord de modules
    /// (`identity::check_grants`, DSD `lymaille#FR-9`, `scripts-hote#FR-2`).
    #[must_use]
    pub fn admin_only_names(&self) -> Vec<&'static str> {
        self.modules
            .iter()
            .filter(|(_, module)| module.access() == HostAccess::AdminOnly)
            .map(|(name, _)| *name)
            .collect()
    }
}

/// Vrai si `name` est un identifiant Rhai valide : première lettre alphabétique ou `_`, puis
/// alphanumérique ou `_` (règle de `rhai::is_valid_identifier`, non exportée sans la feature
/// `internals` de `rhai`).
fn is_valid_rhai_identifier(name: &str) -> bool {
    let mut first_alphabetic = false;
    for ch in name.chars() {
        match ch {
            '_' => (),
            _ if ch.is_alphabetic() => first_alphabetic = true,
            _ if first_alphabetic && ch.is_alphanumeric() => (),
            _ => return false,
        }
    }
    first_alphabetic
}

/// Message Rhai fixe porté par `ErrorRuntime` (forme de `resource_module`).
fn runtime_error(message: &str) -> EvalAltResult {
    EvalAltResult::ErrorRuntime(Dynamic::from(message.to_string()), Position::NONE)
}

/// Panne d'infrastructure (pont refusé, `DbErr`) : détail en `tracing::warn!`, jamais de contenu
/// de ligne ou de session dans la trace ni dans le message rendu — `internal error`.
fn internal_error(detail: impl std::fmt::Display) -> EvalAltResult {
    tracing::warn!("lymaille host modules: {detail}");
    runtime_error("internal error")
}

/// L'enrobage d'une fonction d'hôte (DSD `scripts-hote#FR-2`) : avant chaque appel, le principal
/// du `subject` est relu en base via le pont `block_on` de `resource_module`, puis la politique
/// est appliquée. Aucun principal n'est gardé entre deux appels.
struct Guarded {
    db: DatabaseConnection,
    subject: String,
    access: HostAccess,
    call: HostCall,
}

impl PluginFunc for Guarded {
    fn call(
        &self,
        _context: Option<NativeCallContext>,
        args: &mut [&mut Dynamic],
    ) -> Result<Dynamic, Box<EvalAltResult>> {
        let principal = match block_on(caller_of(&self.db, &self.subject)) {
            // Refus du pont lui-même : panne d'infrastructure, trace et échec fermé.
            Err(bridge) => return Err(Box::new(internal_error(bridge))),
            Ok(Err(LymailleError::UnknownSubject)) => {
                return Err(Box::new(runtime_error("no account for this session")));
            }
            // `Store` (DbErr de la lecture du compte ou du groupe) et toute autre cause : détail
            // en trace expurgée, message fixe.
            Ok(Err(other)) => return Err(Box::new(internal_error(other))),
            Ok(Ok(principal)) => principal,
        };
        if self.access == HostAccess::AdminOnly && !principal.is_admin {
            return Err(Box::new(runtime_error("forbidden")));
        }
        let mut owned: Vec<Dynamic> = args.iter_mut().map(|arg| Dynamic::take(arg)).collect();
        (self.call)(&mut owned).map_err(|err| Box::new(runtime_error(err.message())))
    }

    fn is_method_call(&self) -> bool {
        false
    }

    fn has_context(&self) -> bool {
        false
    }

    /// L'enrobage lit la base et l'appel peut avoir des effets : jamais plié en constante.
    fn is_pure(&self) -> bool {
        false
    }

    fn is_volatile(&self) -> bool {
        true
    }
}

/// Monte les modules du registre pour `subject` : `All` pour une session interactive ou un step
/// batch (`FR-3`), `Named` pour un script (`script_grants.modules`, DSD `DEC-2`) — un nom inconnu
/// est ignoré. Chaque fonction est enregistrée en brut (enrobage [`Guarded`], `FR-2`) sous son
/// arité `params` : un appel d'une arité différente est une fonction inconnue pour Rhai. Le
/// montage ne lit pas la base ; la politique est relue à chaque appel, aucun principal en cache.
// signature contractuelle de ./host_module.sdd : `db` est passé par valeur, à la manière de
// `resources_module`, et cloné dans chaque enrobage — pas une référence.
#[allow(clippy::needless_pass_by_value)]
#[must_use]
pub fn mount_host_modules(
    registry: &HostModules,
    db: DatabaseConnection,
    subject: &str,
    select: HostSelect<'_>,
) -> Vec<(&'static str, Module)> {
    registry
        .modules
        .iter()
        .filter(|(name, _)| match select {
            HostSelect::All => true,
            HostSelect::Named(noms) => noms.iter().any(|nom| nom.as_str() == **name),
        })
        .map(|(name, module)| {
            let mut rhai_module = Module::new();
            for function in module.functions(subject) {
                let guarded = Guarded {
                    db: db.clone(),
                    subject: subject.to_string(),
                    access: module.access(),
                    call: function.call,
                };
                // Enregistrement brut à `params` emplacements de `Dynamic` (précédent
                // `resource_module` pour le pont, `FuncRegistration::set_into_module_raw` sous son nom de
                // l'API `FuncRegistration` en rhai 1.27) : l'arité porte la clé de résolution,
                // un appel d'une autre arité est `Function not found`.
                let _metadata = FuncRegistration::new(function.name).set_into_module_raw(
                    &mut rhai_module,
                    vec![TypeId::of::<Dynamic>(); function.params],
                    RhaiFunc::from(guarded),
                );
            }
            (*name, rhai_module)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migration::Migrator;
    use crate::users::membership::{self, sync_group_memberships};
    use crate::users::resolve_user;
    use rhai::{Dynamic, EvalAltResult};
    use sea_orm::{ColumnTrait, ConnectionTrait, Database, DatabaseConnection, EntityTrait, QueryFilter};
    use sea_orm_migration::MigratorTrait;
    use std::collections::BTreeSet;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Module factice `dashboard` (et ses variantes `a`/`b`) : une fonction `show(x)` d'arité 1
    /// qui rend `x` et compte ses exécutions — les Scenario prouvent que l'enrobage décide avant
    /// ou après, jamais la fonction elle-même.
    struct Fixture {
        name: &'static str,
        access: HostAccess,
        calls: Arc<AtomicUsize>,
    }

    impl HostModule for Fixture {
        fn name(&self) -> &'static str {
            self.name
        }
        fn access(&self) -> HostAccess {
            self.access
        }
        fn functions(&self, _subject: &str) -> Vec<HostFunction> {
            let calls = self.calls.clone();
            vec![HostFunction {
                name: "show",
                params: 1,
                call: Arc::new(move |args: &mut [Dynamic]| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(args.first().cloned().unwrap_or(Dynamic::UNIT))
                }),
            }]
        }
    }

    fn fixture(name: &'static str, access: HostAccess) -> (Arc<dyn HostModule>, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        (
            Arc::new(Fixture {
                name,
                access,
                calls: calls.clone(),
            }),
            calls,
        )
    }

    /// Base sqlite en mémoire migrée (précédent : `resource_module`).
    async fn migrated_db() -> DatabaseConnection {
        let db = Database::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite connects");
        Migrator::up(&db, None).await.expect("migrations apply cleanly");
        db
    }

    /// Moteur Rhai avec tous les modules du registre montés pour `subject` — exécution de
    /// scripts réels, jamais d'appel Rust contournant l'enrobage.
    fn engine_with(registry: &HostModules, db: DatabaseConnection, subject: &str) -> rhai::Engine {
        let mut engine = rhai::Engine::new();
        for (name, module) in mount_host_modules(registry, db, subject, HostSelect::All) {
            let _registered = engine.register_static_module(name, Arc::new(module));
        }
        engine
    }

    /// Texte de l'erreur d'un script qui doit échouer (forme de `resource_module`) : le message
    /// porté par `ErrorRuntime`, ou le `Display` de l'erreur de Rhai elle-même.
    fn error_text<T: std::fmt::Debug>(result: Result<T, Box<EvalAltResult>>) -> String {
        match result {
            Ok(value) => panic!("le script doit échouer, il a rendu {value:?}"),
            Err(error) => match *error {
                EvalAltResult::ErrorRuntime(value, _) => {
                    value.into_string().unwrap_or_else(ToString::to_string)
                }
                other => other.to_string(),
            },
        }
    }

    // ——— Scenario : « un nom réservé ou en double échoue à l'enregistrement » ———
    #[test]
    fn register_refuses_reserved_duplicate_and_invalid_names() {
        let mut registry = HostModules::default();
        assert!(
            registry.names().is_empty() && registry.admin_only_names().is_empty(),
            "`Default` est un registre vide"
        );

        let module = fixture("resources", HostAccess::Authenticated).0;
        assert!(
            matches!(&registry.register(module), Err(LymailleError::Config(message))
                if message == "host module name reserved or taken: resources"),
            "`resources` est réservé et n'entre pas dans le registre"
        );

        let module = fixture("dashboard", HostAccess::Authenticated).0;
        registry.register(module).expect("`dashboard` est accepté");

        let module = fixture("dashboard", HostAccess::AdminOnly).0;
        assert!(
            matches!(&registry.register(module), Err(LymailleError::Config(message))
                if message == "host module name reserved or taken: dashboard"),
            "le second `dashboard` est un doublon"
        );

        let module = fixture("bad name", HostAccess::Authenticated).0;
        assert!(
            matches!(&registry.register(module), Err(LymailleError::Config(message))
                if message == "host module name reserved or taken: bad name"),
            "`bad name` n'est pas un identifiant Rhai valide"
        );

        for name in RESERVED_MODULE_NAMES {
            let module = fixture(name, HostAccess::Authenticated).0;
            assert!(
                matches!(&registry.register(module), Err(LymailleError::Config(message))
                    if message == &format!("host module name reserved or taken: {name}")),
                "`{name}` est réservé"
            );
        }

        assert!(
            registry.get("dashboard").is_some(),
            "le module accepté est récupérable"
        );
        assert!(
            registry.get("resources").is_none(),
            "un refusé n'est jamais entré"
        );
        assert_eq!(registry.names(), vec!["dashboard"], "`names()` est trié");
    }

    // ——— Scenario : « un module réservé à l'admin est refusé après le retrait du groupe » ———
    #[tokio::test(flavor = "multi_thread")]
    async fn admin_only_module_is_refused_after_the_group_is_withdrawn() {
        let db = migrated_db().await;
        let alice = resolve_user(&db, "alice", None)
            .await
            .expect("le compte local d'alice");

        for mode in ["session interactive", "step batch (subject du déclencheur)"] {
            sync_group_memberships(&db, alice.id, &["admin".to_string()])
                .await
                .expect("alice membre du groupe admin");
            let (module, calls) = fixture("dashboard", HostAccess::AdminOnly);
            let mut registry = HostModules::new();
            registry.register(module).expect("`dashboard` s'enregistre");
            let engine = engine_with(&registry, db.clone(), "alice");

            // Arité différente de `params` : erreur Rhai de fonction inconnue, fonction non exécutée.
            let wrong_arity = engine.eval::<Dynamic>(r"dashboard::show()");
            let message = error_text(wrong_arity);
            assert!(
                message.contains("Function not found"),
                "une arité de 0 pour `params = 1` est une fonction inconnue ({mode}) : {message}"
            );
            assert_eq!(
                calls.load(Ordering::SeqCst),
                0,
                "la fonction de l'hôte n'est pas appelée"
            );

            // Premier appel : admin, il réussit.
            let first = engine
                .eval::<Dynamic>(r#"dashboard::show("ok")"#)
                .expect("un admin accède au module");
            assert_eq!(
                first
                    .clone()
                    .into_string()
                    .unwrap_or_else(|v| panic!("le témoin est rendu, obtenu : {v:?}")),
                "ok"
            );
            assert_eq!(
                calls.load(Ordering::SeqCst),
                1,
                "la fonction de l'hôte est appelée une fois"
            );

            // Retrait du groupe admin, puis rappel : la politique est relue, le principal n'est
            // jamais en cache.
            membership::Entity::delete_many()
                .filter(membership::Column::UserId.eq(alice.id))
                .exec(&db)
                .await
                .expect("retrait des appartenances d'alice");
            let second = engine.eval::<Dynamic>(r#"dashboard::show("ok")"#);
            assert_eq!(
                error_text(second),
                "forbidden",
                "le second appel, après retrait du groupe, échoue en `forbidden` ({mode})"
            );
            assert_eq!(
                calls.load(Ordering::SeqCst),
                1,
                "la fonction refusée n'est pas exécutée"
            );

            // Le montage d'un step batch (nouveau `mount_host_modules`, subject du déclencheur)
            // relit la même politique : pas de cache entre montages.
            let batch_engine = engine_with(&registry, db.clone(), "alice");
            let batch = batch_engine.eval::<Dynamic>(r#"dashboard::show("ok")"#);
            assert_eq!(
                error_text(batch),
                "forbidden",
                "le montage batch refuse aussi après le retrait ({mode})"
            );
        }
    }

    // ——— Scenario : « un subject sans compte est refusé » ———
    #[tokio::test(flavor = "multi_thread")]
    async fn subject_without_account_is_refused() {
        let db = migrated_db().await;
        let (module, calls) = fixture("dashboard", HostAccess::Authenticated);
        let mut registry = HostModules::new();
        registry.register(module).expect("`dashboard` s'enregistre");
        let engine = engine_with(&registry, db, "ghost");

        let outcome = engine.eval::<Dynamic>(r"dashboard::show(1)");
        assert_eq!(
            error_text(outcome),
            "no account for this session",
            "un `subject` sans compte est refusé par le message fixe"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "la fonction de l'hôte n'est pas exécutée"
        );
    }

    // ——— Scenario : « Named ne monte que les modules demandés et connus » ———
    #[tokio::test(flavor = "multi_thread")]
    async fn named_mounts_only_the_requested_known_modules() {
        let db = Database::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite connects");
        let mut registry = HostModules::new();
        let (a, _calls_a) = fixture("a", HostAccess::Authenticated);
        registry.register(a).expect("`a` s'enregistre");
        let (b, _calls_b) = fixture("b", HostAccess::AdminOnly);
        registry.register(b).expect("`b` s'enregistre");

        let demandes: BTreeSet<String> = ["a".to_string(), "inconnu".to_string()].into_iter().collect();
        let mounted = mount_host_modules(&registry, db.clone(), "alice", HostSelect::Named(&demandes));
        let names: Vec<&str> = mounted.iter().map(|(name, _)| *name).collect();
        assert_eq!(names, ["a"], "`Named` ne monte que le module demandé et connu");

        let mounted = mount_host_modules(&registry, db, "alice", HostSelect::All);
        let names: Vec<&str> = mounted.iter().map(|(name, _)| *name).collect();
        assert_eq!(
            names,
            ["a", "b"],
            "`All` monte tous les modules enregistrés, triés"
        );
        assert_eq!(
            registry.admin_only_names(),
            ["b"],
            "seul `b` est réservé à l'admin"
        );
    }

    // ——— Scenario : « une panne de lecture est un échec fermé sans détail » ———
    #[tokio::test(flavor = "multi_thread")]
    async fn read_failure_is_a_closed_internal_error_without_detail() {
        let db = migrated_db().await;
        let alice = resolve_user(&db, "alice", None)
            .await
            .expect("le compte local d'alice");
        sync_group_memberships(&db, alice.id, &["admin".to_string()])
            .await
            .expect("alice membre du groupe admin");
        // La lecture du groupe échoue : la table des appartenances est retirée sous la base.
        db.execute_unprepared("DROP TABLE miryad_group_memberships")
            .await
            .expect("la table des appartenances est retirée");

        let (module, calls) = fixture("dashboard", HostAccess::Authenticated);
        let mut registry = HostModules::new();
        registry.register(module).expect("`dashboard` s'enregistre");
        let engine = engine_with(&registry, db, "alice");

        let outcome = engine.eval::<Dynamic>(r"dashboard::show(1)");
        let message = error_text(outcome);
        assert_eq!(
            message, "internal error",
            "la panne de lecture est un échec fermé"
        );
        assert!(
            !message.contains("no such table") && !message.to_lowercase().contains("dberr"),
            "le message ne porte aucun détail de la `DbErr` : {message}"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "la fonction de l'hôte n'est pas exécutée"
        );
    }
}
