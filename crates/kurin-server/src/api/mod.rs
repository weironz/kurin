//! Cluster, configuration and operation endpoints (UI §7, WP-52).
//!
//! These are the routes the OpRunner needs: register/import a cluster,
//! submit an operation, follow its events/logs, cancel and retry. The
//! remaining read-only endpoints (plan, schema, addons, packages) belong to
//! WP-54.

mod clusters;
mod operations;

use std::net::SocketAddr;

use axum::extract::connect_info::ConnectInfo;
use axum::http::HeaderMap;
use axum::routing::{get, post};
use axum::Extension;
use axum::Router;
use sha2::{Digest, Sha256};

use crate::session;
use crate::state::AppState;

/// Routes merged into the main router.
pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/api/v1/clusters",
            get(clusters::list).post(clusters::create),
        )
        .route("/api/v1/clusters/{name}", get(clusters::detail))
        .route(
            "/api/v1/clusters/{name}/config",
            get(clusters::get_config).put(clusters::put_config),
        )
        .route("/api/v1/clusters/{name}/refresh", post(clusters::refresh))
        .route(
            "/api/v1/clusters/{name}/operations",
            post(operations::submit),
        )
        .route("/api/v1/operations", get(operations::list))
        .route("/api/v1/operations/{id}", get(operations::detail))
        .route("/api/v1/operations/{id}/events", get(operations::events))
        .route("/api/v1/operations/{id}/logs", get(operations::logs))
        .route("/api/v1/operations/{id}/cancel", post(operations::cancel))
        .route("/api/v1/operations/{id}/retry", post(operations::retry))
}

/// Actor recorded on an operation: a short hash of the session id plus the
/// peer address (UI §6.7). The raw cookie value is a bearer credential and
/// never reaches the database.
pub(crate) fn actor(
    headers: &HeaderMap,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
) -> String {
    let session = session::cookie_value(headers, session::SESSION_COOKIE).unwrap_or_default();
    let digest = Sha256::digest(session.as_bytes());
    let short = digest
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let ip = peer
        .map(|Extension(ConnectInfo(addr))| addr.ip().to_string())
        .unwrap_or_else(|| "unknown".into());
    format!("{short}@{ip}")
}
