//! WP-18 tracker tests: one JSON fixture per built-in readiness rule
//! (ready / not-ready / failed), the unknown-type heuristic, change-only
//! `Resource` emission with heartbeat, and the stuck-diagnostics text
//! (error message + the warn log lines it publishes).
//!
//! [`ScriptedApi`] serves `get_json` from scripted fixture sequences (the
//! last entry repeats), `list_json` from fixture lists filtered through
//! the requested label selector, and `pod_logs` from fixture line arrays
//! honoring `tail_lines` — the same contract the real backends offer.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use kube::api::DynamicObject;
use kurin_core::events::{Body, Envelope, EventBus, Level};
use kurin_kube::{
    evaluate, track, ApplyOpts, ApplyReport, ClusterApi, DeleteOpts, Gvk, LogOpts, ObjRef,
    TrackOpts, TrackResult,
};
use miette::Result;
use serde_json::Value;
use tokio::sync::mpsc::Receiver;

/// Recorded `list_json` call: (group, kind, ns, selector).
type ListCall = (String, String, Option<String>, Option<String>);

/// One `evaluate` case: (fixture, apiVersion, kind, ns, name, expect_ready,
/// expect_status).
type Case = (
    &'static str,
    &'static str,
    &'static str,
    Option<&'static str>,
    &'static str,
    bool,
    &'static str,
);

// ── scripted ClusterApi ───────────────────────────────────────────────

#[derive(Default)]
struct ScriptedApi {
    /// `get_json` scripts keyed `kind|namespace|name`; each poll serves
    /// the head, then advances until the last entry (which repeats).
    /// A JSON `null` head means "object missing" for that poll.
    objects: Mutex<BTreeMap<String, VecDeque<Value>>>,
    /// `get_json` polls that should fail, keyed like `objects`.
    object_errors: Mutex<BTreeMap<String, usize>>,
    /// `list_json` responses keyed `kind|namespace`.
    lists: Mutex<BTreeMap<String, Vec<Value>>>,
    /// `pod_logs` lines keyed `namespace|pod|container`.
    logs: Mutex<BTreeMap<String, Vec<String>>>,
    /// `pod_logs` failures keyed `namespace|pod|container`.
    log_errors: Mutex<BTreeMap<String, String>>,
    /// Recorded `list_json` calls: (group, kind, ns, selector).
    list_calls: Mutex<Vec<ListCall>>,
    /// Recorded `pod_logs` calls: (pod, container, tail).
    log_calls: Mutex<Vec<(String, String, i64)>>,
    /// `get_json` poll counts per key.
    polls: Mutex<BTreeMap<String, usize>>,
}

impl ScriptedApi {
    fn new() -> Self {
        Self::default()
    }

    /// Serve `file` for every `get_json` of `r`.
    fn serve(self, r: &ObjRef, file: &str) -> Self {
        self.objects
            .lock()
            .unwrap()
            .insert(obj_key(r), VecDeque::from([fixture(file)]));
        self
    }

    /// Serve a scripted sequence (last entry repeats) for `r`.
    fn script(self, r: &ObjRef, files: &[&str]) -> Self {
        let script: Vec<Value> = files.iter().map(|f| fixture(f)).collect();
        self.objects
            .lock()
            .unwrap()
            .insert(obj_key(r), VecDeque::from(script));
        self
    }

    /// Fail the first `n` `get_json` polls of `r` with `message`.
    fn fail_first(self, r: &ObjRef, n: usize) -> Self {
        self.object_errors.lock().unwrap().insert(obj_key(r), n);
        self
    }

    /// Serve `file` (a `*List` fixture) for `list_json` of that kind and
    /// namespace, regardless of selector.
    fn list(self, kind: &str, ns: &str, file: &str) -> Self {
        self.lists
            .lock()
            .unwrap()
            .insert(format!("{kind}|{ns}"), list_items(file));
        self
    }

    /// Serve log `file` (a JSON array of lines) for one container.
    fn logs(self, ns: &str, pod: &str, container: &str, file: &str) -> Self {
        let lines: Vec<String> =
            serde_json::from_str(&fixture_text(file)).expect("log fixture is a JSON array");
        self.logs
            .lock()
            .unwrap()
            .insert(format!("{ns}|{pod}|{container}"), lines);
        self
    }

    /// Fail `pod_logs` of one container with `message`.
    fn fail_logs(self, ns: &str, pod: &str, container: &str, message: &str) -> Self {
        self.log_errors
            .lock()
            .unwrap()
            .insert(format!("{ns}|{pod}|{container}"), message.to_string());
        self
    }

    fn log_calls(&self) -> Vec<(String, String, i64)> {
        self.log_calls.lock().unwrap().clone()
    }

    fn list_calls(&self) -> Vec<ListCall> {
        self.list_calls.lock().unwrap().clone()
    }

    fn polls(&self, key: &str) -> usize {
        *self.polls.lock().unwrap().get(key).unwrap_or(&0)
    }
}

fn obj_key(r: &ObjRef) -> String {
    format!(
        "{}|{}|{}",
        r.kind,
        r.namespace.as_deref().unwrap_or(""),
        r.name
    )
}

#[async_trait]
impl ClusterApi for ScriptedApi {
    async fn apply(&self, _objs: &[DynamicObject], _opts: ApplyOpts) -> Result<ApplyReport> {
        Err(miette::miette!("not used by the tracker"))
    }

    async fn delete(&self, _refs: &[ObjRef], _opts: DeleteOpts) -> Result<()> {
        Err(miette::miette!("not used by the tracker"))
    }

    async fn get_json(&self, r: &ObjRef) -> Result<Option<Value>> {
        let key = obj_key(r);
        let polls = {
            let mut map = self.polls.lock().unwrap();
            let entry = map.entry(key.clone()).or_insert(0);
            *entry += 1;
            *entry
        };
        if let Some(remaining) = self.object_errors.lock().unwrap().get(&key) {
            if polls <= *remaining {
                return Err(miette::miette!("connection refused (scripted)"));
            }
        }
        let mut objects = self.objects.lock().unwrap();
        let Some(script) = objects.get_mut(&key) else {
            return Ok(None);
        };
        let head = script.front().cloned();
        if script.len() > 1 {
            script.pop_front();
        }
        Ok(head.filter(|v| !v.is_null()))
    }

    async fn list_json(
        &self,
        gvk: &Gvk,
        ns: Option<&str>,
        selector: Option<&str>,
    ) -> Result<Vec<Value>> {
        self.list_calls.lock().unwrap().push((
            gvk.group.clone(),
            gvk.kind.clone(),
            ns.map(str::to_string),
            selector.map(str::to_string),
        ));
        let key = format!("{}|{}", gvk.kind, ns.unwrap_or(""));
        let items = self
            .lists
            .lock()
            .unwrap()
            .get(&key)
            .cloned()
            .unwrap_or_default();
        Ok(match selector {
            Some(selector) => items
                .into_iter()
                .filter(|item| matches_selector(item, selector))
                .collect(),
            None => items,
        })
    }

    async fn get_raw(&self, _path: &str) -> Result<String> {
        Err(miette::miette!("not used by the tracker"))
    }

    async fn pod_logs(
        &self,
        pod: &ObjRef,
        opts: LogOpts,
        sink: &mut (dyn for<'a> FnMut(&'a str) + Send),
    ) -> Result<()> {
        let container = opts.container.clone().unwrap_or_default();
        self.log_calls.lock().unwrap().push((
            pod.name.clone(),
            container.clone(),
            opts.tail_lines.unwrap_or(0),
        ));
        let key = format!(
            "{}|{}|{}",
            pod.namespace.as_deref().unwrap_or(""),
            pod.name,
            container
        );
        if let Some(err) = self.log_errors.lock().unwrap().get(&key) {
            return Err(miette::miette!("{err}"));
        }
        let lines = self
            .logs
            .lock()
            .unwrap()
            .get(&key)
            .cloned()
            .unwrap_or_default();
        let tail = opts.tail_lines.unwrap_or(i64::MAX).max(0) as usize;
        let start = lines.len().saturating_sub(tail);
        for line in &lines[start..] {
            sink(line);
        }
        Ok(())
    }
}

/// `k=v[,k2=v2]` selector against an item's `metadata.labels`.
fn matches_selector(item: &Value, selector: &str) -> bool {
    let labels = item.pointer("/metadata/labels");
    selector.split(',').all(|pair| {
        let Some((key, value)) = pair.split_once('=') else {
            return false;
        };
        labels
            .and_then(|l| l.pointer(&format!("/{key}")))
            .and_then(Value::as_str)
            == Some(value)
    })
}

// ── fixtures and helpers ──────────────────────────────────────────────

fn fixture_text(name: &str) -> String {
    let path = format!("tests/fixtures/track/{name}");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"))
}

fn fixture(name: &str) -> Value {
    serde_json::from_str(&fixture_text(name)).unwrap_or_else(|e| panic!("parse {name}: {e}"))
}

fn list_items(name: &str) -> Vec<Value> {
    fixture(name)
        .pointer("/items")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn reference(api_version: &str, kind: &str, ns: Option<&str>, name: &str) -> ObjRef {
    ObjRef {
        api_version: api_version.into(),
        kind: kind.into(),
        name: name.into(),
        namespace: ns.map(str::to_string),
    }
}

fn fast(timeout_ms: u64, interval_ms: u64, heartbeat_ms: u64) -> TrackOpts {
    TrackOpts {
        timeout: Duration::from_millis(timeout_ms),
        interval: Duration::from_millis(interval_ms),
        heartbeat: Duration::from_millis(heartbeat_ms),
    }
}

fn drain(rx: &mut Receiver<Envelope>) -> Vec<Envelope> {
    let mut out = Vec::new();
    while let Ok(envelope) = rx.try_recv() {
        out.push(envelope);
    }
    out
}

fn resource_events(envelopes: &[Envelope]) -> Vec<(bool, String)> {
    envelopes
        .iter()
        .filter_map(|e| match &e.body {
            Body::Resource { ready, status, .. } => Some((*ready, status.clone())),
            _ => None,
        })
        .collect()
}

fn warn_lines(envelopes: &[Envelope]) -> Vec<String> {
    envelopes
        .iter()
        .filter_map(|e| match &e.body {
            Body::Log { level, line, .. } if *level == Level::Warn => Some(line.clone()),
            _ => None,
        })
        .collect()
}

const POD: fn() -> ObjRef = || reference("v1", "Pod", Some("default"), "web-5d8f7b6c4-x2v9p");
const DS: fn() -> ObjRef = || {
    reference(
        "apps/v1",
        "DaemonSet",
        Some("kube-flannel"),
        "kube-flannel-ds",
    )
};

// ── built-in rules: one fixture per ready / not-ready / failed state ──

#[test]
fn evaluate_covers_every_built_in_rule() {
    let cases: &[Case] = &[
        // (fixture, api_version, kind, ns, name, expect_ready, expect_status)
        (
            "deployment-ready.json",
            "apps/v1",
            "Deployment",
            Some("default"),
            "web",
            true,
            "2/2 updated, 2/2 available",
        ),
        (
            "deployment-not-ready.json",
            "apps/v1",
            "Deployment",
            Some("default"),
            "web",
            false,
            "1/2 updated, 1/2 available",
        ),
        (
            "statefulset-ready.json",
            "apps/v1",
            "StatefulSet",
            Some("default"),
            "db",
            true,
            "2/2 ready, 2/2 updated",
        ),
        (
            "statefulset-not-ready.json",
            "apps/v1",
            "StatefulSet",
            Some("default"),
            "db",
            false,
            "1/2 ready, 1/2 updated",
        ),
        (
            "daemonset-ready.json",
            "apps/v1",
            "DaemonSet",
            Some("kube-flannel"),
            "kube-flannel-ds",
            true,
            "3/3 ready",
        ),
        (
            "daemonset-not-ready.json",
            "apps/v1",
            "DaemonSet",
            Some("kube-flannel"),
            "kube-flannel-ds",
            false,
            "2/3 ready",
        ),
        // Zero desired is not ready: a fresh DaemonSet reports 0/0 before
        // the scheduler places it (matches the previous CNI polls).
        (
            "daemonset-zero-desired.json",
            "apps/v1",
            "DaemonSet",
            Some("kube-system"),
            "agent",
            false,
            "0/0 ready",
        ),
        (
            "job-succeeded.json",
            "batch/v1",
            "Job",
            Some("default"),
            "migrate",
            true,
            "1/1 succeeded",
        ),
        (
            "job-running.json",
            "batch/v1",
            "Job",
            Some("default"),
            "migrate",
            false,
            "0/1 succeeded",
        ),
        (
            "job-failed.json",
            "batch/v1",
            "Job",
            Some("default"),
            "migrate",
            false,
            "0/1 succeeded, 5 failed",
        ),
        (
            "pod-ready.json",
            "v1",
            "Pod",
            Some("default"),
            "web-5d8f7b6c4-x2v9p",
            true,
            "Running",
        ),
        (
            "pod-pending.json",
            "v1",
            "Pod",
            Some("default"),
            "web-5d8f7b6c4-x2v9p",
            false,
            "Pending/ContainerCreating",
        ),
        (
            "pod-crashloop.json",
            "v1",
            "Pod",
            Some("default"),
            "web-5d8f7b6c4-x2v9p",
            false,
            "Running/CrashLoopBackOff",
        ),
        (
            "pod-imagepull.json",
            "v1",
            "Pod",
            Some("default"),
            "web-5d8f7b6c4-x2v9p",
            false,
            "Pending/ImagePullBackOff",
        ),
        (
            "pvc-bound.json",
            "v1",
            "PersistentVolumeClaim",
            Some("default"),
            "data-db-0",
            true,
            "Bound",
        ),
        (
            "pvc-pending.json",
            "v1",
            "PersistentVolumeClaim",
            Some("default"),
            "data-db-0",
            false,
            "Pending",
        ),
        (
            "crd-established.json",
            "apiextensions.k8s.io/v1",
            "CustomResourceDefinition",
            None,
            "installations.operator.tigera.io",
            true,
            "Established=True",
        ),
        (
            "crd-not-established.json",
            "apiextensions.k8s.io/v1",
            "CustomResourceDefinition",
            None,
            "installations.operator.tigera.io",
            false,
            "Established=False",
        ),
        (
            "apiservice-available.json",
            "apiregistration.k8s.io/v1",
            "APIService",
            None,
            "v1beta1.metrics.k8s.io",
            true,
            "Available=True",
        ),
        (
            "apiservice-unavailable.json",
            "apiregistration.k8s.io/v1",
            "APIService",
            None,
            "v1beta1.metrics.k8s.io",
            false,
            "Available=False",
        ),
        ("node-ready.json", "v1", "Node", None, "m1", true, "Ready"),
        (
            "node-not-ready.json",
            "v1",
            "Node",
            None,
            "m1",
            false,
            "NotReady",
        ),
        (
            "unknown-ready.json",
            "example.com/v1",
            "Widget",
            Some("default"),
            "gizmo",
            true,
            "Ready=True",
        ),
        (
            "unknown-not-ready.json",
            "example.com/v1",
            "Widget",
            Some("default"),
            "gizmo",
            false,
            "Available=False",
        ),
        (
            "unknown-no-conditions.json",
            "example.com/v1",
            "Widget",
            Some("default"),
            "gizmo",
            false,
            "no Ready/Available condition",
        ),
    ];
    for (file, api_version, kind, ns, name, expect_ready, expect_status) in cases {
        let state = evaluate(
            &reference(api_version, kind, ns.as_deref(), name),
            &fixture(file),
        );
        assert_eq!(state.ready, *expect_ready, "ready flag of {file}");
        assert_eq!(&state.status, expect_status, "status of {file}");
    }
}

// ── the loop ─────────────────────────────────────────────────────────

#[tokio::test]
async fn empty_refs_return_immediately() {
    let api = ScriptedApi::new();
    let bus = EventBus::new("op-1");
    let mut rx = bus.subscribe();
    let result = track(&api, &[], &fast(200, 5, 10_000), &bus)
        .await
        .expect("nothing to track");
    assert_eq!(result, TrackResult { objects: vec![] });
    assert!(drain(&mut rx).is_empty());
}

#[tokio::test]
async fn track_converges_and_emits_resource_events_on_change_only() {
    let pod = POD();
    let ds = DS();
    let api = ScriptedApi::new()
        // pending → ready
        .script(&pod, &["pod-pending.json", "pod-ready.json"])
        // not-ready twice (same status → second poll must stay silent),
        // then ready
        .script(
            &ds,
            &[
                "daemonset-not-ready.json",
                "daemonset-not-ready.json",
                "daemonset-ready.json",
            ],
        );
    let bus = EventBus::new("op-1");
    let mut rx = bus.subscribe();
    let result = track(
        &api,
        &[pod.clone(), ds.clone()],
        &fast(2_000, 2, 10_000),
        &bus,
    )
    .await
    .expect("both objects converge");

    assert_eq!(result.objects.len(), 2);
    assert!(result.objects[0].state.ready);
    assert!(result.objects[1].state.ready);

    // One event per *change*: the repeated not-ready DaemonSet poll
    // (poll 2) published nothing.
    let events = resource_events(&drain(&mut rx));
    assert_eq!(
        events,
        vec![
            (false, "Pending/ContainerCreating".to_string()),
            (false, "2/3 ready".to_string()),
            (true, "Running".to_string()),
            (true, "3/3 ready".to_string()),
        ],
        "exactly the state changes, in poll order"
    );
    // Three polls each: pending/ds, pod-ready/ds-repeat, pod-again/ds-ready.
    // The pod keeps being polled (last script entry repeats) while the
    // DaemonSet catches up; only its *changed* states were published.
    assert_eq!(api.polls(&obj_key(&pod)), 3);
    assert_eq!(api.polls(&obj_key(&ds)), 3);
}

#[tokio::test]
async fn unchanged_states_are_re_emitted_as_heartbeat() {
    let pod = POD();
    let api = ScriptedApi::new().serve(&pod, "pod-pending.json");
    let bus = EventBus::new("op-1");
    let mut rx = bus.subscribe();
    // Heartbeat every 25 ms inside a 120 ms budget: several heartbeats,
    // but the state never changes.
    let err = track(&api, &[pod], &fast(120, 5, 25), &bus)
        .await
        .expect_err("pod never becomes ready");
    let events = resource_events(&drain(&mut rx));
    assert!(
        (2..=15).contains(&events.len()),
        "initial event plus heartbeats, got {}",
        events.len()
    );
    for (ready, status) in &events {
        assert!(!*ready);
        assert_eq!(status, "Pending/ContainerCreating");
    }
    assert!(err.to_string().contains("readiness timeout"), "{err}");
}

#[tokio::test]
async fn transient_api_errors_are_tolerated_until_the_object_appears() {
    let crd = reference(
        "apiextensions.k8s.io/v1",
        "CustomResourceDefinition",
        None,
        "installations.operator.tigera.io",
    );
    // First two polls fail (anchor restarting), then the CRD shows up
    // already Established.
    let api = ScriptedApi::new()
        .fail_first(&crd, 2)
        .serve(&crd, "crd-established.json");
    let bus = EventBus::new("op-1");
    let mut rx = bus.subscribe();
    track(&api, &[crd], &fast(2_000, 2, 10_000), &bus)
        .await
        .expect("transient errors do not abort the wait");

    // Only one api-error event: the second failing poll did not change
    // the state, so change-only emission stayed quiet through it.
    let events = resource_events(&drain(&mut rx));
    assert_eq!(
        events,
        vec![
            (
                false,
                "api error: connection refused (scripted)".to_string()
            ),
            (true, "Established=True".to_string()),
        ]
    );
}

#[tokio::test]
async fn missing_object_surfaces_in_the_timeout_error() {
    let ds = DS();
    let api = ScriptedApi::new();
    let bus = EventBus::new("op-1");
    let mut rx = bus.subscribe();
    let err = track(&api, &[ds], &fast(30, 5, 10_000), &bus)
        .await
        .expect_err("object never exists");
    assert!(
        err.to_string().contains("readiness timeout after 30ms"),
        "{err}"
    );
    assert!(
        err.to_string()
            .contains("DaemonSet kube-flannel/kube-flannel-ds: not found"),
        "{err}"
    );
    assert_eq!(
        resource_events(&drain(&mut rx)),
        vec![(false, "not found".to_string())]
    );
}

// ── failure states and diagnostics ───────────────────────────────────

#[tokio::test]
async fn image_pull_failure_lists_events_and_names_the_cause() {
    let pod = POD();
    let api = ScriptedApi::new()
        .serve(&pod, "pod-imagepull.json")
        .list("Event", "default", "events-imagepull.json")
        .fail_logs(
            "default",
            "web-5d8f7b6c4-x2v9p",
            "web",
            "container \"web\" in pod \"web-5d8f7b6c4-x2v9p\" is waiting to start: trying and failing to pull image",
        );
    let bus = EventBus::new("op-1");
    let mut rx = bus.subscribe();
    let err = track(&api, &[pod], &fast(2_000, 2, 10_000), &bus)
        .await
        .expect_err("image pull failure is a failed state, no timeout needed");

    let text = format!("{err:#}");
    assert!(
        text.starts_with(
            "resource failure: image pull failure: registry.k8s.io/pause:does-not-exist"
        ),
        "{text}"
    );
    assert!(
        text.contains("check the registry mirror configuration"),
        "{text}"
    );
    assert!(
        text.contains("still not ready: Pod default/web-5d8f7b6c4-x2v9p: Pending/ImagePullBackOff"),
        "{text}"
    );
    insta::assert_snapshot!(text);

    let lines = warn_lines(&drain(&mut rx));
    // The fixture carries 25 events for this pod (+1 unrelated): exactly
    // the last 20 are published, newest last, with counts.
    let events: Vec<&String> = lines.iter().filter(|l| l.starts_with("event ")).collect();
    assert_eq!(events.len(), 20, "{lines:?}");
    assert!(
        events[0].contains("event default/web-5d8f7b6c4-x2v9p: ImagePullBackOff:"),
        "{events:?}"
    );
    assert!(
        events[0].contains("(x4)"),
        "oldest of the last 20: {events:?}"
    );
    assert!(events[19].contains("(x23)"), "newest: {events:?}");
    // The container log fetch was attempted with a 50-line tail and its
    // failure is visible.
    let log_calls = api.log_calls();
    assert_eq!(
        log_calls,
        vec![("web-5d8f7b6c4-x2v9p".to_string(), "web".to_string(), 50)]
    );
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("logs default/web-5d8f7b6c4-x2v9p/web: could not fetch logs")),
        "{lines:?}"
    );
}

#[tokio::test]
async fn timeout_diagnoses_the_related_pods_and_tails_their_logs() {
    let deploy = reference("apps/v1", "Deployment", Some("default"), "web");
    let api = ScriptedApi::new()
        .serve(&deploy, "deployment-not-ready.json")
        .list("Pod", "default", "pods-crashloop.json")
        .list("Event", "default", "events-crashloop.json")
        .logs(
            "default",
            "web-5d8f7b6c4-x2v9p",
            "web",
            "logs-crashloop.json",
        );
    let bus = EventBus::new("op-1");
    let mut rx = bus.subscribe();
    let err = track(&api, &[deploy], &fast(60, 5, 10_000), &bus)
        .await
        .expect_err("deployment never converges");

    let text = format!("{err:#}");
    assert!(text.starts_with("readiness timeout after 60ms"), "{text}");
    assert!(
        text.contains("container web in pod default/web-5d8f7b6c4-x2v9p is crash-looping"),
        "{text}"
    );
    assert!(
        text.contains("still not ready: Deployment default/web: 1/2 updated, 1/2 available"),
        "{text}"
    );
    insta::assert_snapshot!(text);

    // The related pods were listed through the deployment selector.
    assert_eq!(
        api.list_calls(),
        vec![
            (
                "".to_string(),
                "Pod".to_string(),
                Some("default".to_string()),
                Some("app=web".to_string())
            ),
            (
                "".to_string(),
                "Event".to_string(),
                Some("default".to_string()),
                None
            ),
        ]
    );

    let lines = warn_lines(&drain(&mut rx));
    // Pod events, chronological order.
    let events: Vec<&String> = lines.iter().filter(|l| l.starts_with("event ")).collect();
    assert_eq!(
        events
            .iter()
            .map(|l| l.contains("reason") as usize)
            .sum::<usize>(),
        0,
        "format check"
    );
    assert_eq!(events.len(), 3, "{lines:?}");
    assert!(events[0].contains("Created container web"), "{events:?}");
    assert!(
        events[1].contains("Back-off restarting failed container (x5)"),
        "{events:?}"
    );
    assert!(
        events[2].contains("Unhealthy: Startup probe failed"),
        "{events:?}"
    );
    // Exactly the last 50 of 60 log lines, prefixed with the container.
    let logs: Vec<&String> = lines.iter().filter(|l| l.starts_with("logs ")).collect();
    assert_eq!(logs.len(), 50, "{lines:?}");
    assert!(
        logs.iter()
            .all(|l| l.starts_with("logs default/web-5d8f7b6c4-x2v9p/web: nginx:")),
        "{logs:?}"
    );
}

#[tokio::test]
async fn failed_job_stops_the_wait_immediately() {
    let job = reference("batch/v1", "Job", Some("default"), "migrate");
    let api = ScriptedApi::new().serve(&job, "job-failed.json");
    let bus = EventBus::new("op-1");
    let mut rx = bus.subscribe();
    let err = track(&api, &[job], &fast(2_000, 2, 10_000), &bus)
        .await
        .expect_err("a Failed job condition is terminal");

    let text = format!("{err:#}");
    insta::assert_snapshot!(text);
    assert!(
        text.contains("job default/migrate failed: BackoffLimitExceeded: Job has reached the specified backoff limit"),
        "{text}"
    );
    // No pods to diagnose (the fixture carries no pod selector): only
    // the Resource event made it to the bus.
    assert_eq!(
        resource_events(&drain(&mut rx)),
        vec![(false, "0/1 succeeded, 5 failed".to_string())]
    );
}

#[tokio::test]
async fn diagnostic_text_snapshot_of_the_full_warn_stream() {
    // The same crashloop timeout as above, this time asserting the whole
    // warn log stream (events + log tail) as one snapshot — the exact
    // text an operator reads when a wait gets stuck.
    let deploy = reference("apps/v1", "Deployment", Some("default"), "web");
    let api = ScriptedApi::new()
        .serve(&deploy, "deployment-not-ready.json")
        .list("Pod", "default", "pods-crashloop.json")
        .list("Event", "default", "events-crashloop.json")
        .logs(
            "default",
            "web-5d8f7b6c4-x2v9p",
            "web",
            "logs-crashloop.json",
        );
    let bus = EventBus::new("op-1");
    let mut rx = bus.subscribe();
    let err = track(&api, &[deploy], &fast(60, 5, 10_000), &bus)
        .await
        .expect_err("deployment never converges");
    let mut text = format!("error: {err:#}\n");
    for line in warn_lines(&drain(&mut rx)) {
        text.push_str(&line);
        text.push('\n');
    }
    insta::assert_snapshot!(text);
}
