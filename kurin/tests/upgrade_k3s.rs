//! WP-72: k3s upgrade is a plan (binary replacement + rolling restart).
//! These tests pin the k3s plan shape (per-node task chain + rolling
//! groups), the command sequence and order on servers vs agents, and the
//! already-upgraded (InSync) path. Self-contained mocks — no network, no
//! shared `support` module.

use std::collections::{BTreeMap, HashMap};
use std::net::{IpAddr, Ipv4Addr};
use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

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

/// One k3s node: a fixed `k3s --version` answer, an agent-unit answer, a
/// command log and a write log. `k3s kubectl get node` answers Ready.
struct MockSession {
    /// `k3s --version` report, e.g. `1.33.1+k3s1`.
    k3s_version: String,
    commands: Mutex<Vec<String>>,
    writes: Mutex<Vec<String>>,
}

impl MockSession {
    fn new(k3s_version: &str) -> Self {
        MockSession {
            k3s_version: k3s_version.into(),
            commands: Mutex::new(Vec::new()),
            writes: Mutex::new(Vec::new()),
        }
    }

    fn commands(&self) -> Vec<String> {
        self.commands.lock().unwrap().clone()
    }
}

/// The version probe every k3s step starts with.
const VERSION_CMD: &str = "/usr/local/bin/k3s --version 2>/dev/null || k3s --version 2>/dev/null";
/// The agent-unit probe (this mock answers the upstream shape).
const UNIT_PROBE: &str =
    "systemctl cat k3s-agent.service >/dev/null 2>&1 && echo k3s-agent || echo k3s";

#[async_trait::async_trait]
impl Session for MockSession {
    async fn exec(&self, command: &str) -> Result<ExecOutput> {
        self.commands.lock().unwrap().push(command.to_string());
        if command == VERSION_CMD {
            return Ok(ok(&format!("k3s version v{}\n", self.k3s_version)));
        }
        if command == UNIT_PROBE {
            return Ok(ok("k3s-agent"));
        }
        // Node Ready probes (agents and servers) and the server /readyz.
        if command.contains("get node ") || command.contains("get --raw /readyz") {
            return Ok(ok("True"));
        }
        Ok(ok("ok"))
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

impl MockPool {
    fn session(&self, node: &str) -> Arc<MockSession> {
        self.sessions
            .get(node)
            .unwrap_or_else(|| panic!("node {node} not in mock pool"))
            .clone()
    }
}

#[async_trait::async_trait]
impl SessionPool for MockPool {
    async fn get(&self, node: &str) -> Result<Arc<dyn Session>> {
        Ok(self.session(node) as Arc<dyn Session>)
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
        labels: Default::default(),
        distribution: kurin_config::Distribution::K3s,
        ha_mode: kurin_config::HaMode::KubeVip,
        runtime: kurin_config::RuntimeKind::Containerd,
        kubernetes_version: Version::parse("1.33.1").unwrap(),
        pod_cidr: "10.42.0.0/16".parse().unwrap(),
        service_cidr: "10.43.0.0/16".parse().unwrap(),
        dns_domain: "cluster.local".into(),
        cni: Default::default(),
        vip: None,
        nodes,
        ssh: kurin_config::SshDefaultsSpec {
            user: "root".into(),
            port: 22,
            r#become: kurin_config::BecomeMode::Auto,
            become_password: None,
            known_hosts: Default::default(),
            auth: None,
        },
        registry: kurin_config::RegistrySpec::default(),
        packages: kurin_config::PackageSources {
            offline_dir: None,
            zone: None,
            ..Default::default()
        },
        addons: Default::default(),
        hardening: Default::default(),
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

/// `--with-kubernetes v1.33.2` -> k3s `1.33.2+k3s1`.
fn target() -> Version {
    Version::parse("1.33.2").unwrap()
}

fn k3s_target() -> String {
    "1.33.2+k3s1".to_string()
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
    .expect("k3s upgrade plan builds")
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

/// An offline bundle carrying the k3s binary for amd64 at `version`.
fn bundle(dir: &Path, version: &str) -> DirManifest {
    let file = dir.join("k3s");
    std::fs::write(&file, b"fake-k3s-binary").unwrap();
    DirManifest {
        path: dir.to_path_buf(),
        artifacts: vec![DirArtifact {
            name: "k3s".into(),
            version: version.into(),
            arch: Arch::Amd64,
            kind: None,
            file,
            sha256: String::new(),
        }],
    }
}

/// A pool where every node reports `version`, except `stale` (the node
/// the test wants to see upgrade).
fn sessions(
    cluster: &Cluster,
    version: &str,
    stale: Option<&str>,
    stale_version: &str,
) -> MockPool {
    let mut map = HashMap::new();
    for node in &cluster.nodes {
        let v = if Some(node.name.as_str()) == stale {
            stale_version
        } else {
            version
        };
        map.insert(node.name.clone(), Arc::new(MockSession::new(v)));
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
fn k3s_upgrade_plan_snapshot_1cp_1w() {
    let cluster = cluster("k3s-one-and-one", 1, 1);
    let plan = plan_for(&cluster, UpgradeOptions::default());
    insta::assert_snapshot!("k3s_upgrade_plan_1cp_1w", render_plan(&plan));
}

#[test]
fn k3s_upgrade_plan_snapshot_3cp_3w_batch2() {
    let cluster = cluster("k3s-three-and-three", 3, 3);
    let plan = plan_for(
        &cluster,
        UpgradeOptions {
            batch: 2,
            force_drain_failure: false,
        },
    );
    insta::assert_snapshot!("k3s_upgrade_plan_3cp_3w_batch2", render_plan(&plan));
}

// ---------------------------------------------------------------------------
// Executor behavior
// ---------------------------------------------------------------------------

/// The server chain, in order: version-check (probe), stop, park+swap,
/// start, gate (Ready + /readyz). The old binary is preserved, never
/// deleted; the new one lands via a staging path.
#[tokio::test]
async fn server_chain_runs_stop_replace_start_gate_in_order() {
    let cluster = cluster("k3s-server-chain", 1, 0);
    let facts = kurin::facts::FactsTable::synthetic(&cluster).unwrap();
    let pool = Arc::new(sessions(
        &cluster,
        "1.33.1+k3s1",
        Some("cp1"),
        "1.33.1+k3s1",
    ));
    let dyn_pool: Arc<dyn SessionPool> = pool.clone();
    let dir = tempfile::tempdir().unwrap();
    let source = Arc::new(BinSource::Bundle(bundle(dir.path(), &k3s_target())));
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
    upgrade::execute(plan, &dyn_pool, &run, &mut emit)
        .await
        .expect("the server chain completes");

    let cp1 = pool.session("cp1").commands();
    let position = |needle: &str| cp1.iter().position(|c| c.contains(needle));
    let probe = position(VERSION_CMD).expect("version probe ran");
    let stop = position("systemctl stop k3s").expect("server stop ran");
    let park = position("cp -a /usr/local/bin/k3s").expect("old binary parked");
    let swap = position("mv -f /usr/local/bin/k3s.kurin-new").expect("binary swapped");
    let start = position("systemctl start k3s").expect("server start ran");
    assert!(
        probe < stop && stop < park && park <= swap && swap < start,
        "server chain order violated: {cp1:?}"
    );

    // The gate is the server shape: /readyz AND node Ready, in one probe.
    assert!(
        cp1.iter().any(|c| c.contains("get --raw /readyz")
            && c.contains("get node cp1")
            && c.contains("/etc/rancher/k3s/k3s.yaml")),
        "the server gate must check /readyz and the node's Ready condition: {cp1:?}"
    );

    // The parked binary is never deleted.
    assert!(
        !cp1.iter().any(|c| c.contains("rm ")),
        "the old binary must never be deleted: {cp1:?}"
    );
    assert_eq!(
        pool.session("cp1").writes.lock().unwrap().as_slice(),
        ["/usr/local/bin/k3s.kurin-new"],
        "the new binary uploads to the staging path"
    );

    // The post summary reads through k3s's own kubectl.
    assert!(
        cp1.iter()
            .any(|c| c.contains("k3s kubectl --kubeconfig /etc/rancher/k3s/k3s.yaml get nodes")),
        "the post summary must use k3s kubectl: {cp1:?}"
    );

    let events = events.lock().unwrap();
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::TaskFailed { .. })),
        "no task may fail: {events:?}"
    );
}

/// The agent chain stops/starts the k3s-agent unit and gates on the
/// node's Ready condition only (no /readyz, agent kubeconfig).
#[tokio::test]
async fn agent_chain_uses_the_agent_unit_and_gates_on_ready() {
    let cluster = cluster("k3s-agent-chain", 1, 1);
    let facts = kurin::facts::FactsTable::synthetic(&cluster).unwrap();
    // cp1 already at the target: only w1's chain is asserted.
    let pool = Arc::new(sessions(&cluster, &k3s_target(), Some("w1"), "1.33.1+k3s1"));
    let dyn_pool: Arc<dyn SessionPool> = pool.clone();
    let dir = tempfile::tempdir().unwrap();
    let source = Arc::new(BinSource::Bundle(bundle(dir.path(), &k3s_target())));
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

    let (_, mut emit) = collect_events();
    upgrade::execute(plan, &dyn_pool, &run, &mut emit)
        .await
        .expect("the agent chain completes");

    let w1 = pool.session("w1").commands();
    assert!(
        w1.iter().any(|c| c.contains("systemctl stop k3s-agent")),
        "the agent must stop its k3s-agent unit: {w1:?}"
    );
    assert!(
        w1.iter().any(|c| c.contains("systemctl start k3s-agent")),
        "the agent must start its k3s-agent unit: {w1:?}"
    );
    assert!(
        !w1.iter().any(|c| c.contains("get --raw /readyz")),
        "an agent does not serve /readyz: {w1:?}"
    );
    assert!(
        w1.iter().any(|c| c.contains("get node w1")
            && c.contains("/var/lib/rancher/k3s/agent/kubelet.kubeconfig")),
        "the agent gate must use the kubelet kubeconfig: {w1:?}"
    );

    // The already-upgraded server was inert.
    let cp1 = pool.session("cp1").commands();
    for forbidden in ["systemctl stop", "systemctl start", "cp -a"] {
        assert!(
            !cp1.iter().any(|c| c.contains(forbidden)),
            "cp1 ran `{forbidden}` though already upgraded: {cp1:?}"
        );
    }
}

/// A node already at the target is InSync: every chain step is skipped
/// and no mutating command runs anywhere.
#[tokio::test]
async fn already_upgraded_k3s_cluster_is_in_sync() {
    let cluster = cluster("k3s-already", 2, 2);
    let facts = kurin::facts::FactsTable::synthetic(&cluster).unwrap();
    let pool = Arc::new(sessions(&cluster, &k3s_target(), None, ""));
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

    let chain_ids: Vec<String> = plan
        .tasks
        .iter()
        .map(|task| task.task.id())
        .filter(|id| !id.starts_with("discover/") && !id.starts_with("post/"))
        .collect();

    let (events, mut emit) = collect_events();
    upgrade::execute(plan, &dyn_pool, &run, &mut emit)
        .await
        .expect("an already-upgraded k3s cluster is a no-op");

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

    for node in ["cp1", "cp2", "w1", "w2"] {
        let commands = pool.session(node).commands();
        for forbidden in ["systemctl stop", "systemctl start", "cp -a", "mv -f"] {
            assert!(
                !commands.iter().any(|c| c.contains(forbidden)),
                "{node} ran `{forbidden}` on an already-upgraded node: {commands:?}"
            );
        }
        assert!(
            pool.session(node).writes.lock().unwrap().is_empty(),
            "{node} had its binary replaced on an already-upgraded node"
        );
    }
}

/// A failed swap blocks the rest of the node's chain and every later
/// node: no start, no gate, and the next server is never touched.
#[tokio::test]
async fn failed_swap_blocks_later_nodes() {
    let cluster = cluster("k3s-swap-fails", 2, 0);
    let facts = kurin::facts::FactsTable::synthetic(&cluster).unwrap();
    // A pool whose cp1 fails the swap command (non-zero exit).
    struct FailingSwap {
        inner: MockSession,
    }

    #[async_trait]
    impl Session for FailingSwap {
        async fn exec(&self, command: &str) -> Result<ExecOutput> {
            if command.contains("cp -a /usr/local/bin/k3s") {
                self.inner.commands.lock().unwrap().push(command.into());
                return Ok(ExecOutput {
                    exit_code: 1,
                    stdout: String::new(),
                    stderr: "cp: cannot stat '/usr/local/bin/k3s': No such file or directory\n"
                        .into(),
                });
            }
            self.inner.exec(command).await
        }
        async fn write_file(&self, path: &str, contents: &[u8]) -> Result<()> {
            self.inner.write_file(path, contents).await
        }
        async fn file_sha256(&self, path: &str) -> Result<Option<String>> {
            self.inner.file_sha256(path).await
        }
        async fn service_active(&self, name: &str) -> Result<bool> {
            self.inner.service_active(name).await
        }
    }

    let mut map: HashMap<String, Arc<dyn Session>> = HashMap::new();
    map.insert(
        "cp1".into(),
        Arc::new(FailingSwap {
            inner: MockSession::new("1.33.1+k3s1"),
        }),
    );
    map.insert("cp2".into(), Arc::new(MockSession::new("1.33.1+k3s1")));
    let pool = Arc::new(MockDynPool { sessions: map });
    let dyn_pool: Arc<dyn SessionPool> = pool.clone();
    let dir = tempfile::tempdir().unwrap();
    let source = Arc::new(BinSource::Bundle(bundle(dir.path(), &k3s_target())));
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
        .expect_err("a failed swap must abort the upgrade");
    assert!(
        err.to_string().contains("replacing /usr/local/bin/k3s"),
        "the error must name the failed swap: {err}"
    );

    // cp1 was stopped but never started again.
    let events = events.lock().unwrap();
    assert!(events.iter().any(|event| matches!(
        event,
        Event::TaskSkipped { id, reason: Some(reason), .. }
            if id == "version-check/version-check@cp2" && reason.contains("blocked by")
    )));
}

/// Thin wrapper so the heterogeneous pool (one failing session) still
/// implements SessionPool.
struct MockDynPool {
    sessions: HashMap<String, Arc<dyn Session>>,
}

#[async_trait::async_trait]
impl SessionPool for MockDynPool {
    async fn get(&self, node: &str) -> Result<Arc<dyn Session>> {
        Ok(self
            .sessions
            .get(node)
            .unwrap_or_else(|| panic!("node {node} not in mock pool"))
            .clone())
    }
}
