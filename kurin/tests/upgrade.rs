//! WP-13: upgrade is a plan. These tests pin the plan shape (per-node task
//! chain + rolling groups), the WP-01 drain semantics through the executor,
//! and the already-upgraded (InSync) path. Self-contained mocks — no
//! network, no shared `support` module.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr};
use std::path::Path;
use std::sync::{Arc, Mutex};

use kurin::artifact::{Arch, DirArtifact, DirManifest};
use kurin::render::render_tree;
use kurin::upgrade::{self, BinSource, UpgradeOptions};
use kurin::{Event, ExecOutput, Plan, Session, SessionPool};
use kurin_config::{Cluster, Node, NodeRole, ResolvedSsh, SshAuth};
use miette::Result;
use semver::Version;

// ---------------------------------------------------------------------------
// Mocks
// ---------------------------------------------------------------------------

fn ok(stdout: &str) -> ExecOutput {
    ExecOutput {
        exit_code: 0,
        stdout: stdout.into(),
        stderr: String::new(),
    }
}

struct MockSession {
    /// Every command the node received, in order.
    commands: Mutex<Vec<String>>,
    /// Every `write_file` target, in order.
    writes: Mutex<Vec<String>>,
    /// `kubectl get node … kubeletVersion` answer.
    kubelet_version: String,
    /// Whether `kubectl drain <node>` fails for a specific node.
    drain_fail_on: Option<String>,
    /// Nodes this session has cordoned through the API.
    cordoned: Mutex<HashSet<String>>,
}

impl MockSession {
    fn new(kubelet_version: &str, drain_fail_on: Option<&str>) -> Self {
        MockSession {
            commands: Mutex::new(Vec::new()),
            writes: Mutex::new(Vec::new()),
            kubelet_version: kubelet_version.into(),
            drain_fail_on: drain_fail_on.map(str::to_string),
            cordoned: Mutex::new(HashSet::new()),
        }
    }

    fn commands(&self) -> Vec<String> {
        self.commands.lock().unwrap().clone()
    }
}

fn node_after<'a>(command: &'a str, verb: &str) -> Option<&'a str> {
    let rest = command.split_once(verb)?.1;
    rest.split_whitespace().next()
}

#[async_trait::async_trait]
impl Session for MockSession {
    async fn exec(&self, command: &str) -> Result<ExecOutput> {
        self.commands.lock().unwrap().push(command.to_string());
        if command.contains("kubeletVersion") {
            return Ok(ok(&self.kubelet_version));
        }
        if command.contains("conditions[?(@.type==") {
            return Ok(ok("True"));
        }
        if command.contains("unschedulable") {
            let node = node_after(command, "get node ").unwrap_or_default();
            let cordoned = self.cordoned.lock().unwrap().contains(node);
            return Ok(ok(if cordoned { "true" } else { "" }));
        }
        if command.contains(" uncordon ") {
            if let Some(node) = node_after(command, "uncordon ") {
                self.cordoned.lock().unwrap().remove(node);
            }
            return Ok(ok(""));
        }
        if command.contains(" cordon ") {
            if let Some(node) = node_after(command, "cordon ") {
                self.cordoned.lock().unwrap().insert(node.to_string());
            }
            return Ok(ok(""));
        }
        if command.contains(" drain ") {
            let fails = self
                .drain_fail_on
                .as_deref()
                .is_some_and(|node| command.contains(&format!(" drain {node} ")));
            if fails {
                let node = self.drain_fail_on.as_deref().unwrap_or("node");
                return Ok(ExecOutput {
                    exit_code: 1,
                    stdout: format!("node/{node} cordoned\nnode/{node} drained\n"),
                    stderr: format!(
                        "error: unable to drain node \"{node}\": cannot delete Pods with local storage\n"
                    ),
                });
            }
            return Ok(ok("node drained\n"));
        }
        if command.starts_with("chmod 0755") {
            return Ok(ok("installed"));
        }
        Ok(ok(""))
    }

    async fn write_file(&self, path: &str, _contents: &[u8]) -> Result<()> {
        self.writes.lock().unwrap().push(path.to_string());
        Ok(())
    }

    async fn file_sha256(&self, _path: &str) -> Result<Option<String>> {
        Ok(None)
    }

    async fn service_active(&self, _name: &str) -> Result<bool> {
        Ok(true)
    }
}

struct MockPool {
    sessions: HashMap<String, Arc<MockSession>>,
}

#[async_trait::async_trait]
impl SessionPool for MockPool {
    async fn get(&self, node: &str) -> Result<Arc<dyn Session>> {
        Ok(self
            .sessions
            .get(node)
            .unwrap_or_else(|| panic!("node {node} not in mock pool"))
            .clone() as Arc<dyn Session>)
    }
}

/// Planner-only pool: `build_plan` never opens a session.
struct NoPool;

#[async_trait::async_trait]
impl SessionPool for NoPool {
    async fn get(&self, node: &str) -> Result<Arc<dyn Session>> {
        panic!("plan construction must not open a session to {node}")
    }
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn node(name: &str, role: NodeRole) -> Node {
    Node {
        name: name.into(),
        role,
        address: IpAddr::V4(Ipv4Addr::new(10, 0, 0, 11)),
        internal_address: IpAddr::V4(Ipv4Addr::new(10, 0, 0, 11)),
        ssh: ResolvedSsh {
            user: "root".into(),
            port: 22,
            r#become: kurin_config::BecomeMode::Auto,
            become_password: None,
            known_hosts: Default::default(),
            auth: SshAuth::Password {
                password: "not-a-real-password".into(),
            },
        },
        labels: BTreeMap::new(),
        arch: None,
    }
}

fn cluster(name: &str, cps: usize, workers: usize) -> Cluster {
    let mut nodes = Vec::new();
    for i in 1..=cps {
        nodes.push(node(&format!("cp{i}"), NodeRole::ControlPlane));
    }
    for i in 1..=workers {
        nodes.push(node(&format!("w{i}"), NodeRole::Worker));
    }
    Cluster {
        name: name.into(),
        offline_dir: None,
        zone: None,
        distribution: kurin_config::Distribution::K8s,
        ha_mode: kurin_config::HaMode::KubeVip,
        runtime: kurin_config::RuntimeKind::Containerd,
        kubernetes_version: Version::parse("1.33.2").unwrap(),
        pod_cidr: "10.244.0.0/16".parse().unwrap(),
        service_cidr: "10.96.0.0/12".parse().unwrap(),
        dns_domain: "cluster.local".into(),
        cni: Default::default(),
        vip: None,
        nodes,
        registry_kind: None,
        registry_port: 5000,
        registry_username: None,
        registry_password: None,
        registry_data_dir: "/var/lib/kurin/registry".into(),
        storage_kind: None,
        storage_default: false,
        storage_server: None,
        storage_path: None,
        source: String::new(),
    }
}

fn run(cluster: &Cluster) -> Arc<kurin::run::RunContext> {
    Arc::new(kurin::run::RunContext::new(
        "test-op",
        &cluster.name,
        kurin::run::ArtifactSources::embedded(),
    ))
}

fn target() -> Version {
    Version::parse("1.33.13").unwrap()
}

fn plan_for(cluster: &Cluster, opts: UpgradeOptions) -> Plan {
    let facts = kurin::facts::FactsTable::synthetic(cluster).unwrap();
    let pool: Arc<dyn SessionPool> = Arc::new(NoPool);
    let run = run(cluster);
    upgrade::build_plan(
        cluster,
        &target(),
        &pool,
        &facts,
        opts,
        Arc::new(BinSource::Unavailable),
        &run,
    )
    .expect("upgrade plan builds")
}

fn render_plan(plan: &Plan) -> String {
    let mut out = render_tree(plan);
    out.push_str("\nrolling groups:\n");
    for group in &plan.rolling {
        let ids: Vec<String> = group
            .tasks
            .iter()
            .map(|&i| plan.tasks[i].task.id())
            .collect();
        out.push_str(&format!(
            "- max_parallel={} [{}]\n",
            group.max_parallel,
            ids.join(", ")
        ));
    }
    out
}

/// An offline bundle carrying the three kube binaries for amd64.
fn bundle(dir: &Path) -> DirManifest {
    let artifacts = ["kubeadm", "kubelet", "kubectl"]
        .into_iter()
        .map(|name| {
            let file = dir.join(name);
            std::fs::write(&file, b"fake-binary").unwrap();
            DirArtifact {
                name: name.into(),
                version: "1.33.13".into(),
                arch: Arch::Amd64,
                kind: None,
                file,
                sha256: String::new(),
            }
        })
        .collect();
    DirManifest {
        path: dir.to_path_buf(),
        artifacts,
    }
}

fn sessions(cluster: &Cluster, kubelet_version: &str, drain_fail_on: Option<&str>) -> MockPool {
    let mut map = HashMap::new();
    for node in &cluster.nodes {
        map.insert(
            node.name.clone(),
            Arc::new(MockSession::new(kubelet_version, drain_fail_on)),
        );
    }
    MockPool { sessions: map }
}

fn collect_events() -> (Arc<Mutex<Vec<Event>>>, impl FnMut(Event) + Send + 'static) {
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&events);
    (events, move |event| sink.lock().unwrap().push(event))
}

// ---------------------------------------------------------------------------
// Plan snapshots
// ---------------------------------------------------------------------------

#[test]
fn upgrade_plan_snapshot_1cp_1w() {
    let cluster = cluster("one-and-one", 1, 1);
    let plan = plan_for(&cluster, UpgradeOptions::default());
    insta::assert_snapshot!("upgrade_plan_1cp_1w", render_plan(&plan));
}

#[test]
fn upgrade_plan_snapshot_3cp_3w() {
    let cluster = cluster("three-and-three", 3, 3);
    let plan = plan_for(&cluster, UpgradeOptions::default());
    insta::assert_snapshot!("upgrade_plan_3cp_3w", render_plan(&plan));
}

#[test]
fn upgrade_plan_snapshot_3cp_3w_batch2() {
    let cluster = cluster("three-and-three-batch2", 3, 3);
    let plan = plan_for(
        &cluster,
        UpgradeOptions {
            batch: 2,
            force_drain_failure: false,
        },
    );
    insta::assert_snapshot!("upgrade_plan_3cp_3w_batch2", render_plan(&plan));
}

// ---------------------------------------------------------------------------
// Executor behavior
// ---------------------------------------------------------------------------

/// A failed drain returns the documented error and blocks the rest of the
/// plan: no later step on the node, no later node, no uncordon.
#[tokio::test]
async fn drain_failure_blocks_later_nodes_and_batches() {
    let cluster = cluster("drain-blocks", 1, 2);
    let facts = kurin::facts::FactsTable::synthetic(&cluster).unwrap();
    let pool = Arc::new(sessions(&cluster, "v1.32.0", Some("cp1")));
    let dyn_pool: Arc<dyn SessionPool> = pool.clone();
    let dir = tempfile::tempdir().unwrap();
    let source = Arc::new(BinSource::Bundle(bundle(dir.path())));
    let run = run(&cluster);
    let plan = upgrade::build_plan(
        &cluster,
        &target(),
        &dyn_pool,
        &facts,
        UpgradeOptions::default(),
        source,
        &run,
    )
    .unwrap();

    let (events, mut emit) = collect_events();
    let err = upgrade::execute(plan, &dyn_pool, &run, &mut emit)
        .await
        .expect_err("a failed drain must abort the upgrade");
    let msg = err.to_string();
    assert!(
        msg.contains("drain cp1 failed:"),
        "unexpected error text: {msg}"
    );
    assert!(
        msg.contains("error: unable to drain node"),
        "error must carry the first non-empty stderr line: {msg}"
    );

    // Nothing after the drain ran on the first control plane.
    let cp1 = pool.sessions["cp1"].commands();
    assert!(
        !cp1.iter().any(|c| c.contains("kubeadm")),
        "kubeadm ran after a failed drain: {cp1:?}"
    );
    assert!(
        !cp1.iter().any(|c| c.contains("uncordon")),
        "node was uncordoned after a failed drain: {cp1:?}"
    );
    assert!(
        !cp1.iter().any(|c| c.starts_with("chmod 0755")),
        "binaries were installed after a failed drain: {cp1:?}"
    );

    // Later nodes never got a session command.
    for node in ["w1", "w2"] {
        assert!(
            pool.sessions[node].commands().is_empty(),
            "{node} was touched after a failed drain: {:?}",
            pool.sessions[node].commands()
        );
    }

    // The blocked nodes are reported, not silently skipped.
    let events = events.lock().unwrap();
    assert!(events.iter().any(|event| matches!(
        event,
        Event::TaskSkipped { id, reason: Some(reason), .. }
            if id == "version-check/version-check@w1" && reason.contains("blocked by")
    )));
}

/// A failed drain in a worker batch blocks the rest of that batch and every
/// later batch.
#[tokio::test]
async fn drain_failure_blocks_later_worker_batches() {
    let cluster = cluster("worker-batches", 1, 3);
    let facts = kurin::facts::FactsTable::synthetic(&cluster).unwrap();
    let pool = Arc::new(sessions(&cluster, "v1.32.0", Some("w1")));
    let dyn_pool: Arc<dyn SessionPool> = pool.clone();
    let dir = tempfile::tempdir().unwrap();
    let source = Arc::new(BinSource::Bundle(bundle(dir.path())));
    let run = run(&cluster);
    let plan = upgrade::build_plan(
        &cluster,
        &target(),
        &dyn_pool,
        &facts,
        UpgradeOptions {
            batch: 2,
            force_drain_failure: false,
        },
        source,
        &run,
    )
    .unwrap();

    let (events, mut emit) = collect_events();
    let err = upgrade::execute(plan, &dyn_pool, &run, &mut emit)
        .await
        .expect_err("a failed worker drain must abort the upgrade");
    assert!(
        err.to_string().contains("drain w1 failed:"),
        "unexpected error: {err}"
    );

    // The later batch never starts. The other member of the failing batch
    // may finish its in-flight drain (the executor lets in-flight tasks
    // settle), but it must not be upgraded or uncordoned.
    assert!(
        pool.sessions["w3"].commands().is_empty(),
        "w3 was touched after w1's failed drain: {:?}",
        pool.sessions["w3"].commands()
    );
    let w2 = pool.sessions["w2"].commands();
    for forbidden in ["chmod 0755", "kubeadm", "systemctl"] {
        assert!(
            !w2.iter().any(|c| c.contains(forbidden)),
            "w2 continued past its drain after w1 failed: {w2:?}"
        );
    }
    let cp1 = pool.sessions["cp1"].commands();
    assert!(
        !cp1.iter().any(|c| c.contains("drain w3")),
        "a later batch was drained: {cp1:?}"
    );
    assert!(
        !cp1.iter()
            .any(|c| c.contains("uncordon w1") || c.contains("uncordon w2")),
        "a failed batch was uncordoned: {cp1:?}"
    );

    let events = events.lock().unwrap();
    assert!(events.iter().any(|event| matches!(
        event,
        Event::TaskSkipped { id, reason: Some(reason), .. }
            if id == "version-check/version-check@w3" && reason.contains("blocked by")
    )));
}

/// `--force-drain-failure` keeps the pre-WP-01 behavior inside the plan:
/// the warning is reported and the node continues through the chain.
#[tokio::test]
async fn forced_drain_failure_continues_through_the_plan() {
    let cluster = cluster("forced-drain", 1, 1);
    let facts = kurin::facts::FactsTable::synthetic(&cluster).unwrap();
    let pool = Arc::new(sessions(&cluster, "v1.32.0", Some("cp1")));
    let dyn_pool: Arc<dyn SessionPool> = pool.clone();
    let dir = tempfile::tempdir().unwrap();
    let source = Arc::new(BinSource::Bundle(bundle(dir.path())));
    let run = run(&cluster);
    let plan = upgrade::build_plan(
        &cluster,
        &target(),
        &dyn_pool,
        &facts,
        UpgradeOptions {
            batch: 1,
            force_drain_failure: true,
        },
        source,
        &run,
    )
    .unwrap();

    let (_, mut emit) = collect_events();
    upgrade::execute(plan, &dyn_pool, &run, &mut emit)
        .await
        .expect("--force-drain-failure must keep going");

    let cp1 = pool.sessions["cp1"].commands();
    assert!(
        cp1.iter()
            .any(|c| c.contains("kubeadm upgrade apply v1.33.13")),
        "first control plane did not upgrade: {cp1:?}"
    );
    assert!(
        cp1.iter().any(|c| c.contains("uncordon cp1")),
        "first control plane was not uncordoned: {cp1:?}"
    );
    let w1 = pool.sessions["w1"].commands();
    assert!(
        w1.iter().any(|c| c == "kubeadm upgrade node"),
        "worker did not upgrade: {w1:?}"
    );
}

/// A node whose kubelet already matches the target is InSync: every chain
/// step is skipped and no mutating command runs.
#[tokio::test]
async fn already_upgraded_node_is_in_sync() {
    let cluster = cluster("already-upgraded", 1, 1);
    let facts = kurin::facts::FactsTable::synthetic(&cluster).unwrap();
    let pool = Arc::new(sessions(&cluster, "v1.33.13", None));
    let dyn_pool: Arc<dyn SessionPool> = pool.clone();
    let run = run(&cluster);
    let plan = upgrade::build_plan(
        &cluster,
        &target(),
        &dyn_pool,
        &facts,
        UpgradeOptions::default(),
        Arc::new(BinSource::Unavailable),
        &run,
    )
    .unwrap();

    let chain_ids: HashSet<String> = plan
        .tasks
        .iter()
        .map(|task| task.task.id())
        .filter(|id| !id.starts_with("discover/") && !id.starts_with("post/"))
        .collect();

    let (events, mut emit) = collect_events();
    upgrade::execute(plan, &dyn_pool, &run, &mut emit)
        .await
        .expect("an already-upgraded cluster is a no-op");

    let events = events.lock().unwrap();
    for id in &chain_ids {
        assert!(
            events.iter().any(|event| matches!(
                event,
                Event::TaskSkipped { id: skipped, .. } if skipped == id
            )),
            "{id} was not skipped as InSync: {events:?}"
        );
    }
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::TaskFailed { .. })),
        "no task may fail on an already-upgraded cluster: {events:?}"
    );

    // No mutation reached either node.
    for name in ["cp1", "w1"] {
        let commands = pool.sessions[name].commands();
        for forbidden in [
            "cordon",
            " drain ",
            "kubeadm",
            "systemctl",
            "chmod 0755",
            "uncordon",
        ] {
            assert!(
                !commands.iter().any(|c| c.contains(forbidden)),
                "{name} ran `{forbidden}` on an already-upgraded node: {commands:?}"
            );
        }
    }
}
