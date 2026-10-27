//! WP-54 acceptance tests: every new endpoint with at least one success
//! and one failure case (UI §7).
//!
//! Handlers that need SSH (`/nodes/probe`, the mutating operations)
//! run against the unreachable `192.0.2.0/24` TEST-NET — the failure
//! paths are the contract under test (never echoing credentials, a
//! clean 4xx instead of a hang).

use std::path::Path;
use std::sync::Arc;

use axum::body::Body;
use axum::http::request::Builder;
use axum::http::{header, Method, Request, StatusCode};
use axum::response::Response;
use axum::Router;
use kurin_server::{AppState, OperationBackend, ServeOptions};
use serde_json::{json, Value};
use tower::ServiceExt;

const HOST: &str = "127.0.0.1:7080";
const CONFIG: &str = include_str!("../../../kurin/tests/fixtures/configs/single.toml");

fn options(dir: &Path) -> ServeOptions {
    ServeOptions {
        data_dir: dir.to_path_buf(),
        ..Default::default()
    }
}

fn state_with(dir: &Path, backend: Arc<dyn OperationBackend>) -> AppState {
    kurin_server::startup_with_backend(&options(dir), backend)
        .expect("startup")
        .state
}

fn import(state: &AppState, name: &str) {
    let toml = CONFIG.replace("name = \"single\"", &format!("name = \"{name}\""));
    let store = state.store();
    if store.get_cluster(name).unwrap().is_none() {
        store
            .register_cluster(name, kurin_server::Lifecycle::Draft)
            .unwrap();
    }
    store
        .import_cluster(name, &toml, Some("test".into()))
        .unwrap();
}

fn token(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("token"))
        .expect("token")
        .trim()
        .to_string()
}

async fn send(app: Router, request: Request<Body>) -> Response {
    app.oneshot(request).await.expect("infallible")
}

fn builder(method: Method, uri: &str) -> Builder {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(header::HOST, HOST)
}

async fn login(app: Router, dir: &Path) -> String {
    let request = builder(Method::POST, "/api/v1/session")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(json!({ "token": token(dir) }).to_string()))
        .unwrap();
    let response = send(app.clone(), request).await;
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

async fn json_body(response: Response) -> Value {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

/// POST/PUT/DELETE with a JSON body and the session cookie.
fn authed(method: Method, uri: &str, cookie: &str, body: Value) -> Request<Body> {
    builder(method, uri)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::COOKIE, cookie)
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// GET with the session cookie.
fn authed_get(uri: &str, cookie: &str) -> Request<Body> {
    builder(Method::GET, uri)
        .header(header::COOKIE, cookie)
        .body(Body::empty())
        .unwrap()
}

/// A minimal valid v1alpha2 config for the config endpoints.
fn minimal_toml() -> String {
    let kubernetes = kurin_ops::component_versions()
        .iter()
        .find(|(name, _)| *name == "kubernetes")
        .map(|(_, v)| *v)
        .unwrap_or("1.33.1");
    format!(
        "version = \"v1alpha2\"\nname = \"wizard\"\n\n[kubernetes]\nversion = \"{kubernetes}\"\n\n\
         [network.cni]\nkind = \"none\"\n\n[ssh]\nuser = \"root\"\n\n[ssh.auth]\nmethod = \"private_key\"\n\
         key-path = \"/root/.ssh/id_ed25519\"\n\
         [nodes.cp-1]\naddress = \"192.168.10.11\"\nrole = \"control-plane\"\n"
    )
}

// ── backend fakes ───────────────────────────────────────────────────────

/// Succeeds immediately; the plan/package endpoints only need the
/// runner's persistence to work.
struct NoopBackend;

#[async_trait::async_trait]
impl OperationBackend for NoopBackend {
    async fn execute(&self, _ctx: kurin_server::OpContext) -> kurin_server::OpExecution {
        kurin_server::OpExecution {
            state: kurin_server::OperationState::Succeeded,
            error: None,
            result: None,
        }
    }
}

/// A backend that records the submitted kind and answers with a canned
/// `result`, so the operation-driven endpoints' 202 path is observable.
struct ResultBackend;

#[async_trait::async_trait]
impl OperationBackend for ResultBackend {
    async fn execute(&self, ctx: kurin_server::OpContext) -> kurin_server::OpExecution {
        kurin_server::OpExecution {
            state: kurin_server::OperationState::Succeeded,
            error: None,
            result: Some(json!({
                "op": ctx.kind,
                "phases": [{ "id": "prepare", "label": "prepare", "tasks": [
                    { "key": "prepare/probe@cp-1", "node": "cp-1", "describe": "probe" }
                ]}],
            })),
        }
    }
}

// ── GET /meta ───────────────────────────────────────────────────────────

#[tokio::test]
async fn meta_reports_versions_and_capabilities() {
    let dir = tempfile::tempdir().unwrap();
    let state = state_with(dir.path(), Arc::new(NoopBackend));
    let app = kurin_server::router(state.clone());
    let cookie = login(app.clone(), dir.path()).await;

    let response = send(app, authed_get("/api/v1/meta", &cookie)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    assert_eq!(body["version"], state.version());
    let components = body["components"].as_object().expect("components");
    assert!(components.contains_key("kubernetes"));
    assert!(components.contains_key("containerd"));
    let capabilities = body["capabilities"].as_array().expect("capabilities");
    assert!(capabilities.iter().any(|row| row["id"] == "cni/calico"));
}

#[tokio::test]
async fn meta_requires_a_session() {
    let dir = tempfile::tempdir().unwrap();
    let state = state_with(dir.path(), Arc::new(NoopBackend));
    let app = kurin_server::router(state);

    let response = send(
        app,
        builder(Method::GET, "/api/v1/meta")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

// ── GET /schema/cluster ────────────────────────────────────────────────

#[tokio::test]
async fn schema_cluster_returns_the_v1alpha2_document() {
    let dir = tempfile::tempdir().unwrap();
    let state = state_with(dir.path(), Arc::new(NoopBackend));
    let app = kurin_server::router(state);
    let cookie = login(app.clone(), dir.path()).await;

    let response = send(app, authed_get("/api/v1/schema/cluster", &cookie)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("application/schema+json")
    );
    let body = json_body(response).await;
    assert!(
        body["$schema"].as_str().is_some(),
        "the schema names its dialect"
    );
    // The properties the wizard's steps bind to are all described.
    for key in ["name", "kubernetes", "network", "ssh", "nodes", "addons"] {
        assert!(
            body["properties"][key].is_object(),
            "schema is missing properties.{key}"
        );
    }
}

#[tokio::test]
async fn schema_cluster_requires_a_session() {
    let dir = tempfile::tempdir().unwrap();
    let state = state_with(dir.path(), Arc::new(NoopBackend));
    let app = kurin_server::router(state);
    let response = send(
        app,
        builder(Method::GET, "/api/v1/schema/cluster")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

// ── POST /configs/render ───────────────────────────────────────────────

#[tokio::test]
async fn render_config_produces_valid_toml() {
    let dir = tempfile::tempdir().unwrap();
    let state = state_with(dir.path(), Arc::new(NoopBackend));
    let app = kurin_server::router(state);
    let cookie = login(app.clone(), dir.path()).await;

    let request = authed(
        Method::POST,
        "/api/v1/configs/render",
        &cookie,
        json!({
            "name": "wizard",
            "nodes": [
                { "address": "192.168.10.11", "role": "control-plane", "name": "cp-1" },
                { "address": "192.168.10.21", "role": "worker", "name": "worker-1" },
            ],
            "ssh": { "user": "root", "port": 22, "key": "/root/.ssh/id_ed25519" },
            "addons": ["metrics-server"],
        }),
    );
    let response = send(app.clone(), request).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    assert_eq!(body["valid"], true);
    let toml = body["toml"].as_str().expect("toml");
    assert!(toml.contains("name = \"wizard\""), "{toml}");
    assert!(toml.contains("[nodes.cp-1]"), "{toml}");
    assert!(toml.contains("192.168.10.11"), "{toml}");
}

#[tokio::test]
async fn render_config_rejects_a_malformed_dto() {
    let dir = tempfile::tempdir().unwrap();
    let state = state_with(dir.path(), Arc::new(NoopBackend));
    let app = kurin_server::router(state);
    let cookie = login(app.clone(), dir.path()).await;

    // Unknown distribution: the DTO is rejected with a 400 before any
    // TOML is produced.
    let request = authed(
        Method::POST,
        "/api/v1/configs/render",
        &cookie,
        json!({ "name": "wizard", "distribution": "nomad" }),
    );
    let response = send(app.clone(), request).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = json_body(response).await;
    assert_eq!(body["code"], "invalid-request");

    // A valid DTO that produces an invalid config (no nodes at all)
    // returns the document plus diagnostics instead of failing.
    let request = authed(Method::POST, "/api/v1/configs/render", &cookie, json!({}));
    let response = send(app.clone(), request).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    assert_eq!(body["valid"], false);
    assert!(
        !body["diagnostics"].as_array().unwrap().is_empty(),
        "expected diagnostics"
    );
}

// ── POST /configs/validate ─────────────────────────────────────────────

#[tokio::test]
async fn validate_config_accepts_a_valid_document() {
    let dir = tempfile::tempdir().unwrap();
    let state = state_with(dir.path(), Arc::new(NoopBackend));
    let app = kurin_server::router(state);
    let cookie = login(app.clone(), dir.path()).await;

    let request = authed(
        Method::POST,
        "/api/v1/configs/validate",
        &cookie,
        json!({ "toml": minimal_toml() }),
    );
    let response = send(app.clone(), request).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    assert_eq!(body["valid"], true);
}

#[tokio::test]
async fn validate_config_returns_spanned_diagnostics() {
    let dir = tempfile::tempdir().unwrap();
    let state = state_with(dir.path(), Arc::new(NoopBackend));
    let app = kurin_server::router(state);
    let cookie = login(app.clone(), dir.path()).await;

    // A document whose pod CIDR collides with the service CIDR: the
    // business validation rejects it and the diagnostic carries a label
    // pointing into the submitted text.
    let mut bad = minimal_toml();
    bad.push_str("\n[network]\npod-cidr = \"10.233.0.0/18\"\nservice-cidr = \"10.233.0.0/18\"\n");
    let request = authed(
        Method::POST,
        "/api/v1/configs/validate",
        &cookie,
        json!({ "toml": bad }),
    );
    let response = send(app.clone(), request).await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body = json_body(response).await;
    assert_eq!(body["code"], "validation-failed");
    let diagnostics = body["diagnostics"].as_array().expect("diagnostics");
    assert!(!diagnostics.is_empty());
    assert!(diagnostics[0]["message"].as_str().is_some());

    // A syntax error produces a label with the offset of the offending
    // byte range inside the submitted document.
    let broken = "version = \"v1alpha2\"\nname = \n";
    let request = authed(
        Method::POST,
        "/api/v1/configs/validate",
        &cookie,
        json!({ "toml": broken }),
    );
    let response = send(app.clone(), request).await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body = json_body(response).await;
    let diagnostics = body["diagnostics"].as_array().expect("diagnostics");
    assert!(!diagnostics.is_empty());
    let labels = diagnostics[0]["labels"].as_array().expect("labels");
    assert!(!labels.is_empty());
    let offset = labels[0]["offset"].as_u64().expect("offset") as usize;
    let len = labels[0]["len"].as_u64().expect("len") as usize;
    let text = labels[0]["text"].as_str().expect("text");
    assert_eq!(&broken[offset..offset + len], text);
}

// ── POST /nodes/parse ──────────────────────────────────────────────────

#[tokio::test]
async fn parse_nodes_expands_ranges_and_roles() {
    let dir = tempfile::tempdir().unwrap();
    let state = state_with(dir.path(), Arc::new(NoopBackend));
    let app = kurin_server::router(state);
    let cookie = login(app.clone(), dir.path()).await;

    let request = authed(
        Method::POST,
        "/api/v1/nodes/parse",
        &cookie,
        json!({
            "input": "10.0.0.1-2 cp\n10.0.1.5 worker\n",
            "existing": ["worker-1"],
        }),
    );
    let response = send(app.clone(), request).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    let nodes = body["nodes"].as_array().expect("nodes");
    assert_eq!(nodes.len(), 3);
    assert_eq!(nodes[0]["address"], "10.0.0.1");
    assert_eq!(nodes[0]["role"], "controlplane");
    // Existing names advance the worker sequence.
    assert_eq!(nodes[2]["name"], "worker-2");
}

#[tokio::test]
async fn parse_nodes_rejects_garbage() {
    let dir = tempfile::tempdir().unwrap();
    let state = state_with(dir.path(), Arc::new(NoopBackend));
    let app = kurin_server::router(state);
    let cookie = login(app.clone(), dir.path()).await;

    let request = authed(
        Method::POST,
        "/api/v1/nodes/parse",
        &cookie,
        json!({ "input": "not-an-ip worker\n" }),
    );
    let response = send(app.clone(), request).await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body = json_body(response).await;
    assert_eq!(body["code"], "validation-failed");
}

// ── POST /nodes/probe ──────────────────────────────────────────────────

#[tokio::test]
async fn probe_nodes_reports_an_unreachable_node_without_credentials() {
    let dir = tempfile::tempdir().unwrap();
    let state = state_with(dir.path(), Arc::new(NoopBackend));
    let app = kurin_server::router(state);
    let cookie = login(app.clone(), dir.path()).await;

    // A key that does not exist: resolution fails before any SSH
    // traffic, and the message names no credential material.
    let request = authed(
        Method::POST,
        "/api/v1/nodes/probe",
        &cookie,
        json!({
            "address": "192.0.2.1",
            "user": "root",
            "key": "/nonexistent/.ssh/id_ed25519",
        }),
    );
    let response = send(app, request).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = json_body(response).await;
    let text = body["message"].as_str().expect("message");
    assert!(!text.to_lowercase().contains("password"), "{text}");
    assert!(!text.contains("passphrase"), "{text}");
}

#[tokio::test]
async fn probe_nodes_rejects_a_malformed_address() {
    let dir = tempfile::tempdir().unwrap();
    let state = state_with(dir.path(), Arc::new(NoopBackend));
    let app = kurin_server::router(state);
    let cookie = login(app.clone(), dir.path()).await;

    let request = authed(
        Method::POST,
        "/api/v1/nodes/probe",
        &cookie,
        json!({ "address": "not-an-ip" }),
    );
    let response = send(app.clone(), request).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

// ── GET /addons/catalog ────────────────────────────────────────────────

#[tokio::test]
async fn addon_catalog_lists_configured_addons_with_schemas() {
    let dir = tempfile::tempdir().unwrap();
    let state = state_with(dir.path(), Arc::new(NoopBackend));
    // A cluster whose addons come from local directories: the catalog
    // endpoint resolves its `source` entries (the built-in catalog is
    // empty until WP-46).
    let addons_dir =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../kurin/tests/fixtures/addons");
    let toml = include_str!("../../../kurin/tests/fixtures/configs/addons.toml")
        .replace("@ADDONS_DIR@", &addons_dir.display().to_string());
    let store = state.store();
    store
        .register_cluster("addons", kurin_server::Lifecycle::Draft)
        .unwrap();
    store.import_cluster("addons", &toml, None).unwrap();
    let app = kurin_server::router(state);
    let cookie = login(app.clone(), dir.path()).await;

    let response = send(app, authed_get("/api/v1/addons/catalog", &cookie)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    let addons = body["addons"].as_array().expect("addons");
    assert!(!addons.is_empty(), "the configured sources must appear");
    let metrics = addons
        .iter()
        .find(|row| row["name"] == "metrics-server")
        .expect("metrics-server in the catalog");
    assert_eq!(metrics["version"], "0.7.2");
    assert_eq!(metrics["namespace"], "kube-system");
    assert!(
        metrics["values_schema"]
            .as_object()
            .is_some_and(|s| !s.is_empty()),
        "the values schema is embedded"
    );
}

#[tokio::test]
async fn addon_catalog_requires_a_session() {
    let dir = tempfile::tempdir().unwrap();
    let state = state_with(dir.path(), Arc::new(NoopBackend));
    let app = kurin_server::router(state);
    let response = send(
        app,
        builder(Method::GET, "/api/v1/addons/catalog")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

// ── GET/POST/PUT/DELETE /clusters/{name}/addons ─────────────────────────

#[tokio::test]
async fn cluster_addons_lists_the_inventory() {
    let dir = tempfile::tempdir().unwrap();
    let state = state_with(dir.path(), Arc::new(NoopBackend));
    import(&state, "single");
    // A snapshot row carrying an inventory (what the collector writes).
    state
        .store()
        .save_snapshot_success(
            "single",
            r#"{"healthy":true,"nodes":[]}"#,
            false,
            None,
            Some(r#"{"metrics-server": {"version": "0.8.0"}}"#),
        )
        .unwrap();
    let app = kurin_server::router(state);
    let cookie = login(app.clone(), dir.path()).await;

    let response = send(app, authed_get("/api/v1/clusters/single/addons", &cookie)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    assert_eq!(body["cluster"], "single");
    assert_eq!(body["addons"]["metrics-server"]["version"], "0.8.0");
}

#[tokio::test]
async fn cluster_addons_unknown_cluster_is_404() {
    let dir = tempfile::tempdir().unwrap();
    let state = state_with(dir.path(), Arc::new(NoopBackend));
    let app = kurin_server::router(state);
    let cookie = login(app.clone(), dir.path()).await;

    let response = send(app, authed_get("/api/v1/clusters/ghost/addons", &cookie)).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

/// Import the fixture cluster whose addons come from local directories.
fn import_addons_cluster(state: &AppState) {
    let addons_dir =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../kurin/tests/fixtures/addons");
    let toml = include_str!("../../../kurin/tests/fixtures/configs/addons.toml")
        .replace("@ADDONS_DIR@", &addons_dir.display().to_string());
    let store = state.store();
    if store.get_cluster("addons").unwrap().is_none() {
        store
            .register_cluster("addons", kurin_server::Lifecycle::Draft)
            .unwrap();
    }
    store.import_cluster("addons", &toml, None).unwrap();
}

#[tokio::test]
async fn addon_install_submits_an_operation() {
    let dir = tempfile::tempdir().unwrap();
    let state = state_with(dir.path(), Arc::new(NoopBackend));
    import_addons_cluster(&state);
    let app = kurin_server::router(state.clone());
    let cookie = login(app.clone(), dir.path()).await;

    let request = authed(
        Method::POST,
        "/api/v1/clusters/addons/addons",
        &cookie,
        json!({ "addon": "metrics-server" }),
    );
    let response = send(app.clone(), request).await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let body = json_body(response).await;
    let operation = body["operation"].as_str().expect("operation id");
    let record = state.store().get_operation(operation).unwrap().unwrap();
    assert_eq!(record.kind, "addon-install");
    // The stored revision now enables the addon.
    let revision = state.store().current_revision("addons").unwrap().unwrap();
    assert!(
        revision.toml.contains("[addons.metrics-server]"),
        "revision missing the addon entry: {}",
        revision.toml
    );
    assert!(revision.toml.contains("enabled = true"));
}

#[tokio::test]
async fn addon_install_unknown_addon_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let state = state_with(dir.path(), Arc::new(NoopBackend));
    import(&state, "single");
    let app = kurin_server::router(state.clone());
    let cookie = login(app.clone(), dir.path()).await;

    let request = authed(
        Method::POST,
        "/api/v1/clusters/single/addons",
        &cookie,
        json!({ "addon": "no-such-addon" }),
    );
    let response = send(app.clone(), request).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn addon_remove_requires_confirmation() {
    let dir = tempfile::tempdir().unwrap();
    let state = state_with(dir.path(), Arc::new(NoopBackend));
    import_addons_cluster(&state);
    let app = kurin_server::router(state.clone());
    let cookie = login(app.clone(), dir.path()).await;

    let request = authed(
        Method::DELETE,
        "/api/v1/clusters/addons/addons",
        &cookie,
        json!({ "addon": "metrics-server" }),
    );
    let response = send(app.clone(), request).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = json_body(response).await;
    assert!(
        body["message"]
            .as_str()
            .is_some_and(|text| text.contains("confirm")),
        "the rejection names the confirm field: {body}"
    );

    let request = authed(
        Method::DELETE,
        "/api/v1/clusters/addons/addons",
        &cookie,
        json!({ "addon": "metrics-server", "confirm": "addons" }),
    );
    let response = send(app.clone(), request).await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let body = json_body(response).await;
    let operation = body["operation"].as_str().expect("operation id");
    let record = state.store().get_operation(operation).unwrap().unwrap();
    assert_eq!(record.kind, "addon-remove");
}

// ── POST /clusters/{name}/plan ─────────────────────────────────────────

#[tokio::test]
async fn cluster_plan_submits_a_read_only_operation() {
    let dir = tempfile::tempdir().unwrap();
    let state = state_with(dir.path(), Arc::new(NoopBackend));
    import(&state, "single");
    let app = kurin_server::router(state.clone());
    let cookie = login(app.clone(), dir.path()).await;

    let request = authed(
        Method::POST,
        "/api/v1/clusters/single/plan",
        &cookie,
        json!({}),
    );
    let response = send(app.clone(), request).await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let body = json_body(response).await;
    let operation = body["operation"].as_str().expect("operation id");
    let record = state.store().get_operation(operation).unwrap().unwrap();
    assert_eq!(record.kind, "plan");
}

#[tokio::test]
async fn cluster_plan_unknown_cluster_is_404() {
    let dir = tempfile::tempdir().unwrap();
    let state = state_with(dir.path(), Arc::new(NoopBackend));
    let app = kurin_server::router(state);
    let cookie = login(app.clone(), dir.path()).await;

    let request = authed(
        Method::POST,
        "/api/v1/clusters/ghost/plan",
        &cookie,
        json!({}),
    );
    let response = send(app.clone(), request).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

// ── /packages ──────────────────────────────────────────────────────────

/// Build a real (tiny) OCI package in `dir` and return its
/// `oci-layout://` source, so import/verify run the genuine code path.
fn build_tiny_package(dir: &Path) -> String {
    // A layout with an empty index: structurally an OCI layout, but not
    // a kurin package (no package index manifest).
    let layout_dir = dir.join("pkg");
    std::fs::create_dir_all(layout_dir.join("blobs/sha256")).unwrap();
    let index = serde_json::json!({
        "schemaVersion": 2,
        "manifests": [],
    });
    std::fs::write(
        layout_dir.join("index.json"),
        serde_json::to_string(&index).unwrap(),
    )
    .unwrap();
    std::fs::write(
        layout_dir.join("oci-layout"),
        r#"{"imageLayoutVersion": "1.0.0"}"#,
    )
    .unwrap();
    format!("oci-layout://{}", layout_dir.display())
}

#[tokio::test]
async fn packages_import_list_get_and_delete() {
    let dir = tempfile::tempdir().unwrap();
    let state = state_with(dir.path(), Arc::new(NoopBackend));
    let app = kurin_server::router(state);
    let cookie = login(app.clone(), dir.path()).await;

    // An empty layout is not a kurin package: import fails with a 400
    // (manifest read fails), which is the failure case under test.
    let empty = build_tiny_package(dir.path());
    let request = authed(
        Method::POST,
        "/api/v1/packages",
        &cookie,
        json!({ "source": empty }),
    );
    let response = send(app.clone(), request).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    // A missing source is rejected the same way.
    let request = authed(
        Method::POST,
        "/api/v1/packages",
        &cookie,
        json!({ "source": "file:///nonexistent/pkg.tar.zst" }),
    );
    let response = send(app.clone(), request).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    // The empty list is a success.
    let response = send(app.clone(), authed_get("/api/v1/packages", &cookie)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    assert_eq!(body["packages"].as_array().unwrap().len(), 0);

    // An unknown package id is 404 on get and delete.
    let response = send(
        app.clone(),
        authed_get("/api/v1/packages/deadbeef", &cookie),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let request = builder(Method::DELETE, "/api/v1/packages/deadbeef")
        .header(header::COOKIE, &cookie)
        .body(Body::empty())
        .unwrap();
    let response = send(app.clone(), request).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn package_verify_and_push_submit_operations() {
    let dir = tempfile::tempdir().unwrap();
    let state = state_with(dir.path(), Arc::new(ResultBackend));
    let app = kurin_server::router(state.clone());
    let cookie = login(app.clone(), dir.path()).await;

    // Register a package by writing the row directly (the import path
    // needs a genuine package; here only the operation submission is
    // under test).
    let record = state
        .store()
        .upsert_package(kurin_server::PackageRegistration {
            source: "file:///tmp/base.tar.zst".into(),
            origin: "imported".into(),
            kind: "base".into(),
            name: "base".into(),
            version: "1.33.1".into(),
            distribution: "kubeadm".into(),
            arch: "amd64".into(),
            size: 1024,
            index: "sha256:abc".into(),
            signatures: 0,
            sbom: None,
        })
        .unwrap();

    let request = builder(
        Method::POST,
        &format!("/api/v1/packages/{}/verify", record.id),
    )
    .header(header::CONTENT_TYPE, "application/json")
    .header(header::COOKIE, &cookie)
    .body(Body::empty())
    .unwrap();
    let response = send(app.clone(), request).await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let body = json_body(response).await;
    let operation = body["operation"].as_str().expect("operation id");
    let row = state.store().get_operation(operation).unwrap().unwrap();
    assert_eq!(row.kind, "package-verify");
    // The clusterless operations run against the synthetic cluster.
    assert_eq!(row.cluster.as_deref(), Some("packages"));

    let request = authed(
        Method::POST,
        &format!("/api/v1/packages/{}/push", record.id),
        &cookie,
        json!({ "destination": "oci://registry.local:5000/kurin/base:v1" }),
    );
    let response = send(app.clone(), request).await;
    let status = response.status();
    let body = json_body(response).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let operation = body["operation"].as_str().expect("operation id");
    let row = state.store().get_operation(operation).unwrap().unwrap();
    assert_eq!(row.kind, "package-push");

    // push to a non-registry destination is a 400.
    let request = authed(
        Method::POST,
        &format!("/api/v1/packages/{}/push", record.id),
        &cookie,
        json!({ "destination": "file:///tmp/out.tar" }),
    );
    let response = send(app.clone(), request).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    // Unknown package: 404.
    let request = builder(Method::POST, "/api/v1/packages/deadbeef/verify")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::COOKIE, &cookie)
        .body(Body::empty())
        .unwrap();
    let response = send(app.clone(), request).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn create_base_package_validates_and_submits() {
    let dir = tempfile::tempdir().unwrap();
    let state = state_with(dir.path(), Arc::new(ResultBackend));
    let app = kurin_server::router(state.clone());
    let cookie = login(app.clone(), dir.path()).await;

    // Missing output is a 400.
    let request = authed(
        Method::POST,
        "/api/v1/packages/create-base",
        &cookie,
        json!({}),
    );
    let response = send(app.clone(), request).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    // A bad output target is a 400.
    let request = authed(
        Method::POST,
        "/api/v1/packages/create-base",
        &cookie,
        json!({ "output": "oci://registry/base" }),
    );
    let response = send(app.clone(), request).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    // A bad distribution is a 400.
    let request = authed(
        Method::POST,
        "/api/v1/packages/create-base",
        &cookie,
        json!({ "output": format!("oci-layout://{}", dir.path().join("out").display()), "distribution": "nomad" }),
    );
    let response = send(app.clone(), request).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    // A valid request submits the operation.
    let request = authed(
        Method::POST,
        "/api/v1/packages/create-base",
        &cookie,
        json!({ "output": format!("oci-layout://{}", dir.path().join("out").display()) }),
    );
    let response = send(app.clone(), request).await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let body = json_body(response).await;
    let operation = body["operation"].as_str().expect("operation id");
    let row = state.store().get_operation(operation).unwrap().unwrap();
    assert_eq!(row.kind, "package-create-base");
}

// ── the backend's own plan and precheck kinds ───────────────────────────

/// The real `KurinBackend` (not a fake): the `plan` kind runs offline
/// against the stored revision and produces the PlanView.
#[tokio::test]
async fn the_real_backend_answers_plan_operations_offline() {
    let dir = tempfile::tempdir().unwrap();
    // The genuine backend against its own store: the plan kind runs the
    // offline path (no SSH) against the imported revision.
    let store = state_with(dir.path(), Arc::new(NoopBackend))
        .store()
        .clone();
    let state = kurin_server::startup_with_backend(
        &options(dir.path()),
        Arc::new(kurin_server::KurinBackend::new(store)),
    )
    .expect("startup")
    .state;
    import(&state, "single");
    let app = kurin_server::router(state.clone());
    let cookie = login(app.clone(), dir.path()).await;

    let request = authed(
        Method::POST,
        "/api/v1/clusters/single/plan",
        &cookie,
        json!({}),
    );
    let response = send(app.clone(), request).await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let body = json_body(response).await;
    let operation = body["operation"]
        .as_str()
        .expect("operation id")
        .to_string();
    for _ in 0..200 {
        let record = state.store().get_operation(&operation).unwrap().unwrap();
        if record.state.is_terminal() {
            assert_eq!(record.state, kurin_server::OperationState::Succeeded);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let record = state.store().get_operation(&operation).unwrap().unwrap();
    assert_eq!(record.state, kurin_server::OperationState::Succeeded);
    let result: Value = serde_json::from_str(record.result.as_deref().expect("result")).unwrap();
    assert_eq!(result["op"], "create");
    let phases = result["phases"].as_array().expect("phases");
    assert!(!phases.is_empty());
    // Every task carries the stable key/node/describe triple.
    for phase in phases {
        for task in phase["tasks"].as_array().expect("tasks") {
            assert!(task["key"].as_str().is_some());
            assert!(task["node"].as_str().is_some());
            assert!(task["describe"].as_str().is_some());
        }
    }
}
