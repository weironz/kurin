//! Drain-gate tests for `kurin upgrade` (WP-01): a failed `kubectl drain`
//! must abort that node's upgrade. Self-contained mocks — no network, no
//! shared `support` module.

use std::collections::{BTreeMap, HashMap};
use std::net::{IpAddr, Ipv4Addr};
use std::path::Path;
use std::sync::{Arc, Mutex};

use kurin::artifact::{Arch, DirArtifact, DirManifest};
use kurin::upgrade::{self, BinSource, DrainOutcome, NodeCtx, UpgradeOptions};
use kurin::{ExecOutput, Session, SessionPool};
use kurin_config::{Node, NodeRole, ResolvedSsh, SshAuth};
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
    /// Response for a `kubectl drain` command.
    drain: ExecOutput,
    /// `kubectl get node … -o jsonpath` answer (kubelet version).
    kubelet_version: String,
}

impl MockSession {
    fn new(drain: ExecOutput, kubelet_version: &str) -> Self {
        MockSession {
            commands: Mutex::new(Vec::new()),
            writes: Mutex::new(Vec::new()),
            drain,
            kubelet_version: kubelet_version.into(),
        }
    }

    fn commands(&self) -> Vec<String> {
        self.commands.lock().unwrap().clone()
    }

    fn writes(&self) -> Vec<String> {
        self.writes.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl Session for MockSession {
    async fn exec(&self, command: &str) -> Result<ExecOutput> {
        self.commands.lock().unwrap().push(command.to_string());
        if command.contains(" drain ") {
            return Ok(self.drain.clone());
        }
        if command.contains("get node") {
            return Ok(ok(&self.kubelet_version));
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

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

const NODE: &str = "w1";
const KUBECTL: &str = "kubectl --kubeconfig /etc/kubernetes/admin.conf";

fn worker(name: &str) -> Node {
    Node {
        name: name.into(),
        role: NodeRole::Worker,
        address: IpAddr::V4(Ipv4Addr::new(10, 0, 0, 11)),
        internal_address: IpAddr::V4(Ipv4Addr::new(10, 0, 0, 11)),
        ssh: ResolvedSsh {
            user: "root".into(),
            port: 22,
            auth: SshAuth::Password {
                password: "not-a-real-password".into(),
            },
        },
        labels: BTreeMap::new(),
    }
}

/// A drain failure shaped like kubectl's: the informational line goes to
/// stdout and the error to stderr.
fn failed_drain() -> ExecOutput {
    ExecOutput {
        exit_code: 1,
        // Deliberately contains "drained": the pre-WP-01 stdout sniffing
        // treated this as success.
        stdout: format!("node/{NODE} cordoned\nnode/{NODE} drained\n"),
        stderr: format!(
            "error: unable to drain node \"{NODE}\": cannot delete Pods with local storage\n\
             There are pending nodes to be drained:\n {NODE}\n"
        ),
    }
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

/// cp0 answers the kubelet-version probe with an old version, so the node is
/// not skipped as up-to-date, and fails the drain.
fn fixtures() -> (MockPool, Arc<MockSession>, Arc<MockSession>) {
    let cp0 = Arc::new(MockSession::new(failed_drain(), "v1.32.0"));
    let node = Arc::new(MockSession::new(ok(""), "v1.32.0"));
    let mut sessions = HashMap::new();
    sessions.insert("cp0".to_string(), Arc::clone(&cp0));
    sessions.insert(NODE.to_string(), Arc::clone(&node));
    (MockPool { sessions }, cp0, node)
}

fn target() -> Version {
    Version::parse("1.33.13").unwrap()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// A failed drain returns the documented error and leaves the node alone:
/// no binary upload, no `kubeadm upgrade node`, no uncordon.
#[tokio::test]
async fn drain_failure_aborts_node_upgrade() {
    let (pool, cp0, node) = fixtures();
    let dir = tempfile::tempdir().unwrap();
    let manifest = bundle(dir.path());
    let source = BinSource::Bundle(&manifest);
    let target = target();
    let ctx = NodeCtx {
        pool: &pool,
        source: &source,
        arch: Arch::Amd64,
        cp0: cp0.as_ref(),
        kubectl: KUBECTL,
        target: &target,
        force_drain_failure: false,
    };

    let err = upgrade::upgrade_one_node(&ctx, &worker(NODE))
        .await
        .expect_err("a failed drain must abort the node upgrade");
    let msg = err.to_string();
    assert!(
        msg.contains(&format!("drain {NODE} failed:")),
        "unexpected error text: {msg}"
    );
    assert!(
        msg.contains("error: unable to drain node"),
        "error must carry the first non-empty stderr line: {msg}"
    );

    // Nothing ran on the node, and the control plane did not uncordon it.
    assert!(
        node.commands().is_empty(),
        "node received commands after a failed drain: {:?}",
        node.commands()
    );
    assert!(
        node.writes().is_empty(),
        "binaries were uploaded after a failed drain: {:?}",
        node.writes()
    );
    let cp0_commands = cp0.commands();
    assert!(
        !cp0_commands.iter().any(|c| c.contains("uncordon")),
        "node was uncordoned after a failed drain: {cp0_commands:?}"
    );
    assert!(
        !cp0_commands.iter().any(|c| c.contains("kubeadm upgrade")),
        "kubeadm ran after a failed drain: {cp0_commands:?}"
    );
}

/// `--force-drain-failure` keeps the old behavior: the failure is reported
/// as forced/ignored and the node upgrade continues.
#[tokio::test]
async fn forced_drain_failure_continues() {
    let (pool, cp0, node) = fixtures();
    let dir = tempfile::tempdir().unwrap();
    let manifest = bundle(dir.path());
    let source = BinSource::Bundle(&manifest);
    let target = target();
    let ctx = NodeCtx {
        pool: &pool,
        source: &source,
        arch: Arch::Amd64,
        cp0: cp0.as_ref(),
        kubectl: KUBECTL,
        target: &target,
        force_drain_failure: true,
    };

    upgrade::upgrade_one_node(&ctx, &worker(NODE))
        .await
        .expect("--force-drain-failure must keep going");

    let node_commands = node.commands();
    assert!(
        node_commands.iter().any(|c| c == "kubeadm upgrade node"),
        "kubeadm upgrade node did not run: {node_commands:?}"
    );
    assert!(
        !node.writes().is_empty(),
        "binaries were not installed with --force-drain-failure"
    );
    assert!(
        cp0.commands().iter().any(|c| c.contains("uncordon")),
        "node was not uncordoned: {:?}",
        cp0.commands()
    );
}

/// The forced path reports the warning text (which is what gets printed).
#[tokio::test]
async fn forced_drain_failure_reports_forced_warning() {
    let cp0 = Arc::new(MockSession::new(failed_drain(), "v1.32.0"));

    let outcome = upgrade::drain_node(cp0.as_ref(), KUBECTL, NODE, true)
        .await
        .expect("forced drain must not fail the run");
    match outcome {
        DrainOutcome::ForcedFailure { detail } => {
            assert!(
                detail.contains("forced: drain failure ignored"),
                "warning does not say the failure was forced: {detail}"
            );
            assert!(
                detail.contains("error: unable to drain node"),
                "warning lost the drain error: {detail}"
            );
        }
        DrainOutcome::Drained => panic!("drain exited 1 but was reported as drained"),
    }
}

/// The failure text falls back to stdout's first non-empty line when stderr
/// is empty, and exit code alone decides success (stdout sniffing is gone).
#[tokio::test]
async fn drain_failure_detail_falls_back_to_stdout() {
    let cp0 = Arc::new(MockSession::new(
        ExecOutput {
            exit_code: 1,
            stdout: format!("\n  node/{NODE} drained\nsecond line\n"),
            stderr: String::new(),
        },
        "v1.32.0",
    ));

    let err = upgrade::drain_node(cp0.as_ref(), KUBECTL, NODE, false)
        .await
        .expect_err("exit 1 must fail even when stdout says `drained`");
    assert_eq!(
        err.to_string(),
        format!("drain {NODE} failed: node/{NODE} drained")
    );
}

/// Exit 0 is success and reports a plain drain.
#[tokio::test]
async fn successful_drain_reports_drained() {
    let cp0 = Arc::new(MockSession::new(
        ExecOutput {
            exit_code: 0,
            stdout: format!("node/{NODE} drained\n"),
            stderr: String::new(),
        },
        "v1.32.0",
    ));

    let outcome = upgrade::drain_node(cp0.as_ref(), KUBECTL, NODE, false)
        .await
        .expect("exit 0 is a successful drain");
    assert_eq!(outcome, DrainOutcome::Drained);
    let commands = cp0.commands();
    assert_eq!(
        commands.len(),
        1,
        "expected exactly one drain: {commands:?}"
    );
    assert!(
        !commands[0].contains("; true"),
        "drain must use its real exit code: {}",
        commands[0]
    );
}

/// The CLI flag exists, is off by default, and the library knobs default to
/// the safe behavior.
#[test]
fn upgrade_options_default_is_safe() {
    let opts = UpgradeOptions::default();
    assert!(!opts.force_drain_failure);
    assert_eq!(opts.batch, 1);
}
