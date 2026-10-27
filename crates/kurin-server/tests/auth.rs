//! WP-50 acceptance tests (axum `oneshot`): the UI §6.10 security guards
//! and the token → cookie session flow, exactly the scenarios the work
//! package lists.

use std::path::Path;

use axum::body::Body;
use axum::http::request::Builder;
use axum::http::{header, Method, Request, StatusCode};
use axum::response::Response;
use axum::Router;
use tower::ServiceExt;

use kurin_server::{router, AppState, ServeOptions};

/// The default loopback `Host` every helper sends.
const HOST: &str = "127.0.0.1:7080";

/// Loopback options on the default port, isolated data dir.
fn opts(data_dir: &Path) -> ServeOptions {
    ServeOptions {
        data_dir: data_dir.to_path_buf(),
        ..Default::default()
    }
}

fn state(data_dir: &Path) -> AppState {
    kurin_server::startup(&opts(data_dir))
        .expect("valid options")
        .state
}

/// The token exactly as an operator would read it from `<data>/token`.
fn token(data_dir: &Path) -> String {
    std::fs::read_to_string(data_dir.join("token"))
        .expect("token file exists")
        .trim()
        .to_string()
}

fn builder(method: Method, uri: &str, host: &str) -> Builder {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(header::HOST, host)
}

async fn send(app: Router, request: Request<Body>) -> Response {
    app.oneshot(request).await.expect("infallible")
}

async fn body(response: Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("in-memory body");
    serde_json::from_slice(&bytes).expect("JSON body")
}

/// `POST /api/v1/session` with a JSON token.
async fn login(app: Router, token: &str) -> Response {
    let request = builder(Method::POST, "/api/v1/session", HOST)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            serde_json::json!({ "token": token }).to_string(),
        ))
        .unwrap();
    send(app, request).await
}

/// The `kurin_session=<id>` pair of a `Set-Cookie` header.
fn cookie_pair(response: &Response) -> String {
    response
        .headers()
        .get(header::SET_COOKIE)
        .expect("login sets a cookie")
        .to_str()
        .expect("ASCII cookie")
        .split(';')
        .next()
        .expect("non-empty")
        .to_string()
}

fn with_cookie(method: Method, uri: &str, cookie: &str) -> Request<Body> {
    builder(method, uri, HOST)
        .header(header::COOKIE, cookie)
        .body(Body::empty())
        .unwrap()
}

// ── the WP-50 acceptance scenarios ──────────────────────────────────────

#[tokio::test]
async fn healthz_works_without_auth() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(state(dir.path()));
    for path in ["/healthz", "/api/v1/healthz"] {
        let request = builder(Method::GET, path, HOST)
            .body(Body::empty())
            .unwrap();
        let response = send(app.clone(), request).await;
        assert_eq!(response.status(), StatusCode::OK, "{path}");
        assert_eq!(body(response).await, serde_json::json!({"status": "ok"}));
    }
}

#[tokio::test]
async fn unauthenticated_api_calls_return_401() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(state(dir.path()));

    // Known and unknown API paths alike: the session check runs first,
    // so the 401 never leaks whether a path exists.
    for path in [
        "/api/v1/meta",
        "/api/v1/clusters",
        "/api/v1/definitely-not-yet",
    ] {
        let request = builder(Method::GET, path, HOST)
            .body(Body::empty())
            .unwrap();
        let response = send(app.clone(), request).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{path}");
        let error = body(response).await;
        assert_eq!(error["code"], "unauthorized");
        assert!(error["message"].is_string());
    }

    // A garbage cookie is not a session either.
    let response = send(
        app,
        with_cookie(Method::GET, "/api/v1/meta", "kurin_session=deadbeef"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn missing_host_header_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(state(dir.path()));
    let request = Request::builder()
        .method(Method::GET)
        .uri("/healthz")
        .body(Body::empty())
        .unwrap();
    let response = send(app, request).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(body(response).await["code"], "forbidden-host");
}

#[tokio::test]
async fn wrong_host_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(state(dir.path()));
    for host in ["evil.example", "evil.example:7080", "127.0.0.1:9999"] {
        let request = builder(Method::GET, "/healthz", host)
            .body(Body::empty())
            .unwrap();
        let response = send(app.clone(), request).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "Host {host}");
        assert_eq!(body(response).await["code"], "forbidden-host");
    }
    // The documented loopback names pass; /healthz is exempt from auth but
    // not from the host rule.
    for host in [
        "127.0.0.1:7080",
        "localhost:7080",
        "[::1]:7080",
        "LOCALHOST:7080",
    ] {
        let request = builder(Method::GET, "/healthz", host)
            .body(Body::empty())
            .unwrap();
        let response = send(app.clone(), request).await;
        assert_eq!(response.status(), StatusCode::OK, "Host {host}");
    }
}

#[tokio::test]
async fn public_host_entries_widen_the_allowlist_only_as_given() {
    let dir = tempfile::tempdir().unwrap();
    let options = ServeOptions {
        public_hosts: vec!["kurin.example.com".into(), "box.lan:9090".into()],
        ..opts(dir.path())
    };
    let state = kurin_server::startup(&options).unwrap().state;
    let app = router(state);
    for host in [
        "kurin.example.com:7080",
        "kurin.example.com",
        "box.lan:9090",
    ] {
        let request = builder(Method::GET, "/healthz", host)
            .body(Body::empty())
            .unwrap();
        let response = send(app.clone(), request).await;
        assert_eq!(response.status(), StatusCode::OK, "Host {host}");
    }
    for host in ["other.example.com", "box.lan:7080", "box.lan"] {
        let request = builder(Method::GET, "/healthz", host)
            .body(Body::empty())
            .unwrap();
        let response = send(app.clone(), request).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "Host {host}");
    }
}

#[tokio::test]
async fn cross_site_form_content_types_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(state(dir.path()));
    let valid = token(dir.path());

    for content_type in [
        "application/x-www-form-urlencoded",
        "multipart/form-data; boundary=x",
        "text/plain",
    ] {
        let request = builder(Method::POST, "/api/v1/session", HOST)
            .header(header::CONTENT_TYPE, content_type)
            .body(Body::from(format!("token={valid}")))
            .unwrap();
        let response = send(app.clone(), request).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{content_type}");
        assert_eq!(body(response).await["code"], "invalid-request");
    }

    // No Content-Type at all on a mutating request: rejected too.
    let request = builder(Method::POST, "/api/v1/session", HOST)
        .body(Body::from(format!("token={valid}")))
        .unwrap();
    let response = send(app.clone(), request).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    // The guard also applies once a session exists: a form POST to a
    // mutating endpoint never reaches the handler.
    let response = login(app.clone(), &valid).await;
    assert_eq!(response.status(), StatusCode::OK);
    let cookie = cookie_pair(&response);
    let request = builder(Method::POST, "/api/v1/clusters", HOST)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .header(header::COOKIE, &cookie)
        .body(Body::from("name=x"))
        .unwrap();
    let response = send(app, request).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn login_flow_exchanges_token_for_a_strict_cookie() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(state(dir.path()));

    // A wrong token never logs in.
    let response = login(app.clone(), "not-the-token").await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(body(response).await["code"], "unauthorized");

    // Malformed JSON is a 400, not a 401.
    let request = builder(Method::POST, "/api/v1/session", HOST)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("{not json"))
        .unwrap();
    let response = send(app.clone(), request).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    // The real token does.
    let response = login(app.clone(), &token(dir.path())).await;
    assert_eq!(response.status(), StatusCode::OK);
    let cookie = cookie_pair(&response);
    let cookie_value = response
        .headers()
        .get(header::SET_COOKIE)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let info = body(response).await;
    assert_eq!(info["version"], env!("CARGO_PKG_VERSION"));
    assert!(info["expiresAt"].as_str().is_some());
    for flag in ["Path=/", "HttpOnly", "SameSite=Strict"] {
        assert!(cookie_value.contains(flag), "missing {flag}");
    }

    // The cookie authenticates API calls, including session introspection.
    let request = with_cookie(Method::GET, "/api/v1/meta", &cookie);
    let response = send(app.clone(), request).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body(response).await["version"], env!("CARGO_PKG_VERSION"));

    let request = with_cookie(Method::GET, "/api/v1/session", &cookie);
    let response = send(app.clone(), request).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(body(response).await["expiresAt"].as_str().is_some());
}

#[tokio::test]
async fn logout_invalidates_the_session() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(state(dir.path()));
    let response = login(app.clone(), &token(dir.path())).await;
    let cookie = cookie_pair(&response);

    let request = with_cookie(Method::DELETE, "/api/v1/session", &cookie);
    let response = send(app.clone(), request).await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let clearing = response
        .headers()
        .get(header::SET_COOKIE)
        .expect("logout clears the cookie")
        .to_str()
        .unwrap();
    assert!(clearing.contains("Max-Age=0"));

    // The old cookie is worthless now.
    let request = with_cookie(Method::GET, "/api/v1/meta", &cookie);
    let response = send(app.clone(), request).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn token_rotation_invalidates_old_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let state = state(dir.path());
    let app = router(state.clone());
    let old_token = token(dir.path());

    let response = login(app.clone(), &old_token).await;
    assert_eq!(response.status(), StatusCode::OK);
    let cookie = cookie_pair(&response);
    let request = with_cookie(Method::GET, "/api/v1/meta", &cookie);
    let response = send(app.clone(), request).await;
    assert_eq!(response.status(), StatusCode::OK);

    state.rotate_token().unwrap();

    // The old session died with the old token…
    let request = with_cookie(Method::GET, "/api/v1/meta", &cookie);
    let response = send(app.clone(), request).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    // …and the old token no longer logs in, but the fresh one does.
    let response = login(app.clone(), &old_token).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let response = login(app.clone(), &token(dir.path())).await;
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn rotation_via_startup_flag_replaces_the_token_file() {
    let dir = tempfile::tempdir().unwrap();
    state(dir.path()); // creates <data>/token
    let first = token(dir.path());

    let options = ServeOptions {
        rotate_token: true,
        ..opts(dir.path())
    };
    let startup = kurin_server::startup(&options).unwrap();
    assert!(startup.fresh_token);
    assert_ne!(token(dir.path()), first);
}

#[tokio::test]
async fn draining_answers_503_but_healthz_stays_alive() {
    let dir = tempfile::tempdir().unwrap();
    let state = state(dir.path());
    let app = router(state.clone());
    let response = login(app.clone(), &token(dir.path())).await;
    let cookie = cookie_pair(&response);

    state.begin_shutdown();

    let request = with_cookie(Method::GET, "/api/v1/meta", &cookie);
    let response = send(app.clone(), request).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body(response).await["code"], "unavailable");

    let request = builder(Method::GET, "/healthz", HOST)
        .body(Body::empty())
        .unwrap();
    let response = send(app, request).await;
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn unknown_paths_are_json_404_after_auth() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(state(dir.path()));
    let response = login(app.clone(), &token(dir.path())).await;
    let cookie = cookie_pair(&response);

    // Authenticated: a real 404 with the unified envelope.
    let request = with_cookie(Method::GET, "/api/v1/definitely-not-yet", &cookie);
    let response = send(app.clone(), request).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(body(response).await["code"], "not-found");

    // Non-API paths are 404 without a session requirement (beyond the
    // host rule).
    let request = builder(Method::GET, "/nope", HOST)
        .body(Body::empty())
        .unwrap();
    let response = send(app, request).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn http2_style_authority_is_checked_like_the_host_header() {
    // HTTP/2 has no Host header; the :authority lands in the URI. The
    // allowlist must apply to it just the same.
    let dir = tempfile::tempdir().unwrap();
    let app = router(state(dir.path()));

    let request = Request::builder()
        .method(Method::GET)
        .uri("http://127.0.0.1:7080/healthz")
        .body(Body::empty())
        .unwrap();
    let response = send(app.clone(), request).await;
    assert_eq!(response.status(), StatusCode::OK);

    let request = Request::builder()
        .method(Method::GET)
        .uri("http://evil.example/healthz")
        .body(Body::empty())
        .unwrap();
    let response = send(app, request).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn first_start_generates_the_token_once() {
    let dir = tempfile::tempdir().unwrap();
    let first = kurin_server::startup(&opts(dir.path())).unwrap();
    assert!(first.fresh_token);
    // Second start: same token, nothing new to print.
    let second = kurin_server::startup(&opts(dir.path())).unwrap();
    assert!(!second.fresh_token);
    assert_eq!(token(dir.path()).len(), 64);
}
