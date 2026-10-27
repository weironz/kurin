//! The operation runner (WP-52): submission, locking, the global
//! concurrency cap, background execution and the live event stream.
//!
//! Submission order follows UI §6.5/§6.7:
//!
//! 1. validate the kind and parameters;
//! 2. take the cluster `flock` (mutating kinds only);
//! 3. insert the `queued` row (the partial unique index is the DB guard);
//! 4. attach the persistence sink and register the bus/cancellation token;
//! 5. spawn the task, which waits for a global semaphore permit and then
//!    calls the backend.
//!
//! The lock is acquired at submission, not when the task starts, so a
//! queued operation already excludes a concurrent CLI mutation — exactly
//! what the 409 contract promises.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use kurin_core::events::{Body, Envelope, EventBus, OpResult, Timestamp};
use kurin_core::lock::ClusterLock;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::error::ApiError;
use crate::ops::backend::{self, OpContext, OperationBackend};
use crate::ops::logs::{LogFilter, LogPage, LogStore};
use crate::ops::sink::{EventStoreSink, FinishWatcher};
use crate::ops::tracing::BusRegistry;
use crate::store::{
    is_read_only_kind, NewOperation, OperationFilter, OperationRecord, OperationState, Store,
    StoreError,
};

/// Default SSE subscriber channel bound (UI §6.8).
pub const DEFAULT_SSE_CAPACITY: usize = 1024;

/// A submission from the API layer.
#[derive(Debug, Clone)]
pub struct SubmitRequest {
    pub cluster: String,
    pub kind: String,
    pub params: Value,
    pub actor: String,
    pub retry_of: Option<String>,
}

/// Cloneable handle to the runner.
#[derive(Clone)]
pub struct OpRunner {
    inner: Arc<Inner>,
}

struct Inner {
    store: Store,
    backend: Arc<dyn OperationBackend>,
    registry: BusRegistry,
    cancels: RwLock<std::collections::HashMap<String, CancellationToken>>,
    sinks: RwLock<std::collections::HashMap<String, Arc<EventStoreSink>>>,
    permits: Semaphore,
    shutting_down: AtomicBool,
    sse_capacity: usize,
    logs: LogStore,
}

impl OpRunner {
    /// Runner with `max_concurrent` global permits and the default SSE
    /// channel bound.
    pub fn new(
        store: Store,
        backend: Arc<dyn OperationBackend>,
        max_concurrent: usize,
        registry: BusRegistry,
    ) -> Self {
        Self::with_sse_capacity(
            store,
            backend,
            max_concurrent,
            DEFAULT_SSE_CAPACITY,
            registry,
        )
    }

    /// Runner with an explicit SSE channel bound (tests use a tiny one to
    /// exercise the lag/disconnect path).
    pub fn with_sse_capacity(
        store: Store,
        backend: Arc<dyn OperationBackend>,
        max_concurrent: usize,
        sse_capacity: usize,
        registry: BusRegistry,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                store,
                backend,
                registry,
                cancels: RwLock::new(Default::default()),
                sinks: RwLock::new(Default::default()),
                permits: Semaphore::new(max_concurrent.max(1)),
                shutting_down: AtomicBool::new(false),
                sse_capacity: sse_capacity.max(1),
                logs: LogStore::new(),
            }),
        }
    }

    /// Startup recovery (UI §6.4): mark every `queued`/`running` operation
    /// left by a previous process as `interrupted`.
    pub fn recover_interrupted(&self) -> Result<Vec<String>, StoreError> {
        self.inner.store.interrupt_active_operations()
    }

    /// The bus registry, for the process-wide tracing router.
    pub fn registry(&self) -> BusRegistry {
        self.inner.registry.clone()
    }

    /// Submit an operation and spawn its background task.
    pub fn submit(&self, request: SubmitRequest) -> Result<OperationRecord, ApiError> {
        if self.inner.shutting_down.load(Ordering::SeqCst) {
            return Err(ApiError::ShuttingDown);
        }
        backend::validate_submission(&request.kind, &request.params)
            .map_err(ApiError::BadRequest)?;
        self.inner
            .store
            .get_cluster(&request.cluster)?
            .ok_or(ApiError::NotFound)?;
        let revision = self
            .inner
            .store
            .current_revision(&request.cluster)?
            .ok_or_else(|| {
                ApiError::Conflict(format!("集群 {:?} 还没有配置，请先导入", request.cluster))
            })?;

        let op_id = new_operation_id();
        let read_only = is_read_only_kind(&request.kind);
        let lock = if read_only {
            None
        } else {
            Some(
                ClusterLock::acquire_with(&request.cluster, &op_id)
                    .map_err(|e| ApiError::Conflict(format!("{e}")))?,
            )
        };

        let params = serde_json::to_string(&request.params).unwrap_or_else(|_| "{}".into());
        let new = NewOperation {
            id: op_id.clone(),
            cluster: Some(request.cluster.clone()),
            kind: request.kind.clone(),
            config_rev: Some(revision.rev),
            params,
            retry_of: request.retry_of,
            actor: request.actor,
        };
        let record = self.inner.store.insert_operation(new)?;

        let log_path = self.log_path(&request.cluster, &op_id);
        let sink = Arc::new(EventStoreSink::create(
            self.inner.store.clone(),
            op_id.clone(),
            log_path.clone(),
        ));
        let bus = Arc::new(EventBus::new(&op_id));
        bus.add_sink(sink.clone());
        let finish = FinishWatcher::new();
        bus.add_sink(Arc::clone(&finish) as Arc<dyn kurin_core::events::EventSink>);
        self.inner.registry.insert(&op_id, Arc::clone(&bus));
        let cancel = CancellationToken::new();
        self.inner
            .cancels
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(op_id.clone(), cancel.clone());
        self.inner
            .sinks
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(op_id.clone(), Arc::clone(&sink));
        if let Err(error) = self
            .inner
            .store
            .set_log_meta(&op_id, &log_path.to_string_lossy(), 0)
        {
            eprintln!("[serve] 记录操作日志路径失败: {error}");
        }

        let runner = self.clone();
        let task_op = record.clone();
        tokio::spawn(async move {
            runner
                .execute_task(task_op, bus, cancel, sink, finish, lock)
                .await;
        });
        Ok(record)
    }

    /// One operation, or 404.
    pub fn get(&self, id: &str) -> Result<OperationRecord, ApiError> {
        self.inner
            .store
            .get_operation(id)?
            .ok_or_else(|| StoreError::OperationNotFound(id.to_string()).into())
    }

    /// Operations matching `filter`.
    pub fn list(&self, filter: &OperationFilter) -> Result<Vec<OperationRecord>, ApiError> {
        Ok(self.inner.store.list_operations(filter)?)
    }

    /// Request cancellation. A queued operation is settled immediately; a
    /// running one stops at the executor's next stage boundary (UI §6.4).
    pub fn cancel(&self, id: &str) -> Result<OperationRecord, ApiError> {
        let operation = self.get(id)?;
        if operation.state.is_terminal() {
            return Ok(operation);
        }
        if let Some(cancel) = self
            .inner
            .cancels
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
        {
            cancel.cancel();
        }
        if operation.state == OperationState::Queued {
            if let Some(bus) = self.inner.registry.get(id) {
                self.settle_cancelled(id, &bus);
            }
        }
        self.get(id)
    }

    /// Re-run a terminal operation as a new row with `retry_of` pointing at
    /// the original. `create`/`add-nodes` retries resume (UI §6.4).
    pub fn retry(&self, id: &str, actor: String) -> Result<OperationRecord, ApiError> {
        let original = self.get(id)?;
        if !original.state.is_terminal() {
            return Err(ApiError::Conflict(format!("操作 {id} 尚未结束，不能重试")));
        }
        let cluster = original
            .cluster
            .clone()
            .ok_or_else(|| ApiError::BadRequest("该操作没有关联集群".into()))?;
        let mut params: Value = serde_json::from_str(&original.params)
            .unwrap_or_else(|_| Value::Object(Default::default()));
        if matches!(original.kind.as_str(), "create" | "add-nodes") {
            if !params.is_object() {
                params = Value::Object(Default::default());
            }
            params["resume"] = Value::Bool(true);
        }
        self.submit(SubmitRequest {
            cluster,
            kind: original.kind,
            params,
            actor,
            retry_of: Some(id.to_string()),
        })
    }

    /// Structured events plus JSONL log lines with `seq > after`, merged and
    /// deduplicated (SSE backfill).
    pub fn backfill(&self, operation: &OperationRecord, after: u64) -> Vec<Envelope> {
        let mut merged: BTreeMap<u64, Envelope> = BTreeMap::new();
        match self.inner.store.events_after(&operation.id, after, 100_000) {
            Ok(rows) => {
                for row in rows {
                    if let Some(envelope) = stored_envelope(operation, row) {
                        merged.insert(envelope.seq, envelope);
                    }
                }
            }
            Err(error) => eprintln!("[serve] 读取操作事件失败: {error}"),
        }
        if let Some(path) = &operation.log_path {
            match self.inner.logs.since(Path::new(path), after) {
                Ok(envelopes) => {
                    for envelope in envelopes {
                        merged.insert(envelope.seq, envelope);
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => eprintln!("[serve] 读取操作日志失败: {error}"),
            }
        }
        merged.into_values().collect()
    }

    /// Live event receiver for SSE. The returned stream ends when the
    /// operation finishes or the subscriber lagged (the client reconnects
    /// with `Last-Event-ID` and backfills).
    pub fn event_stream(&self, operation: OperationRecord, after: u64) -> mpsc::Receiver<Envelope> {
        let (tx, rx) = mpsc::channel(64);
        let runner = self.clone();
        tokio::spawn(async move {
            runner.pump_events(operation, after, tx).await;
        });
        rx
    }

    /// One page of the operation's JSONL log.
    pub fn log_page(
        &self,
        operation: &OperationRecord,
        after: u64,
        before: Option<u64>,
        limit: usize,
        filter: &LogFilter,
    ) -> Result<LogPage, ApiError> {
        let Some(path) = &operation.log_path else {
            return Ok(LogPage {
                items: Vec::new(),
                next_after: None,
                has_more: false,
            });
        };
        self.inner
            .logs
            .page(Path::new(path), after, before, limit, filter)
            .map_err(|e| ApiError::Internal(format!("读取操作日志失败：{e}")))
    }

    /// Graceful shutdown, phase 1: refuse new submissions, mark queued
    /// operations interrupted and ask running ones to stop at the next
    /// boundary.
    pub fn begin_shutdown(&self) {
        self.inner.shutting_down.store(true, Ordering::SeqCst);
        let filter = OperationFilter {
            limit: Some(1000),
            ..Default::default()
        };
        let active = self
            .inner
            .store
            .list_operations(&filter)
            .unwrap_or_default();
        for operation in active {
            match operation.state {
                OperationState::Queued => {
                    if let Ok(true) = self.inner.store.finish_operation(
                        &operation.id,
                        OperationState::Interrupted,
                        Some("服务正在关闭，排队中的操作未开始".into()),
                    ) {
                        if let Some(bus) = self.inner.registry.get(&operation.id) {
                            bus.publish(Body::OpFinished {
                                result: OpResult::Failed,
                                ms: 0,
                                error: Some("interrupted".into()),
                            });
                        }
                        // Wake the queued task so it drops the cluster lock.
                        if let Some(cancel) = self
                            .inner
                            .cancels
                            .read()
                            .unwrap_or_else(|e| e.into_inner())
                            .get(&operation.id)
                        {
                            cancel.cancel();
                        }
                    }
                }
                OperationState::Running => {
                    if let Some(cancel) = self
                        .inner
                        .cancels
                        .read()
                        .unwrap_or_else(|e| e.into_inner())
                        .get(&operation.id)
                    {
                        cancel.cancel();
                    }
                }
                _ => {}
            }
        }
    }

    /// Graceful shutdown, phase 2: give running operations a bounded grace
    /// period, then mark whatever is left interrupted (UI §6.4).
    pub async fn finish_shutdown(&self) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            let running = self
                .inner
                .store
                .list_operations(&OperationFilter {
                    state: Some(OperationState::Running),
                    limit: Some(1000),
                    ..Default::default()
                })
                .unwrap_or_default();
            if running.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        if let Err(error) = self.inner.store.interrupt_active_operations() {
            eprintln!("[serve] 标记中断操作失败: {error}");
        }
    }

    async fn execute_task(
        self,
        operation: OperationRecord,
        bus: Arc<EventBus>,
        cancel: CancellationToken,
        sink: Arc<EventStoreSink>,
        finish: Arc<FinishWatcher>,
        lock: Option<kurin_core::lock::ClusterLockGuard>,
    ) {
        let permit = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                self.settle_cancelled(&operation.id, &bus);
                return;
            }
            permit = self.inner.permits.acquire() => match permit {
                Ok(permit) => permit,
                Err(_) => return,
            },
        };
        if cancel.is_cancelled() {
            drop(permit);
            self.settle_cancelled(&operation.id, &bus);
            return;
        }
        if self.inner.shutting_down.load(Ordering::SeqCst) {
            drop(permit);
            if let Ok(true) = self.inner.store.finish_operation(
                &operation.id,
                OperationState::Interrupted,
                Some("服务正在关闭".into()),
            ) {
                bus.publish(Body::OpFinished {
                    result: OpResult::Failed,
                    ms: 0,
                    error: Some("interrupted".into()),
                });
            }
            return;
        }

        if let Err(error) = self.inner.store.mark_operation_running(&operation.id) {
            eprintln!("[serve] 标记操作运行失败: {error}");
        }
        let started = Instant::now();
        let context = OpContext {
            op_id: operation.id.clone(),
            cluster: operation
                .cluster
                .clone()
                .unwrap_or_else(|| operation.kind.clone()),
            kind: operation.kind.clone(),
            params: serde_json::from_str(&operation.params)
                .unwrap_or_else(|_| Value::Object(Default::default())),
            bus: Arc::clone(&bus),
            cancel: cancel.clone(),
            lock,
        };
        let execution = self.inner.backend.execute(context).await;

        // The real backend publishes `OpFinished` through `OpSession`; a
        // backend that failed before that point still gets a terminal event
        // so SSE closes instead of hanging.
        if !finish.finished() {
            bus.publish(Body::OpFinished {
                result: match execution.state {
                    OperationState::Cancelled => OpResult::Cancelled,
                    _ => OpResult::Failed,
                },
                ms: started.elapsed().as_millis() as u64,
                error: execution.error.clone(),
            });
        }
        if let Err(error) =
            self.inner
                .store
                .finish_operation(&operation.id, execution.state, execution.error)
        {
            eprintln!("[serve] 结束操作失败: {error}");
        }
        if let Some(result) = &execution.result {
            if let Ok(text) = serde_json::to_string(result) {
                if let Err(error) = self.inner.store.set_operation_result(&operation.id, &text) {
                    eprintln!("[serve] 记录操作结果失败: {error}");
                }
            }
        }
        sink.flush_meta();
        drop(permit);
    }

    fn settle_cancelled(&self, id: &str, bus: &EventBus) {
        match self
            .inner
            .store
            .finish_operation(id, OperationState::Cancelled, None)
        {
            Ok(true) => {
                bus.publish(Body::OpFinished {
                    result: OpResult::Cancelled,
                    ms: 0,
                    error: None,
                });
            }
            Ok(false) => {}
            Err(error) => eprintln!("[serve] 取消操作失败: {error}"),
        }
    }

    async fn pump_events(
        &self,
        operation: OperationRecord,
        after: u64,
        tx: mpsc::Sender<Envelope>,
    ) {
        let mut last = after;
        // Subscribe before backfilling: events published in between appear
        // in both sources and are dropped by the `seq <= last` check.
        let mut live = self
            .inner
            .registry
            .get(&operation.id)
            .map(|bus| bus.subscribe_with(self.inner.sse_capacity));

        for envelope in self.backfill(&operation, after) {
            if envelope.seq <= last {
                continue;
            }
            let finished = is_finished(&envelope);
            if tx.send(envelope.clone()).await.is_err() {
                return;
            }
            last = envelope.seq;
            if finished {
                return;
            }
        }
        if operation.state.is_terminal() {
            return;
        }
        let Some(live) = live.as_mut() else {
            return;
        };
        loop {
            match live.recv().await {
                Some(envelope) => {
                    if envelope.seq <= last {
                        continue;
                    }
                    // A gap means the bounded channel dropped events
                    // (`Lagged`): end the stream so the client reconnects
                    // with `Last-Event-ID` and backfills from disk.
                    if envelope.seq > last + 1 {
                        return;
                    }
                    let finished = is_finished(&envelope);
                    if tx.send(envelope).await.is_err() {
                        return;
                    }
                    last += 1;
                    if finished {
                        return;
                    }
                }
                None => return,
            }
        }
    }

    fn log_path(&self, cluster: &str, op_id: &str) -> PathBuf {
        self.inner
            .store
            .data_dir()
            .join("logs")
            .join(sanitize_component(cluster))
            .join(format!("{}.jsonl", sanitize_component(op_id)))
    }

    /// Sinks kept for the operation's lifetime; exposed for tests.
    pub fn sink(&self, id: &str) -> Option<Arc<EventStoreSink>> {
        self.inner
            .sinks
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .cloned()
    }
}

fn stored_envelope(
    operation: &OperationRecord,
    row: crate::store::StoredEvent,
) -> Option<Envelope> {
    let ts = time::OffsetDateTime::parse(&row.ts, &time::format_description::well_known::Rfc3339)
        .ok()?;
    let millis = (ts.unix_timestamp_nanos() / 1_000_000).max(0) as u64;
    let body: Body = serde_json::from_str(&row.payload).ok()?;
    Some(Envelope {
        v: 2,
        seq: row.seq,
        ts: Timestamp::from_millis(millis),
        op: operation.id.clone(),
        body,
    })
}

fn is_finished(envelope: &Envelope) -> bool {
    matches!(envelope.body, Body::OpFinished { .. })
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

/// Sortable operation id: UTC timestamp plus a random suffix. (UI §6.7 says
/// ULID; the timestamp prefix keeps list order stable without a new crate.)
pub fn new_operation_id() -> String {
    format!(
        "{}-{:08x}",
        Timestamp::now().compact(),
        rand::random::<u32>()
    )
}
#[cfg(test)]
mod tests {
    use super::*;
    use kurin_core::events::{Level, Stream};

    struct NoopBackend;

    #[async_trait::async_trait]
    impl OperationBackend for NoopBackend {
        async fn execute(&self, _ctx: OpContext) -> crate::ops::OpExecution {
            crate::ops::OpExecution {
                state: OperationState::Succeeded,
                error: None,
                result: None,
            }
        }
    }

    fn log(seq: u64) -> Body {
        let _ = seq;
        Body::Log {
            level: Level::Info,
            node: Some("n1".into()),
            task: None,
            stream: Stream::Kurin,
            line: "line".into(),
        }
    }

    /// A subscriber that falls behind the bounded bus channel must see the
    /// stream close on the first gap; the client then reconnects with its
    /// last id and backfills from disk (UI §6.8).
    #[tokio::test]
    async fn lagging_subscriber_disconnects_on_a_gap() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store
            .insert_operation(NewOperation {
                id: "op-lag".into(),
                cluster: None,
                kind: "create".into(),
                config_rev: Some(1),
                params: "{}".into(),
                retry_of: None,
                actor: "test".into(),
            })
            .unwrap();
        let runner = OpRunner::with_sse_capacity(
            store.clone(),
            Arc::new(NoopBackend),
            1,
            1,
            BusRegistry::new(),
        );
        let bus = Arc::new(EventBus::new("op-lag"));
        runner.registry().insert("op-lag", Arc::clone(&bus));
        let operation = store.get_operation("op-lag").unwrap().unwrap();
        let mut rx = runner.event_stream(operation, 0);
        // Let the pump subscribe before the burst starts.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        // Only seq 1 fits the bus channel; 2..=200 are dropped for this
        // subscriber while it is not being drained.
        for _ in 0..200 {
            bus.publish(log(0));
        }
        tokio::task::yield_now().await;
        // A later event exposes the gap.
        bus.publish(log(0));

        let mut seqs = Vec::new();
        while let Some(envelope) = rx.recv().await {
            seqs.push(envelope.seq);
        }
        assert_eq!(seqs, vec![1], "the stream ends at the first gap");
    }
}
