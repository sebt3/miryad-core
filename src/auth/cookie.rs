use cookie::Key;
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::auth::error::AuthError;
use crate::auth::oidc::OidcIdentity;

pub const SESSION_COOKIE_NAME: &str = "miryad_session";

#[derive(Serialize)]
struct SessionPayloadRef<'a> {
    id_token: &'a str,
    subject: &'a str,
    email: Option<&'a str>,
}

#[derive(Deserialize)]
struct SessionPayload {
    id_token: String,
    subject: String,
    email: Option<String>,
}

pub fn build_set_cookie(identity: &OidcIdentity, key: &Key) -> String {
    let payload = SessionPayloadRef {
        id_token: &identity.id_token,
        subject: &identity.subject,
        email: identity.email.as_deref(),
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

    format!("{SESSION_COOKIE_NAME}={encrypted_value}; HttpOnly; SameSite=Lax; Path=/; Max-Age={max_age}")
}

pub fn extract_session(cookie_header: Option<&str>, key: &Key) -> Result<OidcIdentity, AuthError> {
    let cookie_header = cookie_header.ok_or(AuthError::NotAuthenticated)?;

    let cookie_value = cookie_header
        .split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .find_map(|c| {
            c.split_once('=').and_then(|(name, value)| {
                if name == SESSION_COOKIE_NAME {
                    Some(value.to_string())
                } else {
                    None
                }
            })
        })
        .ok_or(AuthError::NotAuthenticated)?;

    let jar = cookie::CookieJar::new();
    let private_jar = jar.private(key);
    let raw_cookie = cookie::Cookie::new(SESSION_COOKIE_NAME, cookie_value);
    let decrypted = private_jar.decrypt(raw_cookie).ok_or(AuthError::InvalidSession)?;

    let payload: SessionPayload =
        serde_json::from_str(decrypted.value()).map_err(|_| AuthError::InvalidSession)?;

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
    })
}

fn extract_exp_claim(jwt: &str) -> Option<u64> {
    let parts: Vec<&str> = jwt.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    use base64::Engine;
    let payload_json = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(parts[1])
        .ok()?;
    let payload_str = String::from_utf8(payload_json).ok()?;
    let exp_key = "\"exp\":";
    let start = payload_str.find(exp_key)?.saturating_add(exp_key.len());
    let rest = &payload_str[start..];
    let end = rest.find(|c: char| !c.is_ascii_digit())?;
    rest[..end].parse().ok()
}

pub fn clear_cookie() -> String {
    format!("{SESSION_COOKIE_NAME}=; HttpOnly; SameSite=Lax; Path=/; Max-Age=0")
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    fn test_key() -> Key {
        Key::from(&[0u8; 64])
    }

    fn make_jwt(exp: u64) -> String {
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(format!(r#"{{"exp":{exp}}}"#));
        format!("header.{payload}.sig")
    }

    fn cookie_value_only(set_cookie: &str) -> String {
        set_cookie
            .split(';')
            .next()
            .expect("set-cookie always has at least one segment")
            .to_string()
    }

    #[test]
    fn build_and_extract_valid_session() {
        let key = test_key();
        let exp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is after epoch")
            .as_secs()
            + 3600;
        let identity = OidcIdentity {
            id_token: make_jwt(exp),
            subject: "user-123".to_string(),
            email: Some("test@example.com".to_string()),
        };
        let set_cookie = build_set_cookie(&identity, &key);
        let cookie_header = cookie_value_only(&set_cookie);

        let result = extract_session(Some(&cookie_header), &key).expect("valid session");
        assert_eq!(result.subject, "user-123");
        assert_eq!(result.email.as_deref(), Some("test@example.com"));
        assert_eq!(result.id_token, identity.id_token);
    }

    #[test]
    fn expired_session_returns_invalid() {
        let key = test_key();
        let identity = OidcIdentity {
            id_token: make_jwt(1_000_000),
            subject: "user-123".to_string(),
            email: None,
        };
        let set_cookie = build_set_cookie(&identity, &key);
        let cookie_header = cookie_value_only(&set_cookie);

        let result = extract_session(Some(&cookie_header), &key);
        assert!(matches!(result, Err(AuthError::InvalidSession)));
    }

    #[test]
    fn missing_cookie_returns_not_authenticated() {
        let key = test_key();
        assert!(matches!(
            extract_session(None, &key),
            Err(AuthError::NotAuthenticated)
        ));
    }

    #[test]
    fn clear_cookie_has_max_age_zero() {
        let cleared = clear_cookie();
        assert!(cleared.contains("Max-Age=0"));
        assert!(cleared.contains(SESSION_COOKIE_NAME));
    }
}
