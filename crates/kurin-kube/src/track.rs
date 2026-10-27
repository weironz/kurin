//! Resource readiness tracking (WP-18): nelm-style convergence tracking
//! with automatic stuck diagnostics.
//!
//! [`track`] polls a fixed set of object references until every one of
//! them satisfies its readiness rule, publishing [`Body::Resource`]
//! events on state change (plus a 30 s heartbeat so long waits stay
//! visible). Every Kubernetes built-in kurin installs has a rule here —
//! Deployment, StatefulSet, DaemonSet, Job, Pod, PVC, CRD, APIService,
//! Node — and unknown kinds fall back to the `status.conditions[type =
//! Ready|Available]` heuristic.
//!
//! When tracking times out, or an object reaches a failed state
//! (`CrashLoopBackOff`, `ImagePullBackOff`, `ErrImagePull`, a failed
//! Job), the tracker lists the related pods' last 20 events and fetches
//! the problem container's last 50 log lines, publishes them as
//! `Log{level: warn}` and returns an error naming the most likely cause
//! (for example an image pull failure, with the registry mirror hint).
//!
//! Transient API errors during a poll do not abort the loop — the old
//! shell `until kubectl wait …` loops retried through them too; the
//! error text lands in the object's `Resource` status instead, so a
//! persistent failure is visible in the timeout diagnostics.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use kurin_core::events::{Body, EventBus, Level, Stream};
use miette::Result;
use serde_json::Value;

use crate::{ClusterApi, Gvk, LogOpts, ObjRef};

/// Default poll interval (design: 02 §3.11).
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(2);
/// Re-emit unchanged `Resource` states this often, so long waits keep a
/// pulse in the log stream.
pub const DEFAULT_HEARTBEAT: Duration = Duration::from_secs(30);
/// Container wait reasons that mean the object is stuck, not merely slow.
pub const FAILURE_WAIT_REASONS: [&str; 3] =
    ["CrashLoopBackOff", "ImagePullBackOff", "ErrImagePull"];
/// How many events per related pod the diagnostics list ("last 20").
const DIAGNOSTIC_EVENTS: usize = 20;
/// How many log lines per problem container the diagnostics fetch ("last 50").
const DIAGNOSTIC_TAIL_LINES: i64 = 50;

/// Knobs for one [`track`] call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackOpts {
    /// Give up (and diagnose) after this long.
    pub timeout: Duration,
    /// Poll interval.
    pub interval: Duration,
    /// Re-emit unchanged states at least this often.
    pub heartbeat: Duration,
}

impl TrackOpts {
    /// Options with the design defaults: 2 s interval, 30 s heartbeat.
    pub fn new(timeout: Duration) -> Self {
        Self {
            timeout,
            interval: DEFAULT_INTERVAL,
            heartbeat: DEFAULT_HEARTBEAT,
        }
    }
}

/// Readiness of one object: the verdict plus a short human status
/// (what `kubectl get` would hint at, one line).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceState {
    pub ready: bool,
    pub status: String,
}

impl ResourceState {
    fn not_ready(status: impl Into<String>) -> Self {
        Self {
            ready: false,
            status: status.into(),
        }
    }
}

/// One tracked object's final observed state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackedObject {
    /// The reference as passed to [`track`].
    pub reference: ObjRef,
    /// Last observed state (all ready on success).
    pub state: ResourceState,
}

/// What [`track`] observed when it returned successfully.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackResult {
    /// One entry per input reference, in input order.
    pub objects: Vec<TrackedObject>,
}

/// Track `refs` until every object satisfies its readiness rule.
///
/// Emits [`Body::Resource`] events on state change (first observation
/// counts as a change) plus at least one heartbeat per
/// [`TrackOpts::heartbeat`]. On timeout or a failed state, runs the
/// stuck diagnostics (related pod events + problem container logs as
/// `Log{level: warn}`) and returns an error naming the most likely
/// cause. An empty `refs` returns immediately.
pub async fn track(
    api: &dyn ClusterApi,
    refs: &[ObjRef],
    opts: &TrackOpts,
    bus: &EventBus,
) -> Result<TrackResult> {
    if refs.is_empty() {
        return Ok(TrackResult {
            objects: Vec::new(),
        });
    }
    let start = Instant::now();
    let mut last_emitted: HashMap<ObjRef, ResourceState> = HashMap::new();
    let mut last_beat = start;
    loop {
        let mut objects = Vec::with_capacity(refs.len());
        let mut pending: Vec<PendingObject> = Vec::new();
        let mut failures = Vec::new();
        let mut all_ready = true;
        for r in refs {
            let (state, object) = match fetch(api, r).await {
                Fetched::Missing => (ResourceState::not_ready("not found"), None),
                Fetched::Error(err) => {
                    (ResourceState::not_ready(format!("api error: {err}")), None)
                }
                Fetched::Object(obj) => {
                    let state = evaluate(r, &obj);
                    let object = if state.ready { None } else { Some(obj) };
                    (state, object)
                }
            };
            if !state.ready {
                all_ready = false;
                pending.push(PendingObject {
                    reference: r.clone(),
                    object: object.clone(),
                    state: state.clone(),
                });
                if let Some(obj) = &object {
                    failures.extend(detect_failures(r, obj));
                }
            }
            objects.push(TrackedObject {
                reference: r.clone(),
                state,
            });
        }

        // Emit on change, plus the heartbeat so a long wait stays visible.
        let beat_due = last_beat.elapsed() >= opts.heartbeat;
        for tracked in &objects {
            let changed = last_emitted.get(&tracked.reference) != Some(&tracked.state);
            if changed || beat_due {
                bus.publish(Body::Resource {
                    kind: tracked.reference.kind.clone(),
                    namespace: tracked.reference.namespace.clone(),
                    name: tracked.reference.name.clone(),
                    ready: tracked.state.ready,
                    status: tracked.state.status.clone(),
                });
                last_emitted.insert(tracked.reference.clone(), tracked.state.clone());
            }
        }
        if beat_due {
            last_beat = Instant::now();
        }

        if all_ready {
            return Ok(TrackResult { objects });
        }
        let elapsed = start.elapsed();
        if !failures.is_empty() || elapsed >= opts.timeout {
            let summary = diagnose(api, &pending, bus).await;
            return Err(finish_error(&failures, &pending, &opts.timeout, &summary));
        }
        tokio::time::sleep(opts.interval).await;
    }
}

/// Evaluate one object's readiness from its live JSON.
///
/// Built-in rules for the kinds kurin installs; everything else falls
/// back to the `status.conditions[type = Ready|Available]` heuristic.
/// Exposed so single-shot `Task::check` implementations share the exact
/// rule the tracker waits on.
pub fn evaluate(r: &ObjRef, obj: &Value) -> ResourceState {
    match (group(&r.api_version), r.kind.as_str()) {
        ("apps", "Deployment") => deployment_state(obj),
        ("apps", "StatefulSet") => stateful_set_state(obj),
        ("apps", "DaemonSet") => daemon_set_state(obj),
        ("batch", "Job") => job_state(obj),
        ("", "Pod") => pod_state(obj),
        ("", "PersistentVolumeClaim") => pvc_state(obj),
        ("apiextensions.k8s.io", "CustomResourceDefinition") => condition_state(obj, "Established"),
        ("apiregistration.k8s.io", "APIService") => condition_state(obj, "Available"),
        ("", "Node") => node_state(obj),
        _ => heuristic_state(obj),
    }
}

/// Wait reasons / job conditions worth aborting the wait for, with the
/// details the diagnostics and the cause sentence need.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Failure {
    ImagePull {
        pod: String,
        namespace: Option<String>,
        container: String,
        image: String,
        reason: &'static str,
    },
    CrashLoop {
        pod: String,
        namespace: Option<String>,
        container: String,
        image: String,
    },
    JobFailed {
        namespace: Option<String>,
        name: String,
        reason: Option<String>,
        message: Option<String>,
    },
}

/// One not-yet-ready object plus its last JSON (for pod discovery).
struct PendingObject {
    reference: ObjRef,
    object: Option<Value>,
    state: ResourceState,
}

/// How one poll saw a reference.
enum Fetched {
    Missing,
    Error(String),
    Object(Value),
}

async fn fetch(api: &dyn ClusterApi, r: &ObjRef) -> Fetched {
    match api.get_json(r).await {
        Ok(Some(obj)) => Fetched::Object(obj),
        Ok(None) => Fetched::Missing,
        Err(err) => Fetched::Error(first_line(&format!("{err:#}")).to_string()),
    }
}

fn first_line(text: &str) -> &str {
    text.lines().find(|l| !l.trim().is_empty()).unwrap_or("")
}

// ── built-in readiness rules ──────────────────────────────────────────

fn deployment_state(obj: &Value) -> ResourceState {
    let replicas = uint(obj, "/spec/replicas").unwrap_or(1);
    let updated = uint(obj, "/status/updatedReplicas").unwrap_or(0);
    let available = uint(obj, "/status/availableReplicas").unwrap_or(0);
    let observed = uint(obj, "/status/observedGeneration").unwrap_or(0);
    let generation = uint(obj, "/metadata/generation").unwrap_or(0);
    let ready =
        updated == replicas && available == replicas && observed >= generation && generation > 0;
    ResourceState {
        ready,
        status: format!("{updated}/{replicas} updated, {available}/{replicas} available"),
    }
}

fn stateful_set_state(obj: &Value) -> ResourceState {
    let replicas = uint(obj, "/spec/replicas").unwrap_or(1);
    let ready_replicas = uint(obj, "/status/readyReplicas").unwrap_or(0);
    let updated = uint(obj, "/status/updatedReplicas").unwrap_or(0);
    let observed = uint(obj, "/status/observedGeneration").unwrap_or(0);
    let generation = uint(obj, "/metadata/generation").unwrap_or(0);
    let ready = ready_replicas == replicas
        && updated == replicas
        && observed >= generation
        && generation > 0;
    ResourceState {
        ready,
        status: format!("{ready_replicas}/{replicas} ready, {updated}/{replicas} updated"),
    }
}

fn daemon_set_state(obj: &Value) -> ResourceState {
    let desired = uint(obj, "/status/desiredNumberScheduled").unwrap_or(0);
    let ready = uint(obj, "/status/numberReady").unwrap_or(0);
    // Zero desired is treated as not ready, matching the previous
    // CNI polls (a fresh DaemonSet reports 0/0 before scheduling).
    ResourceState {
        ready: ready == desired && desired > 0,
        status: format!("{ready}/{desired} ready"),
    }
}

fn job_state(obj: &Value) -> ResourceState {
    let completions = uint(obj, "/spec/completions").unwrap_or(1);
    let succeeded = uint(obj, "/status/succeeded").unwrap_or(0);
    let failed = uint(obj, "/status/failed").unwrap_or(0);
    let mut status = format!("{succeeded}/{completions} succeeded");
    if failed > 0 {
        status.push_str(&format!(", {failed} failed"));
    }
    ResourceState {
        ready: succeeded >= completions,
        status,
    }
}

fn pod_state(obj: &Value) -> ResourceState {
    let phase = str_at(obj, "/status/phase").unwrap_or("Unknown");
    let ready = condition_true(obj, "Ready");
    // Mirror `kubectl get pod`'s STATUS column: a waiting reason is more
    // informative than the phase.
    let mut status = phase.to_string();
    if let Some(reason) = pod_wait_reason(obj) {
        status = format!("{phase}/{reason}");
    }
    ResourceState { ready, status }
}

fn pvc_state(obj: &Value) -> ResourceState {
    let phase = str_at(obj, "/status/phase").unwrap_or("Pending");
    ResourceState {
        ready: phase == "Bound",
        status: phase.to_string(),
    }
}

fn node_state(obj: &Value) -> ResourceState {
    let ready = condition_true(obj, "Ready");
    ResourceState {
        ready,
        status: if ready { "Ready" } else { "NotReady" }.to_string(),
    }
}

/// CRD (`Established`) / APIService (`Available`): one make-or-break
/// condition whose type names the state.
fn condition_state(obj: &Value, type_: &str) -> ResourceState {
    match find_condition(obj, type_) {
        Some(condition) => {
            let status = condition
                .pointer("/status")
                .and_then(Value::as_str)
                .unwrap_or("Unknown");
            ResourceState {
                ready: status == "True",
                status: format!("{type_}={status}"),
            }
        }
        None => ResourceState::not_ready(format!("no {type_} condition")),
    }
}

/// Unknown kinds: ready when a `Ready` (or `Available`) condition is
/// `True`; objects without either condition never pass and surface
/// through the timeout diagnostics instead.
fn heuristic_state(obj: &Value) -> ResourceState {
    for type_ in ["Ready", "Available"] {
        if let Some(condition) = find_condition(obj, type_) {
            let status = condition
                .pointer("/status")
                .and_then(Value::as_str)
                .unwrap_or("Unknown");
            return ResourceState {
                ready: status == "True",
                status: format!("{type_}={status}"),
            };
        }
    }
    ResourceState::not_ready("no Ready/Available condition")
}

// ── failure detection ────────────────────────────────────────────────

/// Failed states worth stopping the wait immediately (WP-18 step 3).
fn detect_failures(r: &ObjRef, obj: &Value) -> Vec<Failure> {
    let mut failures = Vec::new();
    if r.kind == "Pod" {
        let namespace = str_at(obj, "/metadata/namespace").map(str::to_string);
        let pod = r.name.clone();
        for status in pod_container_statuses(obj) {
            if let Some(waiting) = status.pointer("/state/waiting") {
                let reason = waiting
                    .pointer("/reason")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                match reason {
                    "ImagePullBackOff" | "ErrImagePull" => failures.push(Failure::ImagePull {
                        pod: pod.clone(),
                        namespace: namespace.clone(),
                        container: name_of(status),
                        image: image_of(status),
                        reason: "ImagePullBackOff",
                    }),
                    "CrashLoopBackOff" => failures.push(Failure::CrashLoop {
                        pod: pod.clone(),
                        namespace: namespace.clone(),
                        container: name_of(status),
                        image: image_of(status),
                    }),
                    _ => {}
                }
            }
        }
    }
    if r.kind == "Job" && condition_true(obj, "Failed") {
        let condition = find_condition(obj, "Failed");
        failures.push(Failure::JobFailed {
            namespace: str_at(obj, "/metadata/namespace").map(str::to_string),
            name: r.name.clone(),
            reason: condition
                .and_then(|c| c.pointer("/reason"))
                .and_then(Value::as_str)
                .map(str::to_string),
            message: condition
                .and_then(|c| c.pointer("/message"))
                .and_then(Value::as_str)
                .map(str::to_string),
        });
    }
    failures
}

// ── stuck diagnostics ────────────────────────────────────────────────

/// List the related pods' events and the problem containers' log tails
/// as `Log{level: warn}`, and collect what they reveal.
///
/// Related pods: the pod itself, or for workload-ish objects the pods
/// matching `spec.selector.matchLabels`. Best effort — a diagnostics
/// fetch that fails is logged and skipped, so a broken API never masks
/// the original timeout.
async fn diagnose(api: &dyn ClusterApi, pending: &[PendingObject], bus: &EventBus) -> Vec<Failure> {
    let mut causes = Vec::new();
    for obj in pending {
        let pods = related_pods(api, &obj.reference, obj.object.as_ref(), bus).await;
        if pods.is_empty() {
            continue;
        }
        publish_pod_events(api, &pods, bus).await;
        for pod in &pods {
            causes.extend(publish_problem_container_logs(api, pod, bus).await);
        }
        // Failures detected from the pods we just listed (a workload in
        // trouble usually shows up in its pods, not its own status).
        for pod in &pods {
            let pod_ref = pod_ref_of(pod);
            causes.extend(detect_failures(&pod_ref, pod));
        }
    }
    causes
}

/// Pods related to one tracked reference.
async fn related_pods(
    api: &dyn ClusterApi,
    r: &ObjRef,
    obj: Option<&Value>,
    bus: &EventBus,
) -> Vec<Value> {
    if r.kind == "Pod" {
        return obj.cloned().into_iter().collect();
    }
    let Some(obj) = obj else {
        return Vec::new();
    };
    let Some(selector) = label_selector(obj) else {
        return Vec::new();
    };
    match api
        .list_json(
            &Gvk::core("v1", "Pod"),
            r.namespace.as_deref(),
            Some(&selector),
        )
        .await
    {
        Ok(pods) => pods,
        Err(err) => {
            bus_log(
                bus,
                &format!(
                    "could not list pods for {} {}: {err}",
                    r.kind,
                    qualified(&r.namespace, &r.name)
                ),
            );
            Vec::new()
        }
    }
}

/// `spec.selector.matchLabels` as a kubectl `-l` selector, when present.
fn label_selector(obj: &Value) -> Option<String> {
    let labels = obj.pointer("/spec/selector/matchLabels")?.as_object()?;
    if labels.is_empty() {
        return None;
    }
    let mut pairs: Vec<String> = labels
        .iter()
        .filter_map(|(k, v)| v.as_str().map(|v| format!("{k}={v}")))
        .collect();
    if pairs.is_empty() {
        return None;
    }
    pairs.sort();
    Some(pairs.join(","))
}

/// Publish the last [`DIAGNOSTIC_EVENTS`] events of every related pod.
async fn publish_pod_events(api: &dyn ClusterApi, pods: &[Value], bus: &EventBus) {
    for pod in pods {
        let (Some(name), namespace) = (str_at(pod, "/metadata/name"), pod_namespace(pod)) else {
            continue;
        };
        let events = match api
            .list_json(&Gvk::core("v1", "Event"), namespace.as_deref(), None)
            .await
        {
            Ok(events) => events,
            Err(err) => {
                bus_log(bus, &format!("could not list events of pod {name}: {err}"));
                continue;
            }
        };
        let mut matching: Vec<&Value> = events
            .iter()
            .filter(|e| event_targets_pod(e, pod))
            .collect();
        matching.sort_by_key(|e| event_time(e));
        for event in matching.iter().rev().take(DIAGNOSTIC_EVENTS).rev() {
            let reason = str_at(event, "/reason").unwrap_or("?");
            let message = str_at(event, "/message").unwrap_or_default();
            let count = uint(event, "/count").unwrap_or(1);
            let mut line = format!(
                "event {}{}: {reason}: {message}",
                namespace_prefix(namespace.as_deref()),
                name
            );
            if count > 1 {
                line.push_str(&format!(" (x{count})"));
            }
            bus_log(bus, &line);
        }
    }
}

/// Publish the last [`DIAGNOSTIC_TAIL_LINES`] log lines of the problem
/// containers (not-ready, waiting reasons first) of one pod.
async fn publish_problem_container_logs(
    api: &dyn ClusterApi,
    pod: &Value,
    bus: &EventBus,
) -> Vec<Failure> {
    let mut failures = Vec::new();
    let Some(pod_name) = str_at(pod, "/metadata/name") else {
        return failures;
    };
    let namespace = pod_namespace(pod);
    let statuses = pod_container_statuses(pod);
    let mut problem: Vec<&Value> = statuses
        .into_iter()
        .filter(|s| {
            !s.pointer("/ready")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        })
        .collect();
    if problem.is_empty() {
        return failures;
    }
    // Waiting-reason failures first: they carry the diagnosis.
    problem.sort_by_key(|s| {
        let waiting = s
            .pointer("/state/waiting/reason")
            .and_then(Value::as_str)
            .unwrap_or_default();
        !FAILURE_WAIT_REASONS.contains(&waiting)
    });
    for status in problem {
        let container = name_of(status);
        let pod_ref = ObjRef {
            api_version: "v1".into(),
            kind: "Pod".into(),
            name: pod_name.to_string(),
            namespace: namespace.clone(),
        };
        let prefix = format!(
            "logs {}{pod_name}/{container}: ",
            namespace_prefix(namespace.as_deref())
        );
        let opts = LogOpts {
            container: Some(container.clone()),
            tail_lines: Some(DIAGNOSTIC_TAIL_LINES),
            ..LogOpts::default()
        };
        let mut sink = |line: &str| {
            bus_log(bus, &format!("{prefix}{line}"));
        };
        if let Err(err) = api.pod_logs(&pod_ref, opts, &mut sink).await {
            bus_log(bus, &format!("{prefix}could not fetch logs: {err}"));
        }
        // Surface the waiting reason as a cause even when the fetch
        // failed: it is already the answer.
        if let Some(reason) = status
            .pointer("/state/waiting/reason")
            .and_then(Value::as_str)
        {
            match reason {
                "ImagePullBackOff" | "ErrImagePull" => failures.push(Failure::ImagePull {
                    pod: pod_name.to_string(),
                    namespace: namespace.clone(),
                    container,
                    image: image_of(status),
                    reason: "ImagePullBackOff",
                }),
                "CrashLoopBackOff" => failures.push(Failure::CrashLoop {
                    pod: pod_name.to_string(),
                    namespace: namespace.clone(),
                    container,
                    image: image_of(status),
                }),
                _ => {}
            }
        }
    }
    failures
}

/// Build the error text: the most likely cause first, then what was
/// still not ready.
fn finish_error(
    failures: &[Failure],
    pending: &[PendingObject],
    timeout: &Duration,
    diagnosed: &[Failure],
) -> miette::Report {
    let mut all: Vec<&Failure> = failures.iter().chain(diagnosed.iter()).collect();
    all.sort_by_key(|f| match f {
        Failure::ImagePull { .. } => 0,
        Failure::CrashLoop { .. } => 1,
        Failure::JobFailed { .. } => 2,
    });
    let mut causes: Vec<String> = Vec::new();
    let mut seen: Vec<&Failure> = Vec::new();
    for failure in &all {
        if seen.contains(failure) {
            continue;
        }
        seen.push(failure);
        causes.push(cause_sentence(failure));
    }
    let not_ready = pending
        .iter()
        .map(|p| {
            format!(
                "{} {}: {}",
                p.reference.kind,
                qualified(&p.reference.namespace, &p.reference.name),
                p.state.status
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    let trigger = if failures.is_empty() {
        format!("readiness timeout after {}", format_duration(*timeout))
    } else {
        "resource failure".to_string()
    };
    let mut message = if causes.is_empty() {
        format!("{trigger}: {not_ready}")
    } else {
        format!(
            "{trigger}: {}; still not ready: {not_ready}",
            causes.join("; ")
        )
    };
    if not_ready.is_empty() {
        message = format!("{trigger}: {}", causes.join("; "));
    }
    miette::miette!("{message}")
}

/// One failure rendered as the "most likely cause" sentence.
fn cause_sentence(failure: &Failure) -> String {
    match failure {
        Failure::ImagePull {
            pod,
            namespace,
            container,
            image,
            ..
        } => format!(
            "image pull failure: {image} (pod {}, container {container}) — \
             check the registry mirror configuration and that the image reference exists upstream",
            qualified(namespace, pod)
        ),
        Failure::CrashLoop {
            pod,
            namespace,
            container,
            ..
        } => format!(
            "container {container} in pod {} is crash-looping — check its logs above",
            qualified(namespace, pod)
        ),
        Failure::JobFailed {
            namespace,
            name,
            reason,
            message,
        } => {
            let mut out = format!("job {} failed", qualified(namespace, name));
            if let Some(reason) = reason {
                out.push_str(&format!(": {reason}"));
            }
            if let Some(message) = message {
                out.push_str(&format!(": {message}"));
            }
            out
        }
    }
}

// ── JSON helpers ─────────────────────────────────────────────────────

fn uint(obj: &Value, pointer: &str) -> Option<u64> {
    obj.pointer(pointer).and_then(Value::as_u64)
}

fn str_at<'a>(obj: &'a Value, pointer: &str) -> Option<&'a str> {
    obj.pointer(pointer).and_then(Value::as_str)
}

fn group(api_version: &str) -> &str {
    match api_version.split_once('/') {
        Some((group, _)) => group,
        None => "",
    }
}

fn find_condition<'a>(obj: &'a Value, type_: &str) -> Option<&'a Value> {
    obj.pointer("/status/conditions")?
        .as_array()?
        .iter()
        .find(|c| c.pointer("/type").and_then(Value::as_str) == Some(type_))
}

fn condition_true(obj: &Value, type_: &str) -> bool {
    find_condition(obj, type_)
        .and_then(|c| c.pointer("/status"))
        .and_then(Value::as_str)
        == Some("True")
}

/// `spec.containerStatuses` + `spec.initContainerStatuses`-style lists
/// (they live under `/status`), in kubelet order.
fn pod_container_statuses(pod: &Value) -> Vec<&Value> {
    let mut out = Vec::new();
    for pointer in ["/status/initContainerStatuses", "/status/containerStatuses"] {
        if let Some(list) = pod.pointer(pointer).and_then(Value::as_array) {
            out.extend(list.iter());
        }
    }
    out
}

fn name_of(status: &Value) -> String {
    str_at(status, "/name").unwrap_or_default().to_string()
}

fn image_of(status: &Value) -> String {
    str_at(status, "/image").unwrap_or_default().to_string()
}

fn pod_wait_reason(pod: &Value) -> Option<&str> {
    pod_container_statuses(pod)
        .iter()
        .find_map(|s| s.pointer("/state/waiting/reason").and_then(Value::as_str))
}

fn pod_namespace(pod: &Value) -> Option<String> {
    str_at(pod, "/metadata/namespace").map(str::to_string)
}

fn pod_ref_of(pod: &Value) -> ObjRef {
    ObjRef {
        api_version: "v1".into(),
        kind: "Pod".into(),
        name: str_at(pod, "/metadata/name")
            .unwrap_or_default()
            .to_string(),
        namespace: pod_namespace(pod),
    }
}

/// Does this event belong to that pod (by uid when present, else by
/// kind + name + namespace)?
fn event_targets_pod(event: &Value, pod: &Value) -> bool {
    let involved = event.pointer("/involvedObject");
    let uid_matches = match (
        involved
            .and_then(|i| i.pointer("/uid"))
            .and_then(Value::as_str),
        str_at(pod, "/metadata/uid"),
    ) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    };
    uid_matches
        || (involved
            .and_then(|i| i.pointer("/kind"))
            .and_then(Value::as_str)
            == Some("Pod")
            && involved
                .and_then(|i| i.pointer("/name"))
                .and_then(Value::as_str)
                == str_at(pod, "/metadata/name"))
}

/// Sort key for events: `lastTimestamp` falling back to
/// `firstTimestamp`, then `eventTime`.
fn event_time(event: &Value) -> String {
    ["/lastTimestamp", "/firstTimestamp", "/eventTime"]
        .iter()
        .find_map(|p| str_at(event, p))
        .unwrap_or_default()
        .to_string()
}

fn namespace_prefix(namespace: Option<&str>) -> String {
    namespace.map(|ns| format!("{ns}/")).unwrap_or_default()
}

fn qualified(namespace: &Option<String>, name: &str) -> String {
    namespace
        .as_deref()
        .map(|ns| format!("{ns}/{name}"))
        .unwrap_or_else(|| name.to_string())
}

fn bus_log(bus: &EventBus, line: &str) {
    bus.publish(Body::Log {
        level: Level::Warn,
        node: None,
        task: None,
        stream: Stream::Kurin,
        line: line.to_string(),
    });
}

fn format_duration(d: Duration) -> String {
    if d.subsec_millis() == 0 {
        format!("{}s", d.as_secs())
    } else {
        format!("{d:?}")
    }
}
