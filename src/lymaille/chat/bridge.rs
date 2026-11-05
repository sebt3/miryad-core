//! Pont Matrix ↔ sessions interactives (`src/lymaille/chat/bridge.sdd`).
//!
//! Une **salle** directe est reliée à une **session** du [`SessionManager`] : mêmes tours, mêmes
//! événements, mêmes droits que la surface REST+SSE — le pont n'a aucune logique de conversation
//! propre (DSD `DEC-6`, `DEC-26`). Le `sender` Matrix est résolu par l'annuaire (./link.sdd) ;
//! un mxid non lié n'a aucun droit. Les événements sont dédoublonnés par `event_id` en base
//! (bascule de bail) et les sorties longues découpées par ./render.sdd.

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use sea_orm::ConnectionTrait as _;
use sea_orm::DatabaseConnection;
use sea_orm::sea_query::Alias;
use sea_orm::sea_query::Expr;
use sea_orm::sea_query::ExprTrait as _;
use sea_orm::sea_query::OnConflict;
use sea_orm::sea_query::Query;

use crate::lymaille::error::LymailleError;
use crate::lymaille::identity::CallerPrincipal;
use crate::lymaille::identity::Identity;
use crate::lymaille::interactive::activation::activate_toolset;
use crate::lymaille::interactive::event::SessionEventKind;
use crate::lymaille::interactive::event::SessionOutcome;
use crate::lymaille::interactive::hub::Subscription;
use crate::lymaille::interactive::hub::SubscriptionError;
use crate::lymaille::interactive::manager::SessionManager;
use crate::matrix::client::MatrixClient;
use crate::matrix::client::MessageKind;
use crate::matrix::sync::MatrixEventHandler;
use crate::matrix::sync::MatrixMessage;

use super::link;
use super::render;

use ::lymaille::Clock;
use ::lymaille::SessionId;
use ::lymaille::SystemClock;
use ::lymaille::Timestamp;

/// Table d'affectation salle → session (`../migration.sdd`, table toujours présente sous
/// `lymaille`) : `room_id`, `subject`, `session_id`, `agent_id`, `updated_at`.
const ROOMS_TABLE: &str = "miryad_lymaille_matrix_rooms";

/// Table des événements vus, clé d'idempotence du pont.
const EVENTS_TABLE: &str = "miryad_lymaille_matrix_events";

/// Réponse d'un mxid non lié, au plus une par salle et par heure (`Must` de `./bridge.sdd`).
const INVITE_TEXT: &str = "Ce compte n'est pas lié : demandez un code sur l'API, puis `!link <code>`.";

/// Réponse fixe à un `!link` refusé (code, débit — jamais le détail).
const LINK_REFUSED: &str = "Liaison refusée.";

/// Réponse à un message dans une salle appartenant à un autre utilisateur (une seule fois).
const ROOM_TAKEN: &str = "Cette salle est liée à un autre utilisateur.";

/// Réponse à un texte alors qu'un tour est actif.
const TURN_ACTIVE: &str = "Un tour est en cours.";

/// Réponse d'un `!new` dont le contexte est inconnu ou inaccessible (`Scenario` R13).
const CONTEXT_UNAVAILABLE: &str = "Contexte inconnu ou inaccessible.";

/// Réponse d'un `!new` dont l'agent est hors de l'ensemble autorisé du type (`Scenario` R13).
const AGENT_REFUSED: &str = "Agent non autorisé pour ce contexte.";

/// Réponse d'un `Cancelled`.
const CANCELLED_TEXT: &str = "Annulé.";

/// Réponse d'une reprise sans avis.
const NOTHING_TO_REPORT: &str = "Rien à signaler.";

/// Réponse d'une annulation sans tour actif.
const NO_TURN: &str = "Aucun tour en cours.";

/// Réponse d'un `!resume` sans session repriseable (texte fixe, le détail `NotFound` reste dans
/// la trace).
const NOTHING_TO_RESUME: &str = "Aucune session à reprendre.";

/// Confirme un `!new` réussi.
const NEW_SESSION: &str = "Nouvelle session ouverte.";

/// Code rendu par `format_error` quand un tour se termine `Failed` — la variante
/// `LymailleError::Run` du moteur (`../error.rs`).
const RUN_FAILED_CODE: &str = "MRD-LYMAILLE-007";

/// Seuil d'âpre-première de l'élagage interne (une purge par heure au plus).
const PURGE_EVERY_SECS: u64 = 3600;

/// Fenêtre d'invitation par salle : au plus une par heure (`Must` point 3 de `./bridge.sdd`).
const INVITE_PERIOD_MS: u64 = 3_600_000;

/// Plafond de la mémoire des salles averties : au-delà, la mémoire repart de zéro (le lot 2 ne
/// persiste pas ces marques).
const WARNED_MAX: usize = 4096;

/// Ce que l'application règle sur le pont (`Must` de `./bridge.sdd`).
#[derive(Debug, Clone)]
pub struct ChatConfig {
    /// Contexte par défaut des ouvertures à la demande (texte libre sans `!new` préalable).
    pub default_agent: String,
    /// Au-delà, un événement est de l'historique : ignoré sans réponse (défaut 5 min).
    pub max_event_age: Duration,
    /// Fenêtre de dédoublonnage en base (défaut 7 jours).
    pub dedupe_window: Duration,
}

impl Default for ChatConfig {
    fn default() -> Self {
        Self {
            default_agent: String::new(),
            max_event_age: Duration::from_mins(5),
            dedupe_window: Duration::from_hours(168),
        }
    }
}

/// Envoi Matrix minimal du pont (`Must` de `./bridge.sdd`) : rendu par le client Matrix du
/// module `../../matrix/client.rs`, espionné en test — aucune dépendance directe à `matrix-sdk`
/// ici.
#[async_trait::async_trait]
pub(crate) trait ChatSender: Send + Sync {
    /// Envoie `body` dans `room_id` ; `txn_id` porte l'idempotence d'envoi ; rend l'identifiant
    /// de l'événement émis.
    async fn send(&self, room_id: &str, body: &str, txn_id: Option<&str>) -> Result<String, String>;

    /// Le mxid du bot, pour le redoublement du filtre anti-écho.
    fn own_user_id(&self) -> String;
}

/// Le pont lui-même (`./bridge.sdd`) : `new(db, manager, client, config)` — `Clone` : les clones
/// partagent les marques (avertissements, invitations, élagage) et le gestionnaire.
#[derive(Clone)]
pub struct ChatBridge {
    db: DatabaseConnection,
    manager: SessionManager,
    client: Arc<dyn ChatSender>,
    config: ChatConfig,
    clock: Arc<dyn Clock>,
    /// Salles déjà averties « occupée » (mémoire du processus, `Must not` de partage de salle).
    warned: Arc<Mutex<HashSet<String>>>,
    /// Instant de la dernière invitation par salle (au plus une par heure).
    invited: Arc<Mutex<HashMap<String, Timestamp>>>,
    /// Instant (millis) du dernier élagage de la table des événements vus.
    last_purge: Arc<AtomicU64>,
}

impl ChatBridge {
    /// Attache un pont aux stores, au gestionnaire de sessions, au client Matrix de la crate et à
    /// la configuration. Le trait `ChatSender` étant `pub(crate)` (`Must` de `./bridge.sdd`), le
    /// constructeur public reçoit le client concret — le montage sur le trait reste intérieur à
    /// la crate.
    #[must_use]
    pub fn new(
        db: DatabaseConnection,
        manager: SessionManager,
        client: Arc<MatrixClient>,
        config: ChatConfig,
    ) -> Self {
        Self::with_sender(db, manager, client, config)
    }

    /// Variante à `ChatSender` injecté (espions de test) — horloge système.
    #[must_use]
    pub(crate) fn with_sender(
        db: DatabaseConnection,
        manager: SessionManager,
        client: Arc<dyn ChatSender>,
        config: ChatConfig,
    ) -> Self {
        Self::with_clock(db, manager, client, config, Arc::new(SystemClock::new()))
    }

    /// Variante à horloge injectée (tests) — même montage que [`Self::new`].
    #[must_use]
    pub(crate) fn with_clock(
        db: DatabaseConnection,
        manager: SessionManager,
        client: Arc<dyn ChatSender>,
        config: ChatConfig,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            db,
            manager,
            client,
            config,
            clock,
            warned: Arc::new(Mutex::new(HashSet::new())),
            invited: Arc::new(Mutex::new(HashMap::new())),
            last_purge: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Le pont ne panique jamais : une erreur interne est tracée (sans contenu de message) et le
    /// silence retombe sur la salle.
    async fn dispatcher(&self, message: MatrixMessage) -> Result<(), String> {
        // Anti-écho (`Must not` de `./bridge.sdd`) : redoublement du filtre de `should_deliver`.
        if message.sender == self.client.own_user_id() {
            return Ok(());
        }
        let now_ms = self.clock.now().as_millis();
        // 1. Dédoublonnement par `event_id` en base, puis élagage opportuniste (`Must` point 1).
        if !self.marquer_vu(&message.event_id, now_ms).await? {
            return Ok(());
        }
        self.purge_opportune(now_ms).await;
        let max_age = u64::try_from(self.config.max_event_age.as_millis()).unwrap_or(u64::MAX);
        if now_ms.saturating_sub(message.timestamp_ms) > max_age {
            tracing::debug!(room_id = %message.room_id, "lymaille pont: événement d'historique ignoré");
            return Ok(());
        }
        let body = message.body.trim();
        // 2. `!link` avant tout autre droit (`Must` point 2) : le mxid non lié peut s'en saisir.
        if let Some(code) = body.strip_prefix("!link").map(str::trim) {
            return self.liaison(&message.room_id, &message.sender, code).await;
        }
        // 3. Identité : annuaire puis principal reconstruit à chaque message (`Must` point 3).
        let sujet = match link::resolve(&self.db, &message.sender).await {
            Ok(Some(sujet)) => sujet,
            Ok(None) => return self.inviter(&message.room_id, now_ms).await,
            Err(erreur) => return Err(tracer(erreur, "annuaire Matrix")),
        };
        let appelant = match Identity::new(self.db.clone()).resolve(&sujet).await {
            Ok(appelant) => appelant,
            Err(erreur) => {
                // Compte supprimé ou inconnu : plus de droit, l'invitation reprend la main.
                tracing::debug!(error = %erreur, "lymaille pont: principal indisponible");
                return self.inviter(&message.room_id, now_ms).await;
            }
        };
        // Salle sous seul utilisateur lié (`Must` tables) : un autre propriétaire éconduit, une
        // seule fois.
        let salle = self.lire_salle(&message.room_id).await?;
        if let Some(salle) = &salle
            && salle.sujet != sujet
        {
            let premiere = {
                let mut marques = garde(&self.warned);
                if marques.len() >= WARNED_MAX {
                    marques.clear();
                }
                marques.insert(message.room_id.clone())
            };
            if premiere {
                self.dire(&message.room_id, ROOM_TAKEN).await?;
            }
            return Ok(());
        }
        let session = salle.as_ref().and_then(|salle| salle.session.clone());
        // 4. Commandes (`Must` point 4, R13), puis 5. texte libre (`Must` point 5).
        if let Some(commande) = body.strip_prefix('!') {
            let (nom, reste) = match commande.split_once(char::is_whitespace) {
                Some((nom, reste)) => (nom, reste.trim()),
                None => (commande, ""),
            };
            match nom {
                "help" => self.dire(&message.room_id, &render::help_text()).await,
                "new" => self.nouvelle_session(&sujet, &message.room_id, reste).await,
                "cancel" => self.annuler(&sujet, &message.room_id, session.as_ref()).await,
                "resume" => self.reprendre(&sujet, &message.room_id, session.as_ref()).await,
                "activate" => self.activer(&message.room_id, &appelant, reste).await,
                autre => {
                    tracing::debug!(command = autre, "lymaille pont: commande inconnue ignorée");
                    Ok(())
                }
            }
        } else {
            self.texte_libre(&sujet, session.as_ref(), &message).await
        }
    }

    /// `!link <code>` (`Must` point 2 de `./bridge.sdd`) : succès (idempotent si déjà lié au même
    /// sujet) « Compte lié. », échec par message fixe sans détail.
    async fn liaison(&self, room_id: &str, mxid: &str, code: &str) -> Result<(), String> {
        let resultat = if code.is_empty() {
            Err(LymailleError::Forbidden("no code presented".to_owned()))
        } else {
            link::consume_code(&self.db, mxid, code, chrono::Utc::now())
                .await
                .map(|_| ())
        };
        match resultat {
            Ok(()) => self.dire(room_id, "Compte lié.").await,
            Err(erreur) => {
                tracing::debug!(error = %erreur, "lymaille pont: !link refusé");
                self.dire(room_id, LINK_REFUSED).await
            }
        }
    }

    /// Invitation à lier, au plus une par salle et par heure (`Must` point 3 de `./bridge.sdd`).
    async fn inviter(&self, room_id: &str, now_ms: u64) -> Result<(), String> {
        let autorisee = {
            let mut dernieres = garde(&self.invited);
            let deja_faisable = dernieres
                .get(room_id)
                .is_some_and(|t| now_ms.saturating_sub(t.as_millis()) < INVITE_PERIOD_MS);
            if deja_faisable {
                false
            } else {
                dernieres.insert(room_id.to_owned(), Timestamp::from_millis(now_ms));
                true
            }
        };
        if autorisee {
            self.dire(room_id, INVITE_TEXT).await
        } else {
            Ok(())
        }
    }

    /// `!new <contexte> [agent]` (`Tasks` R13 de `./bridge.sdd`, DSD `DEC-28`) : ferme la session
    /// de la salle, en ouvre une sur le contexte choisi — la nature vient du type du contexte.
    async fn nouvelle_session(&self, sujet: &str, room_id: &str, reste: &str) -> Result<(), String> {
        let mut mots = reste.split_whitespace();
        let contexte = mots.next().unwrap_or(self.config.default_agent.as_str());
        let agent = mots.next();
        if let Some(salle) = self.lire_salle(room_id).await?
            && let Some(session) = salle.session
        {
            self.manager.close(sujet, &session).await;
        }
        match self.manager.open(sujet, contexte, agent).await {
            Ok(anse) => {
                self.ecrire_salle(room_id, sujet, Some(&anse.id), Some(&anse.agent_id))
                    .await?;
                self.dire(room_id, NEW_SESSION).await
            }
            Err(erreur) => self.dire(room_id, &texte_erreur(&erreur)).await,
        }
    }

    /// `!cancel` : coopère avec le tour en cours ; c'est la collecte qui rendra « Annulé. ».
    async fn annuler(&self, sujet: &str, room_id: &str, session: Option<&SessionId>) -> Result<(), String> {
        let actif = match session {
            Some(id) => self
                .manager
                .cancel(sujet, id)
                .await
                .map_err(|erreur| tracer(erreur, "annulation"))?,
            None => false,
        };
        if actif {
            Ok(())
        } else {
            self.dire(room_id, NO_TURN).await
        }
    }

    /// `!resume` : reprend la session persistée de la salle et rend ses avis de provenance.
    async fn reprendre(&self, sujet: &str, room_id: &str, session: Option<&SessionId>) -> Result<(), String> {
        let Some(id) = session else {
            return self.dire(room_id, NOTHING_TO_RESUME).await;
        };
        match self.manager.resume(sujet, id).await {
            Ok(anse) => {
                self.ecrire_salle(room_id, sujet, Some(&anse.id), Some(&anse.agent_id))
                    .await?;
                let texte =
                    render::format_notice(&anse.notices).unwrap_or_else(|| NOTHING_TO_REPORT.to_owned());
                self.dire(room_id, &texte).await
            }
            Err(erreur) => self.dire(room_id, &texte_erreur(&erreur)).await,
        }
    }

    /// `!activate <toolset>` : l'activation est rendue en une ligne (`Must` point 4).
    async fn activer(&self, room_id: &str, appelant: &CallerPrincipal, toolset: &str) -> Result<(), String> {
        if toolset.is_empty() {
            return self.dire(room_id, "Usage : !activate <toolset>").await;
        }
        match activate_toolset(&self.db, appelant, toolset).await {
            Ok(rapport) => {
                let ligne = match &rapport.previous_version {
                    Some(avant) => format!(
                        "Toolset {} : version {} (remplace {}).",
                        rapport.name, rapport.version, avant
                    ),
                    None => format!("Toolset {} : version {}.", rapport.name, rapport.version),
                };
                self.dire(room_id, &ligne).await
            }
            Err(erreur) => self.dire(room_id, &texte_erreur(&erreur)).await,
        }
    }

    /// Texte libre (`Must` point 5) : session de la salle, ouverte à la demande sur le contexte
    /// par défaut, restaurée si persistée ; abonnement au hub **avant** l'envoi (`Must` point 6).
    async fn texte_libre(
        &self,
        sujet: &str,
        session: Option<&SessionId>,
        message: &MatrixMessage,
    ) -> Result<(), String> {
        let id = match session {
            Some(id) => id.to_owned(),
            None => match self.manager.open(sujet, &self.config.default_agent, None).await {
                Ok(anse) => {
                    self.ecrire_salle(&message.room_id, sujet, Some(&anse.id), Some(&anse.agent_id))
                        .await?;
                    let abonnement = self
                        .manager
                        .subscribe(sujet, &anse.id)
                        .await
                        .map_err(|erreur| tracer(erreur, "abonnement au hub"))?;
                    self.manager
                        .send(sujet, &anse.id, message.body.clone(), None)
                        .await
                        .map_err(|erreur| tracer(erreur, "envoi de tour"))?;
                    return self
                        .collecte(abonnement, &message.room_id, &message.event_id)
                        .await;
                }
                Err(erreur) => {
                    return self.dire(&message.room_id, &texte_erreur(&erreur)).await;
                }
            },
        };
        let abonnement = self
            .manager
            .subscribe(sujet, &id)
            .await
            .map_err(|erreur| tracer(erreur, "abonnement au hub"))?;
        match self.manager.send(sujet, &id, message.body.clone(), None).await {
            Ok(()) => {
                self.collecte(abonnement, &message.room_id, &message.event_id)
                    .await
            }
            Err(LymailleError::NotFound(_)) => {
                // Session persistée mais plus en mémoire (redémarrage du moteur) : reprise, puis
                // renvoi ; si la reprise échoue, réouverture à la demande.
                match self.manager.resume(sujet, &id).await {
                    Ok(_) => match self.manager.send(sujet, &id, message.body.clone(), None).await {
                        Ok(()) => {
                            self.collecte(abonnement, &message.room_id, &message.event_id)
                                .await
                        }
                        Err(LymailleError::Conflict(_)) => self.dire(&message.room_id, TURN_ACTIVE).await,
                        Err(erreur) => self.dire(&message.room_id, &texte_erreur(&erreur)).await,
                    },
                    Err(_) => match self.manager.open(sujet, &self.config.default_agent, None).await {
                        Ok(anse) => {
                            self.ecrire_salle(&message.room_id, sujet, Some(&anse.id), Some(&anse.agent_id))
                                .await?;
                            let abonnement = self
                                .manager
                                .subscribe(sujet, &anse.id)
                                .await
                                .map_err(|erreur| tracer(erreur, "abonnement au hub"))?;
                            self.manager
                                .send(sujet, &anse.id, message.body.clone(), None)
                                .await
                                .map_err(|erreur| tracer(erreur, "envoi de tour"))?;
                            self.collecte(abonnement, &message.room_id, &message.event_id)
                                .await
                        }
                        Err(erreur) => self.dire(&message.room_id, &texte_erreur(&erreur)).await,
                    },
                }
            }
            Err(LymailleError::Conflict(_)) => self.dire(&message.room_id, TURN_ACTIVE).await,
            Err(erreur) => self.dire(&message.room_id, &texte_erreur(&erreur)).await,
        }
    }

    /// Collecte de la réponse (`Must` point 6) : à `turn_ended`, `split_message` est envoyé
    /// partie par partie (`txn_id` = `event_id` + rang) ; `Failed` → `format_error` ;
    /// `Cancelled` → « Annulé. ».
    async fn collecte(
        &self,
        mut abonnement: Subscription,
        room_id: &str,
        event_id: &str,
    ) -> Result<(), String> {
        loop {
            match abonnement.recv().await {
                Ok(eventement) => {
                    let SessionEventKind::TurnEnded {
                        outcome, response, ..
                    } = eventement.kind
                    else {
                        continue;
                    };
                    match outcome {
                        SessionOutcome::Valid => {
                            for (rang, partie) in render::split_message(&response).iter().enumerate() {
                                let txn_id = format!("{event_id}-{rang}");
                                self.client
                                    .send(room_id, partie, Some(&txn_id))
                                    .await
                                    .map_err(|panne| format!("envoi Matrix: {panne}"))?;
                            }
                            return Ok(());
                        }
                        SessionOutcome::Failed => {
                            return self.dire(room_id, &render::format_error(RUN_FAILED_CODE)).await;
                        }
                        SessionOutcome::Cancelled => {
                            return self.dire(room_id, CANCELLED_TEXT).await;
                        }
                    }
                }
                Err(SubscriptionError::Lagged(perdus)) => {
                    tracing::debug!(
                        missed = perdus,
                        "lymaille pont: file d'événements débordée, la suite est lue"
                    );
                }
                Err(SubscriptionError::Closed) => {
                    return Err("abonnement au hub fermé sans fin de tour".to_owned());
                }
            }
        }
    }

    /// Insère l'`event_id` dans la table des événements vus ; `false` s'il y figurait déjà
    /// (`Must` point 1 de `./bridge.sdd`).
    async fn marquer_vu(&self, event_id: &str, now_ms: u64) -> Result<bool, String> {
        let vu = chrono::DateTime::from_timestamp_millis(i64::try_from(now_ms).unwrap_or(i64::MAX))
            .unwrap_or_else(chrono::Utc::now);
        let insertion = Query::insert()
            .into_table(Alias::new(EVENTS_TABLE))
            .columns(["event_id", "seen_at"])
            .values(vec![Expr::val(event_id), Expr::val(vu)])
            .map_err(|panne| format!("requête de dédoublonnement: {panne}"))?
            .on_conflict(OnConflict::column(Alias::new("event_id")).do_nothing().to_owned())
            .to_owned();
        let fait = self
            .db
            .execute(&insertion)
            .await
            .map_err(|panne| format!("dédoublonnement Matrix: {panne}"))?;
        Ok(fait.rows_affected() > 0)
    }

    /// Élagage opportun (`Must` dernier point) : au plus une fois par heure, la table des
    /// événements vus au-delà de `dedupe_window` et les codes de liaison expirés.
    async fn purge_opportune(&self, now_ms: u64) {
        let seuil = now_ms.saturating_sub(PURGE_EVERY_SECS.saturating_mul(1000));
        let dernier = self.last_purge.load(Ordering::SeqCst);
        if dernier > seuil
            || self
                .last_purge
                .compare_exchange(dernier, now_ms, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
        {
            return;
        }
        let secondes =
            i64::try_from(self.config.dedupe_window.as_millis().saturating_div(1_000)).unwrap_or(i64::MAX);
        let avant = chrono::Utc::now()
            .checked_sub_signed(chrono::Duration::seconds(secondes))
            .unwrap_or_else(chrono::Utc::now);
        let mut epurge = Query::delete();
        epurge
            .from_table(Alias::new(EVENTS_TABLE))
            .and_where(Expr::col(Alias::new("seen_at")).lt(avant));
        match self.db.execute(&epurge).await {
            Ok(fait) => {
                tracing::debug!(
                    rows = fait.rows_affected(),
                    "lymaille pont: événements vus élagués"
                );
            }
            Err(panne) => {
                tracing::warn!(
                    error = %panne,
                    "lymaille pont: élagage des événements vus échoué"
                );
            }
        }
        match link::purge_expired(&self.db, chrono::Utc::now()).await {
            Ok(retires) => tracing::debug!(rows = retires, "lymaille pont: codes de liaison expirés purgés"),
            Err(erreur) => {
                tracing::warn!(
                    error = %erreur,
                    "lymaille pont: purge des codes échouée"
                );
            }
        }
    }

    /// Ligne de la salle (`Must` tables de `./bridge.sdd`), `None` si jamais suivie.
    async fn lire_salle(&self, room_id: &str) -> Result<Option<Salle>, String> {
        let mut select = Query::select();
        select
            .columns([Alias::new("subject"), Alias::new("session_id")])
            .from(Alias::new(ROOMS_TABLE))
            .and_where(Expr::col(Alias::new("room_id")).eq(room_id));
        let Some(ligne) = self
            .db
            .query_one(&select)
            .await
            .map_err(|panne| format!("lecture de salle: {panne}"))?
        else {
            return Ok(None);
        };
        let sujet: Option<String> = ligne
            .try_get("", "subject")
            .map_err(|panne| format!("lecture de salle: {panne}"))?;
        let session: Option<String> = ligne
            .try_get("", "session_id")
            .map_err(|panne| format!("lecture de salle: {panne}"))?;
        Ok(sujet.map(|sujet| Salle {
            sujet,
            session: session.as_deref().and_then(SessionId::new),
        }))
    }

    /// Revendique ou rafraîchit la ligne de la salle (`Must` tables) : un seul utilisateur lié,
    /// sa session et son agent courants.
    async fn ecrire_salle(
        &self,
        room_id: &str,
        sujet: &str,
        session: Option<&SessionId>,
        agent: Option<&str>,
    ) -> Result<(), String> {
        let mut update = Query::update();
        update
            .table(Alias::new(ROOMS_TABLE))
            .value(Alias::new("subject"), Expr::val(sujet.to_owned()))
            .value(
                Alias::new("session_id"),
                session.map_or_else(Expr::null, |s| Expr::val(s.as_str().to_owned())),
            )
            .value(
                Alias::new("agent_id"),
                agent.map_or_else(Expr::null, |a| Expr::val(a.to_owned())),
            )
            .value(Alias::new("updated_at"), Expr::val(chrono::Utc::now()))
            .and_where(Expr::col(Alias::new("room_id")).eq(room_id));
        let fait = self
            .db
            .execute(&update)
            .await
            .map_err(|panne| format!("mise à jour de salle: {panne}"))?;
        if fait.rows_affected() > 0 {
            return Ok(());
        }
        let insertion = Query::insert()
            .into_table(Alias::new(ROOMS_TABLE))
            .columns(["room_id", "subject", "session_id", "agent_id", "updated_at"])
            .values(vec![
                Expr::val(room_id.to_owned()),
                Expr::val(sujet.to_owned()),
                session.map_or_else(Expr::null, |s| Expr::val(s.as_str().to_owned())),
                agent.map_or_else(Expr::null, |a| Expr::val(a.to_owned())),
                Expr::val(chrono::Utc::now()),
            ])
            .map_err(|panne| format!("requête de salle: {panne}"))?
            .to_owned();
        self.db
            .execute(&insertion)
            .await
            .map_err(|panne| format!("revendication de salle: {panne}"))?;
        Ok(())
    }

    /// Un message texte du pont vers une salle (un texte fixe, ou la réponse d'un tour — jamais
    /// un détail interne).
    async fn dire(&self, room_id: &str, texte: &str) -> Result<(), String> {
        self.client
            .send(room_id, texte, None)
            .await
            .map(|_| ())
            .map_err(|panne| format!("envoi Matrix: {panne}"))
    }
}

/// Ligne lue de `miryad_lymaille_matrix_rooms` (`Must` tables de `./bridge.sdd`) — l'`agent_id`
/// de la colonne n'est lu qu'à l'écriture.
struct Salle {
    sujet: String,
    session: Option<SessionId>,
}

/// Garde d'un mutex : une tâche panicque ne doit pas figer le pont — la valeur est reprise telle
/// quelle (même motif que `../interactive/manager.rs`).
fn garde<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Erreur interne mise en trace et rendue en chaîne — le détail n'est jamais cité à la salle
/// (`Must` dernier point de `./bridge.sdd`).
fn tracer(erreur: impl std::fmt::Display, contexte: &str) -> String {
    let texte = erreur.to_string();
    tracing::warn!(error = %texte, "lymaille pont: {contexte}");
    texte
}

/// Traduction d'une erreur de gestionnaire en texte de salle (`Tasks` R13) : les deux textes
/// indiscernables de l'accès, sinon `format_error` avec le code de la variante (`../error.rs`).
fn texte_erreur(erreur: &LymailleError) -> String {
    match erreur {
        LymailleError::NotFound(_) => CONTEXT_UNAVAILABLE.to_owned(),
        LymailleError::Forbidden(_) => AGENT_REFUSED.to_owned(),
        other => render::format_error(code_de(other)),
    }
}

/// Code `MRD-LYMAILLE-NNN` de la variante (`../error.rs`) — le message n'est jamais cité.
fn code_de(erreur: &LymailleError) -> &'static str {
    match erreur {
        LymailleError::UnknownSubject => "MRD-LYMAILLE-001",
        LymailleError::Forbidden(_) => "MRD-LYMAILLE-002",
        LymailleError::ReservedPrefix(_) => "MRD-LYMAILLE-003",
        LymailleError::InvalidDefinition(_) => "MRD-LYMAILLE-004",
        LymailleError::SecretUnresolved(_) => "MRD-LYMAILLE-005",
        LymailleError::Inference(_) => "MRD-LYMAILLE-006",
        LymailleError::Run { .. } => "MRD-LYMAILLE-007",
        LymailleError::Config(_) => "MRD-LYMAILLE-008",
        LymailleError::Store(_) => "MRD-LYMAILLE-009",
        LymailleError::NotFound(_) => "MRD-LYMAILLE-010",
        LymailleError::Conflict(_) => "MRD-LYMAILLE-011",
        LymailleError::Serialization(_) => "MRD-LYMAILLE-012",
    }
}

#[async_trait::async_trait]
impl MatrixEventHandler for ChatBridge {
    async fn on_message(&self, message: MatrixMessage) {
        if let Err(erreur) = self.dispatcher(message).await {
            tracing::warn!(error = %erreur, "lymaille pont: message non traité");
        }
    }
}

/// Le `ChatSender` de production : l'envoi texte simple du client Matrix de
/// `../../matrix/client.rs`.
#[async_trait::async_trait]
impl ChatSender for MatrixClient {
    async fn send(&self, room_id: &str, body: &str, txn_id: Option<&str>) -> Result<String, String> {
        MatrixClient::send(self, room_id, body, None, MessageKind::Text, txn_id)
            .await
            .map_err(|panne| panne.to_string())
    }

    fn own_user_id(&self) -> String {
        self.user_id()
    }
}

#[cfg(test)]
mod tests {
    use super::{ChatBridge, ChatConfig, ChatSender, EVENTS_TABLE, ROOMS_TABLE};
    use crate::lymaille::chat::link;
    use crate::lymaille::identity::Identity;
    use crate::lymaille::interactive::assemble::AssembleDeps;
    use crate::lymaille::interactive::assemble::DynInference;
    use crate::lymaille::interactive::assemble::InferenceFactory;
    use crate::lymaille::interactive::assemble::McpConnectorFactory;
    use crate::lymaille::interactive::hub::EventHub;
    use crate::lymaille::interactive::manager::Limits;
    use crate::lymaille::interactive::manager::ManagerDeps;
    use crate::lymaille::interactive::manager::SessionManager;
    use crate::lymaille::resource_module::ResourceReaders;
    use crate::lymaille::secret::SecretResolver;
    use crate::lymaille::store::LymailleStores;
    use crate::lymaille::store::test_support::test_db;
    use crate::matrix::sync::MatrixEventHandler as _;
    use crate::matrix::sync::MatrixMessage;
    use crate::users::group;
    use crate::users::membership::sync_group_memberships;
    use crate::users::resolve_user;

    use sea_orm::ColumnTrait as _;
    use sea_orm::ConnectionTrait as _;
    use sea_orm::DatabaseConnection;
    use sea_orm::EntityTrait as _;
    use sea_orm::QueryFilter as _;
    use sea_orm::sea_query::Alias;
    use sea_orm::sea_query::Expr;
    use sea_orm::sea_query::ExprTrait as _;
    use sea_orm::sea_query::Query;

    use ::lymaille::AgentDefinition;
    use ::lymaille::AgentKind;
    use ::lymaille::AgentStore as _;
    use ::lymaille::Clock;
    use ::lymaille::ContextLevel;
    use ::lymaille::ContextType;
    use ::lymaille::ErrorClass;
    use ::lymaille::InferenceError;
    use ::lymaille::InferenceResponse;
    use ::lymaille::Message;
    use ::lymaille::ModelProfile;
    use ::lymaille::ProfileStore as _;
    use ::lymaille::Provider;
    use ::lymaille::ProviderLevel;
    use ::lymaille::ProviderStore as _;
    use ::lymaille::ScriptedProvider;
    use ::lymaille::Timestamp;
    use ::lymaille::ToolsetContext;
    use ::lymaille::TurnGate;
    use serde_json::json;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
    use std::time::Duration;
    use vynil_core::Semver;
    use zeroize::Zeroizing;

    /// Horloge injectée : les tests posent l'instant, le pont ne voit jamais l'heure système.
    pub struct FixedClock(AtomicU64);

    impl FixedClock {
        fn at(millis: u64) -> Arc<Self> {
            Arc::new(Self(AtomicU64::new(millis)))
        }
    }

    impl Clock for FixedClock {
        fn now(&self) -> Timestamp {
            Timestamp::from_millis(self.0.load(AtomicOrdering::SeqCst))
        }
    }

    /// Horloge système à la millise près pour les scénarios sans contrainte de temps.
    fn wall_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
    }

    /// Espion du pont : chaque `send` est mémorisé, aucun réseau.
    pub struct Spy {
        pub sent: Mutex<Vec<(String, String)>>,
        pub counter: AtomicU64,
    }

    impl Spy {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                sent: Mutex::new(Vec::new()),
                counter: AtomicU64::new(0),
            })
        }
        fn bodies(&self) -> Vec<String> {
            self.sent
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .map(|(_, body)| body.clone())
                .collect()
        }
        fn count(&self) -> usize {
            self.sent
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len()
        }
    }

    #[async_trait::async_trait]
    impl ChatSender for Spy {
        async fn send(&self, room_id: &str, body: &str, _txn_id: Option<&str>) -> Result<String, String> {
            let rang = self.counter.fetch_add(1, AtomicOrdering::SeqCst);
            self.sent
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((room_id.to_owned(), body.to_owned()));
            Ok(format!("$sent-{rang}"))
        }
        fn own_user_id(&self) -> String {
            "@bot:srv".to_owned()
        }
    }

    pub struct ScriptFactory(pub Arc<ScriptedProvider>);

    impl InferenceFactory for ScriptFactory {
        fn make(&self, _address: String, _api_key: Option<String>) -> Arc<dyn DynInference> {
            let provider: Arc<dyn DynInference> = self.0.clone();
            provider
        }
    }

    pub fn final_text(text: &str) -> InferenceResponse {
        InferenceResponse::new(Message::assistant(text.to_owned(), None, Vec::new()), None)
    }

    pub struct HangFactory(pub Arc<Hang>);

    pub struct Hang {
        pub entered: std::sync::atomic::AtomicBool,
        pub release: tokio::sync::Semaphore,
    }

    impl Hang {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                entered: std::sync::atomic::AtomicBool::new(false),
                release: tokio::sync::Semaphore::new(0),
            })
        }
    }

    impl InferenceFactory for HangFactory {
        fn make(&self, _address: String, _api_key: Option<String>) -> Arc<dyn DynInference> {
            Arc::new(HangInference(Arc::clone(&self.0)))
        }
    }

    struct HangInference(Arc<Hang>);

    impl DynInference for HangInference {
        fn complete_dyn(
            &self,
            _request: ::lymaille::InferenceRequest,
        ) -> std::pin::Pin<Box<dyn Future<Output = Result<InferenceResponse, InferenceError>> + Send + '_>>
        {
            let hang = Arc::clone(&self.0);
            Box::pin(async move {
                hang.entered.store(true, AtomicOrdering::SeqCst);
                match hang.release.acquire().await {
                    Ok(_permit) => Ok(final_text("libere")),
                    Err(_closed) => Ok(final_text("libere")),
                }
            })
        }
    }

    pub struct NoMcp;

    impl McpConnectorFactory for NoMcp {
        fn make(
            &self,
            _url: &str,
            _request_timeout: Duration,
            _connect_timeout: Duration,
            _auth: Option<Arc<dyn crate::lymaille::interactive::assemble::DynMcpAuth>>,
        ) -> Arc<dyn crate::lymaille::interactive::assemble::DynMcpConnector> {
            Arc::new(NoConnector)
        }
    }

    struct NoConnector;

    impl ::lymaille::McpLister for NoConnector {
        fn list_tools(
            &self,
        ) -> impl Future<Output = Result<Vec<::lymaille::McpTool>, ::lymaille::McpError>> + Send {
            std::future::ready(Ok(Vec::new()))
        }
    }

    impl ::lymaille::McpConnector for NoConnector {
        fn call_tool(
            &self,
            _name: &str,
            _arguments: &serde_json::Value,
        ) -> impl Future<Output = Result<serde_json::Value, ::lymaille::McpError>> + Send {
            std::future::ready(Ok(json!({})))
        }

        fn reconnect(&self) -> impl Future<Output = ()> + Send {
            std::future::ready(())
        }
    }

    pub struct FixedSecret(pub &'static str);

    impl SecretResolver for FixedSecret {
        fn resolve(&self, reference: &str) -> Result<Zeroizing<String>, crate::lymaille::LymailleError> {
            if reference == "FIXED_KEY" {
                Ok(Zeroizing::new(self.0.to_owned()))
            } else {
                Err(crate::lymaille::LymailleError::SecretUnresolved(
                    reference.to_owned(),
                ))
            }
        }
    }

    async fn compte(db: &DatabaseConnection, subject: &str, admin: bool) {
        let account = resolve_user(db, subject, None)
            .await
            .expect("compte local de test cree");
        if admin {
            sync_group_memberships(db, account.id, &[group::ADMIN_GROUP_NAME.to_owned()])
                .await
                .expect("appartenance au groupe admin");
        }
    }

    async fn wire_catalog(db: &DatabaseConnection) {
        compte(db, "root", true).await;
        let stores = LymailleStores::new(db.clone(), "root".to_owned());
        stores
            .profiles()
            .put_profile(
                &ModelProfile::new("qwen".to_owned(), "local".to_owned(), "qwen-test".to_owned())
                    .with_context_size(262_144),
            )
            .await
            .expect("l'admin ecrit le profil");
        stores
            .providers()
            .put_provider(
                &Provider::new(
                    "local".to_owned(),
                    "openai".to_owned(),
                    "http://inference.invalid/v1".to_owned(),
                    Some("FIXED_KEY".to_owned()),
                    ProviderLevel::User,
                )
                .expect("des references simples font un provider"),
            )
            .await
            .expect("l'admin ecrit le provider");
    }

    async fn put_agent(db: &DatabaseConnection, owner: &str, definition: &AgentDefinition) {
        LymailleStores::new(db.clone(), owner.to_owned())
            .agents()
            .put_agent(definition)
            .await
            .expect("le proprietaire ecrit son agent");
    }

    fn agent(id: &str, kind: AgentKind) -> AgentDefinition {
        AgentDefinition::new(id.to_owned(), kind, "le prompt".to_owned(), "qwen".to_owned())
            .with_modules(vec!["resources".to_owned()])
    }

    async fn put_type(
        db: &DatabaseConnection,
        name: &str,
        session_kind: Option<::lymaille::SessionKind>,
        default_agent: Option<&str>,
        allowed: &[&str],
    ) {
        let context_type = ContextType::new(
            name.to_owned(),
            Semver::parse("1.0.0").expect("la version de fixture se parse"),
            json!({}),
        )
        .expect("le type de fixture se construit");
        let context_type = match session_kind {
            Some(kind) => context_type
                .with_session_kind(kind)
                .expect("la nature de fixture se pose"),
            None => context_type,
        };
        let context_type = match default_agent {
            Some(agent) => context_type
                .with_default_agent(agent.to_owned())
                .expect("l'agent par defaut de fixture se pose"),
            None => context_type,
        };
        let active = serde_json::to_value(context_type).expect("le type se serialise");
        let mut insert = Query::insert();
        insert
            .into_table(Alias::new("miryad_lymaille_catalog_context_types"))
            .columns([
                "name",
                "version",
                "allowed_agents",
                "active",
                "draft",
                "created_at",
            ])
            .values(vec![
                Expr::val(name),
                Expr::val("1.0.0"),
                Expr::val(serde_json::Value::Array(
                    allowed.iter().map(|agent| json!(*agent)).collect(),
                )),
                Expr::val(active),
                Expr::null(),
                Expr::val(chrono::Utc::now()),
            ])
            .expect("les colonnes et valeurs de fixture correspondent");
        db.execute(&insert).await.expect("ligne de type inseree");
    }

    async fn inserer_contexte_prive(
        db: &DatabaseConnection,
        owner_id: i32,
        name: &str,
        context_type: Option<&str>,
    ) {
        let context = ToolsetContext::new(name.to_owned(), ContextLevel::User)
            .expect("le contexte de fixture se construit");
        let value = serde_json::to_value(context).expect("le contexte se serialise");
        let mut insert = Query::insert();
        insert
            .into_table(Alias::new("miryad_lymaille_user_contexts"))
            .columns(["owner_id", "name", "context", "context_type", "created_at"])
            .values(vec![
                Expr::val(owner_id),
                Expr::val(name),
                Expr::val(value),
                context_type.map_or_else(Expr::null, |name| Expr::val(name.to_owned())),
                Expr::val(chrono::Utc::now()),
            ])
            .expect("les colonnes et valeurs de fixture correspondent");
        db.execute(&insert).await.expect("ligne de contexte inseree");
    }

    /// L'installation du pont : catalogue, agents `dev`/`a2` d'alice, type `t` (work, défaut
    /// `dev`, autorisés `{dev}`), contextes `c1` (alice) et `c_bob` (bob). Le champ
    /// `default_agent` de `ChatConfig` porte ici `c1` : le texte libre ouvre une session sur ce
    /// contexte (lecture du `Must` point 5 après `DEC-28` — voir le rapport de tâche).
    struct Monture {
        db: DatabaseConnection,
        bridge: ChatBridge,
        spy: Arc<Spy>,
        clock: Arc<FixedClock>,
    }

    impl Monture {
        fn texte(body: &str, sender: &str, event: &str, room: &str, at_ms: u64) -> MatrixMessage {
            MatrixMessage {
                room_id: room.to_owned(),
                event_id: format!("${event}"),
                sender: sender.to_owned(),
                body: body.to_owned(),
                formatted_body: None,
                msgtype: "m.text".to_owned(),
                timestamp_ms: at_ms,
                encrypted: false,
            }
        }
        fn now(&self) -> u64 {
            self.clock.0.load(AtomicOrdering::SeqCst)
        }
        async fn linked(&self, mxid: &str, subject: &str) {
            let caller = Identity::new(self.db.clone())
                .resolve(subject)
                .await
                .expect("compte de test relu");
            let issued = link::issue_code(&self.db, &caller, chrono::Utc::now())
                .await
                .expect("code emis");
            link::consume_code(&self.db, mxid, &issued.code, chrono::Utc::now())
                .await
                .expect("liaison de fixture");
        }
        async fn rooms_session(&self, room: &str) -> (Option<String>, Option<String>) {
            let mut select = Query::select();
            select
                .columns([Alias::new("subject"), Alias::new("session_id")])
                .from(Alias::new(ROOMS_TABLE))
                .and_where(Expr::col(Alias::new("room_id")).eq(room));
            match self.db.query_one(&select).await.expect("ligne de salle relue") {
                None => (None, None),
                Some(row) => (
                    row.try_get::<Option<String>>("", "subject").expect("subject"),
                    row.try_get::<Option<String>>("", "session_id")
                        .expect("session_id"),
                ),
            }
        }
        async fn sessions_count(&self, subject: &str) -> u64 {
            let mut select = Query::select();
            select
                .column(Alias::new("session_id"))
                .from(Alias::new("miryad_lymaille_sessions"))
                .and_where(Expr::col(Alias::new("subject")).eq(subject));
            u64::try_from(self.db.query_all(&select).await.expect("sessions relues").len())
                .unwrap_or(u64::MAX)
        }
        async fn session_kind(&self, subject: &str) -> Option<String> {
            let mut select = Query::select();
            select
                .column(Alias::new("kind"))
                .from(Alias::new("miryad_lymaille_sessions"))
                .and_where(Expr::col(Alias::new("subject")).eq(subject));
            self.db
                .query_one(&select)
                .await
                .expect("session relue")
                .map(|row| row.try_get::<String>("", "kind").expect("kind lu"))
        }
    }

    fn monture(db: DatabaseConnection, manager: SessionManager, clock: Arc<FixedClock>) -> Monture {
        let spy = Spy::new();
        let bridge = ChatBridge::with_clock(
            db.clone(),
            manager,
            spy.clone(),
            ChatConfig {
                default_agent: "c1".to_owned(),
                max_event_age: Duration::from_mins(5),
                dedupe_window: Duration::from_hours(168),
            },
            clock.clone(),
        );
        Monture {
            db,
            bridge,
            spy,
            clock,
        }
    }

    async fn installe() -> DatabaseConnection {
        let db = test_db().await;
        wire_catalog(&db).await;
        compte(&db, "alice", false).await;
        compte(&db, "bob", false).await;
        compte(&db, "eve", false).await;
        put_agent(&db, "alice", &agent("dev", AgentKind::Work)).await;
        put_agent(&db, "alice", &agent("a2", AgentKind::Work)).await;
        put_type(
            &db,
            "t",
            Some(::lymaille::SessionKind::Work),
            Some("dev"),
            &["dev"],
        )
        .await;
        let alice = Identity::new(db.clone())
            .resolve("alice")
            .await
            .expect("alice")
            .user_id;
        let bob = Identity::new(db.clone())
            .resolve("bob")
            .await
            .expect("bob")
            .user_id;
        inserer_contexte_prive(&db, alice, "c1", Some("t")).await;
        inserer_contexte_prive(&db, bob, "c_bob", Some("t")).await;
        db
    }

    fn gestionnaire(db: &DatabaseConnection, inference: Arc<dyn InferenceFactory>) -> SessionManager {
        let hub = Arc::new(EventHub::new(None, 256));
        SessionManager::new(ManagerDeps {
            assemble: AssembleDeps {
                db: db.clone(),
                readers: Arc::new(ResourceReaders::new()),
                secrets: Arc::new(FixedSecret("cle-fixe")),
                hub,
                clock: Arc::new(::lymaille::SystemClock::new()),
                host: None,
                inference,
                mcp: Arc::new(NoMcp),
            },
            gate: TurnGate::new(),
            limits: Limits::default(),
        })
    }

    async fn monture_scriptee(script: Vec<Result<InferenceResponse, InferenceError>>) -> Monture {
        let db = installe().await;
        let manager = gestionnaire(
            &db,
            Arc::new(ScriptFactory(Arc::new(ScriptedProvider::new(script)))),
        );
        monture(db, manager, FixedClock::at(1_800_000_000_000))
    }

    // ── Scénarios ────────────────────────────────────────────────────────────

    /// Scenario « un mxid non lié n'a aucun droit » : deux messages d'`@eve:srv` dans l'heure,
    /// une seule invitation, aucune session.
    #[tokio::test(flavor = "multi_thread")]
    async fn un_mxid_lie_na_aucun_droit() {
        let m = monture_scriptee(vec![]).await;
        m.bridge
            .on_message(Monture::texte(
                "bonjour",
                "@eve:srv",
                "e1",
                "!direct:srv",
                m.now(),
            ))
            .await;
        m.bridge
            .on_message(Monture::texte(
                "bonjour encore",
                "@eve:srv",
                "e2",
                "!direct:srv",
                m.now(),
            ))
            .await;
        let envoyes = m.spy.bodies();
        assert_eq!(
            envoyes.len(),
            1,
            "une seule invitation dans l'heure : {envoyes:?}"
        );
        assert!(
            envoyes[0].contains("!link"),
            "l'invitation renvoie au code : {}",
            envoyes[0]
        );
        assert_eq!(
            m.sessions_count("eve").await,
            0,
            "aucune session pour un mxid non lié"
        );
    }

    /// Scenario « la liaison puis la conversation » : `!link` répond « Compte lié. », le texte
    /// libre ouvre une session `Work` et la réponse revient en message.
    #[tokio::test(flavor = "multi_thread")]
    async fn la_liaison_puis_la_conversation() {
        let m = monture_scriptee(vec![Ok(final_text("reponse de tour"))]).await;
        let caller = Identity::new(m.db.clone()).resolve("alice").await.expect("alice");
        let issued = link::issue_code(&m.db, &caller, chrono::Utc::now())
            .await
            .expect("code emis");
        m.bridge
            .on_message(Monture::texte(
                &format!("!link {}", issued.code),
                "@alice:srv",
                "l1",
                "!d1:srv",
                m.now(),
            ))
            .await;
        m.bridge
            .on_message(Monture::texte("bonjour", "@alice:srv", "l2", "!d1:srv", m.now()))
            .await;
        let envoyes = m.spy.bodies();
        assert_eq!(envoyes.len(), 2, "confirmation puis réponse : {envoyes:?}");
        assert_eq!(envoyes[0], "Compte lié.");
        assert_eq!(envoyes[1], "reponse de tour");
        let (subject, session) = m.rooms_session("!d1:srv").await;
        assert_eq!(
            subject.as_deref(),
            Some("alice"),
            "la salle est revendiquée par alice"
        );
        assert!(session.is_some(), "une session est persistée pour la salle");
        assert_eq!(
            m.session_kind("alice").await.as_deref(),
            Some("work"),
            "la nature vient du type"
        );
    }

    /// Scenario « un événement remis deux fois ne déclenche qu'un tour » : bascule de bail,
    /// même `event_id`.
    #[tokio::test(flavor = "multi_thread")]
    async fn un_evenement_remis_deux_fois_ne_declenche_qu_un_tour() {
        let m = monture_scriptee(vec![Ok(final_text("une seule fois"))]).await;
        m.linked("@alice:srv", "alice").await;
        let message = Monture::texte("bonjour", "@alice:srv", "dup", "!d2:srv", m.now());
        m.bridge.on_message(message.clone()).await;
        m.bridge.on_message(message).await;
        let envoyes = m.spy.bodies();
        assert_eq!(envoyes, vec!["une seule fois".to_owned()], "une seule réponse");
    }

    /// Scenario « une réponse longue est découpée » : 9000 caractères → trois messages numérotés.
    #[tokio::test(flavor = "multi_thread")]
    async fn une_reponse_longue_est_decoupee() {
        let paragraphe = "a".repeat(300);
        let mut longue = String::new();
        while longue.chars().count() < 9000 {
            longue.push_str(&paragraphe);
            longue.push_str("\n\n");
        }
        let m = monture_scriptee(vec![Ok(final_text(&longue))]).await;
        m.linked("@alice:srv", "alice").await;
        m.bridge
            .on_message(Monture::texte("long", "@alice:srv", "lg", "!d3:srv", m.now()))
            .await;
        let envoyes = m.spy.bodies();
        assert_eq!(envoyes.len(), 3, "trois parties numérotées : {}", envoyes.len());
        assert!(envoyes[0].starts_with("(1/3) "), "{}", &envoyes[0]);
        assert!(envoyes[2].starts_with("(3/3) "));
    }

    /// Scenario « l'historique n'appelle pas de réponse » : plus vieux que `max_event_age`,
    /// aucune réponse, mais l'`event_id` est quand même enregistré.
    #[tokio::test(flavor = "multi_thread")]
    async fn lhistorique_nappelle_pas_de_reponse() {
        let m = monture_scriptee(vec![]).await;
        m.linked("@alice:srv", "alice").await;
        let vieux = m.now().saturating_sub(360_000);
        m.bridge
            .on_message(Monture::texte(
                "historique",
                "@alice:srv",
                "vieux",
                "!d4:srv",
                vieux,
            ))
            .await;
        assert!(m.spy.bodies().is_empty(), "aucune réponse à l'historique");
        let mut select = Query::select();
        select
            .column(Alias::new("event_id"))
            .from(Alias::new(EVENTS_TABLE))
            .and_where(Expr::col(Alias::new("event_id")).eq("$vieux"));
        assert!(
            m.db.query_one(&select).await.expect("evenements relu").is_some(),
            "l'event_id est neanmoins enregistre"
        );
    }

    /// Scenario « un tour actif » : un second message reçoit « Un tour est en cours. », sans
    /// second appel d'inférence.
    #[tokio::test(flavor = "multi_thread")]
    async fn un_tour_actif() {
        let db = installe().await;
        let hang = Hang::new();
        let manager = gestionnaire(&db, Arc::new(HangFactory(Arc::clone(&hang))));
        let m = monture(db, manager, FixedClock::at(wall_ms()));
        m.linked("@alice:srv", "alice").await;

        let premier = Monture::texte("premier", "@alice:srv", "t1", "!d5:srv", m.now());
        let fil = tokio::spawn({
            let bridge = m.bridge.clone();
            async move { bridge.on_message(premier).await }
        });
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !hang.entered.load(AtomicOrdering::SeqCst) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "le provider n'a jamais ete appele"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        m.bridge
            .on_message(Monture::texte("second", "@alice:srv", "t2", "!d5:srv", m.now()))
            .await;
        let envoyes = m.spy.bodies();
        assert!(
            envoyes.contains(&"Un tour est en cours.".to_owned()),
            "{envoyes:?}"
        );
        hang.release.close();
        fil.await.expect("le premier message s'acheve");
    }

    /// Scenario « !new ferme et rouvre » : l'ancienne session est fermée, une nouvelle ouverte,
    /// la salle repart de zéro.
    #[tokio::test(flavor = "multi_thread")]
    async fn new_ferme_et_rouvre() {
        let m = monture_scriptee(vec![Ok(final_text("premiere")), Ok(final_text("seconde"))]).await;
        m.linked("@alice:srv", "alice").await;
        m.bridge
            .on_message(Monture::texte("bonjour", "@alice:srv", "n1", "!d6:srv", m.now()))
            .await;
        let (_subject, avant) = m.rooms_session("!d6:srv").await;
        let avant = avant.expect("une session est ouverte");
        m.bridge
            .on_message(Monture::texte("!new c1", "@alice:srv", "n2", "!d6:srv", m.now()))
            .await;
        let (_subject, apres) = m.rooms_session("!d6:srv").await;
        let apres = apres.expect("une nouvelle session est ouverte");
        assert_ne!(avant, apres, "!new a ouvert une autre session");
        m.bridge
            .on_message(Monture::texte("reprends", "@alice:srv", "n3", "!d6:srv", m.now()))
            .await;
        assert!(
            m.spy.bodies().contains(&"seconde".to_owned()),
            "la salle parle a la nouvelle session"
        );
    }

    /// Scenario « une erreur interne ne fuit pas » : `Failed` répond par `format_error`, sans le
    /// secret du message d'erreur amont.
    #[tokio::test(flavor = "multi_thread")]
    async fn une_erreur_interne_ne_fuit_pas() {
        let m = monture_scriptee(vec![Err(InferenceError::new(
            ErrorClass::Permanent,
            "upstream SECRET-XYZ a explosé".to_owned(),
        ))])
        .await;
        m.linked("@alice:srv", "alice").await;
        m.bridge
            .on_message(Monture::texte(
                "declenche",
                "@alice:srv",
                "x1",
                "!d7:srv",
                m.now(),
            ))
            .await;
        let envoyes = m.spy.bodies();
        assert_eq!(envoyes.len(), 1, "une seule réponse : {envoyes:?}");
        assert!(
            envoyes[0].contains("MRD-LYMAILLE-007"),
            "format_error : {}",
            envoyes[0]
        );
        assert!(!envoyes[0].contains("SECRET-XYZ"), "le secret amont ne fuit pas");
    }

    /// Scenario « un autre utilisateur dans la salle » : un avertissement unique, aucun envoi.
    #[tokio::test(flavor = "multi_thread")]
    async fn un_autre_utilisateur_dans_la_salle() {
        let m = monture_scriptee(vec![Ok(final_text("pour alice"))]).await;
        m.linked("@alice:srv", "alice").await;
        m.linked("@bob:srv", "bob").await;
        m.bridge
            .on_message(Monture::texte("bonjour", "@alice:srv", "s1", "!d8:srv", m.now()))
            .await;
        m.bridge
            .on_message(Monture::texte(
                "bonjour aussi",
                "@bob:srv",
                "s2",
                "!d8:srv",
                m.now(),
            ))
            .await;
        m.bridge
            .on_message(Monture::texte("encore moi", "@bob:srv", "s3", "!d8:srv", m.now()))
            .await;
        let envoyes = m.spy.bodies();
        assert_eq!(
            envoyes.len(),
            2,
            "reponse d'alice + un seul avertissement : {envoyes:?}"
        );
        assert!(envoyes[1].contains("autre utilisateur"), "{}", envoyes[1]);
        assert_eq!(m.sessions_count("bob").await, 0, "aucune session pour l'intrus");
    }

    /// Scenario « le droit est relu à chaque message » : le principal est reconstruit à chaque
    /// message (`Must` point 3), jamais mis en cache — le cas extrême est vérifié ici : compte
    /// supprimé entre deux messages, le second n'ouvre plus rien et retombe sur l'invitation.
    /// (Le droit admin à lui seul est prouvé dans `identity.rs` et `manager.rs`.)
    #[tokio::test(flavor = "multi_thread")]
    async fn le_droit_est_relu_a_chaque_message() {
        let m = monture_scriptee(vec![Ok(final_text("premier tour"))]).await;
        m.linked("@alice:srv", "alice").await;
        m.bridge
            .on_message(Monture::texte("bonjour", "@alice:srv", "r1", "!d9:srv", m.now()))
            .await;
        let supprime = crate::users::user::Entity::delete_many()
            .filter(crate::users::user::Column::Subject.eq("alice"))
            .exec(&m.db)
            .await
            .expect("compte supprime");
        assert_eq!(supprime.rows_affected, 1);
        let avant = m.spy.count();
        m.bridge
            .on_message(Monture::texte("encore", "@alice:srv", "r2", "!d9:srv", m.now()))
            .await;
        assert_eq!(
            m.spy.count(),
            avant.saturating_add(1),
            "seule l'invitation est ajoutee"
        );
        let envoyes = m.spy.bodies();
        assert!(
            envoyes.last().is_some_and(|t| t.contains("!link")),
            "le principal n'est plus reconstruit : {envoyes:?}"
        );
    }

    /// Scenario R13 « un contexte inconnu ou refusé est refusé » : `NotFound` indiscernable,
    /// texte fixe, aucune session.
    #[tokio::test(flavor = "multi_thread")]
    async fn new_contexte_inconnu_ou_refuse() {
        let m = monture_scriptee(vec![]).await;
        m.linked("@alice:srv", "alice").await;
        m.bridge
            .on_message(Monture::texte(
                "!new c_inconnu",
                "@alice:srv",
                "k2",
                "!da:srv",
                m.now(),
            ))
            .await;
        m.bridge
            .on_message(Monture::texte(
                "!new c_bob",
                "@alice:srv",
                "k3",
                "!da:srv",
                m.now(),
            ))
            .await;
        let envoyes = m.spy.bodies();
        assert_eq!(envoyes.len(), 2, "les deux `!new` repondent : {envoyes:?}");
        for texte in &envoyes {
            assert_eq!(
                texte, "Contexte inconnu ou inaccessible.",
                "texte fixe R13 : {texte}"
            );
        }
        assert_eq!(m.sessions_count("alice").await, 0, "aucune session creee");
    }

    /// Scenario R13 « un agent hors ensemble est refusé » : `Forbidden` du gestionnaire traduite
    /// en texte fixe Matrix.
    #[tokio::test(flavor = "multi_thread")]
    async fn new_agent_hors_ensemble() {
        let m = monture_scriptee(vec![]).await;
        m.linked("@alice:srv", "alice").await;
        m.bridge
            .on_message(Monture::texte(
                "!new c1 a2",
                "@alice:srv",
                "a1",
                "!db2:srv",
                m.now(),
            ))
            .await;
        let envoyes = m.spy.bodies();
        assert_eq!(envoyes, vec!["Agent non autorisé pour ce contexte.".to_owned()]);
        assert_eq!(m.sessions_count("alice").await, 0, "aucune session creee");
    }

    /// `Must` : `!help` rend l'aide des commandes.
    #[tokio::test(flavor = "multi_thread")]
    async fn help_rend_laide() {
        let m = monture_scriptee(vec![]).await;
        m.linked("@alice:srv", "alice").await;
        m.bridge
            .on_message(Monture::texte("!help", "@alice:srv", "h1", "!dc:srv", m.now()))
            .await;
        let envoyes = m.spy.bodies();
        assert_eq!(envoyes.len(), 1);
        assert!(
            envoyes[0].contains("!new <contexte>"),
            "l'aide cite R13 : {}",
            envoyes[0]
        );
    }

    /// `Must` : `!cancel` d'un tour actif répond par « Annulé. » (via la collecte).
    #[tokio::test(flavor = "multi_thread")]
    async fn cancel_annule_le_tour() {
        let db = installe().await;
        let hang = Hang::new();
        let manager = gestionnaire(&db, Arc::new(HangFactory(Arc::clone(&hang))));
        let m = monture(db, manager, FixedClock::at(wall_ms()));
        m.linked("@alice:srv", "alice").await;
        let premier = Monture::texte("longtemps", "@alice:srv", "c1ev", "!dd:srv", m.now());
        let fil = tokio::spawn({
            let bridge = m.bridge.clone();
            async move { bridge.on_message(premier).await }
        });
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !hang.entered.load(AtomicOrdering::SeqCst) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "le provider n'a jamais ete appele"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        m.bridge
            .on_message(Monture::texte(
                "!cancel",
                "@alice:srv",
                "c2ev",
                "!dd:srv",
                m.now(),
            ))
            .await;
        fil.await.expect("le premier message s'acheve sur l'annulation");
        assert!(
            m.spy.bodies().contains(&"Annulé.".to_owned()),
            "{:?}",
            m.spy.bodies()
        );
    }

    /// `Must not` : le pont ne répond jamais à ses propres messages (redoublement du filtre de
    /// `should_deliver`).
    #[tokio::test(flavor = "multi_thread")]
    async fn le_pont_ne_se_respond_pas() {
        let m = monture_scriptee(vec![]).await;
        m.linked("@alice:srv", "alice").await;
        m.bridge
            .on_message(Monture::texte(
                "mon propre echo",
                "@bot:srv",
                "echo",
                "!de:srv",
                m.now(),
            ))
            .await;
        assert!(m.spy.bodies().is_empty(), "aucune auto-reponse");
    }
}
