//! WP-52 acceptance tests: SSE reconnect without loss/duplication, restart
//! recovery, cancellation at a phase boundary, the global concurrency cap,
//! log pagination/filtering and retry semantics.
//!
//! The backend seam lets every test run a deterministic operation without
//! SSH; the runner, persistence, SSE and HTTP layers are the production ones.

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::request::Builder;
use axum::http::{header, Method, Request, StatusCode};
use axum::response::Response;
use axum::Router;
use kurin_core::events::{Body as EventBody, Envelope, Level, Stream};
use kurin_server::{
    router, AppState, Lifecycle, OpContext, OpExecution, OperationBackend, OperationState,
    ServeOptions,
};
use serde_json::{json, Value};
use tokio::sync::Semaphore;
use tokio_stream::StreamExt;
use tower::ServiceExt;

const HOST: &str = "127.0.0.1:7080";
const CONFIG: &str = include_str!("../../../kurin/tests/fixtures/configs/single.toml");

fn options(dir: &Path, max_concurrent: usize) -> ServeOptions {
    ServeOptions {
        data_dir: dir.to_path_buf(),
        max_concurrent_ops: max_concurrent,
        ..Default::default()
    }
}

fn state_with(dir: &Path, backend: Arc<dyn OperationBackend>, max_concurrent: usize) -> AppState {
    kurin_server::startup_with_backend(&options(dir, max_concurrent), backend)
        .expect("startup")
        .state
}

fn import(state: &AppState, name: &str) {
    let toml = CONFIG.replace("name = \"single\"", &format!("name = \"{name}\""));
    let store = state.store();
    if store.get_cluster(name).unwrap().is_none() {
        store.register_cluster(name, Lifecycle::Draft).unwrap();
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

async fn json_body(response: Response) -> Value {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

async fn submit(app: Router, cookie: &str, cluster: &str, body: Value) -> Value {
    let request = builder(
        Method::POST,
        &format!("/api/v1/clusters/{cluster}/operations"),
    )
    .header(header::CONTENT_TYPE, "application/json")
    .header(header::COOKIE, cookie)
    .body(Body::from(body.to_string()))
    .unwrap();
    let response = send(app, request).await;
    assert_eq!(response.status(), StatusCode::ACCEPTED, "{response:?}");
    json_body(response).await
}

async fn wait_state(state: &AppState, id: &str, target: OperationState) {
    for _ in 0..200 {
        let record = state.store().get_operation(id).unwrap().unwrap();
        if record.state == target {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let record = state.store().get_operation(id).unwrap().unwrap();
    panic!(
        "operation {id} stayed {:?}, wanted {target:?}",
        record.state
    );
}

fn log_body(node: &str, level: Level, line: &str) -> EventBody {
    EventBody::Log {
        level,
        node: Some(node.into()),
        task: None,
        stream: Stream::Kurin,
        line: line.into(),
    }
}

// ── SSE reconnect ──────────────────────────────────────────────────────

struct ScriptedBackend {
    first_batch: Semaphore,
    second_batch: Semaphore,
    published_first: Semaphore,
}

#[async_trait]
impl OperationBackend for ScriptedBackend {
    async fn execute(&self, ctx: OpContext) -> OpExecution {
        for seq in 1..=20u64 {
            ctx.bus.publish(log_body(
                if seq % 2 == 0 { "n2" } else { "n1" },
                Level::Info,
                &format!("first batch line {seq}"),
            ));
        }
        self.published_first.add_permits(1);
        self.first_batch.acquire().await.unwrap().forget();
        for seq in 21..=40u64 {
            ctx.bus.publish(log_body(
                "n1",
                Level::Info,
                &format!("second batch line {seq}"),
            ));
        }
        self.second_batch.acquire().await.unwrap().forget();
        OpExecution {
            state: OperationState::Succeeded,
            error: None,
        }
    }
}

async fn read_sse(
    response: Response,
    stop_after: Option<usize>,
) -> (Vec<Envelope>, Option<String>) {
    let mut stream = response.into_body().into_data_stream();
    let mut buffer = String::new();
    let mut events = Vec::new();
    let mut last_id = None;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.expect("body chunk");
        buffer.push_str(&String::from_utf8_lossy(&chunk));
        while let Some(index) = buffer.find("\n\n") {
            let frame = buffer[..index].to_string();
            buffer.drain(..index + 2);
            if frame.trim().is_empty() || frame.starts_with(':') {
                continue;
            }
            let mut data = None;
            for line in frame.lines() {
                if let Some(value) = line.strip_prefix("id: ") {
                    last_id = Some(value.to_string());
                } else if let Some(value) = line.strip_prefix("data: ") {
                    data = Some(value.to_string());
                }
            }
            if let Some(data) = data {
                let envelope: Envelope = serde_json::from_str(&data).unwrap();
                events.push(envelope);
            }
            if stop_after.is_some_and(|limit| events.len() >= limit) {
                return (events, last_id);
            }
        }
    }
    (events, last_id)
}

#[tokio::test]
async fn sse_reconnect_loses_and_duplicates_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(ScriptedBackend {
        first_batch: Semaphore::new(0),
        second_batch: Semaphore::new(0),
        published_first: Semaphore::new(0),
    });
    let state = state_with(dir.path(), backend.clone(), 2);
    import(&state, "sse");
    let app = router(state.clone());
    let cookie = login(app.clone(), dir.path()).await;

    let operation = submit(
        app.clone(),
        &cookie,
        "sse",
        json!({"kind": "create", "params": {}}),
    )
    .await;
    let id = operation["id"].as_str().unwrap().to_string();

    // First connection: subscribe, read five events, then disconnect.
    let request = builder(Method::GET, &format!("/api/v1/operations/{id}/events"))
        .header(header::COOKIE, &cookie)
        .body(Body::empty())
        .unwrap();
    let response = send(app.clone(), request).await;
    assert_eq!(response.status(), StatusCode::OK);
    backend.published_first.acquire().await.unwrap().forget();
    let (first, last_id) = read_sse(response, Some(5)).await;
    assert_eq!(first.len(), 5);
    let last_id = last_id.expect("id field");

    // The operation keeps producing while the browser is away.
    backend.first_batch.add_permits(1);
    backend.second_batch.add_permits(1);
    wait_state(&state, &id, OperationState::Succeeded).await;

    // Reconnect with Last-Event-ID: everything after it, exactly once.
    let request = builder(Method::GET, &format!("/api/v1/operations/{id}/events"))
        .header(header::COOKIE, &cookie)
        .header("last-event-id", &last_id)
        .body(Body::empty())
        .unwrap();
    let response = send(app, request).await;
    let (second, _) = read_sse(response, None).await;

    let mut seqs: Vec<u64> = first
        .iter()
        .map(|e| e.seq)
        .chain(second.iter().map(|e| e.seq))
        .collect();
    seqs.sort_unstable();
    let expected: Vec<u64> = (1..=41).collect();
    assert_eq!(seqs, expected, "no gap and no duplicate across reconnect");
}

// ── restart recovery ───────────────────────────────────────────────────

#[tokio::test]
async fn startup_marks_active_operations_interrupted() {
    let dir = tempfile::tempdir().unwrap();
    let first = state_with(dir.path(), Arc::new(NeverBackend), 2);
    import(&first, "restart");
    let store = first.store().clone();
    let operation = store
        .insert_operation(kurin_server::NewOperation {
            id: "op-restart".into(),
            cluster: Some("restart".into()),
            kind: "create".into(),
            config_rev: Some(1),
            params: "{}".into(),
            retry_of: None,
            actor: "test".into(),
        })
        .unwrap();
    store.mark_operation_running(&operation.id).unwrap();

    let second = state_with(dir.path(), Arc::new(NeverBackend), 2);
    let record = second.store().get_operation("op-restart").unwrap().unwrap();
    assert_eq!(record.state, OperationState::Interrupted);
    assert!(record.finished_at.is_some());
}

struct NeverBackend;

#[async_trait]
impl OperationBackend for NeverBackend {
    async fn execute(&self, _ctx: OpContext) -> OpExecution {
        std::future::pending().await
    }
}

// ── cancellation at a phase boundary ───────────────────────────────────

struct PhaseBackend {
    phase_one_done: Semaphore,
}

#[async_trait]
impl OperationBackend for PhaseBackend {
    async fn execute(&self, ctx: OpContext) -> OpExecution {
        ctx.bus.publish(EventBody::PhaseStarted {
            phase: "phase-1".into(),
        });
        ctx.bus.publish(EventBody::PhaseDone {
            phase: "phase-1".into(),
            ms: 1,
        });
        self.phase_one_done.add_permits(1);
        // The executor checks the token before each stage; this is the
        // boundary between phase 1 and phase 2.
        ctx.cancel.cancelled().await;
        OpExecution {
            state: OperationState::Cancelled,
            error: None,
        }
    }
}

#[tokio::test]
async fn cancel_takes_effect_at_a_phase_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(PhaseBackend {
        phase_one_done: Semaphore::new(0),
    });
    let state = state_with(dir.path(), backend.clone(), 2);
    import(&state, "phase");
    let app = router(state.clone());
    let cookie = login(app.clone(), dir.path()).await;
    let operation = submit(
        app.clone(),
        &cookie,
        "phase",
        json!({"kind": "create", "params": {}}),
    )
    .await;
    let id = operation["id"].as_str().unwrap().to_string();
    backend.phase_one_done.acquire().await.unwrap().forget();

    let request = builder(Method::POST, &format!("/api/v1/operations/{id}/cancel"))
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::COOKIE, &cookie)
        .body(Body::empty())
        .unwrap();
    let response = send(app.clone(), request).await;
    assert_eq!(response.status(), StatusCode::OK);
    wait_state(&state, &id, OperationState::Cancelled).await;

    let events = state
        .runner()
        .backfill(&state.store().get_operation(&id).unwrap().unwrap(), 0);
    let phases: Vec<String> = events
        .iter()
        .filter_map(|envelope| match &envelope.body {
            EventBody::PhaseStarted { phase } => Some(phase.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(phases, vec!["phase-1"], "phase 2 never started");
}

// ── concurrency cap ────────────────────────────────────────────────────

struct BlockingBackend {
    active: AtomicUsize,
    max_active: AtomicUsize,
    release: Semaphore,
}

#[async_trait]
impl OperationBackend for BlockingBackend {
    async fn execute(&self, _ctx: OpContext) -> OpExecution {
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_active.fetch_max(active, Ordering::SeqCst);
        let _permit = self.release.acquire().await.unwrap();
        self.active.fetch_sub(1, Ordering::SeqCst);
        OpExecution {
            state: OperationState::Succeeded,
            error: None,
        }
    }
}

#[tokio::test]
async fn global_concurrency_cap_holds() {
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(BlockingBackend {
        active: AtomicUsize::new(0),
        max_active: AtomicUsize::new(0),
        release: Semaphore::new(0),
    });
    let state = state_with(dir.path(), backend.clone(), 2);
    for name in ["c1", "c2", "c3"] {
        import(&state, name);
    }
    let app = router(state.clone());
    let cookie = login(app.clone(), dir.path()).await;

    let mut ids = Vec::new();
    for name in ["c1", "c2", "c3"] {
        let operation = submit(
            app.clone(),
            &cookie,
            name,
            json!({"kind": "create", "params": {}}),
        )
        .await;
        ids.push(operation["id"].as_str().unwrap().to_string());
    }

    for _ in 0..200 {
        if backend.active.load(Ordering::SeqCst) == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(backend.active.load(Ordering::SeqCst), 2);
    let queued = ids
        .iter()
        .filter(|id| {
            state.store().get_operation(id).unwrap().unwrap().state == OperationState::Queued
        })
        .count();
    assert_eq!(queued, 1, "the third operation waits for a permit");

    backend.release.add_permits(3);
    for id in &ids {
        wait_state(&state, id, OperationState::Succeeded).await;
    }
    assert_eq!(backend.max_active.load(Ordering::SeqCst), 2);
}

// ── log pagination and filtering ───────────────────────────────────────

struct LogBackend;

#[async_trait]
impl OperationBackend for LogBackend {
    async fn execute(&self, ctx: OpContext) -> OpExecution {
        for i in 0..200u64 {
            let node = if i % 2 == 0 { "n1" } else { "n2" };
            let level = if i % 10 == 0 {
                Level::Warn
            } else {
                Level::Info
            };
            ctx.bus.publish(log_body(
                node,
                level,
                &format!("line {i} needle={}", i % 7 == 0),
            ));
        }
        OpExecution {
            state: OperationState::Succeeded,
            error: None,
        }
    }
}

#[tokio::test]
async fn logs_page_and_filter() {
    let dir = tempfile::tempdir().unwrap();
    let state = state_with(dir.path(), Arc::new(LogBackend), 2);
    import(&state, "logs");
    let app = router(state.clone());
    let cookie = login(app.clone(), dir.path()).await;
    let operation = submit(
        app.clone(),
        &cookie,
        "logs",
        json!({"kind": "create", "params": {}}),
    )
    .await;
    let id = operation["id"].as_str().unwrap().to_string();
    wait_state(&state, &id, OperationState::Succeeded).await;

    let page = |uri: String| {
        let app = app.clone();
        let cookie = cookie.clone();
        async move {
            let request = builder(Method::GET, &uri)
                .header(header::COOKIE, &cookie)
                .body(Body::empty())
                .unwrap();
            let response = send(app, request).await;
            assert_eq!(response.status(), StatusCode::OK);
            json_body(response).await
        }
    };

    let first = page(format!("/api/v1/operations/{id}/logs?after=0&limit=50")).await;
    let items = first["items"].as_array().unwrap();
    assert_eq!(items.len(), 50);
    assert_eq!(first["has_more"], true);
    let next = first["next_after"].as_u64().unwrap();
    assert_eq!(items.last().unwrap()["seq"].as_u64().unwrap(), next);

    let second = page(format!(
        "/api/v1/operations/{id}/logs?after={next}&limit=50"
    ))
    .await;
    let second_items = second["items"].as_array().unwrap();
    assert_eq!(second_items.len(), 50);
    assert!(second_items[0]["seq"].as_u64().unwrap() > next);

    let filtered = page(format!(
        "/api/v1/operations/{id}/logs?after=0&limit=1000&node=n1&level=warn&q=needle%3Dtrue"
    ))
    .await;
    let filtered_items = filtered["items"].as_array().unwrap();
    assert!(!filtered_items.is_empty());
    for item in filtered_items {
        assert_eq!(item["body"]["node"], "n1");
        assert_eq!(item["body"]["level"], "warn");
        assert!(item["body"]["line"]
            .as_str()
            .unwrap()
            .contains("needle=true"));
    }
}

// ── retry ──────────────────────────────────────────────────────────────

struct FailOnceBackend {
    runs: AtomicUsize,
}

#[async_trait]
impl OperationBackend for FailOnceBackend {
    async fn execute(&self, ctx: OpContext) -> OpExecution {
        let run = self.runs.fetch_add(1, Ordering::SeqCst);
        if run == 0 {
            OpExecution {
                state: OperationState::Failed,
                error: Some("first run fails".into()),
            }
        } else {
            ctx.bus
                .publish(log_body("n1", Level::Info, "retry succeeded"));
            OpExecution {
                state: OperationState::Succeeded,
                error: None,
            }
        }
    }
}

#[tokio::test]
async fn retry_links_to_original_and_resumes() {
    let dir = tempfile::tempdir().unwrap();
    let state = state_with(
        dir.path(),
        Arc::new(FailOnceBackend {
            runs: AtomicUsize::new(0),
        }),
        2,
    );
    import(&state, "retry");
    let app = router(state.clone());
    let cookie = login(app.clone(), dir.path()).await;
    let operation = submit(
        app.clone(),
        &cookie,
        "retry",
        json!({"kind": "create", "params": {}}),
    )
    .await;
    let id = operation["id"].as_str().unwrap().to_string();
    wait_state(&state, &id, OperationState::Failed).await;

    let request = builder(Method::POST, &format!("/api/v1/operations/{id}/retry"))
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::COOKIE, &cookie)
        .body(Body::empty())
        .unwrap();
    let response = send(app, request).await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let retried = json_body(response).await;
    assert_eq!(retried["retry_of"], id);
    assert_eq!(retried["params"]["resume"], true);
    let new_id = retried["id"].as_str().unwrap().to_string();
    wait_state(&state, &new_id, OperationState::Succeeded).await;
}

// ── persistence split ──────────────────────────────────────────────────

#[tokio::test]
async fn structured_events_and_logs_share_seq_but_not_storage() {
    let dir = tempfile::tempdir().unwrap();
    let state = state_with(dir.path(), Arc::new(LogBackend), 2);
    import(&state, "persist");
    let app = router(state.clone());
    let cookie = login(app.clone(), dir.path()).await;
    let operation = submit(
        app.clone(),
        &cookie,
        "persist",
        json!({"kind": "create", "params": {}}),
    )
    .await;
    let id = operation["id"].as_str().unwrap().to_string();
    wait_state(&state, &id, OperationState::Succeeded).await;

    let record = state.store().get_operation(&id).unwrap().unwrap();
    let path = record.log_path.clone().expect("log path");
    let log = std::fs::read_to_string(&path).unwrap();
    assert_eq!(log.lines().count(), 200, "JSONL holds only log lines");
    let events = state.store().events_after(&id, 0, 1000).unwrap();
    assert!(
        events.iter().all(|event| event.kind != "log"),
        "op_events holds structured bodies only"
    );
    assert!(!events.is_empty(), "the terminal event is persisted");

    let merged = state.runner().backfill(&record, 0);
    let seqs: Vec<u64> = merged.iter().map(|envelope| envelope.seq).collect();
    let mut sorted = seqs.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(seqs, sorted, "backfill is ordered and deduplicated");
    assert_eq!(seqs.len(), 201);
}
