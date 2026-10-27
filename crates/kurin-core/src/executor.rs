//! DAG executor over the plan: in-degree table + ready queue, global and
//! per-node concurrency caps, rolling batches, `BestEffort` tasks and
//! cooperative cancellation. Every task follows check -> apply -> verify;
//! `Unknown` outcomes are re-probed, never blind-retried.
//!
//! Scheduling rules (ADR-02):
//! - a task starts only when every dependency edge into it is satisfied;
//! - at most `RunSettings::max_parallel` tasks run at once, and at most
//!   `RunSettings::per_node` per node (the SSH channel count);
//! - phase barriers are ordinary edges produced by the plan builder, so the
//!   executor only ever looks at edges;
//! - a strict failure blocks every transitive successor (`TaskSkipped` with
//!   `reason: "blocked by <key>"`), lets in-flight siblings finish and
//!   unrelated branches run, then returns the first error;
//! - a [`FailurePolicy::BestEffort`] failure publishes `TaskWarn` and does
//!   not block successors;
//! - a rolling group runs at most `n` tasks per batch and waits for the
//!   whole batch before starting the next one;
//! - cancellation is checked before every dequeue: no new task starts,
//!   in-flight tasks finish, and the run returns [`Cancelled`].
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

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use miette::Result;
use tokio::task::JoinSet;
use tracing::Instrument;

use crate::engine::{
    Cmd, Drift, Event, ExecOutput, FailurePolicy, LocalForward, Outcome, PlannedTask,
    RemoteForward, Session, Stage, TransportLost, UploadReport, UploadSource,
};
use crate::events::{Body, Envelope, EventBus, Level, Stream as EventStream};
use crate::plan::{Plan, TaskIdx};
use crate::run::RunContext;
use crate::state::RunState;

/// Default global task concurrency cap.
pub const DEFAULT_MAX_PARALLEL: usize = 32;
/// Default per-node task concurrency cap (one per SSH channel).
pub const DEFAULT_PER_NODE: usize = 4;
/// Default cap on concurrent payload uploads across one run (WP-35,
/// 04 §5.1): the control host's uplink is the shared bottleneck, so
/// uploads are gated independently of task concurrency.
pub const DEFAULT_MAX_UPLOAD_PARALLEL: usize = 4;

/// The run stopped because [`RunContext::cancel`] was triggered. Callers
/// map this to `OpFinished { result: cancelled }`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("operation cancelled")
    }
}

impl std::error::Error for Cancelled {}

impl miette::Diagnostic for Cancelled {
    fn code(&self) -> Option<Box<dyn std::fmt::Display + '_>> {
        Some(Box::new("kurin::cancelled"))
    }
}

/// True when `err` (or anything in its chain) is the cancellation marker.
pub fn is_cancelled(err: &miette::Report) -> bool {
    err.chain()
        .any(|cause| cause.downcast_ref::<Cancelled>().is_some())
}

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
    /// Plan fingerprint (`Plan::fingerprint`). Stored in the run state and
    /// compared on `--resume`; empty for operations without a plan.
    pub plan_fingerprint: String,
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
    let state = match opts.state_file {
        Some(path) => RunState::open(path, cluster, opts.resume)?,
        None => RunState::disabled(),
    };
    if opts.resume {
        state.check_fingerprint(&opts.plan_fingerprint)?;
    }
    let op_id = if opts.op_id.is_empty() {
        run.op_id.clone()
    } else {
        opts.op_id.clone()
    };
    let mut state = state.with_identity(op_id, opts.plan_fingerprint.clone());
    state.set_blackboard_snapshot(run.bb.persistent_snapshot());
    // One subscription for the whole run: the executor drains it while
    // tasks run, so the bound is never reached in practice (lagging
    // consumers are the Web/SSE case, which resumes from JSONL).
    let rx = bus.subscribe();
    Executor::new(plan, pool, run, bus, rx, emit, state)
        .run()
        .await
}

/// Scheduling state of one task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TaskState {
    /// Waiting for its dependencies.
    Pending,
    /// Spawned and not yet joined.
    Running,
    /// Finished successfully (done, in-sync skip or reconciled).
    Done,
    /// Best-effort failure: reported as a warning, successors still run.
    Warned,
    /// Strict failure: the run reports this error.
    Failed,
    /// Never scheduled because a dependency failed (or its group stopped).
    Blocked,
}

/// One rolling group's runtime state.
struct GroupRuntime {
    /// Members in declaration order.
    tasks: Vec<TaskIdx>,
    /// Tasks per batch.
    max_parallel: usize,
    /// Next member to start; members before it have been started or skipped.
    cursor: usize,
    /// Members of the current batch still running.
    in_flight: usize,
    /// Dependencies satisfied; batches may start.
    active: bool,
    /// A strict failure stopped the group.
    stopped: bool,
}

/// The DAG executor: graph bookkeeping, the ready queue and the join set.
///
/// One instance drives one run; [`Executor::run`] owns the loop that fills
/// the join set up to the caps, drains the event channel and applies the
/// failure/cancellation rules.
struct Executor<'a, F: FnMut(Event)> {
    plan: &'a Plan,
    pool: Arc<dyn SessionPool>,
    run: &'a Arc<RunContext>,
    bus: Arc<EventBus>,
    rx: tokio::sync::mpsc::Receiver<Envelope>,
    emit: &'a mut F,
    state: RunState,
    set: JoinSet<(TaskIdx, Result<()>)>,

    max_parallel: usize,
    per_node: usize,
    /// Shared gate for payload uploads: at most `--max-upload-parallel`
    /// blobs are in flight across every task and node of the run.
    upload_gate: Arc<tokio::sync::Semaphore>,

    /// Unsatisfied dependency count per task.
    indegree: Vec<usize>,
    /// Outgoing edges per task (sorted, deduplicated).
    succ: Vec<Vec<TaskIdx>>,
    task_state: Vec<TaskState>,
    /// Tasks whose dependencies are satisfied, ordered by index.
    ready: BinaryHeap<Reverse<TaskIdx>>,
    /// Ready tasks that could not start because their node is at its cap.
    held: Vec<TaskIdx>,
    /// Rolling group of each task, when it belongs to one.
    group_of: Vec<Option<usize>>,
    groups: Vec<GroupRuntime>,

    /// Phase index of each task (display only).
    phase_of: Vec<usize>,
    /// Tasks of each phase that have not reached a terminal state yet.
    phase_remaining: Vec<usize>,
    /// A task of this phase failed or was blocked: no `PhaseDone`.
    phase_failed: Vec<bool>,
    phase_started: Vec<bool>,
    phase_started_at: Vec<Option<Instant>>,
    /// First phase that has not reported `PhaseDone` yet.
    phase_cursor: usize,

    in_flight: usize,
    node_in_flight: HashMap<String, usize>,

    first_err: Option<miette::Report>,
    failures: usize,
    /// A panic left the graph inconsistent: stop scheduling, report.
    halt: bool,
    cancelled: bool,
}

impl<'a, F: FnMut(Event)> Executor<'a, F> {
    fn new(
        plan: &'a Plan,
        pool: &Arc<dyn SessionPool>,
        run: &'a Arc<RunContext>,
        bus: &Arc<EventBus>,
        rx: tokio::sync::mpsc::Receiver<Envelope>,
        emit: &'a mut F,
        state: RunState,
    ) -> Self {
        let n = plan.tasks.len();

        // Rolling groups own their members; a task belongs to at most one.
        let mut group_of = vec![None; n];
        let mut groups = Vec::new();
        for group in &plan.rolling {
            let gi = groups.len();
            let mut tasks = Vec::new();
            for &task in &group.tasks {
                if task < n && group_of[task].is_none() {
                    group_of[task] = Some(gi);
                    tasks.push(task);
                }
            }
            groups.push(GroupRuntime {
                tasks,
                max_parallel: group.max_parallel.max(1),
                cursor: 0,
                in_flight: 0,
                active: false,
                stopped: false,
            });
        }

        // Edges, deduplicated. Edges inside one rolling group are ordering
        // hints already expressed by the group's task list; keeping them
        // would deadlock the batch (a member would wait for its sibling).
        let mut edges: HashSet<(TaskIdx, TaskIdx)> = HashSet::new();
        for &(from, to) in &plan.edges {
            if from >= n || to >= n || from == to {
                continue;
            }
            if group_of[from].is_some() && group_of[from] == group_of[to] {
                continue;
            }
            edges.insert((from, to));
        }
        let mut indegree = vec![0usize; n];
        let mut succ: Vec<Vec<TaskIdx>> = vec![Vec::new(); n];
        for (from, to) in edges {
            succ[from].push(to);
            indegree[to] += 1;
        }
        for next in &mut succ {
            next.sort_unstable();
        }

        // Phase membership is display-only: tasks are stored in phase order.
        let phase_count = plan.phases.len();
        let mut phase_of = vec![0usize; n];
        let mut phase_remaining = vec![0usize; phase_count];
        let mut offset = 0usize;
        for (pi, group) in plan.phase_groups().iter().enumerate() {
            let len = group.tasks.len();
            for slot in phase_of.iter_mut().skip(offset).take(len) {
                *slot = pi;
            }
            phase_remaining[pi] = len;
            offset += len;
        }

        let mut ready = BinaryHeap::new();
        for idx in 0..n {
            if indegree[idx] == 0 && group_of[idx].is_none() {
                ready.push(Reverse(idx));
            }
        }

        let mut executor = Self {
            plan,
            pool: Arc::clone(pool),
            run,
            bus: Arc::clone(bus),
            rx,
            emit,
            state,
            set: JoinSet::new(),
            max_parallel: run
                .settings
                .max_parallel
                .unwrap_or(DEFAULT_MAX_PARALLEL)
                .max(1),
            per_node: run.settings.per_node.unwrap_or(DEFAULT_PER_NODE).max(1),
            upload_gate: Arc::new(tokio::sync::Semaphore::new(
                run.max_upload_parallel()
                    .unwrap_or(DEFAULT_MAX_UPLOAD_PARALLEL)
                    .max(1),
            )),
            indegree,
            succ,
            task_state: vec![TaskState::Pending; n],
            ready,
            held: Vec::new(),
            group_of,
            groups,
            phase_of,
            phase_remaining,
            phase_failed: vec![false; phase_count],
            phase_started: vec![false; phase_count],
            phase_started_at: vec![None; phase_count],
            phase_cursor: 0,
            in_flight: 0,
            node_in_flight: HashMap::new(),
            first_err: None,
            failures: 0,
            halt: false,
            cancelled: false,
        };
        executor.activate_groups();
        executor
    }

    /// Drive the plan to completion (or the first strict failure / cancel).
    async fn run(mut self) -> Result<()> {
        loop {
            self.fill();
            if self.in_flight == 0 {
                self.drain();
                self.flush_phases();
                if self.cancelled {
                    self.state
                        .set_blackboard_snapshot(self.run.bb.persistent_snapshot());
                    let _ = self.state.flush();
                    return Err(miette::Report::new(Cancelled));
                }
                if !self.has_pending() {
                    break;
                }
                // A strict failure or a panic left tasks that can never
                // become ready; report the original error, not the stall.
                if self.first_err.is_some() {
                    break;
                }
                let stuck = self
                    .task_state
                    .iter()
                    .filter(|state| **state == TaskState::Pending)
                    .count();
                return Err(miette::miette!(
                    "plan deadlock: {stuck} task(s) never became ready"
                ));
            }
            tokio::select! {
                biased;
                res = self.set.join_next() => {
                    match res {
                        Some(Ok((idx, result))) => self.on_complete(idx, result),
                        Some(Err(join)) => self.on_panic(join),
                        None => {}
                    }
                    // The task published its terminal envelope before the
                    // join completed; drain it before phase boundaries.
                    self.drain();
                    self.flush_phases();
                }
                envelope = self.rx.recv() => {
                    if let Some(envelope) = envelope {
                        handle_envelope(&envelope, &mut *self.emit, &mut self.state, &self.run.bb);
                    }
                }
            }
        }

        self.drain();
        self.flush_phases();
        self.state
            .set_blackboard_snapshot(self.run.bb.persistent_snapshot());
        self.state.flush()?;
        if let Some(err) = self.first_err.take() {
            if self.failures > 1 {
                return Err(miette::miette!(
                    "{err:?}\n(+ {} more task failures)",
                    self.failures - 1
                ));
            }
            return Err(err);
        }
        self.bus.publish(Body::ClusterReady);
        // The terminal envelopes were published after the last task joined;
        // drain them so the v1 stream still ends with `cluster-ready`.
        self.drain();
        Ok(())
    }

    /// Hand every queued envelope to the v1 adapter / run state.
    fn drain(&mut self) {
        while let Ok(envelope) = self.rx.try_recv() {
            handle_envelope(&envelope, &mut *self.emit, &mut self.state, &self.run.bb);
        }
    }

    fn has_pending(&self) -> bool {
        self.task_state.contains(&TaskState::Pending)
    }

    /// Start ready tasks until a cap is reached. Cancellation is checked
    /// before every dequeue, so no task starts after the token fires.
    fn fill(&mut self) {
        if self.run.cancel.is_cancelled() {
            self.cancelled = true;
        }
        if self.cancelled || self.halt {
            return;
        }

        // Tasks held back by the per-node cap get the first chance: the
        // completion that freed their slot is what called us.
        let held = std::mem::take(&mut self.held);
        for idx in held {
            if !self.try_start(idx) {
                self.held.push(idx);
            }
        }

        while self.in_flight < self.max_parallel {
            if self.run.cancel.is_cancelled() {
                self.cancelled = true;
                return;
            }
            let Some(Reverse(idx)) = self.ready.pop() else {
                break;
            };
            if !self.try_start(idx) {
                self.held.push(idx);
            }
        }

        // Rolling groups: at most `max_parallel` members per batch, and the
        // next batch only starts once the current one has fully joined.
        for gi in 0..self.groups.len() {
            if self.in_flight >= self.max_parallel {
                break;
            }
            if self.run.cancel.is_cancelled() {
                self.cancelled = true;
                return;
            }
            if !self.groups[gi].active || self.groups[gi].stopped || self.groups[gi].in_flight > 0 {
                continue;
            }
            let mut batch: Vec<TaskIdx> = Vec::new();
            loop {
                if batch.len() >= self.groups[gi].max_parallel {
                    break;
                }
                if self.in_flight + batch.len() >= self.max_parallel {
                    break;
                }
                let cursor = self.groups[gi].cursor;
                if cursor >= self.groups[gi].tasks.len() {
                    break;
                }
                let idx = self.groups[gi].tasks[cursor];
                if self.task_state[idx] != TaskState::Pending {
                    self.groups[gi].cursor += 1;
                    continue;
                }
                let node = self.plan.tasks[idx].task.node().to_string();
                let running = self.node_in_flight.get(&node).copied().unwrap_or(0)
                    + batch
                        .iter()
                        .filter(|&&t| self.plan.tasks[t].task.node() == node)
                        .count();
                if running >= self.per_node {
                    break;
                }
                self.groups[gi].cursor += 1;
                batch.push(idx);
            }
            for idx in batch {
                self.groups[gi].in_flight += 1;
                self.start(idx);
            }
        }
    }

    /// Start `idx` when both caps allow it; `true` when it is no longer
    /// pending (started now or terminal).
    fn try_start(&mut self, idx: TaskIdx) -> bool {
        if self.task_state[idx] != TaskState::Pending {
            return true;
        }
        if self.in_flight >= self.max_parallel {
            return false;
        }
        let node = self.plan.tasks[idx].task.node().to_string();
        if self.node_in_flight.get(&node).copied().unwrap_or(0) >= self.per_node {
            return false;
        }
        self.start(idx);
        true
    }

    fn start(&mut self, idx: TaskIdx) {
        self.task_state[idx] = TaskState::Running;
        self.in_flight += 1;
        let node = self.plan.tasks[idx].task.node().to_string();
        *self.node_in_flight.entry(node).or_insert(0) += 1;

        let phase = self.phase_of[idx];
        if phase < self.phase_started.len() && !self.phase_started[phase] {
            self.phase_started[phase] = true;
            self.phase_started_at[phase] = Some(Instant::now());
            self.bus.publish(Body::PhaseStarted {
                phase: self.plan.phases[phase].label.to_string(),
            });
        }

        let planned = self.plan.tasks[idx].clone();
        let pool = Arc::clone(&self.pool);
        let bus = Arc::clone(&self.bus);
        let gate = Arc::clone(&self.upload_gate);
        self.set.spawn(async move {
            let result = execute_one(planned, pool, bus, gate).await;
            (idx, result)
        });
    }

    fn on_complete(&mut self, idx: TaskIdx, result: Result<()>) {
        self.in_flight = self.in_flight.saturating_sub(1);
        let node = self.plan.tasks[idx].task.node().to_string();
        if let Some(count) = self.node_in_flight.get_mut(&node) {
            *count = count.saturating_sub(1);
        }
        if let Some(gi) = self.group_of[idx] {
            self.groups[gi].in_flight = self.groups[gi].in_flight.saturating_sub(1);
        }
        let phase = self.phase_of[idx];
        self.phase_remaining[phase] = self.phase_remaining[phase].saturating_sub(1);

        match result {
            Ok(()) => {
                self.task_state[idx] = TaskState::Done;
                self.release_successors(idx);
            }
            Err(err) => {
                let key = self.plan.tasks[idx].task.id();
                match self.plan.tasks[idx].task.failure_policy() {
                    FailurePolicy::BestEffort => {
                        self.bus.publish(Body::TaskWarn {
                            key,
                            node,
                            message: err.to_string(),
                        });
                        self.task_state[idx] = TaskState::Warned;
                        self.release_successors(idx);
                    }
                    FailurePolicy::Strict => {
                        self.task_state[idx] = TaskState::Failed;
                        self.phase_failed[phase] = true;
                        self.failures += 1;
                        if self.first_err.is_none() {
                            self.first_err = Some(err);
                        }
                        let mut roots = vec![idx];
                        if let Some(gi) = self.group_of[idx] {
                            roots.extend(self.stop_group(gi, &key));
                        }
                        self.block_from(&roots, &key);
                    }
                }
            }
        }
    }

    fn on_panic(&mut self, join: tokio::task::JoinError) {
        self.in_flight = self.in_flight.saturating_sub(1);
        self.failures += 1;
        self.halt = true;
        if self.first_err.is_none() {
            self.first_err = Some(miette::miette!("task panicked: {join}"));
        }
    }

    /// A dependency finished: unlock its successors.
    fn release_successors(&mut self, idx: TaskIdx) {
        for i in 0..self.succ[idx].len() {
            let next = self.succ[idx][i];
            if self.task_state[next] != TaskState::Pending {
                continue;
            }
            self.indegree[next] -= 1;
            if self.indegree[next] == 0 {
                self.release(next);
            }
        }
    }

    fn release(&mut self, idx: TaskIdx) {
        match self.group_of[idx] {
            Some(gi) => {
                if self.group_ready(gi) {
                    self.groups[gi].active = true;
                }
            }
            None => self.ready.push(Reverse(idx)),
        }
    }

    /// Activate every group whose members are all pending with no
    /// unsatisfied dependencies.
    fn activate_groups(&mut self) {
        for gi in 0..self.groups.len() {
            if !self.groups[gi].active && self.group_ready(gi) {
                self.groups[gi].active = true;
            }
        }
    }

    fn group_ready(&self, gi: usize) -> bool {
        let group = &self.groups[gi];
        !group.tasks.is_empty()
            && group.tasks.iter().all(|&task| {
                self.task_state[task] == TaskState::Pending && self.indegree[task] == 0
            })
    }

    /// Block `roots` and every transitive successor, publishing one
    /// `TaskSkipped { reason: "blocked by <key>" }` per blocked task.
    fn block_from(&mut self, roots: &[TaskIdx], key: &str) {
        let mut queue: Vec<TaskIdx> = roots.to_vec();
        let mut seen: HashSet<TaskIdx> = roots.iter().copied().collect();
        while let Some(cur) = queue.pop() {
            let succ = self.succ[cur].clone();
            for next in succ {
                if !seen.insert(next) {
                    continue;
                }
                if self.task_state[next] != TaskState::Pending {
                    continue;
                }
                self.mark_blocked(next, key);
                queue.push(next);
                if let Some(gi) = self.group_of[next] {
                    for member in self.stop_group(gi, key) {
                        if seen.insert(member) {
                            queue.push(member);
                        }
                    }
                }
            }
        }
    }

    /// Stop a rolling group: its remaining members never start.
    fn stop_group(&mut self, gi: usize, key: &str) -> Vec<TaskIdx> {
        if self.groups[gi].stopped {
            return Vec::new();
        }
        self.groups[gi].stopped = true;
        let start = self.groups[gi].cursor.min(self.groups[gi].tasks.len());
        let rest = self.groups[gi].tasks[start..].to_vec();
        let mut blocked = Vec::new();
        for task in rest {
            if self.task_state[task] == TaskState::Pending {
                self.mark_blocked(task, key);
                blocked.push(task);
            }
        }
        blocked
    }

    fn mark_blocked(&mut self, idx: TaskIdx, key: &str) {
        self.task_state[idx] = TaskState::Blocked;
        let phase = self.phase_of[idx];
        self.phase_remaining[phase] = self.phase_remaining[phase].saturating_sub(1);
        self.phase_failed[phase] = true;
        let (id, node, stage) = {
            let task = &self.plan.tasks[idx].task;
            (task.id(), task.node().to_string(), task.stage().label())
        };
        self.bus.publish(Body::TaskSkipped {
            key: id,
            node,
            phase: stage.to_string(),
            reason: Some(format!("blocked by {key}")),
        });
    }

    /// Report `PhaseDone` for every phase that is fully settled, in
    /// template order. A phase that saw a failure or a blocked task gets no
    /// `PhaseDone`: the stage did not complete.
    fn flush_phases(&mut self) {
        loop {
            let phase = self.phase_cursor;
            if phase >= self.phase_remaining.len() {
                break;
            }
            if self.phase_remaining[phase] > 0 || self.phase_failed[phase] {
                break;
            }
            if !self.phase_started[phase] {
                self.phase_started[phase] = true;
                self.phase_started_at[phase] = Some(Instant::now());
                self.bus.publish(Body::PhaseStarted {
                    phase: self.plan.phases[phase].label.to_string(),
                });
            }
            let ms = self.phase_started_at[phase]
                .map(|started| started.elapsed().as_millis() as u64)
                .unwrap_or(0);
            self.bus.publish(Body::PhaseDone {
                phase: self.plan.phases[phase].label.to_string(),
                ms,
            });
            self.phase_cursor += 1;
        }
        self.drain();
    }
}

/// Persist done records eagerly (a SIGINT between tasks must find a complete
/// snapshot) and adapt the envelope to the frozen v1 event stream.
fn handle_envelope(
    envelope: &Envelope,
    emit: &mut (impl FnMut(Event) + ?Sized),
    state: &mut RunState,
    bb: &crate::blackboard::Blackboard,
) {
    match &envelope.body {
        Body::TaskDone { key, .. } | Body::TaskReconciled { key, .. } => {
            state.mark_done(key);
            state.set_blackboard_snapshot(bb.persistent_snapshot());
            if let Err(e) = state.flush() {
                eprintln!("warn: {e:?}");
            }
        }
        // An in-sync skip is done; a blocked task is not — resuming must
        // still run it once the failure is fixed.
        Body::TaskSkipped {
            key, reason: None, ..
        } => {
            state.mark_done(key);
            state.set_blackboard_snapshot(bb.persistent_snapshot());
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
        Body::TaskSkipped {
            key,
            node,
            phase,
            reason,
        } => Event::TaskSkipped {
            id: key.clone(),
            node: node.clone(),
            stage: stage(phase),
            reason: reason.clone(),
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
    /// Run-wide upload gate (`--max-upload-parallel`, WP-35).
    upload_gate: Arc<tokio::sync::Semaphore>,
}

impl LoggingSession {
    fn new(
        inner: Arc<dyn Session>,
        bus: Arc<EventBus>,
        node: String,
        task: String,
        upload_gate: Arc<tokio::sync::Semaphore>,
    ) -> Self {
        Self {
            inner,
            bus,
            node,
            task,
            upload_gate,
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
        // Payload uploads are the run's shared uplink bottleneck: hold a
        // slot for the whole transfer (04 §5.1).
        let _slot = self
            .upload_gate
            .acquire()
            .await
            .map_err(|_| miette::miette!("upload gate closed"))?;
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
    upload_gate: Arc<tokio::sync::Semaphore>,
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
            let session = LoggingSession::new(
                session,
                Arc::clone(&bus),
                node.clone(),
                id.clone(),
                Arc::clone(&upload_gate),
            );

            match task.check(&session).await? {
                Drift::InSync => {
                    // Covers the resume path: a recorded-done task that is
                    // still in sync is skipped exactly like a fresh no-op.
                    bus.publish(Body::TaskSkipped {
                        key: id.clone(),
                        node: node.clone(),
                        phase: phase.clone(),
                        reason: None,
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
    for group in plan.phase_groups_mut() {
        let tasks = group.tasks;
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
        if let Some(err) = first_err {
            return Err(err);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, VecDeque};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use super::*;
    use crate::engine::{Drift, ExecOutput, Outcome, Session, Stage, Task};
    use crate::plan::{OpKind, Phase, Plan, RollingGroup};
    use crate::run::{ArtifactSources, RunSettings};

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
            Arc::new(tokio::sync::Semaphore::new(4)),
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
            Arc::new(tokio::sync::Semaphore::new(4)),
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
        let groups: Vec<(Phase, Vec<PlannedTask>)> = stages
            .iter()
            .map(|stage| {
                let group = tasks
                    .iter()
                    .filter(|t| t.stage() == *stage)
                    .map(|t| PlannedTask {
                        task: Arc::clone(t),
                        drift: None,
                    })
                    .collect();
                (Phase::new(stage.label(), stage.label()), group)
            })
            .collect();
        Plan::from_stage_groups(OpKind::Create, groups)
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
                plan_fingerprint: String::new(),
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
                plan_fingerprint: String::new(),
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
            plan.tasks
                .iter()
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

    // ── WP-11: DAG scheduler invariants ────────────────────────────────

    /// Records start/finish order and the concurrency actually observed.
    #[derive(Default)]
    struct Trace {
        log: Mutex<Vec<String>>,
        in_flight: AtomicUsize,
        max_in_flight: AtomicUsize,
        node_in_flight: Mutex<HashMap<String, usize>>,
        max_node_in_flight: Mutex<HashMap<String, usize>>,
    }

    impl Trace {
        fn start(&self, id: &str, node: &str) {
            self.log.lock().unwrap().push(format!("start {id}"));
            let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_in_flight.fetch_max(now, Ordering::SeqCst);
            let mut running = self.node_in_flight.lock().unwrap();
            let count = running.entry(node.to_string()).or_insert(0);
            *count += 1;
            let mut max = self.max_node_in_flight.lock().unwrap();
            let peak = max.entry(node.to_string()).or_insert(0);
            *peak = (*peak).max(*count);
        }

        fn finish(&self, id: &str, node: &str) {
            self.log.lock().unwrap().push(format!("finish {id}"));
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            *self
                .node_in_flight
                .lock()
                .unwrap()
                .get_mut(node)
                .expect("node was started") -= 1;
        }

        fn log(&self) -> Vec<String> {
            self.log.lock().unwrap().clone()
        }

        fn max_node(&self, node: &str) -> usize {
            self.max_node_in_flight
                .lock()
                .unwrap()
                .get(node)
                .copied()
                .unwrap_or(0)
        }
    }

    /// A task that records scheduling facts and can fail, wait on a barrier,
    /// wait for a release signal or run a callback when it starts.
    struct ProbeTask {
        id: String,
        node: String,
        policy: FailurePolicy,
        fail: bool,
        trace: Arc<Trace>,
        barrier: Option<Arc<tokio::sync::Barrier>>,
        gate: Option<Arc<tokio::sync::Notify>>,
        on_start: Option<Arc<dyn Fn() + Send + Sync>>,
    }

    impl ProbeTask {
        fn new(id: &str, node: &str, trace: &Arc<Trace>) -> Self {
            Self {
                id: id.to_string(),
                node: node.to_string(),
                policy: FailurePolicy::Strict,
                fail: false,
                trace: Arc::clone(trace),
                barrier: None,
                gate: None,
                on_start: None,
            }
        }

        fn failing(mut self) -> Self {
            self.fail = true;
            self
        }

        fn best_effort(mut self) -> Self {
            self.policy = FailurePolicy::BestEffort;
            self
        }

        fn at_barrier(mut self, barrier: Arc<tokio::sync::Barrier>) -> Self {
            self.barrier = Some(barrier);
            self
        }

        fn gated(mut self, gate: Arc<tokio::sync::Notify>) -> Self {
            self.gate = Some(gate);
            self
        }

        fn on_start(mut self, callback: Arc<dyn Fn() + Send + Sync>) -> Self {
            self.on_start = Some(callback);
            self
        }

        fn arc(self) -> Arc<dyn Task> {
            Arc::new(self)
        }
    }

    #[async_trait::async_trait]
    impl Task for ProbeTask {
        fn id(&self) -> String {
            self.id.clone()
        }
        fn stage(&self) -> Stage {
            Stage::Prepare
        }
        fn node(&self) -> &str {
            &self.node
        }
        fn describe(&self) -> String {
            format!("probe {}", self.id)
        }
        fn failure_policy(&self) -> FailurePolicy {
            self.policy
        }
        async fn check(&self, _session: &dyn Session) -> Result<Drift> {
            Ok(Drift::Drifted("probe".into()))
        }
        async fn apply(&self, _session: &dyn Session) -> Result<Outcome> {
            self.trace.start(&self.id, &self.node);
            if let Some(callback) = &self.on_start {
                callback();
            }
            if let Some(barrier) = &self.barrier {
                barrier.wait().await;
            }
            if let Some(gate) = &self.gate {
                gate.notified().await;
            }
            self.trace.finish(&self.id, &self.node);
            if self.fail {
                Err(miette::miette!("probe failure: {}", self.id))
            } else {
                Ok(Outcome::Verified)
            }
        }
    }

    /// One phase, explicit edges, no barrier edges.
    fn dag_plan(tasks: Vec<Arc<dyn Task>>, edges: &[(usize, usize)]) -> Plan {
        let mut plan = plan_with(tasks, &[Stage::Prepare]);
        plan.edges.extend_from_slice(edges);
        plan
    }

    fn settings(max_parallel: usize, per_node: usize) -> RunSettings {
        RunSettings {
            max_parallel: Some(max_parallel),
            per_node: Some(per_node),
            ..RunSettings::default()
        }
    }

    struct RunOutcome {
        result: Result<()>,
        events: Vec<Event>,
        bodies: Vec<Body>,
    }

    async fn run_dag(
        plan: &mut Plan,
        pool: &Arc<dyn SessionPool>,
        settings: RunSettings,
    ) -> RunOutcome {
        let run = Arc::new(
            RunContext::new("op-test", "demo", ArtifactSources::embedded()).with_settings(settings),
        );
        let bus = Arc::clone(&run.events);
        let mut rx = bus.subscribe();
        let mut events = Vec::new();
        let result = run_with_bus(
            plan,
            pool,
            &run,
            &bus,
            &mut |event| events.push(event),
            RunOptions::default(),
        )
        .await;
        let mut bodies = Vec::new();
        while let Ok(envelope) = rx.try_recv() {
            bodies.push(envelope.body);
        }
        RunOutcome {
            result,
            events,
            bodies,
        }
    }

    fn skip_reason(events: &[Event], id: &str) -> Option<String> {
        events.iter().find_map(|event| match event {
            Event::TaskSkipped { id: i, reason, .. } if i == id => reason.clone(),
            _ => None,
        })
    }

    /// Every started task has exactly one terminal event.
    fn assert_one_terminal_per_start(events: &[Event]) {
        let mut starts: HashMap<String, usize> = HashMap::new();
        let mut terminals: HashMap<String, usize> = HashMap::new();
        for event in events {
            match event {
                Event::TaskStart { id, .. } => *starts.entry(id.clone()).or_insert(0) += 1,
                Event::TaskSkipped { id, .. }
                | Event::TaskDone { id, .. }
                | Event::TaskReconciled { id, .. }
                | Event::TaskFailed { id, .. } => *terminals.entry(id.clone()).or_insert(0) += 1,
                _ => {}
            }
        }
        for (id, count) in &starts {
            assert_eq!(count, &1, "{id} started once");
            assert_eq!(
                terminals.get(id),
                Some(&1),
                "{id} has exactly one terminal event: {events:?}"
            );
        }
    }

    #[tokio::test]
    async fn dag_starts_a_task_only_after_all_dependencies_finished() {
        let trace = Arc::new(Trace::default());
        let tasks = vec![
            ProbeTask::new("a", "n1", &trace).arc(),
            ProbeTask::new("b", "n2", &trace).arc(),
            ProbeTask::new("c", "n3", &trace).arc(),
            ProbeTask::new("d", "n4", &trace).arc(),
        ];
        let mut plan = dag_plan(tasks, &[(0, 1), (1, 2)]);
        let outcome = run_dag(&mut plan, &pool(), settings(8, 4)).await;
        outcome.result.unwrap();

        let log = trace.log();
        let pos = |entry: &str| {
            log.iter()
                .position(|line| line == entry)
                .unwrap_or_else(|| panic!("missing {entry:?} in {log:?}"))
        };
        assert!(pos("finish a") < pos("start b"), "{log:?}");
        assert!(pos("finish b") < pos("start c"), "{log:?}");
        assert!(
            log.contains(&"finish d".to_string()),
            "independent task ran"
        );
        assert_one_terminal_per_start(&outcome.events);
    }

    #[tokio::test]
    async fn global_concurrency_cap_is_never_exceeded() {
        let trace = Arc::new(Trace::default());
        let barrier = Arc::new(tokio::sync::Barrier::new(3));
        let tasks = (0..6)
            .map(|i| {
                ProbeTask::new(&format!("t{i}"), &format!("n{i}"), &trace)
                    .at_barrier(Arc::clone(&barrier))
                    .arc()
            })
            .collect();
        let mut plan = dag_plan(tasks, &[]);
        let outcome = tokio::time::timeout(
            Duration::from_secs(10),
            run_dag(&mut plan, &pool(), settings(3, 4)),
        )
        .await
        .expect("both waves complete (cap 3)");
        outcome.result.unwrap();
        assert_eq!(
            trace.max_in_flight.load(Ordering::SeqCst),
            3,
            "max_parallel is a hard cap"
        );
    }

    #[tokio::test]
    async fn per_node_cap_is_never_exceeded() {
        let trace = Arc::new(Trace::default());
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let tasks = (0..4)
            .map(|i| {
                ProbeTask::new(&format!("t{i}"), "n1", &trace)
                    .at_barrier(Arc::clone(&barrier))
                    .arc()
            })
            .collect();
        let mut plan = dag_plan(tasks, &[]);
        let outcome = tokio::time::timeout(
            Duration::from_secs(10),
            run_dag(&mut plan, &pool(), settings(8, 2)),
        )
        .await
        .expect("both waves complete (per_node 2)");
        outcome.result.unwrap();
        assert_eq!(trace.max_node("n1"), 2, "per_node is a hard cap");
        assert_eq!(trace.max_in_flight.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn strict_failure_blocks_transitive_successors_only() {
        let trace = Arc::new(Trace::default());
        let tasks = vec![
            ProbeTask::new("a", "n1", &trace).failing().arc(),
            ProbeTask::new("b", "n2", &trace).arc(),
            ProbeTask::new("c", "n3", &trace).arc(),
            ProbeTask::new("d", "n4", &trace).arc(),
        ];
        // a -> b -> c; d is an unrelated branch.
        let mut plan = dag_plan(tasks, &[(0, 1), (1, 2)]);
        let outcome = run_dag(&mut plan, &pool(), settings(8, 4)).await;

        let err = outcome.result.unwrap_err();
        assert!(err.to_string().contains("probe failure: a"), "{err:?}");
        let log = trace.log();
        assert!(
            log.contains(&"finish d".to_string()),
            "unrelated branch runs to completion: {log:?}"
        );
        assert!(
            !log.iter()
                .any(|line| line == "start b" || line == "start c"),
            "blocked tasks never start: {log:?}"
        );
        assert_eq!(
            skip_reason(&outcome.events, "b").as_deref(),
            Some("blocked by a")
        );
        assert_eq!(
            skip_reason(&outcome.events, "c").as_deref(),
            Some("blocked by a")
        );
        assert!(
            !outcome
                .events
                .iter()
                .any(|e| matches!(e, Event::ClusterReady { .. })),
            "a failed run is never cluster-ready"
        );
        assert_one_terminal_per_start(&outcome.events);
    }

    #[tokio::test]
    async fn best_effort_failure_warns_and_successors_run() {
        let trace = Arc::new(Trace::default());
        let tasks = vec![
            ProbeTask::new("a", "n1", &trace)
                .failing()
                .best_effort()
                .arc(),
            ProbeTask::new("b", "n2", &trace).arc(),
        ];
        let mut plan = dag_plan(tasks, &[(0, 1)]);
        let outcome = run_dag(&mut plan, &pool(), settings(8, 4)).await;
        outcome
            .result
            .expect("a best-effort failure does not fail the run");

        assert!(
            trace.log().contains(&"finish b".to_string()),
            "successors still run"
        );
        assert!(
            outcome.bodies.iter().any(|body| matches!(
                body,
                Body::TaskWarn { key, message, .. }
                    if key == "a" && message.contains("probe failure: a")
            )),
            "the failure is reported as TaskWarn: {:?}",
            outcome.bodies
        );
        assert!(outcome
            .events
            .iter()
            .any(|e| matches!(e, Event::TaskDone { id, .. } if id == "b")));
        assert_one_terminal_per_start(&outcome.events);
    }

    #[tokio::test]
    async fn rolling_group_runs_one_batch_at_a_time() {
        let trace = Arc::new(Trace::default());
        let tasks = (0..4)
            .map(|i| ProbeTask::new(&format!("t{i}"), &format!("n{i}"), &trace).arc())
            .collect();
        let mut plan = dag_plan(tasks, &[]);
        plan.rolling = vec![RollingGroup {
            tasks: vec![0, 1, 2, 3],
            max_parallel: 2,
        }];
        let outcome = run_dag(&mut plan, &pool(), settings(8, 4)).await;
        outcome.result.unwrap();

        let log = trace.log();
        let pos = |entry: &str| {
            log.iter()
                .position(|line| line == entry)
                .unwrap_or_else(|| panic!("missing {entry:?} in {log:?}"))
        };
        let first_batch_done = pos("finish t0").max(pos("finish t1"));
        let second_batch_start = pos("start t2").min(pos("start t3"));
        assert!(
            first_batch_done < second_batch_start,
            "batch 2 waits for batch 1: {log:?}"
        );
    }

    #[tokio::test]
    async fn rolling_group_stops_after_a_strict_failure() {
        let trace = Arc::new(Trace::default());
        let tasks = vec![
            ProbeTask::new("t0", "n1", &trace).arc(),
            ProbeTask::new("t1", "n2", &trace).failing().arc(),
            ProbeTask::new("t2", "n3", &trace).arc(),
            ProbeTask::new("t3", "n4", &trace).arc(),
        ];
        let mut plan = dag_plan(tasks, &[]);
        plan.rolling = vec![RollingGroup {
            tasks: vec![0, 1, 2, 3],
            max_parallel: 1,
        }];
        let outcome = run_dag(&mut plan, &pool(), settings(8, 4)).await;
        assert!(outcome.result.is_err(), "the group failure fails the run");

        let log = trace.log();
        assert!(log.contains(&"start t1".to_string()), "{log:?}");
        assert!(
            !log.iter()
                .any(|line| line == "start t2" || line == "start t3"),
            "the rest of the group never starts: {log:?}"
        );
        assert_eq!(
            skip_reason(&outcome.events, "t2").as_deref(),
            Some("blocked by t1")
        );
        assert_eq!(
            skip_reason(&outcome.events, "t3").as_deref(),
            Some("blocked by t1")
        );
    }

    #[tokio::test]
    async fn cancel_stops_new_starts_and_lets_in_flight_tasks_finish() {
        let token = tokio_util::sync::CancellationToken::new();
        let gate = Arc::new(tokio::sync::Notify::new());
        let trace = Arc::new(Trace::default());

        let cancel = token.clone();
        let notify = Arc::clone(&gate);
        let tasks = vec![
            ProbeTask::new("a", "n1", &trace)
                .on_start(Arc::new(move || {
                    cancel.cancel();
                    notify.notify_one();
                }))
                .arc(),
            ProbeTask::new("b", "n2", &trace)
                .gated(Arc::clone(&gate))
                .arc(),
            ProbeTask::new("c", "n3", &trace).arc(),
        ];
        let mut plan = dag_plan(tasks, &[]);

        let mut run = RunContext::new("op-test", "demo", ArtifactSources::embedded());
        run.cancel = token;
        run.settings = settings(2, 4);
        let run = Arc::new(run);
        let bus = Arc::clone(&run.events);
        let mut rx = bus.subscribe();
        let mut events = Vec::new();
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            run_with_bus(
                &mut plan,
                &pool(),
                &run,
                &bus,
                &mut |event| events.push(event),
                RunOptions::default(),
            ),
        )
        .await
        .expect("in-flight tasks finish after cancellation");

        let err = result.unwrap_err();
        assert!(is_cancelled(&err), "expected Cancelled, got {err:?}");
        let log = trace.log();
        assert!(
            !log.iter().any(|line| line == "start c"),
            "no task starts after the cancel token fires: {log:?}"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::TaskDone { id, .. } if id == "b")),
            "the in-flight sibling finished: {events:?}"
        );
        assert!(!events
            .iter()
            .any(|e| matches!(e, Event::ClusterReady { .. })));
        let bodies: Vec<Body> = std::iter::from_fn(|| rx.try_recv().ok())
            .map(|envelope| envelope.body)
            .collect();
        assert!(!bodies.iter().any(|b| matches!(b, Body::ClusterReady)));
    }
}
