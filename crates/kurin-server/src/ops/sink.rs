//! Persistence sinks attached to every server-owned operation bus.
//!
//! UI §6.7 v1.1 splits the stream: structured bodies go to `op_events`,
//! raw log lines go to `<data>/logs/<cluster>/<op>.jsonl` in the CLI's
//! envelope format. Both keep the bus `seq`, so SSE can merge them.
//!
//! Sinks run on the executor's publishing thread. They never publish and
//! never call `tracing` (that would feed the router and loop); failures are
//! reported on stderr.

use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use kurin_core::events::{Body, Envelope, EventSink, JsonlSink};

use crate::store::Store;

/// How often the DB copy of `operations.log_lines` is refreshed.
const LOG_LINES_FLUSH: u64 = 100;

/// Writes structured events to `op_events` and log lines to JSONL.
pub struct EventStoreSink {
    store: Store,
    op_id: String,
    path: PathBuf,
    log: Option<JsonlSink>,
    lines: AtomicU64,
}

impl EventStoreSink {
    /// Create the JSONL file (parent dirs included) and the sink.
    pub fn create(store: Store, op_id: String, path: PathBuf) -> Self {
        // Logs live under the 0700 data dir; create the directory and the
        // file 0600 before the core sink opens it in append mode.
        if let Some(parent) = path.parent() {
            let _ = std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(parent);
        }
        if !path.exists() {
            let _ = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .mode(0o600)
                .open(&path);
        }
        let log = match JsonlSink::create(&path) {
            Ok(sink) => Some(sink),
            Err(error) => {
                eprintln!("[serve] 无法创建操作日志 {}: {error}", path.display());
                None
            }
        };
        Self {
            store,
            op_id,
            path,
            log,
            lines: AtomicU64::new(0),
        }
    }

    /// JSONL path of this operation.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Lines written so far.
    pub fn lines(&self) -> i64 {
        self.lines.load(Ordering::Relaxed) as i64
    }

    /// Flush the line count to the operations row.
    pub fn flush_meta(&self) {
        if let Err(error) =
            self.store
                .set_log_meta(&self.op_id, &self.path.to_string_lossy(), self.lines())
        {
            eprintln!("[serve] 更新操作日志行数失败: {error}");
        }
    }
}

impl EventSink for EventStoreSink {
    fn handle(&self, envelope: &Envelope) {
        match &envelope.body {
            Body::Log { .. } => {
                let Some(log) = &self.log else {
                    return;
                };
                log.handle(envelope);
                let lines = self.lines.fetch_add(1, Ordering::Relaxed) + 1;
                if lines.is_multiple_of(LOG_LINES_FLUSH) {
                    self.flush_meta();
                }
            }
            _ => {
                let payload = match serde_json::to_string(&envelope.body) {
                    Ok(payload) => payload,
                    Err(error) => {
                        eprintln!("[serve] 序列化操作事件失败: {error}");
                        return;
                    }
                };
                if let Err(error) = self.store.append_event(
                    &self.op_id,
                    envelope.seq,
                    &envelope.ts.format_rfc3339(),
                    envelope.body.kind(),
                    &payload,
                ) {
                    eprintln!("[serve] 写入操作事件失败: {error}");
                }
            }
        }
    }
}

/// Records whether the operation already published its terminal event, so
/// the runner can add one only when the backend failed before publishing.
#[derive(Debug, Default)]
pub struct FinishWatcher {
    finished: AtomicBool,
}

impl FinishWatcher {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// True once an `OpFinished` body crossed the bus.
    pub fn finished(&self) -> bool {
        self.finished.load(Ordering::SeqCst)
    }
}

impl EventSink for FinishWatcher {
    fn handle(&self, envelope: &Envelope) {
        if matches!(envelope.body, Body::OpFinished { .. }) {
            self.finished.store(true, Ordering::SeqCst);
        }
    }
}
