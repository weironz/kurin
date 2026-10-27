//! Kurin web service (WP-50): the axum router skeleton, the UI §6.10
//! security guards and the token → cookie authentication flow.
//!
//! Design: `docs/ui/ui-architecture-final.md` §3.1/§6.10/§7. The crate is
//! deliberately free of `kurin-ops` dependencies until WP-52 introduces
//! the OpRunner; the surface is `/healthz`, `/api/v1/session` and a
//! placeholder `/api/v1/meta`.
//!
//! The auth flow:
//!
//! 1. first start generates a 256-bit token into `<data>/token` (0600) and
//!    prints the `…/#token=…` login link exactly once;
//! 2. the frontend exchanges the token for a `kurin_session` cookie
//!    (`HttpOnly; SameSite=Strict`) via `POST /api/v1/session`;
//! 3. every other `/api/v1` call needs that cookie — even on loopback;
//! 4. `DELETE /api/v1/session` logs out, `--rotate-token` starts a fresh
//!    token and invalidates every session.

mod auth;
mod config;
mod error;
mod guard;
mod host;
mod routes;
mod session;
mod state;
pub mod store;

use std::os::unix::fs::DirBuilderExt;
use std::time::Duration;

use axum::Router;

pub use auth::TokenStore;
pub use config::{
    default_data_dir, default_listen, validate, ServeOptions, TlsPaths, DEFAULT_LISTEN,
};
pub use error::{ApiError, Diagnostic, Label};
pub use host::HostAllowlist;
pub use routes::router;
pub use session::{SessionStore, SESSION_COOKIE, SESSION_TTL};
pub use state::AppState;
pub use store::{
    ClusterRecord, ConfigRevision, Lifecycle, RevisionSummary, Store, StoreError, StoreOptions,
    SECRET_SENTINEL,
};

/// Failures of `kurin serve` itself (bad arguments, IO).
#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    #[error("serve 参数错误：{0}")]
    InvalidArgs(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Result of [`startup`]: the ready state plus the one-time print flag.
pub struct Startup {
    pub state: AppState,
    /// A token was generated (first start or `--rotate-token`): the caller
    /// prints the login link exactly once.
    pub fresh_token: bool,
}

/// Validate `opts`, load (create/rotate) the token and build the state.
pub fn startup(opts: &ServeOptions) -> Result<Startup, ServeError> {
    validate(opts).map_err(ServeError::InvalidArgs)?;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&opts.data_dir)?;
    let (mut token, mut fresh) = TokenStore::load_or_create(&opts.data_dir)?;
    if opts.rotate_token {
        token.rotate()?;
        fresh = true;
    }
    Ok(Startup {
        state: AppState::new(opts, token),
        fresh_token: fresh,
    })
}

/// Run the server until SIGINT/SIGTERM, then drain: `/api/v1` answers 503,
/// in-flight requests get 30 s to finish (UI §6.4/§7).
pub async fn serve(opts: ServeOptions) -> Result<(), ServeError> {
    install_logger();

    let startup = startup(&opts)?;
    if startup.fresh_token {
        println!(
            "登录链接（仅此一次）：{}",
            login_url(&opts, startup.state.token_value())
        );
        eprintln!(
            "[serve] 令牌已写入 {}（0600）；丢失令牌时用 `kurin serve --rotate-token` 重新生成",
            opts.data_dir.join("token").display()
        );
    }
    if !opts.listen.ip().is_loopback() && opts.tls.is_none() {
        eprintln!(
            "[serve] 警告：明文 HTTP 监听非回环地址 {}，令牌与会话 cookie 将以明文传输",
            opts.listen
        );
    }

    let app: Router = router(startup.state.clone());
    let handle = axum_server::Handle::new();
    let draining = drain(handle.clone(), startup.state);
    tokio::spawn(draining);

    match opts.tls.as_ref() {
        Some(tls) => {
            install_crypto_provider();
            let config =
                axum_server::tls_rustls::RustlsConfig::from_pem_file(&tls.cert, &tls.key).await?;
            axum_server::bind_rustls(opts.listen, config)
                .handle(handle)
                .serve(app.into_make_service())
                .await?;
        }
        None => {
            axum_server::bind(opts.listen)
                .handle(handle)
                .serve(app.into_make_service())
                .await?;
        }
    }
    Ok(())
}

/// On the shutdown signal: flip the drain flag (API → 503), stop accepting
/// and give in-flight requests 30 s.
async fn drain(handle: axum_server::Handle<std::net::SocketAddr>, state: AppState) {
    shutdown_signal().await;
    state.begin_shutdown();
    tracing::info!(target: guard::REQUEST_TARGET, "shutdown signal received; draining");
    handle.graceful_shutdown(Some(Duration::from_secs(30)));
}

/// SIGINT or SIGTERM, whichever comes first.
async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut terminate = signal(SignalKind::terminate()).expect("install SIGTERM handler");
        tokio::select! {
            _ = ctrl_c => {}
            _ = terminate.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = ctrl_c.await;
    }
}

/// The one-time login link (UI §3.1). Loopback bindings print `127.0.0.1`;
/// a widened listener uses the first `--public-host`, else its own address.
fn login_url(opts: &ServeOptions, token: String) -> String {
    let scheme = if opts.tls.is_some() { "https" } else { "http" };
    let authority = if opts.listen.ip().is_loopback() {
        format!("127.0.0.1:{}", opts.listen.port())
    } else {
        opts.public_hosts
            .iter()
            .find(|host| !host.trim().is_empty())
            .cloned()
            .unwrap_or_else(|| opts.listen.to_string())
    };
    format!("{scheme}://{authority}/#token={token}")
}

/// stderr log at INFO. Request logs use the `kurin_server::request` target
/// (see [`guard::REQUEST_TARGET`]) so they stay separable from operation
/// output. WP-52 will compose the per-operation `TracingLayer` into this
/// subscriber instead of installing a second global one.
fn install_logger() {
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_target(true)
        .with_max_level(tracing::Level::INFO)
        .try_init();
}

/// rustls needs one process-wide crypto provider; ring is what russh and
/// kurin-kube already ship (UI §5.2), so no second crypto library links in.
fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}
