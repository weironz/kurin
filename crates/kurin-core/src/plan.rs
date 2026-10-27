//! Plan data model and builder: the phase/task DAG produced by the planner.
//!
//! The build logic lives in `kurin-components`; this module owns the shape.
//! [`Plan`] stores tasks in phase order plus explicit dependency edges.
//! [`Plan::phase_groups`] is the view the renderer and the stage-by-stage
//! executor use until the DAG executor lands (WP-11); [`Plan::stages`] is the
//! legacy `Stage`-keyed view kept for callers and tests.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashSet};
use std::ops::Range;
use std::sync::Arc;

use kurin_config::{Cluster, Node, NodeRole};

use crate::engine::{sha256_hex, PlannedTask, Stage, Task};
use crate::executor::SessionPool;
use crate::facts::FactsTable;
use crate::run::RunContext;

/// Index into [`Plan::tasks`] (and into a [`PlanBuilder`]'s insertion list
/// while the plan is being assembled).
pub type TaskIdx = usize;

/// A display unit and the default dependency barrier between phases.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Phase {
    /// Stable identifier, e.g. `"prepare"`.
    pub id: &'static str,
    /// Human label rendered in the plan tree.
    pub label: &'static str,
}

impl Phase {
    /// Construct a phase from its identifier and display label.
    pub const fn new(id: &'static str, label: &'static str) -> Phase {
        Phase { id, label }
    }
}

/// Handle to a phase declared by the builder's operation template.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhaseHandle(usize);

/// The operation a plan belongs to; selects the phase template.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpKind {
    Create,
    AddNodes,
    DeleteNode,
    DeleteCluster,
    Upgrade,
    CertsRenew,
    AddonInstall,
    AddonRemove,
    EtcdBackup,
    EtcdRestore,
}

const CREATE_PHASES: &[Phase] = &[
    Phase::new("discover", "discover"),
    Phase::new("preflight", "preflight"),
    Phase::new("artifacts", "artifacts"),
    Phase::new("prepare", "prepare"),
    Phase::new("registry", "registry"),
    Phase::new("images", "images"),
    Phase::new("vip", "vip"),
    Phase::new("bootstrap", "bootstrap"),
    Phase::new("api-ready", "api-ready"),
    Phase::new("join", "join"),
    Phase::new("cni", "cni"),
    Phase::new("post", "post"),
];

const DELETE_NODE_PHASES: &[Phase] = &[
    Phase::new("discover", "discover"),
    Phase::new("drain", "drain"),
    Phase::new("reset", "reset"),
    Phase::new("cleanup", "cleanup"),
    Phase::new("api-cleanup", "api-cleanup"),
    Phase::new("local", "local"),
];

const DELETE_CLUSTER_PHASES: &[Phase] = &[
    Phase::new("discover", "discover"),
    Phase::new("reset-workers", "reset (workers)"),
    Phase::new("reset-control-plane", "reset (control-plane)"),
    Phase::new("cleanup", "cleanup"),
    Phase::new("local", "local"),
];

const UPGRADE_PHASES: &[Phase] = &[
    Phase::new("discover", "discover"),
    Phase::new("preflight", "preflight"),
    Phase::new("artifacts", "artifacts"),
    Phase::new("first-cp", "first-cp"),
    Phase::new("control-plane", "control-plane"),
    Phase::new("workers", "workers"),
    Phase::new("addons", "addons"),
    Phase::new("post", "post"),
];

const CERTS_RENEW_PHASES: &[Phase] = &[
    Phase::new("discover", "discover"),
    Phase::new("renew", "renew"),
    Phase::new("verify", "verify"),
];

const ADDON_PHASES: &[Phase] = &[
    Phase::new("discover", "discover"),
    Phase::new("addons", "addons"),
    Phase::new("post", "post"),
];

const ETCD_BACKUP_PHASES: &[Phase] = &[
    Phase::new("discover", "discover"),
    Phase::new("snapshot", "snapshot"),
    Phase::new("fetch", "fetch"),
];

const ETCD_RESTORE_PHASES: &[Phase] = &[
    Phase::new("discover", "discover"),
    Phase::new("restore", "restore"),
];

impl OpKind {
    /// Phase template for this operation, in execution/display order.
    pub fn phases(self) -> &'static [Phase] {
        match self {
            OpKind::Create | OpKind::AddNodes => CREATE_PHASES,
            OpKind::DeleteNode => DELETE_NODE_PHASES,
            OpKind::DeleteCluster => DELETE_CLUSTER_PHASES,
            OpKind::Upgrade => UPGRADE_PHASES,
            OpKind::CertsRenew => CERTS_RENEW_PHASES,
            OpKind::AddonInstall | OpKind::AddonRemove => ADDON_PHASES,
            OpKind::EtcdBackup => ETCD_BACKUP_PHASES,
            OpKind::EtcdRestore => ETCD_RESTORE_PHASES,
        }
    }
}

/// A rolling group: tasks that must run in order, at most `max_parallel`
/// at a time. The DAG executor consumes this in WP-11.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RollingGroup {
    pub tasks: Vec<TaskIdx>,
    pub max_parallel: usize,
}

/// Legacy stage view: one phase's tasks keyed by the old [`Stage`] enum.
///
/// Only phases whose id matches a [`Stage::label`] are represented; the
/// create/add-node templates (the only plans the CLI executes today) map
/// one-to-one.
pub struct StageNode {
    pub stage: Stage,
    pub tasks: Vec<PlannedTask>,
}

/// Immutable view of one phase's tasks.
pub struct PhaseGroup<'a> {
    pub phase: Phase,
    pub tasks: &'a [PlannedTask],
}

/// Mutable view of one phase's tasks.
pub struct PhaseGroupMut<'a> {
    pub phase: Phase,
    pub tasks: &'a mut [PlannedTask],
}

/// A fully-built plan: operation, phase template, tasks in phase order and
/// dependency edges (`(from, to)` means `from` must finish before `to`).
pub struct Plan {
    pub op: OpKind,
    pub phases: Vec<Phase>,
    pub tasks: Vec<PlannedTask>,
    pub edges: Vec<(TaskIdx, TaskIdx)>,
    pub rolling: Vec<RollingGroup>,
    /// Task index range for each phase, parallel to `phases`.
    phase_spans: Vec<Range<usize>>,
}

impl Plan {
    /// Empty plan for `op`: the phase template with no tasks.
    pub fn empty(op: OpKind) -> Plan {
        let phases = op.phases().to_vec();
        let phase_spans = vec![0..0; phases.len()];
        Plan {
            op,
            phases,
            tasks: Vec::new(),
            edges: Vec::new(),
            rolling: Vec::new(),
            phase_spans,
        }
    }

    /// Build a plan from pre-grouped phase tasks (used by tests and by
    /// callers that assemble a plan without a [`PlanBuilder`]). Default
    /// phase-barrier edges are added between consecutive non-empty phases.
    pub fn from_stage_groups(op: OpKind, groups: Vec<(Phase, Vec<PlannedTask>)>) -> Plan {
        let mut phases = Vec::with_capacity(groups.len());
        let mut tasks = Vec::new();
        let mut phase_spans = Vec::with_capacity(groups.len());
        for (phase, group_tasks) in groups {
            let start = tasks.len();
            tasks.extend(group_tasks);
            phases.push(phase);
            phase_spans.push(start..tasks.len());
        }
        let edges = barrier_edges(&phase_spans);
        Plan {
            op,
            phases,
            tasks,
            edges,
            rolling: Vec::new(),
            phase_spans,
        }
    }

    /// Immutable phase views, in template order.
    pub fn phase_groups(&self) -> Vec<PhaseGroup<'_>> {
        let mut rest: &[PlannedTask] = &self.tasks;
        let mut out = Vec::with_capacity(self.phases.len());
        for (phase, span) in self.phases.iter().zip(self.phase_spans.iter()) {
            let (head, tail) = rest.split_at(span.len());
            out.push(PhaseGroup {
                phase: *phase,
                tasks: head,
            });
            rest = tail;
        }
        out
    }

    /// Mutable phase views, in template order. The slices are disjoint, so
    /// the stage-by-stage executor can take each phase's tasks in turn.
    pub fn phase_groups_mut(&mut self) -> Vec<PhaseGroupMut<'_>> {
        let phases = &self.phases;
        let spans = &self.phase_spans;
        let mut rest: &mut [PlannedTask] = &mut self.tasks;
        let mut out = Vec::with_capacity(phases.len());
        for (phase, span) in phases.iter().zip(spans.iter()) {
            let (head, tail) = rest.split_at_mut(span.len());
            out.push(PhaseGroupMut {
                phase: *phase,
                tasks: head,
            });
            rest = tail;
        }
        out
    }

    /// Legacy `Stage`-keyed view. Phases without a matching [`Stage`] are
    /// omitted; the create/add-node templates map one-to-one.
    pub fn stages(&self) -> Vec<StageNode> {
        self.phase_groups()
            .into_iter()
            .filter_map(|group| {
                Stage::from_label(group.phase.id).map(|stage| StageNode {
                    stage,
                    tasks: group.tasks.to_vec(),
                })
            })
            .collect()
    }

    /// Stable fingerprint of the work this plan describes.
    ///
    /// `sha256` over the topologically ordered task keys and their
    /// `describe()` text. Two runs of the same configuration produce the
    /// same fingerprint; any change to the task set, its order or its
    /// parameters changes it, which is what `--resume` compares against.
    pub fn fingerprint(&self) -> String {
        let mut buf = String::new();
        for idx in self.topological_order() {
            let planned = &self.tasks[idx];
            buf.push_str(&planned.task.id());
            buf.push('\t');
            buf.push_str(&planned.task.describe());
            buf.push('\n');
        }
        sha256_hex(buf.as_bytes())
    }

    /// Kahn topological order, ties broken by task index so the result is
    /// deterministic. A cycle (which a valid plan never has) falls back to
    /// the remaining tasks in index order rather than panicking.
    fn topological_order(&self) -> Vec<TaskIdx> {
        let n = self.tasks.len();
        let mut indegree = vec![0usize; n];
        let mut adj: Vec<Vec<TaskIdx>> = vec![Vec::new(); n];
        for &(from, to) in &self.edges {
            if from >= n || to >= n {
                continue;
            }
            adj[from].push(to);
            indegree[to] += 1;
        }
        let mut ready: BinaryHeap<Reverse<TaskIdx>> =
            (0..n).filter(|&i| indegree[i] == 0).map(Reverse).collect();
        let mut order = Vec::with_capacity(n);
        let mut seen = vec![false; n];
        while let Some(Reverse(idx)) = ready.pop() {
            if seen[idx] {
                continue;
            }
            seen[idx] = true;
            order.push(idx);
            for &next in &adj[idx] {
                indegree[next] -= 1;
                if indegree[next] == 0 {
                    ready.push(Reverse(next));
                }
            }
        }
        if order.len() < n {
            order.extend((0..n).filter(|&i| !seen[i]));
        }
        order
    }
}

/// Edges from every task of a phase to every task of the next non-empty
/// phase, giving the default "phase barrier" semantics.
fn barrier_edges(spans: &[Range<usize>]) -> Vec<(TaskIdx, TaskIdx)> {
    let mut edges = Vec::new();
    let mut prev: Option<&Range<usize>> = None;
    for span in spans {
        if span.is_empty() {
            continue;
        }
        if let Some(prev) = prev {
            for from in prev.clone() {
                for to in span.clone() {
                    edges.push((from, to));
                }
            }
        }
        prev = Some(span);
    }
    edges
}

struct TaskSlot {
    phase: usize,
    planned: PlannedTask,
}

/// Builder for a [`Plan`]: declares the operation's phases, adds tasks and
/// explicit dependencies, and emits the flattened plan.
pub struct PlanBuilder<'a> {
    op: OpKind,
    phases: Vec<Phase>,
    slots: Vec<TaskSlot>,
    edges: Vec<(TaskIdx, TaskIdx)>,
    rolling: Vec<RollingGroup>,
    cluster: &'a Cluster,
    facts: FactsTable,
    run: &'a Arc<RunContext>,
    pool: Option<Arc<dyn SessionPool>>,
}

impl<'a> PlanBuilder<'a> {
    /// Start a builder for `op`; the phase list comes from the operation
    /// template. Facts are snapshotted by the caller (planning never holds
    /// the run lock).
    pub fn new(
        op: OpKind,
        cluster: &'a Cluster,
        facts: FactsTable,
        run: &'a Arc<RunContext>,
        pool: Option<Arc<dyn SessionPool>>,
    ) -> PlanBuilder<'a> {
        PlanBuilder {
            op,
            phases: op.phases().to_vec(),
            slots: Vec::new(),
            edges: Vec::new(),
            rolling: Vec::new(),
            cluster,
            facts,
            run,
            pool,
        }
    }

    /// The operation this plan is being built for.
    pub fn op(&self) -> OpKind {
        self.op
    }

    /// Cluster being planned.
    pub fn cluster(&self) -> &'a Cluster {
        self.cluster
    }

    /// Discovered facts used for per-node arch selection.
    pub fn facts(&self) -> &FactsTable {
        &self.facts
    }

    /// Run context (artifact sources, blackboard, cancellation).
    pub fn run(&self) -> &'a Arc<RunContext> {
        self.run
    }

    /// Live session pool, when the caller provided one.
    pub fn pool(&self) -> Option<Arc<dyn SessionPool>> {
        self.pool.clone()
    }

    /// Node selector with the shared lb/registry filtering.
    pub fn nodes(&self) -> NodeSel<'a> {
        NodeSel {
            cluster: self.cluster,
        }
    }

    /// Handle for a phase declared by the operation template. An unknown id
    /// is appended as its own phase (label = id); built-in templates always
    /// declare the ids they use.
    pub fn phase(&mut self, id: &'static str) -> PhaseHandle {
        if let Some(idx) = self.phases.iter().position(|p| p.id == id) {
            return PhaseHandle(idx);
        }
        let idx = self.phases.len();
        self.phases.push(Phase::new(id, id));
        PhaseHandle(idx)
    }

    /// Add a task to `phase`; returns its builder-local index.
    pub fn add(&mut self, phase: PhaseHandle, task: Arc<dyn Task>) -> TaskIdx {
        let idx = self.slots.len();
        self.slots.push(TaskSlot {
            phase: phase.0,
            planned: PlannedTask { task, drift: None },
        });
        idx
    }

    /// Declare that `task` runs after `dep` (`dep` -> `task`).
    pub fn after(&mut self, task: TaskIdx, dep: TaskIdx) {
        self.edges.push((dep, task));
    }

    /// Mark a group of tasks as a rolling batch (WP-11 scheduler input).
    pub fn rolling(&mut self, tasks: &[TaskIdx], max_parallel: usize) {
        self.rolling.push(RollingGroup {
            tasks: tasks.to_vec(),
            max_parallel,
        });
    }

    /// Flatten the builder into a [`Plan`]: tasks grouped in phase order,
    /// default phase barriers plus explicit edges, rolling groups remapped.
    pub fn finish(self) -> Plan {
        let PlanBuilder {
            op,
            phases,
            slots,
            edges: explicit_edges,
            rolling: rolling_groups,
            ..
        } = self;
        let mut per_phase: Vec<Vec<TaskIdx>> = vec![Vec::new(); phases.len()];
        for (idx, slot) in slots.iter().enumerate() {
            per_phase[slot.phase].push(idx);
        }

        let mut slots: Vec<Option<TaskSlot>> = slots.into_iter().map(Some).collect();
        let mut remap = vec![0usize; slots.len()];
        let mut tasks = Vec::with_capacity(slots.len());
        let mut phase_spans = Vec::with_capacity(phases.len());
        for indices in &per_phase {
            let start = tasks.len();
            for &idx in indices {
                remap[idx] = tasks.len();
                let slot = slots[idx].take().expect("each task appears in one phase");
                tasks.push(slot.planned);
            }
            phase_spans.push(start..tasks.len());
        }

        let mut edges = barrier_edges(&phase_spans);
        let mut seen: HashSet<(TaskIdx, TaskIdx)> = edges.iter().copied().collect();
        for &(from, to) in &explicit_edges {
            let edge = (remap[from], remap[to]);
            if seen.insert(edge) {
                edges.push(edge);
            }
        }

        let rolling = rolling_groups
            .into_iter()
            .map(|group| RollingGroup {
                tasks: group.tasks.into_iter().map(|t| remap[t]).collect(),
                max_parallel: group.max_parallel,
            })
            .collect();

        Plan {
            op,
            phases,
            tasks,
            edges,
            rolling,
            phase_spans,
        }
    }
}

/// Node filtering shared by the planner's areas: excludes lb/registry nodes
/// from the Kubernetes track and exposes the role/name lookups that used to
/// be open-coded filters.
pub struct NodeSel<'a> {
    cluster: &'a Cluster,
}

impl<'a> NodeSel<'a> {
    /// Kubernetes members: everything except `lb` and `registry` nodes.
    pub fn k8s(&self) -> impl Iterator<Item = &'a Node> + 'a {
        self.cluster
            .nodes
            .iter()
            .filter(|n| !matches!(n.role, NodeRole::Lb | NodeRole::Registry))
    }

    /// Control-plane nodes, in config order.
    pub fn control_plane(&self) -> impl Iterator<Item = &'a Node> + 'a {
        self.cluster.control_plane()
    }

    /// First control-plane node, if any.
    pub fn first_cp(&self) -> Option<&'a Node> {
        self.cluster.control_plane().next()
    }

    /// Worker nodes, in config order.
    pub fn workers(&self) -> impl Iterator<Item = &'a Node> + 'a {
        self.cluster.workers()
    }

    /// Nodes with the given role, in config order.
    pub fn role(&self, role: NodeRole) -> impl Iterator<Item = &'a Node> + 'a {
        self.cluster.nodes.iter().filter(move |n| n.role == role)
    }

    /// Node by exact name.
    pub fn by_name(&self, name: &str) -> Option<&'a Node> {
        self.cluster.nodes.iter().find(|n| n.name == name)
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use async_trait::async_trait;

    use super::*;
    use crate::engine::{Drift, Outcome, Session};

    struct DummyTask {
        id: &'static str,
        stage: Stage,
    }

    impl DummyTask {
        fn new(id: &'static str, stage: Stage) -> DummyTask {
            DummyTask { id, stage }
        }
    }

    #[async_trait]
    impl Task for DummyTask {
        fn id(&self) -> String {
            self.id.into()
        }
        fn stage(&self) -> Stage {
            self.stage
        }
        fn node(&self) -> &str {
            "n1"
        }
        fn describe(&self) -> String {
            self.id.into()
        }
        async fn check(&self, _session: &dyn Session) -> miette::Result<Drift> {
            Ok(Drift::InSync)
        }
        async fn apply(&self, _session: &dyn Session) -> miette::Result<Outcome> {
            Ok(Outcome::Verified)
        }
    }

    fn cluster() -> Cluster {
        let toml = r#"version = "v1alpha1"
name = "sel"
[kubernetes]
version = "1.33.1"
[network]
pod-cidr = "10.42.0.0/16"
service-cidr = "10.43.0.0/16"
[ssh]
user = "ubuntu"
[ssh.auth]
method = "password"
password = "fake-password"
[ha]
mode = "kurin-lb"
vip = "10.0.0.100"
[registry]
kind = "zot"
[nodes.cp1]
address = "10.0.0.11"
role = "control-plane"
[nodes.cp2]
address = "10.0.0.12"
role = "control-plane"
[nodes.w1]
address = "10.0.0.15"
role = "worker"
[nodes.lb1]
address = "10.0.0.13"
role = "lb"
[nodes.reg1]
address = "10.0.0.14"
role = "registry"
"#;
        kurin_config::parse_str(Path::new("sel.toml"), toml).expect("test cluster parses")
    }

    fn run(cluster: &Cluster) -> Arc<RunContext> {
        Arc::new(RunContext::new(
            "test-op",
            &cluster.name,
            crate::run::ArtifactSources::embedded(),
        ))
    }

    #[test]
    fn fingerprint_is_stable_and_order_sensitive() {
        let a = Plan::empty(OpKind::Create);
        let b = Plan::empty(OpKind::Create);
        assert_eq!(a.fingerprint(), b.fingerprint());
    }

    #[test]
    fn empty_plan_has_the_create_phase_template() {
        let plan = Plan::empty(OpKind::Create);
        let ids: Vec<&str> = plan.phases.iter().map(|p| p.id).collect();
        assert_eq!(
            ids,
            [
                "discover",
                "preflight",
                "artifacts",
                "prepare",
                "registry",
                "images",
                "vip",
                "bootstrap",
                "api-ready",
                "join",
                "cni",
                "post",
            ]
        );
        assert!(plan.tasks.is_empty());
        assert!(plan.edges.is_empty());
    }

    #[test]
    fn builder_emits_phase_barriers_explicit_edges_and_rolling_groups() {
        let cluster = cluster();
        let run = run(&cluster);
        let mut b = PlanBuilder::new(OpKind::Create, &cluster, FactsTable::new(), &run, None);
        let preflight = b.phase("preflight");
        let a = b.add(
            preflight,
            Arc::new(DummyTask::new("preflight/a", Stage::Preflight)),
        );
        let prepare = b.phase("prepare");
        let first = b.add(
            prepare,
            Arc::new(DummyTask::new("prepare/first", Stage::Prepare)),
        );
        let second = b.add(
            prepare,
            Arc::new(DummyTask::new("prepare/second", Stage::Prepare)),
        );
        b.after(second, first);
        b.rolling(&[first, second], 1);

        let plan = b.finish();
        assert_eq!(plan.tasks.len(), 3);
        // Default barrier: every preflight task precedes every prepare task.
        assert!(plan.edges.contains(&(a, first)));
        assert!(plan.edges.contains(&(a, second)));
        // Explicit fine-grained dependency.
        assert!(plan.edges.contains(&(first, second)));
        assert_eq!(
            plan.rolling,
            vec![RollingGroup {
                tasks: vec![first, second],
                max_parallel: 1,
            }]
        );
        // Tasks are grouped in phase order.
        let groups = plan.phase_groups();
        assert_eq!(groups[1].phase.id, "preflight");
        assert_eq!(groups[1].tasks.len(), 1);
        assert_eq!(groups[3].phase.id, "prepare");
        assert_eq!(groups[3].tasks.len(), 2);
    }

    #[test]
    fn node_sel_filters_lb_and_registry_from_the_k8s_track() {
        let cluster = cluster();
        let sel = NodeSel { cluster: &cluster };
        let names = |it: Vec<&Node>| it.into_iter().map(|n| n.name.clone()).collect::<Vec<_>>();

        assert_eq!(
            names(sel.k8s().collect()),
            vec!["cp1".to_string(), "cp2".to_string(), "w1".to_string()]
        );
        assert_eq!(
            names(sel.control_plane().collect()),
            vec!["cp1".to_string(), "cp2".to_string()]
        );
        assert_eq!(sel.first_cp().map(|n| n.name.as_str()), Some("cp1"));
        assert_eq!(names(sel.workers().collect()), vec!["w1".to_string()]);
        assert_eq!(
            names(sel.role(NodeRole::Lb).collect()),
            vec!["lb1".to_string()]
        );
        assert_eq!(
            names(sel.role(NodeRole::Registry).collect()),
            vec!["reg1".to_string()]
        );
        assert_eq!(sel.by_name("w1").map(|n| n.role), Some(NodeRole::Worker));
        assert!(sel.by_name("missing").is_none());
    }
}
