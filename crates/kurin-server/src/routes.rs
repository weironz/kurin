//! Router and handlers (WP-50 surface: `/healthz`, `/session` and a
//! placeholder `/api/v1/meta`; the cluster, operation and store APIs land
//! in WP-51+).

use std::collections::BTreeMap;
use std::time::SystemTime;

use axum::extract::rejection::JsonRejection;
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use crate::error::ApiError;
use crate::guard;
use crate::session;
use crate::state::AppState;

/// The WP-50 API surface behind the UI §6.10 guards. Layers run
/// outermost-last: the request logger wraps the guards, the guards wrap the
/// handlers (and the 404 fallback).
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/api/v1/healthz", get(healthz))
        .route(
            "/api/v1/session",
            get(session_info)
                .post(create_session)
                .delete(delete_session),
        )
        .route("/api/v1/meta", get(meta))
        .fallback(not_found)
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            guard::guard,
        ))
        .layer(axum::middleware::from_fn(guard::log_request))
        .with_state(state)
}

/// `GET /healthz` — liveness, no auth (UI §6.10 exempts it).
async fn healthz() -> Json<Healthz> {
    Json(Healthz { status: "ok" })
}

#[derive(Serialize)]
struct Healthz {
    status: &'static str,
}

/// `POST /api/v1/session` body (UI §6.10: the token from the URL hash).
#[derive(Deserialize)]
struct LoginRequest {
    token: String,
}

/// `POST`/`GET /api/v1/session` response. Field names follow the WP-60
/// frontend contract (`camelCase`).
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SessionInfo {
    expires_at: String,
    version: &'static str,
}

/// `POST /api/v1/session`: exchange the bootstrap token for a session
/// cookie (UI §6.10).
async fn create_session(
    State(state): State<AppState>,
    body: Result<Json<LoginRequest>, JsonRejection>,
) -> Response {
    let Json(body) = match body {
        Ok(body) => body,
        Err(rejection) => return ApiError::bad_json(&rejection).into_response(),
    };
    if !state.verify_token(&body.token) {
        return ApiError::unauthorized("令牌无效").into_response();
    }
    let created = state.create_session();
    respond_with_cookie(
        Json(SessionInfo {
            expires_at: rfc3339(created.expires_at),
            version: state.version(),
        })
        .into_response(),
        session::session_cookie(&created.id, state.secure_cookies()),
    )
}

/// `GET /api/v1/session`: metadata of the current session.
async fn session_info(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<SessionInfo>, ApiError> {
    let id = session::cookie_value(&headers, session::SESSION_COOKIE)
        .ok_or_else(|| ApiError::unauthorized("未登录"))?;
    let deadline = state
        .session_deadline(&id)
        .ok_or_else(|| ApiError::unauthorized("未登录"))?;
    Ok(Json(SessionInfo {
        expires_at: rfc3339(deadline),
        version: state.version(),
    }))
}

/// `DELETE /api/v1/session`: log out and clear the cookie.
async fn delete_session(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(id) = session::cookie_value(&headers, session::SESSION_COOKIE) {
        state.remove_session(&id);
    }
    respond_with_cookie(
        StatusCode::NO_CONTENT.into_response(),
        session::clearing_cookie(state.secure_cookies()),
    )
}

/// `GET /api/v1/meta`: placeholder returning the version; WP-54 fills the
/// component version table (`create config --print-versions`) and the
/// capability table.
async fn meta(State(state): State<AppState>) -> Json<Meta> {
    Json(Meta {
        version: state.version(),
        component_versions: BTreeMap::new(),
        capabilities: BTreeMap::new(),
    })
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Meta {
    version: &'static str,
    component_versions: BTreeMap<String, String>,
    capabilities: BTreeMap<String, String>,
}

/// Unknown path → the unified 404 envelope. (WP-55 splits this into the
/// SPA fallback and the `/api` 404.)
async fn not_found(_req: Request) -> Response {
    ApiError::NotFound.into_response()
}

fn respond_with_cookie(mut response: Response, cookie: String) -> Response {
    let value = HeaderValue::from_str(&cookie).expect("cookie value is ASCII");
    response.headers_mut().append(header::SET_COOKIE, value);
    response
}

/// RFC 3339 UTC timestamp (the frontend `Timestamp` is a string).
fn rfc3339(timestamp: SystemTime) -> String {
    OffsetDateTime::from(timestamp)
        .format(&Rfc3339)
        .expect("RFC 3339 formatting cannot fail")
}
