//! Shared server state: the token store, the sessions, the host allowlist
//! and the drain flag, all behind one `Arc` handed to the router.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use crate::auth::TokenStore;
use crate::config::ServeOptions;
use crate::host::HostAllowlist;
use crate::session::{Session, SessionStore};

/// Cloneable handle to the server state.
#[derive(Clone)]
pub struct AppState(Arc<Inner>);

struct Inner {
    token: Mutex<TokenStore>,
    sessions: SessionStore,
    hosts: HostAllowlist,
    version: &'static str,
    shutdown: AtomicBool,
    secure_cookies: bool,
}

impl AppState {
    /// Wrap an already loaded token store.
    pub fn new(opts: &ServeOptions, token: TokenStore) -> Self {
        Self(Arc::new(Inner {
            token: Mutex::new(token),
            sessions: SessionStore::new(),
            hosts: HostAllowlist::new(opts.listen.port(), &opts.public_hosts),
            version: env!("CARGO_PKG_VERSION"),
            shutdown: AtomicBool::new(false),
            secure_cookies: opts.tls.is_some(),
        }))
    }

    /// kurin version reported by `/api/v1/meta` and the session info.
    pub fn version(&self) -> &'static str {
        self.0.version
    }

    /// Constant-time token check (the login exchange).
    pub fn verify_token(&self, presented: &str) -> bool {
        self.token().verify(presented)
    }

    /// Generate a fresh token, persist it and drop every session
    /// (`kurin serve --rotate-token`).
    pub fn rotate_token(&self) -> std::io::Result<()> {
        self.token().rotate()?;
        self.0.sessions.invalidate_all();
        Ok(())
    }

    /// The current token value — only for building the one-time login link.
    pub(crate) fn token_value(&self) -> String {
        self.token().current().to_string()
    }

    /// Create a session (the login exchange succeeded).
    pub fn create_session(&self) -> Session {
        self.0.sessions.create()
    }

    /// True when `id` names a live session.
    pub fn validate_session(&self, id: &str) -> bool {
        self.0.sessions.validate(id)
    }

    /// Remove a session (logout).
    pub fn remove_session(&self, id: &str) {
        self.0.sessions.remove(id);
    }

    /// Deadline of a live session, if any.
    pub fn session_deadline(&self, id: &str) -> Option<SystemTime> {
        self.0.sessions.deadline(id)
    }

    /// `Host` allowlist check (UI §6.10).
    pub fn allowed_host(&self, host: &str) -> bool {
        self.0.hosts.allows(host)
    }

    /// Enter drain mode: `/api/v1` answers 503 while in-flight requests
    /// finish (UI §7).
    pub fn begin_shutdown(&self) {
        self.0.shutdown.store(true, Ordering::SeqCst);
    }

    /// True once the shutdown started.
    pub fn is_shutting_down(&self) -> bool {
        self.0.shutdown.load(Ordering::SeqCst)
    }

    /// Whether cookies carry the `Secure` attribute (TLS only).
    pub fn secure_cookies(&self) -> bool {
        self.0.secure_cookies
    }

    fn token(&self) -> std::sync::MutexGuard<'_, TokenStore> {
        self.0.token.lock().expect("token store poisoned")
    }
}
