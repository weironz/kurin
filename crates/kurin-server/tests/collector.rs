//! WP-53 acceptance tests: the collector's scheduling rules (UI §6.9)
//! against a fake status provider — interval scheduling, failure
//! retention, the certificate throttle, refresh dedupe, pause/resume
//! around operations and the global concurrency cap. The store, the
//! runner bookkeeping (operations) and the HTTP surface are the
//! production ones.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::request::Builder;
use axum::http::{header, Method, Request, StatusCode};
use axum::response::Response;
use axum::Router;
use kurin_components::status::{NodeStatus, StatusReport};
use kurin_config::Cluster;
use kurin_server::{
    Collection, Collector, CollectorSettings, Lifecycle, RefreshOutcome, StatusProvider, Store,
    StoreOptions,
};
use miette::Result;
use serde_json::{json, Value};
use tower::ServiceExt;

const HOST: &str = "127.0.0.1:7080";
const CONFIG: &str = include_str!("../../../kurin/tests/fixtures/configs/single.toml");

fn store_in(dir: &Path) -> Store {
    Store::open_with(StoreOptions::new(dir)).expect("open store")
}

/// A cluster with an imported config and `lifecycle = active`.
fn active_cluster(store: &Store, name: &str) {
    let toml = CONFIG.replace("name = \"single\"", &format!("name = \"{name}\""));
    store
        .import_cluster(name, &toml, Some("test".into()))
        .expect("import");
    let _ = store
        .set_lifecycle(name, Lifecycle::Active)
        .expect("activate");
}

/// What the fake provider does for one collection.
enum Behavior {
    /// Return this report.
    Report(StatusReport),
    /// Fail with this message.
    Fail(&'static str),
    /// Block until the shared latch fires, then succeed. The provider
    /// tracks how many collections ran concurrently.
    Gate,
}

struct FakeProvider {
    /// (cluster, certs) per call, in order.
    calls: Arc<Mutex<Vec<(String, bool)>>>,
    behaviors: Mutex<std::collections::VecDeque<Behavior>>,
    report: StatusReport,
    /// Gates block until this flips to `true`; once flipped, every gate
    /// (present and future) passes.
    release: tokio::sync::watch::Receiver<bool>,
    release_tx: tokio::sync::watch::Sender<bool>,
    in_flight: std::sync::atomic::AtomicUsize,
    max_in_flight: std::sync::atomic::AtomicUsize,
}

impl FakeProvider {
    fn new(report: StatusReport) -> Self {
        Self::with_behaviors(report, Vec::new())
    }

    fn with_behaviors(report: StatusReport, behaviors: Vec<Behavior>) -> Self {
        let (release_tx, release) = tokio::sync::watch::channel(false);
        Self {
            calls: Arc::new(Mutex::new(Vec::new())),
            behaviors: Mutex::new(behaviors.into()),
            report,
            release,
            release_tx,
            in_flight: 0.into(),
            max_in_flight: 0.into(),
        }
    }

    fn calls(&self) -> Vec<(String, bool)> {
        self.calls.lock().unwrap().clone()
    }

    /// Unblock every gate, current and future.
    fn release_gates(&self) {
        self.release_tx.send_replace(true);
    }
}

#[async_trait]
impl StatusProvider for FakeProvider {
    async fn collect(&self, spec: &Cluster, certs: bool) -> Result<Collection> {
        self.calls.lock().unwrap().push((spec.name.clone(), certs));
        let behavior = self.behaviors.lock().unwrap().pop_front();
        let entered = self
            .in_flight
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        self.max_in_flight
            .fetch_max(entered, std::sync::atomic::Ordering::SeqCst);
        match behavior {
            Some(Behavior::Fail(message)) => {
                self.in_flight
                    .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                Err(miette::miette!("{message}"))
            }
            Some(Behavior::Report(report)) => {
                self.in_flight
                    .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                Ok(Collection {
                    report,
                    addons: None,
                })
            }
            Some(Behavior::Gate) => {
                let mut gate = self.release.clone();
                while !*gate.borrow() {
                    if gate.changed().await.is_err() {
                        break;
                    }
                }
                self.in_flight
                    .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                Ok(Collection {
                    report: self.report.clone(),
                    addons: None,
                })
            }
            _ => {
                self.in_flight
                    .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                Ok(Collection {
                    report: self.report.clone(),
                    addons: None,
                })
            }
        }
    }
}

fn report(healthy: bool, cert_min_days: Option<i64>) -> StatusReport {
    StatusReport {
        healthy,
        nodes: vec![
            NodeStatus {
                name: "master01".into(),
                ready: true,
                kubelet_version: "v1.33.1".into(),
                roles: "control-plane".into(),
            },
            NodeStatus {
                name: "worker01".into(),
                ready: healthy,
                kubelet_version: "v1.33.1".into(),
                roles: "worker".into(),
            },
        ],
        pods_not_running: if healthy {
            vec![]
        } else {
            vec!["kube-system/coredns-1".into()]
        },
        etcd_ok: true,
        cert_min_days,
        expected_version: None,
        observed_version: Some("v1.33.1".into()),
    }
}

fn settings() -> CollectorSettings {
    CollectorSettings {
        interval: Duration::from_secs(600),
        jitter: 0.0,
        timeout: Duration::from_secs(5),
        concurrency: 4,
        cert_interval: Duration::from_secs(3600),
        refresh_dedupe: Duration::from_millis(250),
        tick: Duration::from_millis(20),
    }
}

async fn wait_for<F: FnMut() -> bool>(mut condition: F, what: &str) {
    for _ in 0..500 {
        if condition() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for {what}");
}

async fn wait_for_snapshot(store: &Store, cluster: &str) -> kurin_server::SnapshotRecord {
    let mut last = None;
    wait_for(
        || {
            last = store.get_snapshot(cluster).ok().flatten();
            last.is_some()
        },
        "the snapshot",
    )
    .await;
    last.unwrap()
}

fn collector(store: &Store, provider: Arc<FakeProvider>, settings: CollectorSettings) -> Collector {
    Collector::with_provider(store.clone(), provider, settings)
}

#[tokio::test]
async fn collects_active_clusters_and_schedules_a_jittered_next_round() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_in(dir.path());
    active_cluster(&store, "alpha");
    // Draft clusters are not collected (UI §6.9: only `active`).
    let toml = CONFIG.replace("name = \"single\"", "name = \"draft\"");
    store.import_cluster("draft", &toml, None).unwrap();

    let provider = Arc::new(FakeProvider::new(report(true, Some(42))));
    let mut settings = settings();
    settings.interval = Duration::from_secs(60);
    settings.jitter = 0.10;
    let collector = collector(&store, provider.clone(), settings);

    collector.tick().await;
    let snapshot = wait_for_snapshot(&store, "alpha").await;
    assert_eq!(snapshot.error, None);
    assert!(snapshot.observed_at.is_some());
    let parsed: Value = serde_json::from_str(snapshot.report.as_deref().unwrap()).unwrap();
    assert_eq!(parsed["nodes"].as_array().unwrap().len(), 2);
    assert_eq!(snapshot.cert_min_days, Some(42));

    // The draft cluster never reached the provider.
    assert_eq!(provider.calls().len(), 1);
    assert_eq!(provider.calls()[0].0, "alpha");

    // The next round is a full interval away, jittered by ±10 %.
    let due = collector.next_due_in("alpha").expect("scheduled");
    assert!(
        due >= Duration::from_secs(50) && due <= Duration::from_secs(66),
        "next round {due:?} outside the jitter bounds"
    );
    // Ticks in between do not start another collection.
    collector.tick().await;
    collector.tick().await;
    assert_eq!(provider.calls().len(), 1);
}

#[tokio::test]
async fn certificate_rounds_run_only_every_cert_interval() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_in(dir.path());
    active_cluster(&store, "alpha");

    // A previous cert probe two days ago: the first round is due, and the
    // persisted cert value survives into the stored report meanwhile.
    store
        .save_snapshot_success(
            "alpha",
            r#"{"healthy":true,"nodes":[]}"#,
            true,
            Some(7),
            None,
        )
        .unwrap();
    store
        .write(|conn| {
            conn.execute(
                "UPDATE snapshots SET certs_at = '2026-01-01T00:00:00.000000Z' WHERE cluster = 'alpha'",
                [],
            )
            .map_err(kurin_server::StoreError::from)
        })
        .unwrap();

    // Round 1 finds 11 days left; the stale probe makes it due.
    let provider = Arc::new(FakeProvider::with_behaviors(
        report(true, None),
        vec![Behavior::Report(report(true, Some(11)))],
    ));
    let mut settings = settings();
    settings.refresh_dedupe = Duration::ZERO;
    let collector = collector(&store, provider.clone(), settings);

    collector.tick().await;
    wait_for(|| provider.calls().len() == 1, "the first round").await;
    assert!(provider.calls()[0].1, "first round must probe certificates");
    let first = wait_for_snapshot(&store, "alpha").await;
    assert_eq!(first.cert_min_days, Some(11));
    assert!(first.certs_at.is_some());

    // Round 2 (forced by a refresh): the probe just happened, so it is
    // skipped and the stored report keeps the last known value.
    collector.request_refresh(&store.get_cluster("alpha").unwrap().unwrap());
    collector.tick().await;
    wait_for(|| provider.calls().len() == 2, "the second round").await;
    assert!(!provider.calls()[1].1, "certificates must be throttled");
    let second = wait_for_snapshot(&store, "alpha").await;
    let parsed: Value = serde_json::from_str(second.report.as_deref().unwrap()).unwrap();
    assert_eq!(
        parsed["cert_min_days"],
        Value::from(11),
        "the last known cert value is carried into the stored report"
    );
    assert_eq!(second.cert_min_days, Some(11));
    assert_eq!(second.certs_at, first.certs_at);

    // Round 3: two days have "passed" since the last probe again. This
    // probe finds nothing, so the columns reflect exactly that.
    store
        .write(|conn| {
            conn.execute(
                "UPDATE snapshots SET certs_at = '2026-01-01T00:00:00.000000Z' WHERE cluster = 'alpha'",
                [],
            )
            .map_err(kurin_server::StoreError::from)
        })
        .unwrap();
    collector.request_refresh(&store.get_cluster("alpha").unwrap().unwrap());
    collector.tick().await;
    wait_for(|| provider.calls().len() == 3, "the third round").await;
    assert!(
        provider.calls()[2].1,
        "certificates are probed again once due"
    );
    let third = wait_for_snapshot(&store, "alpha").await;
    assert_eq!(
        third.cert_min_days, None,
        "a probe that found nothing clears the column"
    );
    assert!(third.certs_at > first.certs_at);
}

#[tokio::test]
async fn a_failed_collection_never_overwrites_the_last_good_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_in(dir.path());
    active_cluster(&store, "alpha");

    let provider = Arc::new(FakeProvider::with_behaviors(
        report(true, Some(30)),
        vec![
            Behavior::Report(report(true, Some(30))),
            Behavior::Fail("所有控制面节点 SSH 连接失败"),
        ],
    ));
    let mut settings = settings();
    settings.refresh_dedupe = Duration::ZERO;
    let collector = collector(&store, provider.clone(), settings);

    collector.tick().await;
    let good = wait_for_snapshot(&store, "alpha").await;
    assert_eq!(good.error, None);

    collector.request_refresh(&store.get_cluster("alpha").unwrap().unwrap());
    collector.tick().await;
    wait_for(|| provider.calls().len() == 2, "the failing round").await;
    let failed = wait_for_snapshot(&store, "alpha").await;
    // Only `attempted_at` and `error` moved (UI §6.9).
    assert_eq!(failed.report, good.report, "the report must survive");
    assert_eq!(failed.observed_at, good.observed_at);
    assert_eq!(failed.cert_min_days, good.cert_min_days);
    assert!(failed.attempted_at > good.attempted_at);
    assert!(failed
        .error
        .as_deref()
        .unwrap()
        .contains("所有控制面节点 SSH 连接失败"));
}

#[tokio::test]
async fn a_slow_collection_times_out_and_keeps_the_last_good_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_in(dir.path());
    active_cluster(&store, "alpha");

    store
        .save_snapshot_success(
            "alpha",
            r#"{"healthy":true,"nodes":[]}"#,
            true,
            Some(9),
            None,
        )
        .unwrap();
    let previous = store.get_snapshot("alpha").unwrap().unwrap();

    let provider = Arc::new(FakeProvider::with_behaviors(
        report(true, None),
        vec![Behavior::Gate],
    ));
    let mut settings = settings();
    settings.timeout = Duration::from_millis(80);
    settings.refresh_dedupe = Duration::ZERO;
    let collector = collector(&store, provider.clone(), settings);

    collector.tick().await;
    wait_for(|| provider.calls().len() == 1, "the gated round").await;
    wait_for(
        || {
            store
                .get_snapshot("alpha")
                .unwrap()
                .is_some_and(|row| row.error.as_deref().is_some_and(|e| e.contains("采集超时")))
        },
        "the timeout failure",
    )
    .await;

    let failed = store.get_snapshot("alpha").unwrap().unwrap();
    assert_eq!(failed.report, previous.report);
    assert_eq!(failed.observed_at, previous.observed_at);
    // The gated provider never answered; releasing it after the fact must
    // not resurrect a stale write over the failure row.
    provider.release_gates();
}

#[tokio::test]
async fn refresh_is_deduplicated_and_moves_the_cluster_to_the_queue_head() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_in(dir.path());
    active_cluster(&store, "alpha");

    let provider = Arc::new(FakeProvider::new(report(true, None)));
    let mut settings = settings();
    settings.refresh_dedupe = Duration::from_millis(250);
    let collector = collector(&store, provider.clone(), settings);

    let record = store.get_cluster("alpha").unwrap().unwrap();
    assert_eq!(
        collector.request_refresh(&record),
        RefreshOutcome::Scheduled
    );
    // Multiple tabs inside the window: one collection, not N.
    assert_eq!(
        collector.request_refresh(&record),
        RefreshOutcome::Deduplicated
    );
    assert_eq!(
        collector.request_refresh(&record),
        RefreshOutcome::Deduplicated
    );
    assert!(collector.next_due_in("alpha").unwrap() < Duration::from_millis(50));

    collector.tick().await;
    wait_for(|| provider.calls().len() == 1, "the refresh round").await;
    wait_for_snapshot(&store, "alpha").await;

    // Non-active clusters are never collected, so their refresh is a no-op.
    store.set_lifecycle("alpha", Lifecycle::Archived).unwrap();
    let record = store.get_cluster("alpha").unwrap().unwrap();
    assert_eq!(collector.request_refresh(&record), RefreshOutcome::Inactive);
}

#[tokio::test]
async fn collection_pauses_while_an_operation_runs_and_resumes_right_after() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_in(dir.path());
    active_cluster(&store, "alpha");

    let provider = Arc::new(FakeProvider::new(report(true, None)));
    let collector = collector(&store, provider.clone(), settings());

    // A queued mutation for the cluster pauses its collection.
    let operation = store
        .insert_operation(kurin_server::NewOperation {
            id: "op-1".into(),
            cluster: Some("alpha".into()),
            kind: "create".into(),
            config_rev: Some(1),
            params: "{}".into(),
            retry_of: None,
            actor: "test".into(),
        })
        .unwrap();
    assert_eq!(operation.state, kurin_server::OperationState::Queued);

    collector.tick().await;
    collector.tick().await;
    assert!(
        provider.calls().is_empty(),
        "paused while the operation runs"
    );
    assert!(store.get_snapshot("alpha").unwrap().is_none());

    // The operation ends: the very next tick collects immediately.
    store
        .finish_operation("op-1", kurin_server::OperationState::Succeeded, None)
        .unwrap();
    collector.tick().await;
    wait_for(|| provider.calls().len() == 1, "the resume round").await;
    let snapshot = wait_for_snapshot(&store, "alpha").await;
    assert_eq!(snapshot.error, None);
}

#[tokio::test]
async fn the_global_concurrency_cap_bounds_parallel_collections() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_in(dir.path());
    for name in ["one", "two", "three", "four", "five"] {
        active_cluster(&store, name);
    }

    let provider = Arc::new(FakeProvider::with_behaviors(
        report(true, None),
        (0..5).map(|_| Behavior::Gate).collect(),
    ));
    let mut settings = settings();
    settings.concurrency = 2;
    let collector = collector(&store, provider.clone(), settings);

    collector.tick().await;
    // Five clusters are due, but only two collections run at a time.
    wait_for(|| provider.calls().len() == 2, "the two permits").await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(provider.calls().len(), 2, "the semaphore blocks the rest");

    provider.release_gates();
    wait_for(
        || {
            store
                .list_clusters()
                .unwrap()
                .iter()
                .all(|record| store.get_snapshot(&record.name).unwrap().is_some())
        },
        "every cluster collected",
    )
    .await;
    assert_eq!(provider.calls().len(), 5);
    assert_eq!(
        provider
            .max_in_flight
            .load(std::sync::atomic::Ordering::SeqCst),
        2,
        "never more than two concurrent collections"
    );
}

/* --------------------------------- HTTP ---------------------------------- */

fn options(dir: &Path) -> kurin_server::ServeOptions {
    kurin_server::ServeOptions {
        data_dir: dir.to_path_buf(),
        ..Default::default()
    }
}

fn noop_backend() -> Arc<dyn kurin_server::OperationBackend> {
    struct Noop;
    #[async_trait]
    impl kurin_server::OperationBackend for Noop {
        async fn execute(&self, _ctx: kurin_server::OpContext) -> kurin_server::OpExecution {
            kurin_server::OpExecution {
                state: kurin_server::OperationState::Succeeded,
                error: None,
                result: None,
            }
        }
    }
    Arc::new(Noop)
}

fn token(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("token"))
        .unwrap()
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

#[tokio::test]
async fn the_refresh_endpoint_deduplicates_over_the_window() {
    let dir = tempfile::tempdir().unwrap();
    let startup =
        kurin_server::startup_with_backend(&options(dir.path()), noop_backend()).expect("startup");
    let app = kurin_server::router(startup.state.clone());
    let cookie = login(app.clone(), dir.path()).await;

    let toml = CONFIG.replace("name = \"single\"", "name = \"alpha\"");
    let request = builder(Method::POST, "/api/v1/clusters")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::COOKIE, &cookie)
        .body(Body::from(json!({ "toml": toml }).to_string()))
        .unwrap();
    let response = send(app.clone(), request).await;
    assert_eq!(response.status(), StatusCode::CREATED);

    // Unknown cluster: 404.
    let request = builder(Method::POST, "/api/v1/clusters/nope/refresh")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::COOKIE, &cookie)
        .body(Body::empty())
        .unwrap();
    let response = send(app.clone(), request).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    // Draft cluster: nothing collects it.
    let request = builder(Method::POST, "/api/v1/clusters/alpha/refresh")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::COOKIE, &cookie)
        .body(Body::empty())
        .unwrap();
    let response = send(app.clone(), request).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        json_body(response).await,
        json!({ "scheduled": false, "reason": "inactive" })
    );

    // Active: scheduled, then deduplicated inside the 10 s window.
    startup
        .state
        .store()
        .set_lifecycle("alpha", Lifecycle::Active)
        .unwrap();
    let request = builder(Method::POST, "/api/v1/clusters/alpha/refresh")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::COOKIE, &cookie)
        .body(Body::empty())
        .unwrap();
    let response = send(app.clone(), request).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(json_body(response).await, json!({ "scheduled": true }));

    let request = builder(Method::POST, "/api/v1/clusters/alpha/refresh")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::COOKIE, &cookie)
        .body(Body::empty())
        .unwrap();
    let response = send(app.clone(), request).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        json_body(response).await,
        json!({ "scheduled": false, "reason": "deduplicated" })
    );
}

#[tokio::test]
async fn the_overview_carries_the_snapshot_and_the_health_badge() {
    let dir = tempfile::tempdir().unwrap();
    let startup =
        kurin_server::startup_with_backend(&options(dir.path()), noop_backend()).expect("startup");
    let store = startup.state.store();
    let app = kurin_server::router(startup.state.clone());
    let cookie = login(app.clone(), dir.path()).await;

    // A healthy collected cluster.
    active_cluster(store, "healthy");
    store
        .save_snapshot_success(
            "healthy",
            &serde_json::to_string(&report(true, Some(210))).unwrap(),
            true,
            Some(210),
            None,
        )
        .unwrap();
    // A degraded one: fresh report, unhealthy verdict.
    active_cluster(store, "degraded");
    store
        .save_snapshot_success(
            "degraded",
            &serde_json::to_string(&report(false, None)).unwrap(),
            true,
            None,
            None,
        )
        .unwrap();
    // An unreachable one: the last attempt failed, the good report stays.
    active_cluster(store, "down");
    store
        .save_snapshot_success(
            "down",
            &serde_json::to_string(&report(true, Some(45))).unwrap(),
            true,
            Some(45),
            None,
        )
        .unwrap();
    store
        .save_snapshot_failure("down", "dial tcp 10.0.1.11:22: i/o timeout")
        .unwrap();
    // A stale one: the last good snapshot is a day old.
    active_cluster(store, "old");
    store
        .save_snapshot_success(
            "old",
            &serde_json::to_string(&report(true, Some(45))).unwrap(),
            true,
            Some(45),
            None,
        )
        .unwrap();
    store
        .write(|conn| {
            conn.execute(
                "UPDATE snapshots SET observed_at = '2026-01-01T00:00:00.000000Z' \
                 WHERE cluster = 'old'",
                [],
            )
            .map_err(kurin_server::StoreError::from)
        })
        .unwrap();
    // Never collected.
    active_cluster(store, "fresh");

    let request = builder(Method::GET, "/api/v1/clusters")
        .header(header::COOKIE, &cookie)
        .body(Body::empty())
        .unwrap();
    let response = send(app.clone(), request).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    let clusters: Vec<Value> = body["clusters"].as_array().unwrap().clone();
    let by_name = |name: &str| {
        clusters
            .iter()
            .find(|cluster| cluster["name"] == json!(name))
            .unwrap_or_else(|| panic!("cluster {name} missing"))
            .clone()
    };

    assert_eq!(by_name("healthy")["health"], json!("healthy"));
    assert_eq!(by_name("healthy")["nodes_ready"], json!(2));
    assert_eq!(by_name("healthy")["nodes_total"], json!(2));
    assert_eq!(by_name("healthy")["cert_min_days"], json!(210));
    assert_eq!(by_name("healthy")["distribution"], json!("kubeadm"));
    assert_eq!(by_name("healthy")["kubernetes_version"], json!("v1.33.1"));
    assert_eq!(by_name("healthy")["collect_error"], Value::Null);
    assert!(by_name("healthy")["observed_at"].is_string());

    assert_eq!(by_name("degraded")["health"], json!("degraded"));
    assert_eq!(by_name("degraded")["nodes_ready"], json!(1));

    assert_eq!(by_name("down")["health"], json!("unreachable"));
    assert_eq!(
        by_name("down")["collect_error"],
        json!("dial tcp 10.0.1.11:22: i/o timeout")
    );
    // The last good data survives for the detail view, but the badge says
    // unreachable — old data is never presented as current health.
    assert_eq!(by_name("down")["nodes_ready"], json!(2));

    assert_eq!(by_name("old")["health"], json!("stale"));

    assert_eq!(by_name("fresh")["health"], json!("unknown"));
    assert_eq!(by_name("fresh")["nodes_ready"], Value::Null);
    assert_eq!(by_name("fresh")["observed_at"], Value::Null);

    // The detail view carries the raw snapshot row.
    let request = builder(Method::GET, "/api/v1/clusters/down")
        .header(header::COOKIE, &cookie)
        .body(Body::empty())
        .unwrap();
    let response = send(app.clone(), request).await;
    assert_eq!(response.status(), StatusCode::OK);
    let detail = json_body(response).await;
    assert!(detail["snapshot"]["report"]["nodes"].is_array());
    assert_eq!(
        detail["snapshot"]["error"],
        json!("dial tcp 10.0.1.11:22: i/o timeout")
    );
    assert_eq!(detail["snapshot"]["cert_min_days"], json!(45));
    assert_eq!(detail["health"], json!("unreachable"));
}
