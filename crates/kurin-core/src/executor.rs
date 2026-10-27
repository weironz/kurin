//! Stage executor over the plan DAG. Stages serialize; tasks within a stage
//! parallelize across nodes. Every task follows check -> apply -> verify;
//! `Unknown` outcomes are re-probed, never blind-retried.
//!
//! Event flow: tasks publish [`Body`](crate::events::Body) envelopes to the
//! operation's [`EventBus`]; the executor subscribes and adapts them to the
//! frozen v1 [`Event`] stream (`--json` and the terminal) while the workers
//! run (`tokio::select!` between `JoinSet::join_next` and `rx.recv()`), so
//! events stream to the caller in near real time. Every task produces
//! exactly one terminal event after its `TaskStart`:
//! `TaskSkipped | TaskDone | TaskReconciled | TaskFailed`.
//!
//! Remote output: each task's session is wrapped in a [`LoggingSession`]
//! that publishes every `exec` line as `Body::Log { stream, level: Debug }`,
//! so `kurin logs -vv` can show `kubeadm init` output without any task
//! knowing about the bus.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use miette::Result;
use tokio::task::JoinSet;
use tracing::Instrument;

use crate::engine::{
    Cmd, Drift, Event, ExecOutput, LocalForward, Outcome, PlannedTask, RemoteForward, Session,
    Stage, TransportLost, UploadReport, UploadSource,
};
use crate::events::{Body, Envelope, EventBus, Level, Stream as EventStream};
use crate::plan::Plan;
use crate::run::RunContext;
use crate::state::RunState;

/// Session factory: tasks resolve their node's session by name.
#[async_trait::async_trait]
pub trait SessionPool: Send + Sync {
    async fn get(&self, node: &str) -> Result<Arc<dyn Session>>;
}

/// Run knobs. `cluster` + `state_file` enable resume persistence;
/// `resume` decides whether existing records are loaded (and kept) or
/// discarded before the run starts.
///
/// CLI wiring is the integration owner's job (main.rs is not owned by this
/// track):
/// ```text
/// TODO(integration): create cluster --json    => emit callback printing
/// TODO(integration):   `kurin::render::event_json_line(&event)` per line.
/// TODO(integration): create cluster --resume => RunOptions {
/// TODO(integration):   state_file: RunState::default_path(&cluster.name),
/// TODO(integration):   resume: true, cluster: cluster.name.clone() }
/// TODO(integration): SIGINT => RunState::flush (see state.rs module docs)
/// ```
#[derive(Debug, Clone, Default)]
pub struct RunOptions {
    /// Cluster name, recorded in the state file.
    pub cluster: String,
    /// Where to persist task-level done records; `None` disables persistence.
    pub state_file: Option<PathBuf>,
    /// Load (rather than reset) records from `state_file`.
    pub resume: bool,
    /// Operation id carried by the envelopes this run publishes. Empty when
    /// the caller uses [`run_with_bus`] with a bus that already has one.
    pub op_id: String,
}

impl RunOptions {
    /// Enable the default state file (`~/.cache/kurin/runs/<cluster>.json`).
    pub fn with_default_state(mut self, cluster: &str) -> Self {
        self.cluster = cluster.to_string();
        self.state_file = RunState::default_path(cluster);
        self
    }
}

/// Default run: no resume state, same signature the library exports.
pub async fn run(
    plan: &mut Plan,
    pool: &Arc<dyn SessionPool>,
    emit: &mut impl FnMut(Event),
) -> Result<()> {
    run_with(plan, pool, emit, RunOptions::default()).await
}

pub async fn run_with(
    plan: &mut Plan,
    pool: &Arc<dyn SessionPool>,
    emit: &mut impl FnMut(Event),
    opts: RunOptions,
) -> Result<()> {
    let run = Arc::new(RunContext::new(
        opts.op_id.clone(),
        opts.cluster.clone(),
        crate::run::ArtifactSources::embedded(),
    ));
    let bus = Arc::clone(&run.events);
    run_with_bus(plan, pool, &run, &bus, emit, opts).await
}

/// Run with a caller-owned bus. The CLI attaches the terminal renderer, the
/// JSONL writer and `--json=v2` sink to this bus before calling; library
/// callers use [`run_with`], which creates a bare bus.
pub async fn run_with_bus(
    plan: &mut Plan,
    pool: &Arc<dyn SessionPool>,
    run: &Arc<RunContext>,
    bus: &Arc<EventBus>,
    emit: &mut impl FnMut(Event),
    opts: RunOptions,
) -> Result<()> {
    let cluster = if opts.cluster.is_empty() {
        run.cluster.clone()
    } else {
        opts.cluster.clone()
    };
    let mut state = match opts.state_file {
        Some(path) => RunState::open(path, cluster, opts.resume)?,
        None => RunState::disabled(),
    };
    // One subscription for the whole run: stages are serial, and the
    // executor drains it while tasks run, so the bound is never reached in
    // practice (lagging consumers are the Web/SSE case, which resumes from
    // JSONL).
    let mut rx = bus.subscribe();

    for &idx in &plan.stages {
        if run.cancel.is_cancelled() {
            return Err(miette::miette!("operation cancelled"));
        }
        let node = &mut plan.graph[idx];
        let stage = node.stage;
        let phase = stage.label().to_string();
        let tasks = std::mem::take(&mut node.tasks);
        bus.publish(Body::PhaseStarted {
            phase: phase.clone(),
        });
        if tasks.is_empty() {
            bus.publish(Body::PhaseDone {
                phase: phase.clone(),
                ms: 0,
            });
            node.tasks = tasks;
            continue;
        }

        let started = Instant::now();
        let mut set: JoinSet<Result<()>> = JoinSet::new();
        for planned in tasks.iter().cloned() {
            set.spawn(execute_one(planned, Arc::clone(pool), Arc::clone(bus)));
        }

        // Fail-fast between stages, but in-flight siblings settle first:
        // every failure is collected, the first is propagated. Events are
        // drained concurrently so the v1 stream keeps its live ordering.
        let mut first_err: Option<miette::Report> = None;
        let mut failures = 0usize;
        loop {
            if set.is_empty() {
                // Every task has published its terminal event by the time
                // its join completes; drain what is still queued.
                while let Ok(envelope) = rx.try_recv() {
                    handle_envelope(&envelope, emit, &mut state);
                }
                break;
            }
            tokio::select! {
                biased;
                res = set.join_next() => {
                    match res {
                        Some(Ok(Ok(()))) => {}
                        Some(Ok(Err(e))) => {
                            failures += 1;
                            if first_err.is_none() { first_err = Some(e); }
                        }
                        Some(Err(je)) => {
                            failures += 1;
                            if first_err.is_none() { first_err = Some(miette::miette!("task panicked: {je}")); }
                        }
                        None => {}
                    }
                }
                ev = rx.recv() => {
                    if let Some(envelope) = ev {
                        handle_envelope(&envelope, emit, &mut state);
                    }
                }
            }
        }

        node.tasks = tasks;
        if let Some(err) = first_err {
            // Drop impl flushes the state file; remaining stages do not run.
            if failures > 1 {
                return Err(miette::miette!(
                    "{err:?}\n(+ {} more task failures in stage {})",
                    failures - 1,
                    stage.label()
                ));
            }
            return Err(err);
        }
        bus.publish(Body::PhaseDone {
            phase,
            ms: started.elapsed().as_millis() as u64,
        });
    }

    state.flush()?;
    bus.publish(Body::ClusterReady);
    // The terminal envelopes were published after the last stage loop;
    // drain them so the v1 stream still ends with `cluster-ready`.
    while let Ok(envelope) = rx.try_recv() {
        handle_envelope(&envelope, emit, &mut state);
    }
    Ok(())
}

/// Persist done records eagerly (a SIGINT between tasks must find a complete
/// snapshot) and adapt the envelope to the frozen v1 event stream.
fn handle_envelope(envelope: &Envelope, emit: &mut impl FnMut(Event), state: &mut RunState) {
    match &envelope.body {
        Body::TaskDone { key, .. }
        | Body::TaskReconciled { key, .. }
        | Body::TaskSkipped { key, .. } => {
            state.mark_done(key);
            if let Err(e) = state.flush() {
                eprintln!("warn: {e:?}");
            }
        }
        _ => {}
    }
    if let Some(event) = v1_event(&envelope.body, envelope.ts.millis()) {
        emit(event);
    }
}

/// Map a v2 body onto the frozen v1 [`Event`] shape, adding only `ts`.
/// Bodies without a v1 counterpart (logs, warnings, op boundaries) are
/// skipped.
fn v1_event(body: &Body, ts: u64) -> Option<Event> {
    fn stage(phase: &str) -> &'static str {
        Stage::from_label(phase)
            .map(Stage::label)
            .unwrap_or("unknown")
    }
    Some(match body {
        Body::TaskStart { key, node, phase } => Event::TaskStart {
            id: key.clone(),
            node: node.clone(),
            stage: stage(phase),
            ts,
        },
        Body::TaskSkipped { key, node, phase } => Event::TaskSkipped {
            id: key.clone(),
            node: node.clone(),
            stage: stage(phase),
            ts,
        },
        Body::TaskDone {
            key,
            node,
            phase,
            ms,
        } => Event::TaskDone {
            id: key.clone(),
            node: node.clone(),
            stage: stage(phase),
            ms: u128::from(*ms),
            ts,
        },
        Body::TaskReconciled { key, node, phase } => Event::TaskReconciled {
            id: key.clone(),
            node: node.clone(),
            stage: stage(phase),
            ts,
        },
        Body::TaskFailed {
            key,
            node,
            phase,
            reason,
        } => Event::TaskFailed {
            id: key.clone(),
            node: node.clone(),
            stage: stage(phase),
            reason: reason.clone(),
            ts,
        },
        Body::PhaseDone { phase, .. } => Event::StageDone {
            stage: stage(phase),
            ts,
        },
        Body::ClusterReady => Event::ClusterReady { ts },
        _ => return None,
    })
}

/// Find a [`TransportLost`] anywhere in the error chain (tasks may add
/// context around a transport failure).
fn transport_lost_reason(err: &miette::Report) -> Option<String> {
    err.chain()
        .find_map(|cause| cause.downcast_ref::<TransportLost>())
        .map(|lost| lost.to_string())
}

/// Map the transport's stream tag onto the event stream tag.
fn event_stream(stream: crate::engine::Stream) -> EventStream {
    match stream {
        crate::engine::Stream::Stdout => EventStream::Stdout,
        crate::engine::Stream::Stderr => EventStream::Stderr,
    }
}

/// Session decorator that publishes every remote output line as a
/// `Body::Log` at debug level, plus the command's declared secrets into the
/// bus redactor. Tasks stay unaware of the event system.
struct LoggingSession {
    inner: Arc<dyn Session>,
    bus: Arc<EventBus>,
    node: String,
    task: String,
}

impl LoggingSession {
    fn new(inner: Arc<dyn Session>, bus: Arc<EventBus>, node: String, task: String) -> Self {
        Self {
            inner,
            bus,
            node,
            task,
        }
    }

    fn publish_line(&self, stream: EventStream, line: &str) {
        self.bus.publish(Body::Log {
            level: Level::Debug,
            node: Some(self.node.clone()),
            task: Some(self.task.clone()),
            stream,
            line: line.to_string(),
        });
    }

    fn register_secrets(&self, cmd: &Cmd) {
        for secret in &cmd.redact {
            self.bus.redactor().register(secret.clone());
        }
    }

    /// Replay a captured output into the event stream (read-only probes,
    /// which use the retrying `exec_cmd` path).
    fn publish_captured(&self, out: &ExecOutput) {
        let mut stdout = 0usize;
        let mut stderr = 0usize;
        for line in out.stdout.lines() {
            stdout += 1;
            self.publish_line(EventStream::Stdout, line);
        }
        for line in out.stderr.lines() {
            stderr += 1;
            self.publish_line(EventStream::Stderr, line);
        }
        self.publish_summary(stdout, stderr);
    }

    fn publish_summary(&self, stdout: usize, stderr: usize) {
        if stdout + stderr == 0 {
            return;
        }
        // Debug + Kurin stream: hidden by default, shown by `-v` as the
        // remote-output summary, and always present in the JSONL file.
        self.bus.publish(Body::Log {
            level: Level::Debug,
            node: Some(self.node.clone()),
            task: Some(self.task.clone()),
            stream: EventStream::Kurin,
            line: format!(
                "[{}] {}: {stdout} stdout line(s), {stderr} stderr line(s)",
                self.node, self.task
            ),
        });
    }

    /// Run `cmd` through the inner transport's streaming path, publishing
    /// every line and a one-line summary (`-v`) when the command produced
    /// output. `forward` receives the lines the task itself asked for.
    async fn run_streamed(
        &self,
        cmd: &Cmd,
        forward: &mut (dyn for<'a> FnMut(crate::engine::Stream, &'a str) + Send),
    ) -> Result<ExecOutput> {
        use std::sync::atomic::{AtomicUsize, Ordering};

        self.register_secrets(cmd);
        let stdout_lines = AtomicUsize::new(0);
        let stderr_lines = AtomicUsize::new(0);
        let out = {
            let mut sink = |stream: crate::engine::Stream, line: &str| {
                match stream {
                    crate::engine::Stream::Stdout => {
                        stdout_lines.fetch_add(1, Ordering::Relaxed);
                    }
                    crate::engine::Stream::Stderr => {
                        stderr_lines.fetch_add(1, Ordering::Relaxed);
                    }
                }
                self.publish_line(event_stream(stream), line);
                forward(stream, line);
            };
            self.inner.exec_stream(cmd, &mut sink).await
        };

        self.publish_summary(
            stdout_lines.load(Ordering::Relaxed),
            stderr_lines.load(Ordering::Relaxed),
        );
        out
    }
}

#[async_trait::async_trait]
impl Session for LoggingSession {
    async fn exec(&self, command: &str) -> Result<ExecOutput> {
        self.exec_cmd(&Cmd::new(command)).await
    }

    async fn exec_cmd(&self, cmd: &Cmd) -> Result<ExecOutput> {
        if cmd.mutating {
            // Mutating commands must never be retried after a transport
            // loss (WP-08); the streaming path has exactly those semantics.
            return self.run_streamed(cmd, &mut |_, _| {}).await;
        }
        // Read-only probes keep the transport's single transparent retry;
        // their (small) output is replayed into the event stream afterwards.
        self.register_secrets(cmd);
        let out = self.inner.exec_cmd(cmd).await?;
        self.publish_captured(&out);
        Ok(out)
    }

    async fn exec_stream(
        &self,
        cmd: &Cmd,
        sink: &mut (dyn for<'a> FnMut(crate::engine::Stream, &'a str) + Send),
    ) -> Result<ExecOutput> {
        self.run_streamed(cmd, sink).await
    }

    async fn write_file(&self, path: &str, contents: &[u8]) -> Result<()> {
        self.inner.write_file(path, contents).await
    }

    async fn write_file_mode(&self, path: &str, contents: &[u8], mode: u32) -> Result<()> {
        self.inner.write_file_mode(path, contents, mode).await
    }

    async fn upload(&self, src: UploadSource, dst: &str, mode: u32) -> Result<UploadReport> {
        self.inner.upload(src, dst, mode).await
    }

    async fn file_sha256(&self, path: &str) -> Result<Option<String>> {
        self.inner.file_sha256(path).await
    }

    async fn service_active(&self, name: &str) -> Result<bool> {
        self.inner.service_active(name).await
    }

    async fn forward_local(&self, remote_addr: SocketAddr) -> Result<LocalForward> {
        self.inner.forward_local(remote_addr).await
    }

    async fn forward_remote(&self, local_addr: SocketAddr) -> Result<RemoteForward> {
        self.inner.forward_remote(local_addr).await
    }
}

async fn execute_one(
    planned: PlannedTask,
    pool: Arc<dyn SessionPool>,
    bus: Arc<EventBus>,
) -> Result<()> {
    let task = planned.task;
    let id = task.id();
    let node = task.node().to_string();
    let stage = task.stage().label();
    let phase = stage.to_string();

    // The span carries op/phase/task/node so `tracing` logs emitted by the
    // task (and by any helper it calls) inherit the context.
    let span = tracing::info_span!(
        "task",
        op = %bus.op(),
        phase = stage,
        task = %id,
        node = %node,
    );

    async move {
        // Published before anything can fail, so every task always starts
        // its event sequence — a session failure still yields TaskFailed.
        bus.publish(Body::TaskStart {
            key: id.clone(),
            node: node.clone(),
            phase: phase.clone(),
        });

        let result: Result<()> = async {
            let session = pool.get(&node).await?;
            let session = LoggingSession::new(session, Arc::clone(&bus), node.clone(), id.clone());

            match task.check(&session).await? {
                Drift::InSync => {
                    // Covers the resume path: a recorded-done task that is
                    // still in sync is skipped exactly like a fresh no-op.
                    bus.publish(Body::TaskSkipped {
                        key: id.clone(),
                        node: node.clone(),
                        phase: phase.clone(),
                    });
                    return Ok(());
                }
                Drift::Drifted(_reason) => {}
            }

            let started = Instant::now();
            let outcome = match task.apply(&session).await {
                Ok(outcome) => outcome,
                // A lost connection during a mutating apply means the remote
                // state is unknown: re-probe instead of blindly retrying.
                Err(err) => match transport_lost_reason(&err) {
                    Some(reason) => Outcome::Unknown(reason),
                    None => return Err(err),
                },
            };
            let ms = started.elapsed().as_millis() as u64;

            match outcome {
                Outcome::Verified => {
                    bus.publish(Body::TaskDone {
                        key: id.clone(),
                        node: node.clone(),
                        phase: phase.clone(),
                        ms,
                    });
                    Ok(())
                }
                Outcome::Unknown(reason) => {
                    // Recovery rule: probe again before deciding anything.
                    match task.check(&session).await? {
                        Drift::InSync => {
                            bus.publish(Body::TaskReconciled {
                                key: id.clone(),
                                node: node.clone(),
                                phase: phase.clone(),
                            });
                            Ok(())
                        }
                        Drift::Drifted(still) => Err(miette::miette!(
                            "apply reported unknown ({reason}) and re-probe shows drift: {still}"
                        )),
                    }
                }
            }
        }
        .await;

        if let Err(err) = &result {
            bus.publish(Body::TaskFailed {
                key: id,
                node,
                phase,
                reason: err.to_string(),
            });
        }
        result
    }
    .instrument(span)
    .await
}

/// `--dry-run --check`: probe every task concurrently and annotate the plan
/// with `would-change` / `unchanged` (Terraform-plan semantics). No events,
/// no applies. Integration owner wires this from the CLI (main.rs TODO).
pub async fn probe(plan: &mut Plan, pool: &Arc<dyn SessionPool>) -> Result<()> {
    for &idx in &plan.stages {
        let node = &mut plan.graph[idx];
        let mut tasks = std::mem::take(&mut node.tasks);
        let mut set: JoinSet<Result<(usize, Drift)>> = JoinSet::new();
        for (i, planned) in tasks.iter().enumerate() {
            let task = Arc::clone(&planned.task);
            let pool = Arc::clone(pool);
            set.spawn(async move {
                let session = pool.get(task.node()).await?;
                let drift = task.check(session.as_ref()).await?;
                Ok((i, drift))
            });
        }
        let mut first_err = None;
        while let Some(res) = set.join_next().await {
            match res {
                Ok(Ok((i, drift))) => tasks[i].drift = Some(drift),
                Ok(Err(e)) => {
                    if first_err.is_none() {
                        first_err = Some(e);
                    }
                }
                Err(je) => {
                    if first_err.is_none() {
                        first_err = Some(miette::miette!("check panicked: {je}"));
                    }
                }
            }
        }
        node.tasks = tasks;
        if let Some(err) = first_err {
            return Err(err);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use petgraph::stable_graph::StableDiGraph;

    use super::*;
    use crate::engine::{Drift, ExecOutput, Outcome, Session, Stage, Task};
    use crate::plan::{Plan, StageNode};

    struct MockSession;

    #[async_trait::async_trait]
    impl Session for MockSession {
        async fn exec(&self, _command: &str) -> Result<ExecOutput> {
            unreachable!("scripted tasks never exec")
        }
        async fn write_file(&self, _path: &str, _contents: &[u8]) -> Result<()> {
            unreachable!()
        }
        async fn file_sha256(&self, _path: &str) -> Result<Option<String>> {
            unreachable!()
        }
        async fn service_active(&self, _name: &str) -> Result<bool> {
            unreachable!()
        }
    }

    struct MockPool;

    #[async_trait::async_trait]
    impl SessionPool for MockPool {
        async fn get(&self, _node: &str) -> Result<Arc<dyn Session>> {
            Ok(Arc::new(MockSession))
        }
    }

    fn pool() -> Arc<dyn SessionPool> {
        Arc::new(MockPool)
    }

    /// A session that answers with fixed output, exercising the streaming
    /// decorator without a transport.
    struct OutputSession;

    #[async_trait::async_trait]
    impl Session for OutputSession {
        async fn exec(&self, _command: &str) -> Result<ExecOutput> {
            Ok(ExecOutput {
                exit_code: 0,
                stdout: "one\ntwo\n".into(),
                stderr: "warn\n".into(),
            })
        }
        async fn write_file(&self, _path: &str, _contents: &[u8]) -> Result<()> {
            Ok(())
        }
        async fn file_sha256(&self, _path: &str) -> Result<Option<String>> {
            Ok(None)
        }
        async fn service_active(&self, _name: &str) -> Result<bool> {
            Ok(false)
        }
    }

    /// Every remote line is published at debug, the per-command summary at
    /// debug/Kurin, and `Cmd.redact` values are scrubbed before publication.
    #[tokio::test]
    async fn logging_session_publishes_lines_summary_and_redacts() {
        const SECRET: &str = "fake-command-secret-not-real";
        struct SecretSession;
        #[async_trait::async_trait]
        impl Session for SecretSession {
            async fn exec(&self, _command: &str) -> Result<ExecOutput> {
                Ok(ExecOutput {
                    exit_code: 0,
                    stdout: format!("token {SECRET}\n"),
                    stderr: String::new(),
                })
            }
            async fn write_file(&self, _path: &str, _contents: &[u8]) -> Result<()> {
                Ok(())
            }
            async fn file_sha256(&self, _path: &str) -> Result<Option<String>> {
                Ok(None)
            }
            async fn service_active(&self, _name: &str) -> Result<bool> {
                Ok(false)
            }
        }

        let bus = Arc::new(EventBus::new("op"));
        let mut rx = bus.subscribe();
        let session = LoggingSession::new(
            Arc::new(OutputSession),
            Arc::clone(&bus),
            "n1".into(),
            "bootstrap/kubeadm@n1".into(),
        );
        let out = session.exec("run").await.unwrap();
        assert_eq!(out.stdout, "one\ntwo\n");

        let mut bodies = Vec::new();
        while let Ok(envelope) = rx.try_recv() {
            bodies.push(envelope.body);
        }
        assert_eq!(bodies.len(), 4, "three lines plus one summary: {bodies:?}");
        assert!(matches!(
            &bodies[0],
            Body::Log {
                level: Level::Debug,
                stream: EventStream::Stdout,
                ..
            }
        ));
        assert!(matches!(
            &bodies[2],
            Body::Log {
                stream: EventStream::Stderr,
                ..
            }
        ));
        match &bodies[3] {
            Body::Log {
                level: Level::Debug,
                stream: EventStream::Kurin,
                line,
                ..
            } => {
                assert!(line.contains("2 stdout line(s), 1 stderr line(s)"));
            }
            other => panic!("expected summary, got {other:?}"),
        }

        let bus = Arc::new(EventBus::new("op"));
        let mut rx = bus.subscribe();
        let session = LoggingSession::new(
            Arc::new(SecretSession),
            Arc::clone(&bus),
            "n1".into(),
            "task@n1".into(),
        );
        session
            .exec_cmd(&Cmd::new("run").redact(SECRET))
            .await
            .unwrap();
        while let Ok(envelope) = rx.try_recv() {
            let text = serde_json::to_string(&envelope).unwrap();
            assert!(!text.contains(SECRET), "secret leaked: {text}");
        }
    }

    /// Hand-scripted task: pops check/apply results in order; no network.
    struct ScriptedTask {
        id: String,
        node: String,
        stage: Stage,
        checks: Mutex<VecDeque<Drift>>,
        outcomes: Mutex<VecDeque<Outcome>>,
        applies: AtomicUsize,
        barrier: Option<Arc<tokio::sync::Barrier>>,
    }

    impl ScriptedTask {
        fn new(
            id: &str,
            node: &str,
            stage: Stage,
            checks: Vec<Drift>,
            outcomes: Vec<Outcome>,
        ) -> Arc<Self> {
            Arc::new(Self {
                id: id.to_string(),
                node: node.to_string(),
                stage,
                checks: Mutex::new(checks.into()),
                outcomes: Mutex::new(outcomes.into()),
                applies: AtomicUsize::new(0),
                barrier: None,
            })
        }

        fn with_barrier(
            id: &str,
            node: &str,
            stage: Stage,
            checks: Vec<Drift>,
            outcomes: Vec<Outcome>,
            barrier: Arc<tokio::sync::Barrier>,
        ) -> Arc<dyn Task> {
            Arc::new(Self {
                id: id.to_string(),
                node: node.to_string(),
                stage,
                checks: Mutex::new(checks.into()),
                outcomes: Mutex::new(outcomes.into()),
                applies: AtomicUsize::new(0),
                barrier: Some(barrier),
            })
        }

        fn applies(&self) -> usize {
            self.applies.load(Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl Task for ScriptedTask {
        fn id(&self) -> String {
            self.id.clone()
        }
        fn stage(&self) -> Stage {
            self.stage
        }
        fn node(&self) -> &str {
            &self.node
        }
        fn describe(&self) -> String {
            format!("scripted {}", self.id)
        }
        async fn check(&self, _session: &dyn Session) -> Result<Drift> {
            Ok(self
                .checks
                .lock()
                .unwrap()
                .pop_front()
                .expect("check called more often than scripted"))
        }
        async fn apply(&self, _session: &dyn Session) -> Result<Outcome> {
            self.applies.fetch_add(1, Ordering::SeqCst);
            if let Some(barrier) = &self.barrier {
                barrier.wait().await;
            }
            Ok(self
                .outcomes
                .lock()
                .unwrap()
                .pop_front()
                .expect("apply called more often than scripted"))
        }
    }

    fn plan_with(tasks: Vec<Arc<dyn Task>>, stages: &[Stage]) -> Plan {
        let mut graph = StableDiGraph::new();
        let mut idx = Vec::with_capacity(stages.len());
        for s in stages {
            idx.push(graph.add_node(StageNode {
                stage: *s,
                tasks: Vec::new(),
            }));
        }
        for w in idx.windows(2) {
            graph.add_edge(w[0], w[1], ());
        }
        for t in tasks {
            let pos = stages
                .iter()
                .position(|s| *s == t.stage())
                .expect("task stage in stages");
            graph[idx[pos]].tasks.push(PlannedTask {
                task: t,
                drift: None,
            });
        }
        Plan { graph, stages: idx }
    }

    fn task_events(events: &[Event]) -> Vec<&'static str> {
        events
            .iter()
            .map(|e| e.kind())
            .filter(|k| k.starts_with("task-"))
            .collect()
    }

    #[tokio::test]
    async fn unknown_then_in_sync_emits_reconciled() {
        let task = ScriptedTask::new(
            "prepare/sysctl@n1",
            "n1",
            Stage::Prepare,
            vec![
                Drift::Drifted("kernel params missing".into()),
                Drift::InSync,
            ],
            vec![Outcome::Unknown("ssh channel dropped mid-apply".into())],
        );
        let mut plan = plan_with(vec![task.clone()], &[Stage::Preflight, Stage::Prepare]);
        let mut events = Vec::new();
        run(&mut plan, &pool(), &mut |e| events.push(e))
            .await
            .unwrap();

        assert_eq!(task_events(&events), ["task-start", "task-reconciled"]);
        assert_eq!(task.applies(), 1);
        assert!(events.iter().any(|e| e.kind() == "stage-done"));
        assert_eq!(events.last().map(|e| e.kind()), Some("cluster-ready"));
    }

    #[tokio::test]
    async fn unknown_still_drifted_fails_with_task_failed() {
        let task = ScriptedTask::new(
            "bootstrap/kubeadm-init@cp1",
            "cp1",
            Stage::Bootstrap,
            vec![
                Drift::Drifted("no cluster".into()),
                Drift::Drifted("still no cluster".into()),
            ],
            vec![Outcome::Unknown("connection reset".into())],
        );
        let mut plan = plan_with(vec![task.clone()], &[Stage::Bootstrap]);
        let mut events = Vec::new();
        let err = run(&mut plan, &pool(), &mut |e| events.push(e))
            .await
            .unwrap_err();

        assert!(err.to_string().contains("still no cluster"));
        assert_eq!(
            task_events(&events),
            ["task-start", "task-failed"],
            "no blind retry: apply stays at 1"
        );
        assert_eq!(task.applies(), 1);
        let failed = events
            .iter()
            .find(|e| e.kind() == "task-failed")
            .expect("TaskFailed event");
        match failed {
            Event::TaskFailed { reason, .. } => {
                assert!(reason.contains("connection reset") && reason.contains("still no cluster"))
            }
            _ => panic!("wrong variant"),
        }
        // StageDone / ClusterReady never fire after a failure.
        assert!(!events
            .iter()
            .any(|e| matches!(e.kind(), "stage-done" | "cluster-ready")));
    }

    #[tokio::test]
    async fn in_sync_task_is_skipped_without_apply() {
        let task = ScriptedTask::new(
            "preflight/preflight@n1",
            "n1",
            Stage::Preflight,
            vec![Drift::InSync],
            vec![],
        );
        let mut plan = plan_with(vec![task.clone()], &[Stage::Preflight]);
        let mut events = Vec::new();
        run(&mut plan, &pool(), &mut |e| events.push(e))
            .await
            .unwrap();

        assert_eq!(task_events(&events), ["task-start", "task-skipped"]);
        assert_eq!(task.applies(), 0);
    }

    #[tokio::test]
    async fn drifted_task_runs_and_is_done() {
        let task = ScriptedTask::new(
            "prepare/sysctl@n1",
            "n1",
            Stage::Prepare,
            vec![Drift::Drifted("needs change".into())],
            vec![Outcome::Verified],
        );
        let mut plan = plan_with(vec![task.clone()], &[Stage::Prepare]);
        let mut events = Vec::new();
        run(&mut plan, &pool(), &mut |e| events.push(e))
            .await
            .unwrap();

        assert_eq!(task_events(&events), ["task-start", "task-done"]);
        assert_eq!(task.applies(), 1);
        assert!(
            events.iter().any(|e| matches!(e, Event::TaskDone { .. })),
            "TaskDone emitted"
        );
    }

    #[tokio::test]
    async fn tasks_within_a_stage_run_in_parallel() {
        // Both applies rendezvous on the same 2-party barrier: if the
        // executor serialized stage members, neither would ever finish and
        // the timeout below would fire.
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let a = ScriptedTask::with_barrier(
            "prepare/a@n1",
            "n1",
            Stage::Prepare,
            vec![Drift::Drifted("a".into())],
            vec![Outcome::Verified],
            Arc::clone(&barrier),
        );
        let b = ScriptedTask::with_barrier(
            "prepare/b@n2",
            "n2",
            Stage::Prepare,
            vec![Drift::Drifted("b".into())],
            vec![Outcome::Verified],
            barrier,
        );
        let mut plan = plan_with(vec![a, b], &[Stage::Prepare]);
        let mut events = Vec::new();
        tokio::time::timeout(
            Duration::from_secs(10),
            run(&mut plan, &pool(), &mut |e| events.push(e)),
        )
        .await
        .expect("stage members run in parallel (no barrier deadlock)")
        .unwrap();
        assert_eq!(
            task_events(&events),
            ["task-start", "task-start", "task-done", "task-done"]
        );
    }

    #[tokio::test]
    async fn stages_serialize_and_events_stream_in_order() {
        // Stage 1 must fully complete (task-done) before stage 2's first
        // task-start appears.
        let first = ScriptedTask::new(
            "prepare/a@n1",
            "n1",
            Stage::Prepare,
            vec![Drift::Drifted("a".into())],
            vec![Outcome::Verified],
        );
        let second = ScriptedTask::new(
            "join/kubeadm-join@w1",
            "w1",
            Stage::Join,
            vec![Drift::Drifted("not joined".into())],
            vec![Outcome::Verified],
        );
        let mut plan = plan_with(vec![first, second], &[Stage::Prepare, Stage::Join]);
        let mut events = Vec::new();
        run(&mut plan, &pool(), &mut |e| events.push(e))
            .await
            .unwrap();

        let pos = |k: &'static str, id: &str| {
            events
                .iter()
                .position(|e| {
                    e.kind() == k
                        && match e {
                            Event::TaskStart { id: i, .. }
                            | Event::TaskDone { id: i, .. }
                            | Event::TaskSkipped { id: i, .. }
                            | Event::TaskFailed { id: i, .. }
                            | Event::TaskReconciled { id: i, .. } => i == id,
                            _ => false,
                        }
                })
                .expect("event present")
        };
        assert!(pos("task-done", "prepare/a@n1") < pos("task-start", "join/kubeadm-join@w1"));
        let stage_done = events.iter().filter(|e| e.kind() == "stage-done").count();
        assert_eq!(stage_done, 2);
    }

    #[tokio::test]
    async fn failing_task_emits_terminal_event_before_error_return() {
        let fail = ScriptedTask::new(
            "prepare/fail@n1",
            "n1",
            Stage::Prepare,
            vec![Drift::Drifted("x".into()), Drift::Drifted("y".into())],
            vec![Outcome::Unknown("drop".into())],
        );
        let mut plan = plan_with(vec![fail], &[Stage::Prepare]);
        let mut events = Vec::new();
        run(&mut plan, &pool(), &mut |e| events.push(e))
            .await
            .unwrap_err();
        assert_eq!(task_events(&events), ["task-start", "task-failed"]);
    }

    #[tokio::test]
    async fn resume_persists_done_records_and_fresh_runs_reset_them() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cluster.json");

        let task = ScriptedTask::new(
            "prepare/sysctl@n1",
            "n1",
            Stage::Prepare,
            vec![Drift::InSync],
            vec![],
        );
        let mut plan = plan_with(vec![task], &[Stage::Prepare]);
        let mut events = Vec::new();
        run_with(
            &mut plan,
            &pool(),
            &mut |e| events.push(e),
            RunOptions {
                cluster: "demo".into(),
                state_file: Some(path.clone()),
                resume: true,
                op_id: String::new(),
            },
        )
        .await
        .unwrap();
        assert_eq!(task_events(&events), ["task-start", "task-skipped"]);

        // Seed an older record to prove resume keeps prior-run entries.
        {
            let mut seeded = RunState::open(&path, "demo", true).unwrap();
            seeded.mark_done("bootstrap/kubeadm-init@cp1");
            seeded.flush().unwrap();
        }

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("prepare/sysctl@n1"), "current run recorded");
        assert!(
            text.contains("bootstrap/kubeadm-init@cp1"),
            "resume preserved old record"
        );

        // A fresh (non-resume) run wipes the file before starting.
        let task2 = ScriptedTask::new(
            "prepare/containerd-config@n1",
            "n1",
            Stage::Prepare,
            vec![Drift::Drifted("cfg".into())],
            vec![Outcome::Verified],
        );
        let mut plan2 = plan_with(vec![task2], &[Stage::Prepare]);
        run_with(
            &mut plan2,
            &pool(),
            &mut |_| {},
            RunOptions {
                cluster: "demo".into(),
                state_file: Some(path.clone()),
                resume: false,
                op_id: String::new(),
            },
        )
        .await
        .unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("kubeadm-init"), "fresh run reset the state");
        assert!(text.contains("containerd-config@n1"));
    }

    #[tokio::test]
    async fn probe_annotates_drift_without_events_or_applies() {
        let drifted = ScriptedTask::new(
            "prepare/sysctl@n1",
            "n1",
            Stage::Prepare,
            vec![Drift::Drifted("missing".into())],
            vec![],
        );
        let in_sync = ScriptedTask::new(
            "preflight/preflight@n1",
            "n1",
            Stage::Preflight,
            vec![Drift::InSync],
            vec![],
        );
        let mut plan = plan_with(
            vec![drifted.clone(), in_sync.clone()],
            &[Stage::Preflight, Stage::Prepare],
        );
        probe(&mut plan, &pool()).await.unwrap();

        let get = |id: &str| {
            plan.stages
                .iter()
                .flat_map(|&i| plan.graph[i].tasks.iter())
                .find(|t| t.task.id() == id)
                .map(|t| t.drift.clone())
                .expect("task in plan")
        };
        assert!(matches!(get("prepare/sysctl@n1"), Some(Drift::Drifted(_))));
        assert!(matches!(get("preflight/preflight@n1"), Some(Drift::InSync)));
        assert_eq!(drifted.applies() + in_sync.applies(), 0);
    }

    /// A task whose apply loses the connection: the executor must re-probe
    /// (not retry) and reconcile when the re-probe is in sync.
    struct LostTask {
        checks: Mutex<VecDeque<Drift>>,
        applies: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl Task for LostTask {
        fn id(&self) -> String {
            "prepare/lost@n1".into()
        }
        fn stage(&self) -> Stage {
            Stage::Prepare
        }
        fn node(&self) -> &str {
            "n1"
        }
        fn describe(&self) -> String {
            "lost apply".into()
        }
        async fn check(&self, _session: &dyn Session) -> Result<Drift> {
            Ok(self.checks.lock().unwrap().pop_front().expect("scripted"))
        }
        async fn apply(&self, _session: &dyn Session) -> Result<Outcome> {
            self.applies.fetch_add(1, Ordering::SeqCst);
            Err(miette::Report::new(TransportLost::new(
                "n1",
                "connection reset by peer",
            )))
        }
    }

    #[tokio::test]
    async fn transport_lost_during_apply_is_reprobed_not_retried() {
        let task = Arc::new(LostTask {
            checks: Mutex::new(vec![Drift::Drifted("needs change".into()), Drift::InSync].into()),
            applies: AtomicUsize::new(0),
        });
        let mut plan = plan_with(vec![task.clone()], &[Stage::Prepare]);
        let mut events = Vec::new();
        run(&mut plan, &pool(), &mut |e| events.push(e))
            .await
            .unwrap();

        assert_eq!(
            task.applies.load(Ordering::SeqCst),
            1,
            "a lost mutating apply must not be retried"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::TaskReconciled { .. })),
            "the re-probe should reconcile: {events:?}"
        );
        assert!(
            !events.iter().any(|e| matches!(e, Event::TaskFailed { .. })),
            "a reconciled task is not a failure: {events:?}"
        );
    }
}
