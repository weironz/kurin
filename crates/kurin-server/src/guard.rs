//! Request guards and logging (UI §6.10 + the WP-50 request log).

use std::time::Instant;

use axum::extract::{Request, State};
use axum::http::{header, Method};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::error::ApiError;
use crate::session;
use crate::state::AppState;

/// `tracing` target of the HTTP request log. Deliberately separate from the
/// operation logs (the per-operation event bus and its JSONL files, WP-06):
/// filtering one target never touches the other.
pub const REQUEST_TARGET: &str = "kurin_server::request";

/// Log every request into `tracing`: method, path, status and duration.
/// Query strings and headers are never logged (R8: the login token rides in
/// a header or the URL hash, both must stay out of the logs).
pub async fn log_request(req: Request, next: Next) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let started = Instant::now();
    let response = next.run(req).await;
    tracing::info!(
        target: REQUEST_TARGET,
        method = %method,
        path = %path,
        status = response.status().as_u16(),
        elapsed_ms = started.elapsed().as_millis() as u64,
        "request"
    );
    response
}

/// The UI §6.10 guards, in order:
///
/// 1. Host allowlist (`:authority` over HTTP/2, `Host` header otherwise) —
///    every path, `/healthz` included;
/// 2. drain mode — `/api` answers 503 once shutdown began;
/// 3. JSON `Content-Type` on mutating requests (a cross-site form cannot
///    fake it);
/// 4. session cookie for everything under `/api/v1` except the liveness
///    probe and the token exchange — enforced even on loopback, so a
///    random web page cannot probe the local port.
pub async fn guard(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let host = request_host(&req);
    if !state.allowed_host(host) {
        return ApiError::forbidden_host(host).into_response();
    }

    if req.uri().path().starts_with("/api/") {
        if state.is_shutting_down() {
            return ApiError::ShuttingDown.into_response();
        }
        if requires_json(&req) && !is_json(&req) {
            return ApiError::bad_content_type(
                req.headers()
                    .get(header::CONTENT_TYPE)
                    .and_then(|value| value.to_str().ok()),
            )
            .into_response();
        }
        if !is_session_exempt(&req) && !has_live_session(&state, &req) {
            return ApiError::unauthorized("未登录").into_response();
        }
    }

    next.run(req).await
}

/// The host the client claims: the URI authority (HTTP/2 has no `Host`
/// header) with the `Host` header as the HTTP/1.1 fallback.
fn request_host(req: &Request) -> &str {
    req.uri()
        .host()
        .or_else(|| {
            req.headers()
                .get(header::HOST)
                .and_then(|value| value.to_str().ok())
        })
        .unwrap_or_default()
}

/// `/api/v1/healthz` is the liveness probe and `POST /api/v1/session` is
/// the token exchange itself; every other `/api/v1` request needs a
/// session (UI §6.10).
fn is_session_exempt(req: &Request) -> bool {
    req.uri().path() == "/api/v1/healthz"
        || (req.method() == Method::POST && req.uri().path() == "/api/v1/session")
}

fn has_live_session(state: &AppState, req: &Request) -> bool {
    session::cookie_value(req.headers(), session::SESSION_COOKIE)
        .is_some_and(|id| state.validate_session(&id))
}

/// Mutating methods carry their parameters as JSON. `DELETE` usually has no
/// body; when it does, it must be JSON too.
fn requires_json(req: &Request) -> bool {
    match *req.method() {
        Method::POST | Method::PUT | Method::PATCH => true,
        Method::DELETE => req
            .headers()
            .get(header::CONTENT_LENGTH)
            .is_some_and(|len| len != "0"),
        _ => false,
    }
}

/// `application/json`; parameters such as `; charset=utf-8` are ignored, as
/// is the case of the media type.
fn is_json(req: &Request) -> bool {
    req.headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|content_type| content_type.split(';').next())
        .is_some_and(|media_type| media_type.trim().eq_ignore_ascii_case("application/json"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request as HttpRequest;

    fn req(method: Method, content_type: Option<&str>, length: Option<&str>) -> Request {
        let mut builder = HttpRequest::builder().method(method).uri("/api/v1/x");
        if let Some(ct) = content_type {
            builder = builder.header(header::CONTENT_TYPE, ct);
        }
        if let Some(len) = length {
            builder = builder.header(header::CONTENT_LENGTH, len);
        }
        builder.body(Body::empty()).unwrap()
    }

    #[test]
    fn content_type_rules() {
        for method in [Method::POST, Method::PUT, Method::PATCH] {
            assert!(requires_json(&req(method.clone(), None, None)), "{method}");
            assert!(is_json(&req(
                method.clone(),
                Some("application/json; charset=utf-8"),
                None
            )));
            assert!(is_json(&req(
                method.clone(),
                Some("Application/JSON"),
                None
            )));
            assert!(!is_json(&req(
                method.clone(),
                Some("application/x-www-form-urlencoded"),
                None
            )));
            assert!(!is_json(&req(method.clone(), None, None)));
        }
        // GET is never body-carrying; DELETE only when a body is present.
        assert!(!requires_json(&req(Method::GET, None, None)));
        assert!(!requires_json(&req(Method::DELETE, None, None)));
        assert!(requires_json(&req(Method::DELETE, None, Some("12"))));
        assert!(!requires_json(&req(Method::DELETE, None, Some("0"))));
    }

    fn exempt(method: Method, uri: &str) -> bool {
        let req = HttpRequest::builder()
            .method(method)
            .uri(uri)
            .body(Body::empty())
            .unwrap();
        is_session_exempt(&req)
    }

    #[test]
    fn only_liveness_and_login_skip_the_session_check() {
        assert!(exempt(Method::GET, "/api/v1/healthz"));
        assert!(exempt(Method::POST, "/api/v1/session"));
        assert!(!exempt(Method::GET, "/api/v1/session"));
        assert!(!exempt(Method::DELETE, "/api/v1/session"));
        assert!(!exempt(Method::GET, "/api/v1/meta"));
    }
}
