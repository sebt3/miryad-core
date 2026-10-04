use cookie::Key;
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::auth::error::AuthError;
use crate::auth::oidc::OidcIdentity;

/// Nom contractuel du cookie de session, posé et cherché : littéral `miryad_session`. Affirmé
/// par les tests de [`auth_router`](super::auth_router) ; le pending `miryad_oidc_pending` est un
/// autre cookie, hors du périmètre de ce fichier.
pub const SESSION_COOKIE_NAME: &str = "miryad_session";

#[derive(Serialize)]
struct SessionPayloadRef<'a> {
    id_token: &'a str,
    subject: &'a str,
    email: Option<&'a str>,
    preferred_username: Option<&'a str>,
}

#[derive(Deserialize)]
struct SessionPayload {
    id_token: String,
    subject: String,
    email: Option<String>,
    preferred_username: Option<String>,
}

/// Pose le cookie de session : scelle le payload `JSON` à quatre clés de l'`OidcIdentity`
/// (`AES-256-GCM` par `PrivateJar`, nom du cookie comme données associées) puis rend l'unique
/// en-tête `Set-Cookie`, littéral et ordonné : `miryad_session=<valeur scellée>; HttpOnly`, puis
/// `; Secure` si et seulement si `secure` (posé depuis `MiryadAuthState::secure_cookies`), puis
/// `; SameSite=Strict; Path=/; Max-Age=<n>` avec `n = exp - now` en `saturating_sub` — un
/// `id_token` sans claim `exp` ou déjà expiré produit `Max-Age=0` que le navigateur jette
/// aussitôt. Infaillible : ne retourne pas de `Result`.
pub fn build_set_cookie(identity: &OidcIdentity, key: &Key, secure: bool) -> String {
    let payload = SessionPayloadRef {
        id_token: &identity.id_token,
        subject: &identity.subject,
        email: identity.email.as_deref(),
        preferred_username: identity.preferred_username.as_deref(),
    };
    let value = serde_json::to_string(&payload).unwrap_or_default();
    let exp = extract_exp_claim(&identity.id_token).unwrap_or(0);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let max_age = exp.saturating_sub(now);

    let mut jar = cookie::CookieJar::new();
    let mut private_jar = jar.private_mut(key);
    private_jar.add(cookie::Cookie::new(SESSION_COOKIE_NAME, value));

    let encrypted = jar.get(SESSION_COOKIE_NAME);
    let encrypted_value = encrypted.map_or("", cookie::Cookie::value);

    let secure_attr = if secure { "; Secure" } else { "" };
    format!(
        "{SESSION_COOKIE_NAME}={encrypted_value}; HttpOnly{secure_attr}; SameSite=Strict; Path=/; Max-Age={max_age}"
    )
}

/// Unique chemin de lecture du cookie de session `miryad_session` : découpe brute et figée de
/// l'en-tête `Cookie` (première occurrence du nom exact retenue), déchiffrement de la valeur
/// scellée sous `key`, désérialisation du payload, puis ré-lecture serveur de la claim `exp` du
/// `id_token` déchiffré (`exp ≤ now` est déjà un rejet). Émetteur exclusif des deux erreurs
/// ci-dessous, pour les trois surfaces via `AuthUser` (REST) et `AuthPrincipal` (REST/GraphQL/MCP).
///
/// # Errors
///
/// `AuthError::NotAuthenticated` (`MRD-AUTH-001`) — l'en-tête passé est `None` (aucun en-tête
/// `Cookie`), ou aucune ligne du texte ne porte le nom exact `miryad_session` après
/// découpe/trim/filtrage.
///
/// `AuthError::InvalidSession` (`MRD-AUTH-002`) — cinq familles de cause : déchiffrement
/// `PrivateJar::decrypt` en échec (base64 invalide, longueur décodée `≤ 12` octets couvrant la
/// valeur vide, tag GCM mauvais — clé différente, valeur altérée ou nom transplanté —, ou clair
/// non UTF-8) ; désérialisation `serde_json` du clair en échec ; claim `exp` absente ou illisible
/// du `id_token` déchiffré ; horloge système antérieure à `UNIX_EPOCH` ; ou `exp ≤ now`.
pub fn extract_session(cookie_header: Option<&str>, key: &Key) -> Result<OidcIdentity, AuthError> {
    let clear = find_sealed_cookie(cookie_header, SESSION_COOKIE_NAME, key)?;

    let payload: SessionPayload = serde_json::from_str(&clear).map_err(|_| AuthError::InvalidSession)?;

    let exp = extract_exp_claim(&payload.id_token).ok_or(AuthError::InvalidSession)?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| AuthError::InvalidSession)?
        .as_secs();

    if exp <= now {
        return Err(AuthError::InvalidSession);
    }

    Ok(OidcIdentity {
        id_token: payload.id_token,
        subject: payload.subject,
        email: payload.email,
        preferred_username: payload.preferred_username,
    })
}

/// Trouve la première ligne `nom=valeur` du cookie `name` dans un en-tête `Cookie` brut et
/// déchiffre sa valeur sous `key`. Découpe figée du contrat `Must` « Lecture brute et figée » :
/// séparation par `;`, `trim` de chaque segment, segments vides filtrés, `split_once` au premier
/// `=` seulement (le bourrage base64 de fin reste dans la valeur), comparaison de nom exacte et
/// sensible à la casse, première occurrence retenue par `find_map`, segments sans `=` ignorés —
/// aucune parse par le crate `cookie`, aucun percent-decode.
///
/// Erreurs : en-tête absent ou nom introuvable → `NotAuthenticated` ; échec du déchiffrement
/// `PrivateJar` (base64, longueur, tag GCM, clair non UTF-8) → `InvalidSession`.
pub(crate) fn find_sealed_cookie(header: Option<&str>, name: &str, key: &Key) -> Result<String, AuthError> {
    let header = header.ok_or(AuthError::NotAuthenticated)?;
    let value = header
        .split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .find_map(|segment| {
            segment
                .split_once('=')
                .filter(|(cookie_name, _)| *cookie_name == name)
                .map(|(_, value)| value)
        })
        .ok_or(AuthError::NotAuthenticated)?;

    let jar = cookie::CookieJar::new();
    let raw_cookie = cookie::Cookie::new(name.to_string(), value.to_string());
    let decrypted = jar
        .private(key)
        .decrypt(raw_cookie)
        .ok_or(AuthError::InvalidSession)?;
    Ok(decrypted.value().to_string())
}

/// Claim `exp` du segment payload d'un JWT, lue par `serde_json` (arbitré 2026-09-27, unifié
/// sur la technique de `oidc.rs::extract_groups_claim`) : décodage en base64 url-safe sans
/// bourrage — un payload bourré est un `exp` illisible, pas une tolérance — puis
/// désérialisation vers une structure minimale portant `exp: Option<f64>`. Aucune indexation
/// ni tranche de chaîne ici.
///
/// Portillon d'entier exact (arbitré par Sébastien le 2026-09-30, option A′,
/// `src/auth/cookie.sdd` `Must`) : JSON n'ayant qu'un seul type nombre, `1700000000` et
/// `1.7e9` sont le même nombre ; la tolérance porte sur le seul format de sérialisation,
/// jamais sur la valeur. La valeur `f` n'est retenue que si `f.is_finite() && f == f.trunc()
/// && 0.0 <= f && f < 9007199254740992.0` (`2^53`, mantisse exacte de `f64`) ; un `f64` entier
/// dans cette plage est la représentation bit-exacte du même entier timestamp — aucun arrondi,
/// aucune troncature. `NaN`, infinis, fractions, négatifs et `>= 2^53` rendent `None`, donc
/// `MRD-AUTH-002` par le chemin `ok_or` existant de la lecture et `Max-Age=0` à la pose.
///
/// Conversion sans `unsafe` (`to_int_unchecked` est proscrit par `unsafe_code = "forbid"`) :
/// `as u64` sous la garde — la seule des deux conversions admises de la tâche B1a qui compile
/// sur stable 1.97.1 (`u64::TryFrom<f64>` est instable, vérifié au compilateur).
#[allow(clippy::float_cmp, clippy::cast_possible_truncation, clippy::cast_sign_loss)] // portillon d'entier exact impose par `src/auth/cookie.sdd` `Must` (arbitre Sebastien 2026-09-30, option A') : la garde rend l'egalite et la conversion exactes, clippy pedantic ne peut pas la voir
fn extract_exp_claim(jwt: &str) -> Option<u64> {
    use base64::Engine;

    /// `2^53` : borne exclusive de la mantisse exacte de `f64` (portillon 2026-09-30).
    const F64_EXACT_INTEGER_LIMIT: f64 = 9_007_199_254_740_992.0;

    #[derive(Deserialize)]
    struct ExpClaim {
        exp: Option<f64>,
    }

    let mut segments = jwt.split('.');
    let (_header, payload, _signature) = (segments.next()?, segments.next()?, segments.next()?);
    if segments.next().is_some() {
        return None;
    }

    let payload_json = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    let exp = serde_json::from_slice::<ExpClaim>(&payload_json).ok()?.exp?;
    // Portillon d'entier exact verbatim du `Must` : fini, égal à sa partie entière, dans
    // `0.0..<2^53`. Sous cette garde, `f == f.trunc()` est un test d'appartenance exact (jamais
    // une comparaison de valeurs mesurées) et `f as u64` est total et bit-exacte (pas de NaN,
    // pas d'infini, pas de fraction, pas de négatif, `2^53 < u64::MAX`) — clippy ne peut pas
    // le prouver et l'alternative stable `u64::try_from(f64)` n'existe pas (TryFrom<f64> pour
    // les entiers est instable, vérifié sur 1.97.1), `to_int_unchecked` étant proscrit par
    // `unsafe_code = "forbid"`. L'`#[allow]` de ces trois lints `pedantic`, cité en tête de
    // fonction, est la voie autorisée par `tooling.sdd` (`Must not`, commentaire de
    // justification citant la spec).
    let exact_seconds =
        exp.is_finite() && exp == exp.trunc() && (0.0..F64_EXACT_INTEGER_LIMIT).contains(&exp);
    exact_seconds.then_some(exp as u64)
}

/// Chaîne littérale de retrait du cookie de session : valeur vide, `Max-Age=0`, mêmes attributs
/// que la pose correspondante (nom, `Path=/`, hôte-only, `Secure` conditionnel via `secure`) pour
/// que le client retire bien le cookie posé. `secure` doit valoir la même valeur qu'à la pose.
/// Aucune erreur possible — la valeur retournée est l'unique produit de la fonction.
#[must_use]
pub fn clear_cookie(secure: bool) -> String {
    let secure_attr = if secure { "; Secure" } else { "" };
    format!("{SESSION_COOKIE_NAME}=; HttpOnly{secure_attr}; SameSite=Strict; Path=/; Max-Age=0")
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    fn test_key() -> Key {
        Key::from(&[0u8; 64])
    }

    fn other_key() -> Key {
        Key::from(&[7u8; 64])
    }

    fn now_secs() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is after epoch")
            .as_secs()
    }

    fn future_exp() -> u64 {
        now_secs() + 3600
    }

    /// JWT tri-segment dont la claim d'expiration vaut `exp` (base64 url-safe sans bourrage).
    fn make_jwt(exp: u64) -> String {
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(format!(r#"{{"exp":{exp}}}"#));
        format!("header.{payload}.sig")
    }

    /// JWT tri-segment dont le segment payload est encodé en base64 url-safe **bourré** (`=` de
    /// fin), claim d'expiration future bien présente.
    fn make_jwt_padded(exp: u64) -> String {
        let payload =
            base64::engine::general_purpose::URL_SAFE.encode(format!(r#"{{"exp":{exp},"sub":"x"}}"#));
        assert!(
            payload.ends_with('='),
            "fixture : le segment payload doit porter un bourrage `=`"
        );
        format!("header.{payload}.sig")
    }

    /// JWT tri-segment dont le payload JSON porte `"exp": <exp_json>` **verbatim** — notation
    /// flottante JSON (`1.7e9`, `1700000000.5`, `-1`, `9007199254740993.0`, …) ou entière : la
    /// clé du portillon d'entier exact (arbitrage 2026-09-30) porte sur le format sérialisé,
    /// pas sur la valeur.
    fn make_jwt_exp_json(exp_json: &str) -> String {
        let payload =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(format!(r#"{{"exp":{exp_json}}}"#));
        format!("header.{payload}.sig")
    }

    /// Scelle à la main (sans `build_set_cookie`) un payload `SessionPayload` à quatre clés
    /// porteur de l'`id_token` donné, sous `key`, et rend la ligne `miryad_session=<scellé>`.
    fn seal_session_line(id_token: &str, key: &Key) -> String {
        let clear = format!(
            r#"{{"id_token":"{id_token}","subject":"user-123","email":"user-123@example.com","preferred_username":"alice"}}"#
        );
        seal_raw(SESSION_COOKIE_NAME, &clear, key)
    }

    /// JWT tri-segment dont le payload encodé ne porte que `sub`, aucune claim `exp`.
    fn make_jwt_without_exp() -> String {
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(r#"{"sub":"x"}"#);
        format!("header.{payload}.sig")
    }

    /// Identité quadruplée complète autour d'un `id_token` donné.
    fn identity_with(id_token: String) -> OidcIdentity {
        OidcIdentity {
            id_token,
            subject: "user-123".to_string(),
            email: Some("user-123@example.com".to_string()),
            preferred_username: Some("alice".to_string()),
        }
    }

    /// Paire `nom=valeur` isolée d'un en-tête `Set-Cookie` (premier segment avant `; `).
    fn cookie_pair(set_cookie: &str) -> String {
        set_cookie
            .split(';')
            .next()
            .expect("set-cookie always has a name=value pair")
            .to_string()
    }

    /// Déchiffre la valeur scellée d'une ligne `nom=valeur` sous la clé donnée, et rend le clair.
    fn unseal_payload(pair: &str, key: &Key) -> String {
        let value = pair.split_once('=').expect("sealed line has a value").1;
        let jar = cookie::CookieJar::new();
        let raw = cookie::Cookie::new(SESSION_COOKIE_NAME, value.to_string());
        jar.private(key)
            .decrypt(raw)
            .expect("sealed value decrypts under the same key")
            .value()
            .to_string()
    }

    /// Sceau une valeur claire sous `name` via `cookie::CookieJar` (sans `build_set_cookie`) et
    /// rend la ligne `nom=valeur`.
    fn seal_raw(name: &str, clear: &str, key: &Key) -> String {
        let mut jar = cookie::CookieJar::new();
        jar.private_mut(key)
            .add(cookie::Cookie::new(name.to_string(), clear.to_string()));
        format!(
            "{name}={}",
            jar.get(name).expect("sealed cookie present in delta").value()
        )
    }

    /// Attributs d'un `Set-Cookie` (segments après la paire `nom=valeur`), hors `Max-Age` dont
    /// la valeur est seule à diverger entre pose et retrait.
    fn attributes_without_max_age(set_cookie: &str) -> Vec<&str> {
        set_cookie
            .split("; ")
            .skip(1)
            .filter(|attr| !attr.starts_with("Max-Age"))
            .collect()
    }

    /// Vérification octet pour octet d'un `Set-Cookie` : paire `nom=valeur` attendue, table
    /// d'attributs complète dans l'ordre contractuel avec `Secure` présent si et seulement si
    /// `secure`, `Max-Age` dans l'intervalle attendu — sans `Domain`, sans `Expires`, sans `;`
    /// finale (aucun `contains` sur un fragment d'attribut).
    fn assert_set_cookie_full(
        set_cookie: &str,
        expected_pair: &str,
        secure: bool,
        max_age_min: u64,
        max_age_max: u64,
    ) {
        assert!(
            expected_pair.starts_with("miryad_session="),
            "le nom posé est `miryad_session` : {expected_pair}"
        );
        let (head, max_age_raw) = set_cookie
            .rsplit_once("; Max-Age=")
            .expect("`Max-Age` terminal séparé par `; `");
        let max_age: u64 = max_age_raw
            .parse()
            .expect("`Max-Age` numérique terminal, sans suffixe ni `;` finale");
        assert!(
            max_age.ge(&max_age_min) && max_age.le(&max_age_max),
            "Max-Age {max_age} hors de l'intervalle [{max_age_min}; {max_age_max}]"
        );
        let secure_attr = if secure { "; Secure" } else { "" };
        assert_eq!(
            head,
            format!("{expected_pair}; HttpOnly{secure_attr}; SameSite=Strict; Path=/"),
            "attributs complets dans l'ordre contractuel"
        );
        assert!(!set_cookie.contains("Domain"));
        assert!(!set_cookie.contains("Expires"));
        assert!(!set_cookie.ends_with(';'));
    }

    /// `Scenario` : « Pose — contrat session complet avec email et `preferred_username`,
    /// `secure: true` » (arbitré 2026-09-27).
    #[test]
    fn pose_contrat_session_complet_avec_email_et_preferred_username_secure_true() {
        let key = test_key();
        let exp = future_exp();
        let now = now_secs();
        let identity = identity_with(make_jwt(exp));

        let set_cookie = build_set_cookie(&identity, &key, true);

        let pair = cookie_pair(&set_cookie);
        assert_set_cookie_full(&set_cookie, &pair, true, exp - now - 1, exp - now);

        let clear = unseal_payload(&pair, &key);
        let payload: serde_json::Value = serde_json::from_str(&clear).expect("payload is JSON");
        let mut keys: Vec<&str> = payload
            .as_object()
            .expect("payload is an object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec!["email", "id_token", "preferred_username", "subject"],
            "payload scellé à exactement quatre clés : {clear}"
        );
        assert_eq!(payload["preferred_username"], "alice");
        assert_eq!(payload["email"], "user-123@example.com");
        assert_eq!(payload["subject"], "user-123");
        assert_eq!(payload["id_token"], identity.id_token);
    }

    /// `Scenario` : « Pose — `secure: false` omet l'attribut `Secure` » (arbitré 2026-09-27).
    #[test]
    fn pose_secure_false_omet_attribut_secure() {
        let key = test_key();
        let exp = future_exp();
        let now = now_secs();
        let identity = identity_with(make_jwt(exp));

        let set_cookie = build_set_cookie(&identity, &key, false);

        let pair = cookie_pair(&set_cookie);
        assert_set_cookie_full(&set_cookie, &pair, false, exp - now - 1, exp - now);
        assert!(
            !set_cookie.contains("Secure"),
            "aucun segment `Secure` quand `secure: false` (dev HTTP local)"
        );
        let payload: serde_json::Value =
            serde_json::from_str(&unseal_payload(&pair, &key)).expect("payload is JSON");
        assert_eq!(payload["subject"], "user-123", "tout le reste identique");
    }

    /// `Scenario` : « Retrait — les attributs de `clear_cookie` suivent la même valeur de
    /// `secure` qu'à la pose » (arbitré 2026-09-27).
    #[test]
    fn retrait_attributs_suivent_la_meme_valeur_de_secure_qu_a_la_pose() {
        let key = test_key();
        let identity = identity_with(make_jwt(future_exp()));

        let set_true = build_set_cookie(&identity, &key, true);
        let clear_true = clear_cookie(true);
        let set_false = build_set_cookie(&identity, &key, false);
        let clear_false = clear_cookie(false);

        assert_eq!(
            attributes_without_max_age(&set_true),
            vec!["HttpOnly", "Secure", "SameSite=Strict", "Path=/"],
            "pose `secure: true` : `Secure` présent"
        );
        assert_eq!(
            attributes_without_max_age(&clear_true),
            attributes_without_max_age(&set_true),
            "retrait `secure: true` : mêmes attributs que la pose, seul `Max-Age` diffère"
        );
        assert_eq!(
            attributes_without_max_age(&set_false),
            vec!["HttpOnly", "SameSite=Strict", "Path=/"],
            "pose `secure: false` : pas de `Secure`"
        );
        assert_eq!(
            attributes_without_max_age(&clear_false),
            attributes_without_max_age(&set_false),
            "retrait `secure: false` : mêmes attributs que la pose"
        );
    }

    /// `Scenario` : « Pose — email et `preferred_username` absents sérialisés null »
    /// (2026-09-27).
    #[test]
    fn pose_email_et_preferred_username_absents_serialises_null() {
        let key = test_key();
        let identity = OidcIdentity {
            id_token: make_jwt(future_exp()),
            subject: "user-123".to_string(),
            email: None,
            preferred_username: None,
        };

        let set_cookie = build_set_cookie(&identity, &key, true);
        let pair = cookie_pair(&set_cookie);
        let clear = unseal_payload(&pair, &key);
        let payload: serde_json::Value = serde_json::from_str(&clear).expect("payload is JSON");
        assert_eq!(
            payload.get("email"),
            Some(&serde_json::Value::Null),
            "`email` absent = clé sérialisée `null`, pas clé disparue : {clear}"
        );
        assert_eq!(
            payload.get("preferred_username"),
            Some(&serde_json::Value::Null),
            "`preferred_username` absent = clé sérialisée `null` (2026-09-27) : {clear}"
        );
    }

    /// `Scenario` : « Pose — `id_token` expiré donne `Max-Age` nul » — `saturating_sub` ne
    /// déborde jamais, la valeur scellée est posée malgré tout avec le même jeu d'attributs.
    #[test]
    fn pose_id_token_expire_donne_max_age_nul() {
        let key = test_key();
        let identity = identity_with(make_jwt(1_000_000));

        let set_cookie = build_set_cookie(&identity, &key, true);

        let pair = cookie_pair(&set_cookie);
        assert_set_cookie_full(&set_cookie, &pair, true, 0, 0);
        assert!(
            pair.len() > SESSION_COOKIE_NAME.len() + 1,
            "la valeur scellée est posée malgré l'expiration : {pair}"
        );
    }

    /// `Scenario` : « Pose — expiration illisible donne `Max-Age` nul » — payload sans claim
    /// `exp`, puis `id_token` à deux segments (`not-a-jwt`), même fin de chaîne pour la même
    /// raison (`unwrap_or(0)`).
    #[test]
    fn pose_expiration_illisible_donne_max_age_nul() {
        let key = test_key();

        let without_exp = build_set_cookie(&identity_with(make_jwt_without_exp()), &key, false);
        let pair = cookie_pair(&without_exp);
        assert_set_cookie_full(&without_exp, &pair, false, 0, 0);

        let not_jwt = build_set_cookie(&identity_with("not-a-jwt".to_string()), &key, false);
        let pair = cookie_pair(&not_jwt);
        assert_set_cookie_full(&not_jwt, &pair, false, 0, 0);
    }

    /// `Scenario` : « Pose — deux poses consécutives divergent et se relisent » — nonce
    /// aléatoire par pose, mais même clair sous la même clé.
    #[test]
    fn pose_deux_poses_consecutives_divergent_et_se_relisent() {
        let key = test_key();
        let identity = identity_with(make_jwt(future_exp()));

        let first = cookie_pair(&build_set_cookie(&identity, &key, false));
        let second = cookie_pair(&build_set_cookie(&identity, &key, false));

        assert_ne!(first, second, "nonce aléatoire par pose, sans reproductibilité");
        assert_eq!(
            unseal_payload(&first, &key),
            unseal_payload(&second, &key),
            "les deux déchiffrent au même payload JSON sous la même clé"
        );
    }

    /// `Scenario` : « Retrait — chaîne exacte valeur vide `Max-Age` nul, `secure: true` »
    /// (resserré en chaîne entière le 2026-09-27, l'ancien `clear_cookie_has_max_age_zero`
    /// n'affirmait que `contains`).
    #[test]
    fn retrait_chaine_exacte_valeur_vide_max_age_nul_secure_true() {
        assert_eq!(
            clear_cookie(true),
            "miryad_session=; HttpOnly; Secure; SameSite=Strict; Path=/; Max-Age=0",
            "chaîne de retrait affirmée octet pour octet"
        );
    }

    /// `Scenario` : « Lecture — aller-retour restitue l'identité quadruplée » (2026-09-27).
    #[test]
    fn lecture_aller_retour_restitue_identite_quadruplee() {
        let key = test_key();
        let identity = identity_with(make_jwt(future_exp()));

        let set_cookie = build_set_cookie(&identity, &key, true);
        let cookie_header = cookie_pair(&set_cookie);
        let restored = extract_session(Some(&cookie_header), &key).expect("valid round-trip");

        assert_eq!(restored.id_token, identity.id_token);
        assert_eq!(restored.subject, "user-123");
        assert_eq!(restored.email, identity.email);
        assert_eq!(
            restored.preferred_username,
            Some("alice".to_string()),
            "le claim survit au cycle session (2026-09-27)"
        );
    }

    /// `Scenario` : « Lecture — payload hérité à trois clés rend `preferred_username` None »
    /// (2026-09-27). Valeur scellée à la main via `cookie::CookieJar`, sans
    /// `build_set_cookie`.
    #[test]
    fn lecture_payload_herite_trois_cles_rend_preferred_username_none() {
        let key = test_key();
        let id_token = make_jwt(future_exp());
        let legacy_json =
            format!(r#"{{"id_token":"{id_token}","subject":"legacy-user","email":"legacy@example.com"}}"#);

        let mut jar = cookie::CookieJar::new();
        jar.private_mut(&key)
            .add(cookie::Cookie::new(SESSION_COOKIE_NAME, legacy_json));
        let sealed = format!(
            "{SESSION_COOKIE_NAME}={}",
            jar.get(SESSION_COOKIE_NAME)
                .expect("sealed cookie present in delta")
                .value()
        );

        let restored = extract_session(Some(&sealed), &key)
            .expect("session posée avant le quatrième champ non invalidée (2026-09-27)");

        assert_eq!(restored.id_token, id_token);
        assert_eq!(restored.subject, "legacy-user");
        assert_eq!(restored.email, Some("legacy@example.com".to_string()));
        assert_eq!(
            restored.preferred_username, None,
            "payload hérité à trois clés : le claim regagnera le payload au re-login"
        );
    }

    /// `Scenario` : « Lecture — ligne de session sélectionnée parmi plusieurs cookies ».
    #[test]
    fn lecture_ligne_de_session_selectionnee_parmi_plusieurs_cookies() {
        let key = test_key();
        let identity = identity_with(make_jwt(future_exp()));
        let pair = cookie_pair(&build_set_cookie(&identity, &key, true));
        let header = format!("x=1; {pair}; csrftoken=two");

        let restored =
            extract_session(Some(&header), &key).expect("ligne retrouvée parmi les cookies étrangers");

        assert_eq!(restored.id_token, identity.id_token);
        assert_eq!(restored.subject, "user-123");
        assert_eq!(restored.preferred_username.as_deref(), Some("alice"));
    }

    /// `Scenario` : « Lecture — en-tête `Cookie` absent rend `MRD-AUTH-001` » (test inline
    /// historique, appariement de variante, pas de chaîne).
    #[test]
    fn missing_cookie_returns_not_authenticated() {
        let key = test_key();
        assert!(matches!(
            extract_session(None, &key),
            Err(AuthError::NotAuthenticated)
        ));
    }

    /// `Scenario` : « Lecture — absence de la ligne de session rend `MRD-AUTH-001` » — ligne
    /// sans `=` ignorée, segment vide final filtré, et surtout pas `MRD-AUTH-002`.
    #[test]
    fn lecture_absence_de_la_ligne_de_session_rend_mrd_auth_001() {
        let key = test_key();
        let header = "other=1; miryad_autre=v; fragment";

        let result = extract_session(Some(header), &key);
        assert!(
            matches!(result, Err(AuthError::NotAuthenticated)),
            "un nom absent ne se confond jamais avec un cookie vidé : {:?}",
            result.err()
        );
    }

    /// `Scenario` : « Lecture — clé différente rend `MRD-AUTH-002` » — sous-clés de
    /// chiffrement `32..64` différentes, tag GCM non vérifié.
    #[test]
    fn lecture_cle_differente_rend_mrd_auth_002() {
        let key_a = test_key();
        let key_b = other_key();
        let identity = identity_with(make_jwt(future_exp()));
        let pair = cookie_pair(&build_set_cookie(&identity, &key_a, false));

        let result = extract_session(Some(&pair), &key_b);
        assert!(
            matches!(result, Err(AuthError::InvalidSession)),
            "le tag `AES-256-GCM` ne se vérifie pas sous une autre clé : {:?}",
            result.err()
        );
    }

    /// `Scenario` : « Lecture — valeur altérée rend `MRD-AUTH-002` » — un seul caractère de la
    /// partie chiffrée remplacé (hors bourrage `=`), le tag GCM casse.
    #[test]
    fn lecture_valeur_alteree_rend_mrd_auth_002() {
        let key = test_key();
        let identity = identity_with(make_jwt(future_exp()));
        let pair = cookie_pair(&build_set_cookie(&identity, &key, false));
        let value = pair.split_once('=').expect("pair has a value").1;

        let mut chars: Vec<char> = value.chars().collect();
        let middle = chars.len() / 2;
        chars[middle] = if chars[middle] == 'Q' { 'R' } else { 'Q' };
        let altered: String = chars.into_iter().collect();
        assert_ne!(altered, value, "un caractère de la zone chiffrée a été remplacé");

        let header = format!("{SESSION_COOKIE_NAME}={altered}");
        let result = extract_session(Some(&header), &key);
        assert!(
            matches!(result, Err(AuthError::InvalidSession)),
            "la retouche, d'un seul caractère soit-elle, casse le tag GCM : {:?}",
            result.err()
        );
    }

    /// `Scenario` : « Lecture — cookie effacé reposté rend `MRD-AUTH-002` » — distinct de
    /// l'absence (`MRD-AUTH-001`), l'un et l'autre affirmés ici par appariement de variante.
    #[test]
    fn lecture_cookie_efface_reposte_rend_mrd_auth_002() {
        let key = test_key();
        let reposted = format!("{SESSION_COOKIE_NAME}=");

        let result = extract_session(Some(&reposted), &key);
        assert!(
            matches!(result, Err(AuthError::InvalidSession)),
            "valeur décodée de 0 octet ≤ nonce de 12 : {:?}",
            result.err()
        );
        assert!(
            matches!(extract_session(None, &key), Err(AuthError::NotAuthenticated)),
            "l'absence reste MRD-AUTH-001 : un effacement reposté n'est pas une absence"
        );
    }

    /// `Scenario` : « Lecture — payload déchiffré non-JSON rend `MRD-AUTH-002` » — valeur
    /// scellée à la main, sans `build_set_cookie`.
    #[test]
    fn lecture_payload_dechiffre_non_json_rend_mrd_auth_002() {
        let key = test_key();
        let pair = seal_raw(SESSION_COOKIE_NAME, "pas-du-json", &key);

        let result = extract_session(Some(&pair), &key);
        assert!(
            matches!(result, Err(AuthError::InvalidSession)),
            "`PrivateJar::decrypt` passe, la désérialisation échoue : {:?}",
            result.err()
        );
    }

    /// `Scenario` : « Lecture — `id_token` sans claim `exp` rend `MRD-AUTH-002` » — la pose a
    /// rendu `Max-Age=0`, la lecture n'a pas de repli `0` : la claim est exigée.
    #[test]
    fn lecture_id_token_sans_claim_exp_rend_mrd_auth_002() {
        let key = test_key();
        let set_cookie = build_set_cookie(&identity_with(make_jwt_without_exp()), &key, false);
        let pair = cookie_pair(&set_cookie);
        assert_set_cookie_full(&set_cookie, &pair, false, 0, 0);

        let result = extract_session(Some(&pair), &key);
        assert!(
            matches!(result, Err(AuthError::InvalidSession)),
            "`extract_exp_claim` ne trouve aucune claim, `ok_or` exige la claim : {:?}",
            result.err()
        );
    }

    /// `Scenario` : « Lecture — `exp` flottant exactement entier est accepté, aller-retour
    /// bit-exact » (arbitré 2026-09-30, option A′). Le `Scenario` écrit `"exp": 1.7e9` —
    /// l'entier `1700000000`, désormais passé (nous sommes en 2026) ; sa clause « postérieur à
    /// l'heure courante de la fixture » est verrouillée à `now + 3600` rendu en notation
    /// flottante (`<n>.0` = représentation bit-exacte de l'entier, tolérance de format seul),
    /// et le passage par le littéral `1.7e9` — rouge avant le portillon, rejeté en
    /// `MRD-AUTH-002` par le parse `Option<u64>` — est verrouillé par
    /// `lecture_exp_flottant_1_7e9_rejeté_comme_expire_apres_le_portillon`.
    #[test]
    fn lecture_exp_flottant_exactement_entier_accepte_aller_retour_bit_exact() {
        let key = test_key();
        let exp = now_secs() + 3600;
        let float_identity = identity_with(make_jwt_exp_json(&format!("{exp}.0")));
        let int_identity = identity_with(make_jwt(exp));

        let set_float = build_set_cookie(&float_identity, &key, false);
        let pair_float = cookie_pair(&set_float);
        // La pose relit l'exp par le même extract_exp_claim : le portillon s'y applique aussi.
        // Avant l'arbitrage 2026-09-30, le flottant était illisible et la pose rendait
        // silencieusement `Max-Age=0` ; après, Max-Age porte l'intervalle réel — contrat voulu,
        // cohérent lecture/pose (consigné dans le rapport B1a).
        assert_set_cookie_full(
            &set_float,
            &pair_float,
            false,
            exp - now_secs() - 1,
            exp - now_secs(),
        );

        let restored = extract_session(Some(&pair_float), &key)
            .expect("un f64 entier dans 0..<2^53 est la valeur temporelle exacte");
        assert_eq!(restored.id_token, float_identity.id_token);
        assert_eq!(restored.subject, "user-123");
        assert_eq!(restored.email, float_identity.email);
        assert_eq!(restored.preferred_username, Some("alice".to_string()));

        // « identique à celui du même payload écrit `"exp": <n>` » : lecture integer seule,
        // puis lecture des deux identités sous un en-tête combiné — première occurrence
        // retenue (contrat `find_map` de `Handles`), la seconde est rendue intacte par la
        // dédup, pas écrasée.
        let pair_int = cookie_pair(&build_set_cookie(&int_identity, &key, false));
        let restored_int = extract_session(Some(&pair_int), &key)
            .expect("la même valeur écrite en entier lit à l'identique");
        let both = format!("{pair_float}; {pair_int}");
        let restored_first = extract_session(Some(&both), &key)
            .expect("float-written id_token is the first occurrence of the session name");
        assert_eq!(restored_first.id_token, float_identity.id_token);
        assert_eq!(restored.subject, restored_int.subject);
        assert_eq!(restored.email, restored_int.email);
        assert_eq!(restored.preferred_username, restored_int.preferred_username);
    }

    /// Contrôle positif du littéral exact du `Scenario`, `"exp": 1.7e9` = `1700000000`. Cette
    /// valeur est passée à la date de la fixture (2026) : le texte du `Scenario` la dit
    /// « postérieure à l'heure courante », l'affirmation est devenue fausse — écart consigné
    /// dans le rapport B1a, la sémantique du Scenario est verrouillée par le test précédent
    /// (`now + 3600` en notation flottante, rouge avant portillon). Ce verrou-ci prouve que le
    /// littéral traverse le portillon comme une valeur **lisible** : après l'arbitrage, la pose
    /// lit `1700000000` (passé) et la pose comme la lecture rendent leur repli/`MRD-AUTH-002`
    /// d'horloge (`exp <= now`), non plus une erreur de désérialisation. Non discriminant en
    /// sortie (avant : parse refusé, après : horloge — même variante observable, même
    /// `Max-Age=0`), consigné comme tel ; le rouge attendu est porté par le test précédent.
    #[test]
    fn lecture_exp_flottant_1_7e9_lisible_puis_rejete_par_horloge() {
        let key = test_key();
        let identity = identity_with(make_jwt_exp_json("1.7e9"));
        let set_cookie = build_set_cookie(&identity, &key, false);
        let pair = cookie_pair(&set_cookie);

        assert_set_cookie_full(&set_cookie, &pair, false, 0, 0);
        let result = extract_session(Some(&pair), &key);
        assert!(
            matches!(result, Err(AuthError::InvalidSession)),
            "1.7e9 = 1700000000 est une valeur lisible, rejetée par l'horloge : {result:?}"
        );
    }

    /// `Scenario` : « Lecture — `exp` flottant sans valeur temporelle exacte rend
    /// `MRD-AUTH-002` » (arbitré 2026-09-30) — fraction, négatif et `>= 2^53` refusés par le
    /// portillon, aucune troncature silencieuse. Ces trois cas pouvaient déjà passer avant
    /// l'arbitrage (le parse `Option<u64>` refusait tout flottant) : verrouillés contre la
    /// régression d'une tolérance mal calibrée.
    #[test]
    fn lecture_exp_flottant_sans_valeur_temporelle_exacte_rend_mrd_auth_002() {
        let key = test_key();
        for exp_json in ["1700000000.5", "-1", "9007199254740993.0"] {
            let pair = seal_session_line(&make_jwt_exp_json(exp_json), &key);
            let result = extract_session(Some(&pair), &key);
            assert!(
                matches!(result, Err(AuthError::InvalidSession)),
                "`exp` {exp_json} hors du portillon (fraction, négatif, au-delà de la mantisse) : {result:?}"
            );
        }
    }

    /// `Scenario` : « Lecture — payload de JWT bourré rend `MRD-AUTH-002` » — `URL_SAFE_NO_PAD`
    /// refuse le bourrage `=`, la claim devient introuvable ; la pose portait `Max-Age=0` pour
    /// cette même raison.
    #[test]
    fn lecture_payload_de_jwt_bourre_rend_mrd_auth_002() {
        let key = test_key();
        let identity = identity_with(make_jwt_padded(future_exp()));

        let set_cookie = build_set_cookie(&identity, &key, false);
        let pair = cookie_pair(&set_cookie);
        assert_set_cookie_full(&set_cookie, &pair, false, 0, 0);

        let result = extract_session(Some(&pair), &key);
        assert!(
            matches!(result, Err(AuthError::InvalidSession)),
            "`InvalidPadding` sur le segment payload : la claim est introuvable : {:?}",
            result.err()
        );
    }

    /// `Scenario` : « Lecture — expiration dépassée rend `MRD-AUTH-002` » (test inline
    /// historique, cité par le Scenario) — déchiffrement et JSON passent, c'est le contrôle
    /// d'horloge serveur `exp <= now` qui rejette.
    #[test]
    fn expired_session_returns_invalid() {
        let key = test_key();
        let identity = identity_with(make_jwt(1_000_000));
        let pair = cookie_pair(&build_set_cookie(&identity, &key, false));

        let result = extract_session(Some(&pair), &key);
        assert!(matches!(result, Err(AuthError::InvalidSession)));
    }

    /// `Scenario` : « Lecture — valeur du cookie pending transplantée rend `MRD-AUTH-002` » —
    /// le nom du cookie est la donnée associée AEAD, aucune valeur ne traverse du pending vers
    /// la session.
    #[test]
    fn lecture_valeur_pending_transplantee_rend_mrd_auth_002() {
        let key = test_key();
        let pending_pair = seal_raw("miryad_oidc_pending", "csrf-secret:nonce-secret", &key);
        let pending_value = pending_pair.split_once('=').expect("pending pair has a value").1;
        let transplanted = format!("{SESSION_COOKIE_NAME}={pending_value}");

        let result = extract_session(Some(&transplanted), &key);
        assert!(
            matches!(result, Err(AuthError::InvalidSession)),
            "le nom est l'AAD GCM : rien ne traverse du pending vers la session : {:?}",
            result.err()
        );
    }

    /// `Scenario` : « Lecture — doublon de session et première ligne invalide rend
    /// `MRD-AUTH-002` » — `find_map` retient la première occurrence, sans rattrapage.
    #[test]
    fn lecture_doublon_de_session_premiere_ligne_invalide_rend_mrd_auth_002() {
        let key = test_key();
        let pair = cookie_pair(&build_set_cookie(
            &identity_with(make_jwt(future_exp())),
            &key,
            false,
        ));
        let header = format!("{SESSION_COOKIE_NAME}=invalide; {pair}");

        let result = extract_session(Some(&header), &key);
        assert!(
            matches!(result, Err(AuthError::InvalidSession)),
            "dupliquer le cookie de session ne force aucun rattrapage : {:?}",
            result.err()
        );
    }
}
