//! Stage executor over the plan DAG. Stages serialize; tasks within a stage
//! parallelize across nodes. Every task follows check -> apply -> verify;
//! `Unknown` outcomes are re-probed, never blind-retried.
//!
//! Event flow: the parent task owns the `emit` callback and drains the
//! per-stage event channel *while* the workers run (`tokio::select!` between
//! `JoinSet::join_next` and `rx.recv()`), so events stream to the caller in
//! near real time instead of arriving after the whole stage joins. Every task
//! produces exactly one terminal event after its `TaskStart`:
//! `TaskSkipped | TaskDone | TaskReconciled | TaskFailed`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use miette::Result;
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};
use tokio::task::JoinSet;

use crate::engine::{Drift, Event, Outcome, PlannedTask, Session, TransportLost};
use crate::plan::Plan;
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
    let mut state = match opts.state_file {
        Some(path) => RunState::open(path, opts.cluster, opts.resume)?,
        None => RunState::disabled(),
    };

    for &idx in &plan.stages {
        let node = &mut plan.graph[idx];
        let stage = node.stage;
        let tasks = std::mem::take(&mut node.tasks);
        if tasks.is_empty() {
            emit(Event::StageDone {
                stage: stage.label(),
            });
            node.tasks = tasks;
            continue;
        }

        // Unbounded so workers never block on the event sink; ordering within
        // a single task is preserved (one producer per task).
        let (tx, mut rx) = unbounded_channel::<Event>();
        let mut set: JoinSet<Result<()>> = JoinSet::new();
        for planned in tasks.iter().cloned() {
            set.spawn(execute_one(planned, Arc::clone(pool), tx.clone()));
        }
        drop(tx);

        // Fail-fast between stages, but in-flight siblings settle first:
        // every failure is collected, the first is propagated.
        let mut first_err: Option<miette::Report> = None;
        let mut failures = 0usize;
        let mut rx_open = true;
        while rx_open || !set.is_empty() {
            tokio::select! {
                biased;
                res = set.join_next(), if !set.is_empty() => {
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
                ev = rx.recv(), if rx_open => {
                    match ev {
                        Some(ev) => {
                            // Persist eagerly so a SIGINT between tasks finds
                            // a complete snapshot on disk.
                            match &ev {
                                Event::TaskDone { id, .. }
                                | Event::TaskReconciled { id, .. }
                                | Event::TaskSkipped { id, .. } => {
                                    state.mark_done(id);
                                    if let Err(e) = state.flush() {
                                        eprintln!("warn: {e:?}");
                                    }
                                }
                                _ => {}
                            }
                            emit(ev);
                        }
                        None => rx_open = false,
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
        emit(Event::StageDone {
            stage: stage.label(),
        });
    }

    state.flush()?;
    emit(Event::ClusterReady);
    Ok(())
}

/// Find a [`TransportLost`] anywhere in the error chain (tasks may add
/// context around a transport failure).
fn transport_lost_reason(err: &miette::Report) -> Option<String> {
    err.chain()
        .find_map(|cause| cause.downcast_ref::<TransportLost>())
        .map(|lost| lost.to_string())
}

async fn execute_one(
    planned: PlannedTask,
    pool: Arc<dyn SessionPool>,
    tx: UnboundedSender<Event>,
) -> Result<()> {
    let task = planned.task;
    let id = task.id();
    let node = task.node().to_string();
    let stage = task.stage().label();

    // Emitted before anything can fail, so every task always starts its
    // event sequence — a session failure still yields TaskFailed.
    let _ = tx.send(Event::TaskStart {
        id: id.clone(),
        node: node.clone(),
        stage,
    });

    let result: Result<()> = async {
        let session = pool.get(&node).await?;

        match task.check(session.as_ref()).await? {
            Drift::InSync => {
                // Covers the resume path: a recorded-done task that is still
                // in sync is skipped exactly like a fresh no-op.
                let _ = tx.send(Event::TaskSkipped {
                    id: id.clone(),
                    node: node.clone(),
                    stage,
                });
                return Ok(());
            }
            Drift::Drifted(_reason) => {}
        }

        let started = Instant::now();
        let outcome = match task.apply(session.as_ref()).await {
            Ok(outcome) => outcome,
            // A lost connection during a mutating apply means the remote
            // state is unknown: re-probe instead of blindly retrying.
            Err(err) => match transport_lost_reason(&err) {
                Some(reason) => Outcome::Unknown(reason),
                None => return Err(err),
            },
        };
        let ms = started.elapsed().as_millis();

        match outcome {
            Outcome::Verified => {
                let _ = tx.send(Event::TaskDone {
                    id: id.clone(),
                    node: node.clone(),
                    stage,
                    ms,
                });
                Ok(())
            }
            Outcome::Unknown(reason) => {
                // Recovery rule: probe again before deciding anything.
                match task.check(session.as_ref()).await? {
                    Drift::InSync => {
                        let _ = tx.send(Event::TaskReconciled {
                            id: id.clone(),
                            node: node.clone(),
                            stage,
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
        let _ = tx.send(Event::TaskFailed {
            id,
            node,
            stage,
            reason: err.to_string(),
        });
    }
    result
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
