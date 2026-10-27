//! Cookie sessions (UI §6.10): `HttpOnly; SameSite=Strict`, held in memory.
//!
//! The browser exchanges the bootstrap token for a session cookie once
//! (`POST /api/v1/session`); every later API call rides the cookie, which a
//! cross-site page cannot attach thanks to `SameSite=Strict`.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, SystemTime};

use axum::http::{header, HeaderMap};

/// Session cookie name.
pub const SESSION_COOKIE: &str = "kurin_session";

/// Session lifetime; a new login link is needed after it expires.
pub const SESSION_TTL: Duration = Duration::from_secs(24 * 60 * 60);

type Sessions = HashMap<String, SystemTime>;

/// In-memory session registry. Sessions never outlive the process, so a
/// restart (or a token rotation) logs everyone out by construction.
#[derive(Default)]
pub struct SessionStore {
    sessions: Mutex<Sessions>,
}

/// A freshly created session.
pub struct Session {
    pub id: String,
    pub expires_at: SystemTime,
}

impl SessionStore {
    /// Empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a session (login succeeded); expired ones are pruned on the way.
    pub fn create(&self) -> Session {
        let mut sessions = self.lock();
        sessions.retain(|_, deadline| *deadline > SystemTime::now());
        let id = crate::auth::random_hex(32);
        let expires_at = SystemTime::now() + SESSION_TTL;
        sessions.insert(id.clone(), expires_at);
        Session { id, expires_at }
    }

    /// True when `id` names a live (unexpired) session.
    pub fn validate(&self, id: &str) -> bool {
        self.deadline(id).is_some()
    }

    /// Deadline of a live session, if any.
    pub fn deadline(&self, id: &str) -> Option<SystemTime> {
        self.lock()
            .get(id)
            .copied()
            .filter(|deadline| *deadline > SystemTime::now())
    }

    /// Remove one session (logout).
    pub fn remove(&self, id: &str) {
        self.lock().remove(id);
    }

    /// Drop every session (token rotation).
    pub fn invalidate_all(&self) {
        self.lock().clear();
    }

    fn lock(&self) -> MutexGuard<'_, Sessions> {
        self.sessions.lock().expect("session store poisoned")
    }
}

/// `Set-Cookie` header value for a fresh session.
pub fn session_cookie(id: &str, secure: bool) -> String {
    format!(
        "{SESSION_COOKIE}={id}; Path=/; Max-Age={}; HttpOnly; SameSite=Strict{}",
        SESSION_TTL.as_secs(),
        if secure { "; Secure" } else { "" }
    )
}

/// `Set-Cookie` header value clearing the session cookie (logout).
pub fn clearing_cookie(secure: bool) -> String {
    format!(
        "{SESSION_COOKIE}=; Path=/; Max-Age=0; HttpOnly; SameSite=Strict{}",
        if secure { "; Secure" } else { "" }
    )
}

/// Value of cookie `name` across the (possibly several) `Cookie` headers.
pub fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    for header in headers.get_all(header::COOKIE) {
        let value = header.to_str().ok()?;
        for pair in value.split(';') {
            let mut parts = pair.trim().splitn(2, '=');
            if parts.next() == Some(name) {
                return parts.next().map(str::to_string);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn cookie_roundtrip() {
        let mut headers = HeaderMap::new();
        headers.append(
            header::COOKIE,
            HeaderValue::from_static("other=1; kurin_session=abc; more=2"),
        );
        assert_eq!(
            cookie_value(&headers, SESSION_COOKIE),
            Some("abc".to_string())
        );
        assert_eq!(cookie_value(&headers, "other"), Some("1".to_string()));
        assert_eq!(cookie_value(&headers, "missing"), None);
    }

    #[test]
    fn session_cookie_carries_the_hardening_flags() {
        let cookie = session_cookie("id", true);
        assert!(cookie.starts_with("kurin_session=id;"));
        for flag in [
            "Path=/",
            "Max-Age=86400",
            "HttpOnly",
            "SameSite=Strict",
            "Secure",
        ] {
            assert!(cookie.contains(flag), "{cookie} lacks {flag}");
        }
        assert!(!clearing_cookie(false).contains("Secure"));
    }

    #[test]
    fn sessions_validate_expire_and_invalidate() {
        let store = SessionStore::new();
        let session = store.create();
        assert!(store.validate(&session.id));
        assert_eq!(store.deadline(&session.id), Some(session.expires_at));

        store.remove(&session.id);
        assert!(!store.validate(&session.id));

        let session = store.create();
        store.invalidate_all();
        assert!(!store.validate(&session.id));
    }
}
