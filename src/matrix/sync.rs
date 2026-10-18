//! Partie pure de la synchronisation Matrix (`src/matrix/sync.sdd`) : types remis à l'application,
//! filtrage des messages, conversion, règle d'auto-join, durée de bail et backoff.
//!
//! Rien ici ne touche à `matrix-sdk` : les fonctions prennent des champs simples, pour être testées
//! sans homeserver. La boucle `run_sync` (étape suivante, `./sync.sdd`) s'appuie sur elles.

use std::time::Duration;

/// Message texte remis à l'application (`src/matrix/sync.sdd`). Identifiants en `String` : aucun
/// type de `matrix-sdk` ni de `ruma` n'entre dans l'API publique.
#[derive(Debug, Clone, PartialEq)]
pub struct MatrixMessage {
    /// Identifiant de la salle, ex. `!room:server`.
    pub room_id: String,
    /// Identifiant de l'événement : clé naturelle d'idempotence côté handler.
    pub event_id: String,
    /// Expéditeur, ex. `@alice:server`.
    pub sender: String,
    /// Corps en texte brut.
    pub body: String,
    /// Corps HTML, quand l'événement en porte un.
    pub formatted_body: Option<String>,
    /// `m.text` ou `m.emote` en remise en direct.
    pub msgtype: String,
    /// Horodatage serveur, en millisecondes depuis l'époque Unix.
    pub timestamp_ms: u64,
    /// Vrai si le message a été déchiffré (salle chiffrée).
    pub encrypted: bool,
}

/// Réception des messages texte par l'application (`src/matrix/sync.sdd`). Le harnais de chatbot
/// l'implémente ; le handler gère ses propres erreurs et doit être idempotent (un message peut être
/// remis deux fois après une bascule de bail).
#[async_trait::async_trait]
pub trait MatrixEventHandler: Send + Sync + 'static {
    /// Reçoit un message nouveau. Aucune erreur n'est remontée à la boucle de synchronisation.
    async fn on_message(&self, message: MatrixMessage);
}

/// Entrées de [`to_message`] regroupées en un seul argument (`src/matrix/sync.sdd`) : huit
/// paramètres nus dépasseraient `clippy::too_many_arguments`. Réutilisée par `recent_messages`.
#[derive(Clone, Copy)]
pub(crate) struct MessageParts<'a> {
    /// Identifiant de la salle.
    pub(crate) room_id: &'a str,
    /// Identifiant de l'événement.
    pub(crate) event_id: &'a str,
    /// Expéditeur.
    pub(crate) sender: &'a str,
    /// Type de message (`m.text`, `m.emote`, `m.notice`).
    pub(crate) msgtype: &'a str,
    /// Corps en texte brut.
    pub(crate) body: &'a str,
    /// Corps HTML, s'il existe.
    pub(crate) formatted_body: Option<&'a str>,
    /// Horodatage serveur, en millisecondes.
    pub(crate) timestamp_ms: u64,
    /// Vrai si la salle est chiffrée.
    pub(crate) encrypted: bool,
}

/// Décide si un message est remis en direct au handler (`src/matrix/sync.sdd`). Faux pour le bot
/// lui-même, pour les `m.notice` (convention anti-boucle entre bots) et pour les remplacements
/// (éditions).
#[must_use]
pub(crate) fn should_deliver(own_user: &str, sender: &str, msgtype: &str, is_replacement: bool) -> bool {
    sender != own_user && matches!(msgtype, "m.text" | "m.emote") && !is_replacement
}

/// Convertit les entrées déjà extraites d'un événement en [`MatrixMessage`] (`src/matrix/sync.sdd`).
#[must_use]
pub(crate) fn to_message(parts: MessageParts<'_>) -> MatrixMessage {
    MatrixMessage {
        room_id: parts.room_id.to_owned(),
        event_id: parts.event_id.to_owned(),
        sender: parts.sender.to_owned(),
        body: parts.body.to_owned(),
        formatted_body: parts.formatted_body.map(str::to_owned),
        msgtype: parts.msgtype.to_owned(),
        timestamp_ms: parts.timestamp_ms,
        encrypted: parts.encrypted,
    }
}

/// Une invitation n'est acceptée que si son inviteur figure dans `auto_join_from`
/// (`src/matrix/sync.sdd`). Liste vide : aucune invitation acceptée.
#[must_use]
pub(crate) fn invite_allowed(auto_join_from: &[String], inviter: &str) -> bool {
    auto_join_from.iter().any(|entry| entry == inviter)
}

/// Durée de bail de `miryad.matrix.sync` pour un `sync_timeout_ms` donné : `2 * sync_timeout_ms +
/// 15_000` millisecondes (`src/matrix/sync.sdd`). Saturée à `u32::MAX` plutôt que débordante.
#[must_use]
pub(crate) fn lease_ms(sync_timeout_ms: u64) -> u32 {
    let ms = sync_timeout_ms.saturating_mul(2).saturating_add(15_000);
    u32::try_from(ms).unwrap_or(u32::MAX)
}

/// Délai avant la reprise après une erreur retryable (`src/matrix/sync.sdd`) : 1 s doublant à
/// chaque tentative, plafonné à 60 s.
#[must_use]
pub(crate) fn backoff(attempt: u32) -> Duration {
    // `checked_shl` rend `None` à partir de 64 : le plafond l'absorbe sans panique.
    let secs = 1_u64.checked_shl(attempt).unwrap_or(u64::MAX).min(60);
    Duration::from_secs(secs)
}

#[cfg(test)]
mod tests {
    use super::{MatrixMessage, MessageParts, backoff, invite_allowed, lease_ms, should_deliver, to_message};
    use std::time::Duration;

    // Scenario: should_deliver filtre soi-même, les notices et les éditions
    #[test]
    fn should_deliver_filters_self_notices_and_edits() {
        let own = "@bot:x";
        assert!(!should_deliver(own, "@bot:x", "m.text", false));
        assert!(!should_deliver(own, "@a:x", "m.notice", false));
        assert!(!should_deliver(own, "@a:x", "m.text", true));
        assert!(should_deliver(own, "@a:x", "m.text", false));
        assert!(should_deliver(own, "@a:x", "m.emote", false));
    }

    // Scenario: invite_allowed n'accepte que les inviteurs listés
    #[test]
    fn invite_allowed_accepts_only_listed_inviters() {
        let allowed = vec!["@xavier:x".to_owned()];
        assert!(invite_allowed(&allowed, "@xavier:x"));
        assert!(!invite_allowed(&allowed, "@inconnu:x"));
        assert!(!invite_allowed(&[], "@xavier:x"));
    }

    // Scenario: le bail couvre un cycle de sync complet
    #[test]
    fn lease_covers_a_full_sync_cycle() {
        assert_eq!(lease_ms(30_000), 75_000);
    }

    #[test]
    fn lease_saturates_instead_of_overflowing() {
        assert_eq!(lease_ms(u64::MAX), u32::MAX);
    }

    // Scenario: le backoff double et plafonne
    #[test]
    fn backoff_doubles_then_caps_at_sixty_seconds() {
        assert_eq!(backoff(0), Duration::from_secs(1));
        assert_eq!(backoff(1), Duration::from_secs(2));
        assert_eq!(backoff(2), Duration::from_secs(4));
        assert_eq!(backoff(6), Duration::from_secs(60));
        assert_eq!(backoff(20), Duration::from_secs(60));
    }

    #[test]
    fn backoff_never_panics_on_large_attempts() {
        assert_eq!(backoff(64), Duration::from_secs(60));
        assert_eq!(backoff(u32::MAX), Duration::from_secs(60));
    }

    // Scenario: to_message reprend tous les champs
    #[test]
    fn to_message_copies_every_field() {
        let message = to_message(MessageParts {
            room_id: "!r:x",
            event_id: "$e",
            sender: "@a:x",
            msgtype: "m.text",
            body: "salut",
            formatted_body: Some("<b>salut</b>"),
            timestamp_ms: 1_700_000_000_000,
            encrypted: true,
        });
        assert_eq!(
            message,
            MatrixMessage {
                room_id: "!r:x".into(),
                event_id: "$e".into(),
                sender: "@a:x".into(),
                body: "salut".into(),
                formatted_body: Some("<b>salut</b>".into()),
                msgtype: "m.text".into(),
                timestamp_ms: 1_700_000_000_000,
                encrypted: true,
            }
        );
    }

    #[test]
    fn to_message_keeps_absent_formatted_body() {
        let message = to_message(MessageParts {
            room_id: "!r:x",
            event_id: "$e",
            sender: "@a:x",
            msgtype: "m.emote",
            body: "fait",
            formatted_body: None,
            timestamp_ms: 1,
            encrypted: false,
        });
        assert_eq!(message.formatted_body, None);
        assert_eq!(message.msgtype, "m.emote");
        assert!(!message.encrypted);
    }
}
