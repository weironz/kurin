//! WP-55 acceptance: embedded assets, the SPA fallback and the separate
//! `/api` 404 (UI §4.2/§9).

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use axum::response::Response;
use axum::Router;
use kurin_server::{
    router, AppState, OpContext, OpExecution, OperationBackend, ServeOptions, StaticAssets,
};
use serde_json::{json, Value};
use tower::ServiceExt;

const HOST: &str = "127.0.0.1:7080";

struct NeverBackend;

#[async_trait]
impl OperationBackend for NeverBackend {
    async fn execute(&self, _ctx: OpContext) -> OpExecution {
        std::future::pending().await
    }
}

fn app(dir: &Path) -> (Router, AppState) {
    let options = ServeOptions {
        data_dir: dir.to_path_buf(),
        ..Default::default()
    };
    let state = kurin_server::startup_with_backend(&options, Arc::new(NeverBackend))
        .expect("startup")
        .state;
    state.set_assets(StaticAssets::from_pairs([
        ("index.html.gz", b"<html>spa</html>".to_vec()),
        ("assets/app-abc.js.gz", b"console.log(1)".to_vec()),
    ]));
    (router(state.clone()), state)
}

fn request(method: Method, uri: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(header::HOST, HOST)
        .body(Body::empty())
        .unwrap()
}

async fn send(app: Router, request: Request<Body>) -> Response {
    app.oneshot(request).await.expect("infallible")
}

async fn body(response: Response) -> Vec<u8> {
    axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("in-memory body")
        .to_vec()
}

async fn login(app: Router, dir: &Path) -> String {
    let token = std::fs::read_to_string(dir.join("token"))
        .expect("token")
        .trim()
        .to_string();
    let request = Request::builder()
        .method(Method::POST)
        .uri("/api/v1/session")
        .header(header::HOST, HOST)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(json!({ "token": token }).to_string()))
        .unwrap();
    let response = send(app, request).await;
    assert_eq!(response.status(), StatusCode::OK);
    response
        .headers()
        .get(header::SET_COOKIE)
        .unwrap()
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn index_and_client_routes_serve_the_embedded_spa() {
    let dir = tempfile::tempdir().unwrap();
    let (app, _) = app(dir.path());
    for path in ["/", "/clusters/demo"] {
        let response = send(app.clone(), request(Method::GET, path)).await;
        assert_eq!(response.status(), StatusCode::OK, "{path}");
        assert_eq!(response.headers()[header::CONTENT_ENCODING], "gzip");
        assert_eq!(response.headers()[header::CONTENT_TYPE], "text/html");
        assert_eq!(body(response).await, b"<html>spa</html>", "{path}");
    }
}

#[tokio::test]
async fn hashed_assets_are_served_from_the_embed() {
    let dir = tempfile::tempdir().unwrap();
    let (app, _) = app(dir.path());
    let response = send(app, request(Method::GET, "/assets/app-abc.js")).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CONTENT_TYPE], "text/javascript");
    assert_eq!(body(response).await, b"console.log(1)");
}

#[tokio::test]
async fn unknown_api_paths_stay_json_404() {
    let dir = tempfile::tempdir().unwrap();
    let (app, _) = app(dir.path());
    let cookie = login(app.clone(), dir.path()).await;

    for method in [Method::GET, Method::POST] {
        let mut builder = Request::builder()
            .method(method.clone())
            .uri("/api/v1/nope")
            .header(header::HOST, HOST)
            .header(header::COOKIE, &cookie);
        if method == Method::POST {
            builder = builder.header(header::CONTENT_TYPE, "application/json");
        }
        let response = send(app.clone(), builder.body(Body::empty()).unwrap()).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{method}");
        assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
        let value: Value = serde_json::from_slice(&body(response).await).unwrap();
        assert_eq!(value["code"], "not-found");
    }
}

#[tokio::test]
async fn a_missing_asset_is_a_404_not_the_spa() {
    let dir = tempfile::tempdir().unwrap();
    let (app, _) = app(dir.path());
    let response = send(app, request(Method::GET, "/assets/missing.js")).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}
