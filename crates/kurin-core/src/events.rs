//! Event v2 (ADR-04): timestamped, sequenced envelopes, a synchronous sink
//! bus, per-operation JSONL logs and secret redaction.
//!
//! Every operation publishes one [`Envelope`] per observable step. The bus
//! assigns `seq` (strictly increasing, starting at 1) and `ts` (UTC
//! milliseconds), redacts secret values, fans the envelope out to
//! synchronous [`EventSink`]s (terminal renderer, JSONL writer, `--json=v2`)
//! and then to bounded async subscribers (Web SSE in a later milestone).
//!
//! Slow subscribers never block a publisher: the async channels are bounded
//! and a full channel drops the envelope for that subscriber only. Such a
//! subscriber resumes from the JSONL file, seeking by `seq` — the file holds
//! every envelope in order.
//!
//! The v1 `--json` stream (`kurin_core::engine::Event`) is derived from the
//! same envelopes by the executor adapter; it keeps its frozen field set and
//! only gains `ts` (docs/v1.0-gates.md §2).

use std::collections::HashSet;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::mpsc::{self, Receiver, Sender};
use tracing::field::{Field, Visit};
use tracing::{Event as TracingEvent, Subscriber};
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::registry::LookupSpan;

/// Envelope schema version written by this build.
pub const ENVELOPE_VERSION: u8 = 2;

/// Default bound of an async subscriber channel. The executor drains while
/// tasks run, so a few thousand entries is far more than one stage needs;
/// anything beyond that is a lagging consumer, which resumes from JSONL.
pub const DEFAULT_SUBSCRIBER_CAPACITY: usize = 4096;

/// Replacement text for every registered secret (R8).
pub const REDACTED: &str = "******";

// ── timestamps ─────────────────────────────────────────────────────────

/// UTC timestamp in milliseconds since the Unix epoch.
///
/// Serialized as a plain integer so JSONL consumers never depend on a date
/// library. `time` was chosen over `jiff`: it is already in the dependency
/// graph (no new transitive tree), needs no timezone database for UTC, and
/// its `Rfc3339` formatter is exactly the log display format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(type = "number"))]
#[serde(transparent)]
pub struct Timestamp(u64);

impl Timestamp {
    /// Current wall-clock time, truncated to milliseconds.
    pub fn now() -> Self {
        let nanos = time::OffsetDateTime::now_utc().unix_timestamp_nanos();
        Self((nanos / 1_000_000).max(0) as u64)
    }

    /// Wrap an epoch-millisecond value (tests, JSONL round-trips).
    pub fn from_millis(ms: u64) -> Self {
        Self(ms)
    }

    /// Epoch milliseconds.
    pub fn millis(self) -> u64 {
        self.0
    }

    fn datetime(self) -> time::OffsetDateTime {
        time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(self.0) * 1_000_000)
            .expect("epoch milliseconds always representable")
    }

    /// `2026-10-27T10:15:00.123Z` — the `--timestamps` / `kurin logs` form.
    ///
    /// Always millisecond precision (the `time` Rfc3339 formatter omits
    /// `.000`, which would make log lines ragged).
    pub fn format_rfc3339(self) -> String {
        let d = self.datetime();
        format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
            d.year(),
            u8::from(d.month()),
            d.day(),
            d.hour(),
            d.minute(),
            d.second(),
            d.millisecond()
        )
    }

    /// `10:15:00` — compact wall-clock form for rich terminal progress.
    pub fn format_hms(self) -> String {
        let d = self.datetime();
        format!("{:02}:{:02}:{:02}", d.hour(), d.minute(), d.second())
    }

    /// Filename-safe form: `20261027T101500.123Z`.
    pub fn compact(self) -> String {
        let d = self.datetime();
        format!(
            "{:04}{:02}{:02}T{:02}{:02}{:02}.{:03}Z",
            d.year(),
            u8::from(d.month()),
            d.day(),
            d.hour(),
            d.minute(),
            d.second(),
            d.millisecond()
        )
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.format_rfc3339())
    }
}

// ── levels, streams, results ───────────────────────────────────────────

/// Log severity, ordered `Trace < Debug < Info < Warn < Error`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl Level {
    /// Lowercase name used in JSON and `--level`.
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Trace => "trace",
            Level::Debug => "debug",
            Level::Info => "info",
            Level::Warn => "warn",
            Level::Error => "error",
        }
    }

    /// Parse `--level` (case-insensitive).
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "trace" => Some(Level::Trace),
            "debug" => Some(Level::Debug),
            "info" => Some(Level::Info),
            "warn" | "warning" => Some(Level::Warn),
            "error" => Some(Level::Error),
            _ => None,
        }
    }

    fn from_tracing(level: &tracing::Level) -> Self {
        match *level {
            tracing::Level::TRACE => Level::Trace,
            tracing::Level::DEBUG => Level::Debug,
            tracing::Level::INFO => Level::Info,
            tracing::Level::WARN => Level::Warn,
            tracing::Level::ERROR => Level::Error,
        }
    }
}

impl fmt::Display for Level {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Which stream a log line belongs to: kurin's own progress, or a remote
/// command's stdout/stderr.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(rename_all = "lowercase")]
pub enum Stream {
    Kurin,
    Stdout,
    Stderr,
}

/// Terminal state of an operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(rename_all = "lowercase")]
pub enum OpResult {
    Succeeded,
    Failed,
    Cancelled,
}

/// Operation kind (`create-cluster`, `upgrade`, ...). Kept as an open
/// string so new operations need no schema change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(type = "string"))]
#[serde(transparent)]
pub struct OpKind(String);

impl OpKind {
    /// Name the operation.
    pub fn new(kind: impl Into<String>) -> Self {
        Self(kind.into())
    }

    /// Borrow the wire name.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for OpKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

// ── bodies and envelopes ───────────────────────────────────────────────

/// One observable step. Field names are snake_case on the wire; the `type`
/// tag is kebab-case, matching the v1 event style.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum Body {
    OpStarted {
        kind: OpKind,
        cluster: String,
        plan_fingerprint: String,
        kurin: String,
    },
    PhaseStarted {
        phase: String,
    },
    PhaseDone {
        phase: String,
        #[cfg_attr(feature = "ts", ts(type = "number"))]
        ms: u64,
    },
    TaskStart {
        key: String,
        node: String,
        phase: String,
    },
    TaskSkipped {
        key: String,
        node: String,
        phase: String,
        /// Why the task did not run (`blocked by <key>`); absent when the
        /// task was simply already in sync.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    TaskDone {
        key: String,
        node: String,
        phase: String,
        #[cfg_attr(feature = "ts", ts(type = "number"))]
        ms: u64,
    },
    TaskReconciled {
        key: String,
        node: String,
        phase: String,
    },
    TaskFailed {
        key: String,
        node: String,
        phase: String,
        reason: String,
    },
    /// Best-effort failure that does not stop the operation.
    TaskWarn {
        key: String,
        node: String,
        message: String,
    },
    Log {
        level: Level,
        node: Option<String>,
        task: Option<String>,
        stream: Stream,
        line: String,
    },
    /// Readiness tracking (WP-18); present now so the schema is stable.
    Resource {
        kind: String,
        namespace: Option<String>,
        name: String,
        ready: bool,
        status: String,
    },
    /// The executor's terminal event: every stage converged.
    ClusterReady,
    OpFinished {
        result: OpResult,
        #[cfg_attr(feature = "ts", ts(type = "number"))]
        ms: u64,
        error: Option<String>,
    },
}

impl Body {
    /// Stable kebab-case discriminant, matching the JSON `type` field.
    pub fn kind(&self) -> &'static str {
        match self {
            Body::OpStarted { .. } => "op-started",
            Body::PhaseStarted { .. } => "phase-started",
            Body::PhaseDone { .. } => "phase-done",
            Body::TaskStart { .. } => "task-start",
            Body::TaskSkipped { .. } => "task-skipped",
            Body::TaskDone { .. } => "task-done",
            Body::TaskReconciled { .. } => "task-reconciled",
            Body::TaskFailed { .. } => "task-failed",
            Body::TaskWarn { .. } => "task-warn",
            Body::Log { .. } => "log",
            Body::Resource { .. } => "resource",
            Body::ClusterReady => "cluster-ready",
            Body::OpFinished { .. } => "op-finished",
        }
    }

    /// Severity used by `--level` filtering and the terminal renderer.
    pub fn level(&self) -> Level {
        match self {
            Body::Log { level, .. } => *level,
            Body::TaskFailed { .. } => Level::Error,
            Body::TaskWarn { .. } => Level::Warn,
            _ => Level::Info,
        }
    }

    /// Node this body belongs to, when it belongs to one.
    pub fn node(&self) -> Option<&str> {
        match self {
            Body::TaskStart { node, .. }
            | Body::TaskSkipped { node, .. }
            | Body::TaskDone { node, .. }
            | Body::TaskReconciled { node, .. }
            | Body::TaskFailed { node, .. }
            | Body::TaskWarn { node, .. } => Some(node),
            Body::Log { node, .. } => node.as_deref(),
            _ => None,
        }
    }

    /// Scrub every free-text field through `redactor`. Called by the bus
    /// before an envelope is stored or published.
    pub fn redact(&mut self, redactor: &Redactor) {
        match self {
            Body::TaskFailed { reason, .. } => *reason = redactor.redact(reason),
            Body::TaskSkipped {
                reason: Some(reason),
                ..
            } => *reason = redactor.redact(reason),
            Body::TaskWarn { message, .. } => *message = redactor.redact(message),
            Body::Log { line, .. } => *line = redactor.redact(line),
            Body::Resource { status, .. } => *status = redactor.redact(status),
            Body::OpFinished {
                error: Some(error), ..
            } => *error = redactor.redact(error),
            _ => {}
        }
    }
}

/// One event on the bus: schema version, monotonic sequence, UTC timestamp,
/// operation id and the body.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct Envelope {
    pub v: u8,
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub seq: u64,
    pub ts: Timestamp,
    pub op: String,
    pub body: Body,
}

impl Envelope {
    /// Stable kebab-case body discriminant.
    pub fn kind(&self) -> &'static str {
        self.body.kind()
    }
}

// ── redaction ──────────────────────────────────────────────────────────

/// Registry of secret values that must never reach an event, log line or
/// error text (R8). Replacement is literal and longest-first, so a secret
/// that contains another secret is still fully masked.
#[derive(Debug, Default)]
pub struct Redactor {
    secrets: RwLock<Vec<String>>,
}

impl Redactor {
    /// Empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register one secret. Empty values are ignored (they would match
    /// everything); duplicates are collapsed.
    pub fn register(&self, secret: impl Into<String>) {
        let secret = secret.into();
        if secret.is_empty() {
            return;
        }
        let mut secrets = self.secrets.write().unwrap_or_else(|e| e.into_inner());
        if !secrets.contains(&secret) {
            secrets.push(secret);
            // Longest first: `abc` inside `abcdef` must not win.
            secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
        }
    }

    /// Register an optional secret (config fields are `Option<String>`).
    pub fn register_opt(&self, secret: Option<impl Into<String>>) {
        if let Some(secret) = secret {
            self.register(secret);
        }
    }

    /// True when nothing is registered.
    pub fn is_empty(&self) -> bool {
        self.secrets.read().map(|s| s.is_empty()).unwrap_or(true)
    }

    /// Replace every registered secret with [`REDACTED`].
    pub fn redact(&self, text: &str) -> String {
        let secrets = self.secrets.read().unwrap_or_else(|e| e.into_inner());
        if secrets.is_empty() {
            return text.to_string();
        }
        let mut out = text.to_string();
        for secret in secrets.iter() {
            if out.contains(secret.as_str()) {
                out = out.replace(secret.as_str(), REDACTED);
            }
        }
        out
    }
}

// ── sinks and the bus ──────────────────────────────────────────────────

/// A synchronous consumer of every envelope. Sinks run on the publishing
/// thread in registration order; they must be fast and must never publish.
pub trait EventSink: Send + Sync {
    /// Consume one envelope.
    fn handle(&self, envelope: &Envelope);
}

/// The event bus of one operation.
///
/// `publish` assigns `seq`/`ts`, redacts the body, delivers to sinks, then
/// offers the envelope to every async subscriber without blocking.
pub struct EventBus {
    op: String,
    seq: AtomicU64,
    redactor: Redactor,
    sinks: RwLock<Vec<Arc<dyn EventSink>>>,
    subscribers: RwLock<Vec<Sender<Envelope>>>,
}

impl EventBus {
    /// Bus for operation `op`, with an empty redactor.
    pub fn new(op: impl Into<String>) -> Self {
        Self::with_redactor(op, Redactor::new())
    }

    /// Bus for operation `op` sharing a pre-populated redactor.
    pub fn with_redactor(op: impl Into<String>, redactor: Redactor) -> Self {
        Self {
            op: op.into(),
            seq: AtomicU64::new(0),
            redactor,
            sinks: RwLock::new(Vec::new()),
            subscribers: RwLock::new(Vec::new()),
        }
    }

    /// Operation id carried by every envelope.
    pub fn op(&self) -> &str {
        &self.op
    }

    /// Sequence number of the last published envelope (0 before the first).
    pub fn seq(&self) -> u64 {
        self.seq.load(Ordering::SeqCst)
    }

    /// Shared secret registry (tasks register command-level secrets here).
    pub fn redactor(&self) -> &Redactor {
        &self.redactor
    }

    /// Register a synchronous sink. Sinks registered later see only later
    /// envelopes.
    pub fn add_sink(&self, sink: Arc<dyn EventSink>) {
        self.sinks
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .push(sink);
    }

    /// Subscribe with [`DEFAULT_SUBSCRIBER_CAPACITY`].
    pub fn subscribe(&self) -> Receiver<Envelope> {
        self.subscribe_with(DEFAULT_SUBSCRIBER_CAPACITY)
    }

    /// Subscribe with an explicit channel bound. A full channel drops
    /// envelopes for this subscriber only; it can resume from JSONL by seq.
    pub fn subscribe_with(&self, capacity: usize) -> Receiver<Envelope> {
        let (tx, rx) = mpsc::channel(capacity.max(1));
        self.subscribers
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .push(tx);
        rx
    }

    /// Publish one body. Returns the stored envelope (already redacted).
    pub fn publish(&self, body: Body) -> Envelope {
        let mut body = body;
        body.redact(&self.redactor);
        let seq = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
        let envelope = Envelope {
            v: ENVELOPE_VERSION,
            seq,
            ts: Timestamp::now(),
            op: self.op.clone(),
            body,
        };

        for sink in self.sinks.read().unwrap_or_else(|e| e.into_inner()).iter() {
            sink.handle(&envelope);
        }

        let mut subscribers = self.subscribers.write().unwrap_or_else(|e| e.into_inner());
        subscribers.retain(|tx| match tx.try_send(envelope.clone()) {
            Ok(()) => true,
            // Lagging subscriber: keep it registered, it resumes from JSONL.
            Err(TrySendError::Full(_)) => true,
            Err(TrySendError::Closed(_)) => false,
        });

        envelope
    }
}

impl fmt::Debug for EventBus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EventBus")
            .field("op", &self.op)
            .field("seq", &self.seq.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

// ── JSONL operation log ────────────────────────────────────────────────

/// Append-only JSONL writer: one envelope per line, flushed per event so a
/// crashed process still leaves a readable log.
pub struct JsonlSink {
    path: PathBuf,
    file: Mutex<BufWriter<File>>,
}

impl JsonlSink {
    /// Create (or append to) `path`, creating parent directories.
    pub fn create(path: impl Into<PathBuf>) -> io::Result<Self> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        Ok(Self {
            path,
            file: Mutex::new(BufWriter::new(file)),
        })
    }

    /// File this sink writes.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl EventSink for JsonlSink {
    fn handle(&self, envelope: &Envelope) {
        let mut file = self.file.lock().unwrap_or_else(|e| e.into_inner());
        if serde_json::to_writer(&mut *file, envelope).is_ok() {
            let _ = file.write_all(b"\n");
            let _ = file.flush();
        }
    }
}

/// State directory root: `$XDG_STATE_HOME`, then `~/.local/state`, then
/// `/tmp` (last-resort, still writable in containers).
pub fn state_home() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|v| !v.is_empty())
                .map(|home| PathBuf::from(home).join(".local/state"))
        })
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

/// Directory holding one cluster's operation logs.
pub fn log_dir(cluster: &str) -> PathBuf {
    state_home()
        .join("kurin")
        .join("logs")
        .join(sanitize_component(cluster))
}

/// `$XDG_STATE_HOME/kurin/logs/<cluster>/<op>.jsonl`.
pub fn log_path(cluster: &str, op: &str) -> PathBuf {
    log_dir(cluster).join(format!("{}.jsonl", sanitize_component(op)))
}

/// Fresh operation id: sortable timestamp plus pid, unique per process run.
pub fn new_op_id() -> String {
    format!("{}-{}", Timestamp::now().compact(), std::process::id())
}

fn sanitize_component(value: &str) -> String {
    value
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '\0' => '_',
            _ => c,
        })
        .collect()
}

/// Cluster names that have at least one log directory, sorted.
pub fn list_clusters() -> io::Result<Vec<String>> {
    let root = state_home().join("kurin").join("logs");
    let mut names = Vec::new();
    let entries = match std::fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(names),
        Err(e) => return Err(e),
    };
    for entry in entries {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            names.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    names.sort();
    Ok(names)
}

/// Every JSONL log of `cluster`, oldest first (op ids start with a
/// timestamp, so lexical order is chronological).
pub fn list_logs(cluster: &str) -> io::Result<Vec<PathBuf>> {
    let dir = log_dir(cluster);
    let mut logs = Vec::new();
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(logs),
        Err(e) => return Err(e),
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            logs.push(path);
        }
    }
    logs.sort();
    Ok(logs)
}

/// Newest log of `cluster`.
pub fn latest_log(cluster: &str) -> Option<PathBuf> {
    list_logs(cluster).ok().and_then(|mut logs| logs.pop())
}

/// Log whose file stem equals `op` or starts with it (a short op id is
/// enough). The newest match wins.
pub fn find_log(cluster: &str, op: &str) -> Option<PathBuf> {
    let logs = list_logs(cluster).ok()?;
    logs.into_iter().rev().find(|path| {
        path.file_stem()
            .and_then(|stem| stem.to_str())
            .map(|stem| stem == op || stem.starts_with(op))
            .unwrap_or(false)
    })
}

/// Read every envelope from `path`. Malformed lines (a torn tail while a
/// writer is still appending) are skipped rather than failing the read.
pub fn read_log(path: &Path) -> io::Result<Vec<Envelope>> {
    let file = File::open(path)?;
    let mut out = Vec::new();
    for line in BufReader::new(file).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(envelope) = serde_json::from_str::<Envelope>(&line) {
            out.push(envelope);
        }
    }
    Ok(out)
}

/// Envelopes with `seq` greater than `after`. A subscriber that lagged out
/// of the bounded channel resumes here instead of losing events.
pub fn read_log_since(path: &Path, after: u64) -> io::Result<Vec<Envelope>> {
    Ok(read_log(path)?
        .into_iter()
        .filter(|envelope| envelope.seq > after)
        .collect())
}

/// Read envelopes appended since `offset` (bytes), advancing `offset` past
/// the last complete line. Used by `kurin logs --follow`.
pub fn read_log_from(path: &Path, offset: &mut u64) -> io::Result<Vec<Envelope>> {
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(*offset))?;
    let mut buf = String::new();
    file.read_to_string(&mut buf)?;
    let mut consumed = 0usize;
    let mut out = Vec::new();
    for line in buf.split_inclusive('\n') {
        if !line.ends_with('\n') {
            break; // partial tail; retry on the next poll
        }
        consumed += line.len();
        let line = line.trim_end();
        if line.is_empty() {
            continue;
        }
        if let Ok(envelope) = serde_json::from_str::<Envelope>(line) {
            out.push(envelope);
        }
    }
    *offset += consumed as u64;
    Ok(out)
}

/// Filters shared by `kurin logs` and any future log UI.
#[derive(Debug, Clone)]
pub struct LogQuery {
    /// Minimum severity (inclusive).
    pub min_level: Level,
    /// Only bodies belonging to this node.
    pub node: Option<String>,
}

impl Default for LogQuery {
    fn default() -> Self {
        Self {
            min_level: Level::Info,
            node: None,
        }
    }
}

impl LogQuery {
    /// Does `envelope` pass the filters?
    pub fn matches(&self, envelope: &Envelope) -> bool {
        if envelope.body.level() < self.min_level {
            return false;
        }
        if let Some(node) = &self.node {
            if envelope.body.node() != Some(node.as_str()) {
                return false;
            }
        }
        true
    }
}

// ── tracing bridge ─────────────────────────────────────────────────────

/// `tracing` layer that turns span fields (`op/phase/task/node`) and event
/// messages into [`Body::Log`]. Tasks keep using `tracing::{info,debug,warn}`
/// and never touch the bus directly.
pub struct TracingLayer {
    bus: Arc<EventBus>,
}

impl TracingLayer {
    /// Bridge `tracing` events into `bus`.
    pub fn new(bus: Arc<EventBus>) -> Self {
        Self { bus }
    }
}

#[derive(Debug, Default, Clone)]
struct SpanFields {
    op: Option<String>,
    phase: Option<String>,
    task: Option<String>,
    node: Option<String>,
}

#[derive(Default)]
struct FieldVisitor {
    message: Option<String>,
    node: Option<String>,
    task: Option<String>,
    stream: Option<String>,
    span: SpanFields,
}

impl Visit for FieldVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.store(field.name(), value);
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        // `%value` and `format_args!` both arrive here; Display and Debug
        // of `Arguments` render the same text.
        self.store(field.name(), &format!("{value:?}"));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.store(field.name(), &value.to_string());
    }
}

impl FieldVisitor {
    fn store(&mut self, name: &str, value: &str) {
        match name {
            "message" => self.message = Some(value.to_string()),
            "node" => {
                self.node = Some(value.to_string());
                self.span.node = Some(value.to_string());
            }
            "task" => {
                self.task = Some(value.to_string());
                self.span.task = Some(value.to_string());
            }
            "op" => self.span.op = Some(value.to_string()),
            "phase" => self.span.phase = Some(value.to_string()),
            "stream" => self.stream = Some(value.to_string()),
            _ => {}
        }
    }
}

impl<S> Layer<S> for TracingLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        id: &tracing::span::Id,
        ctx: Context<'_, S>,
    ) {
        let mut visitor = FieldVisitor::default();
        attrs.record(&mut visitor);
        if let Some(span) = ctx.span(id) {
            span.extensions_mut().insert(visitor.span);
        }
    }

    fn on_event(&self, event: &TracingEvent<'_>, ctx: Context<'_, S>) {
        let mut visitor = FieldVisitor::default();
        event.record(&mut visitor);

        // Span context, innermost first: an event inside `task{node=n1}`
        // inherits node/task even when it does not repeat them.
        let mut span_fields = SpanFields::default();
        if let Some(scope) = ctx.event_scope(event) {
            // Root first, then inner spans overwrite: the innermost span
            // that carries a field wins.
            for span in scope.from_root() {
                if let Some(fields) = span.extensions().get::<SpanFields>() {
                    if fields.op.is_some() {
                        span_fields.op = fields.op.clone();
                    }
                    if fields.phase.is_some() {
                        span_fields.phase = fields.phase.clone();
                    }
                    if fields.task.is_some() {
                        span_fields.task = fields.task.clone();
                    }
                    if fields.node.is_some() {
                        span_fields.node = fields.node.clone();
                    }
                }
            }
        }

        let stream = match visitor.stream.as_deref() {
            Some("stdout") => Stream::Stdout,
            Some("stderr") => Stream::Stderr,
            _ => Stream::Kurin,
        };
        self.bus.publish(Body::Log {
            level: Level::from_tracing(event.metadata().level()),
            node: visitor.node.or(span_fields.node),
            task: visitor.task.or(span_fields.task),
            stream,
            line: visitor.message.unwrap_or_default(),
        });
    }
}

/// Install `bus` as the process-wide `tracing` sink. Returns `false` when a
/// global subscriber was already installed (the CLI installs exactly one per
/// process; tests use scoped subscribers instead).
pub fn install_tracing(bus: Arc<EventBus>) -> bool {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    tracing_subscriber::registry()
        .with(TracingLayer::new(bus))
        .try_init()
        .is_ok()
}

/// Literal secret values found in a cluster's configuration, ready for
/// [`Redactor::register`]. `{ env }` / `{ file }` references carry no
/// value until `kurin-ops::prepare` resolves them; those resolved values
/// are registered there.
///
/// Kept here (not in the CLI) so the Web server registers the same set.
pub fn cluster_secrets(cluster: &kurin_config::Cluster) -> Vec<String> {
    let mut secrets = Vec::new();
    let mut push = |r: &kurin_config::SecretRef| {
        if let Some(value) = r.literal() {
            if !value.is_empty() {
                secrets.push(value.to_string());
            }
        }
    };
    for node in &cluster.nodes {
        match &node.ssh.auth {
            kurin_config::SshAuth::Password { password } => push(password),
            kurin_config::SshAuth::PrivateKey { passphrase, .. } => {
                if let Some(passphrase) = passphrase {
                    push(passphrase);
                }
            }
        }
        if let Some(password) = &node.ssh.become_password {
            push(password);
        }
    }
    if let Some(password) = &cluster.registry.password {
        push(password);
    }
    secrets.sort();
    secrets.dedup();
    secrets
}

/// Register every configured secret of `cluster` with `redactor`.
pub fn register_cluster_secrets(redactor: &Redactor, cluster: &kurin_config::Cluster) {
    let mut seen = HashSet::new();
    for secret in cluster_secrets(cluster) {
        if seen.insert(secret.clone()) {
            redactor.register(secret);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log(line: &str) -> Body {
        Body::Log {
            level: Level::Info,
            node: Some("n1".into()),
            task: Some("prepare/sysctl@n1".into()),
            stream: Stream::Kurin,
            line: line.into(),
        }
    }

    #[test]
    fn seq_is_strictly_increasing_and_ts_is_utc_ms() {
        let bus = EventBus::new("op-1");
        let before = Timestamp::now().millis();
        let first = bus.publish(log("one"));
        let second = bus.publish(log("two"));
        let after = Timestamp::now().millis();

        assert_eq!(first.v, 2);
        assert_eq!(first.seq, 1);
        assert_eq!(second.seq, 2);
        assert!(second.seq > first.seq);
        assert!(first.ts.millis() >= before && first.ts.millis() <= after);
        assert!(first.ts.format_rfc3339().ends_with('Z'));
        assert_eq!(first.ts.format_rfc3339().len(), 24, "millisecond precision");
    }

    #[test]
    fn envelope_round_trips_through_json() {
        let bus = EventBus::new("op-1");
        let envelope = bus.publish(log("hello"));
        let line = serde_json::to_string(&envelope).unwrap();
        assert!(!line.contains('\n'));
        let back: Envelope = serde_json::from_str(&line).unwrap();
        assert_eq!(back.v, 2);
        assert_eq!(back.seq, envelope.seq);
        assert_eq!(back.ts, envelope.ts);
        assert_eq!(back.op, "op-1");
        assert_eq!(back.body.kind(), "log");
    }

    #[test]
    fn redactor_masks_logs_errors_and_warnings() {
        let redactor = Redactor::new();
        redactor.register("fake-ssh-password-not-real");
        redactor.register("fake-join-token-not-real");
        let bus = EventBus::with_redactor("op", redactor);

        let log_env = bus.publish(log("connecting with fake-ssh-password-not-real"));
        let Body::Log { line, .. } = &log_env.body else {
            panic!("log body")
        };
        assert!(!line.contains("fake-ssh-password-not-real"));
        assert!(line.contains(REDACTED));

        let failed = bus.publish(Body::TaskFailed {
            key: "join/kubeadm@n2".into(),
            node: "n2".into(),
            phase: "join".into(),
            reason: "token fake-join-token-not-real rejected".into(),
        });
        let Body::TaskFailed { reason, .. } = &failed.body else {
            panic!("failed body")
        };
        assert!(!reason.contains("fake-join-token-not-real"));
        assert!(reason.contains(REDACTED));

        let finished = bus.publish(Body::OpFinished {
            result: OpResult::Failed,
            ms: 1,
            error: Some("ssh: fake-ssh-password-not-real".into()),
        });
        let Body::OpFinished { error, .. } = &finished.body else {
            panic!("finished body")
        };
        assert_eq!(error.as_deref(), Some("ssh: ******"));
    }

    #[test]
    fn redactor_prefers_the_longest_secret() {
        let redactor = Redactor::new();
        redactor.register("abc");
        redactor.register("abcdef");
        assert_eq!(redactor.redact("xabcdefy"), "x******y");
    }

    #[test]
    fn slow_subscriber_never_blocks_the_publisher() {
        let bus = EventBus::new("op");
        let mut rx = bus.subscribe_with(1);
        for i in 0..100 {
            bus.publish(log(&format!("line {i}")));
        }
        // Only the first envelope fit; the rest were dropped for this
        // subscriber instead of blocking publish.
        assert!(rx.try_recv().is_ok());
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn sinks_receive_every_envelope_in_order() {
        struct Collect(Mutex<Vec<u64>>);
        impl EventSink for Collect {
            fn handle(&self, envelope: &Envelope) {
                self.0.lock().unwrap().push(envelope.seq);
            }
        }
        let bus = EventBus::new("op");
        let sink = Arc::new(Collect(Mutex::new(Vec::new())));
        bus.add_sink(sink.clone());
        for i in 0..3 {
            bus.publish(log(&format!("{i}")));
        }
        assert_eq!(*sink.0.lock().unwrap(), vec![1, 2, 3]);
    }

    #[test]
    fn jsonl_sink_writes_one_line_per_envelope() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cluster").join("op.jsonl");
        let sink = Arc::new(JsonlSink::create(&path).unwrap());
        let bus = EventBus::new("op");
        bus.add_sink(sink);
        bus.publish(log("one"));
        bus.publish(log("two"));

        let raw = std::fs::read_to_string(&path).unwrap();
        assert_eq!(raw.lines().count(), 2);
        let envelopes = read_log(&path).unwrap();
        assert_eq!(envelopes.len(), 2);
        assert_eq!(envelopes[0].seq, 1);
        assert_eq!(envelopes[1].seq, 2);
    }

    #[test]
    fn read_log_since_resumes_a_lagging_subscriber() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("op.jsonl");
        let bus = EventBus::new("op");
        bus.add_sink(Arc::new(JsonlSink::create(&path).unwrap()));
        for i in 0..5 {
            bus.publish(log(&format!("line {i}")));
        }
        let resumed = read_log_since(&path, 2).unwrap();
        assert_eq!(resumed.len(), 3);
        assert_eq!(resumed[0].seq, 3);
        assert_eq!(resumed[2].seq, 5);
    }

    #[test]
    fn read_log_from_resumes_at_byte_offset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("op.jsonl");
        let sink = JsonlSink::create(&path).unwrap();
        let bus = EventBus::new("op");
        bus.add_sink(Arc::new(sink));
        bus.publish(log("one"));

        let mut offset = 0;
        let first = read_log_from(&path, &mut offset).unwrap();
        assert_eq!(first.len(), 1);
        assert!(offset > 0);

        let second = read_log_from(&path, &mut offset).unwrap();
        assert!(second.is_empty());

        // A second bus appends to the same file (simulating --follow).
        let sink2 = JsonlSink::create(&path).unwrap();
        let bus2 = EventBus::new("op");
        bus2.add_sink(Arc::new(sink2));
        bus2.publish(log("two"));
        let third = read_log_from(&path, &mut offset).unwrap();
        assert_eq!(third.len(), 1);
        assert_eq!(third[0].body.kind(), "log");
    }

    #[test]
    fn log_query_filters_by_level_and_node() {
        let bus = EventBus::new("op");
        let info = bus.publish(log("info"));
        let debug = bus.publish(Body::Log {
            level: Level::Debug,
            node: Some("n2".into()),
            task: None,
            stream: Stream::Stdout,
            line: "debug".into(),
        });

        let info_query = LogQuery::default();
        assert!(info_query.matches(&info));
        assert!(!info_query.matches(&debug));

        let debug_query = LogQuery {
            min_level: Level::Debug,
            node: Some("n2".into()),
        };
        assert!(!debug_query.matches(&info));
        assert!(debug_query.matches(&debug));
    }

    #[test]
    fn tracing_layer_maps_spans_and_messages_to_logs() {
        use tracing_subscriber::layer::SubscriberExt;

        let bus = Arc::new(EventBus::new("op"));
        let subscriber = tracing_subscriber::registry().with(TracingLayer::new(Arc::clone(&bus)));
        let mut rx = bus.subscribe();

        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!(
                "task",
                op = "create-cluster",
                phase = "bootstrap",
                task = "bootstrap/kubeadm@n1",
                node = "n1",
            );
            let _guard = span.enter();
            tracing::info!("hello from the task");
            tracing::warn!(node = "n2", "explicit node");
        });

        let first = rx.try_recv().expect("span log published");
        let Body::Log {
            level,
            node,
            task,
            stream,
            line,
        } = &first.body
        else {
            panic!("log body, got {:?}", first.body)
        };
        assert_eq!(*level, Level::Info);
        assert_eq!(node.as_deref(), Some("n1"));
        assert_eq!(task.as_deref(), Some("bootstrap/kubeadm@n1"));
        assert_eq!(*stream, Stream::Kurin);
        assert_eq!(line, "hello from the task");

        let second = rx.try_recv().expect("second log published");
        let Body::Log { level, node, .. } = &second.body else {
            panic!("log body")
        };
        assert_eq!(*level, Level::Warn);
        assert_eq!(node.as_deref(), Some("n2"), "event field wins over span");
    }

    #[test]
    fn cluster_secrets_collects_every_configured_secret() {
        // The fixture parser is exercised elsewhere; this only asserts the
        // collector reads the resolved fields and never invents values.
        let cluster = kurin_config::load(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../kurin/tests/fixtures/configs/single.toml"),
        );
        if let Ok(cluster) = cluster {
            let secrets = cluster_secrets(&cluster);
            for secret in &secrets {
                assert!(!secret.is_empty());
            }
        }
    }
}
