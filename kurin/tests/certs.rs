//! Integration tests for the certs track (mock Session, no network).
//!
//! `lib.rs` is integrator-owned, so until `pub mod certs;` lands the module
//! is compiled in via `#[path]`; `crate::executor` below satisfies its only
//! `crate::` dependency. When the integrator wires the module into lib.rs,
//! these tests still pass unchanged.

// Integration wiring landed: the module is `kurin::certs` now.
use kurin::certs;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use kurin::{ExecOutput, Session};
use kurin_config::Cluster;

// ---------------------------------------------------------------------------
// Mocks (shape follows kurin/tests/engine.rs MockSession)
// ---------------------------------------------------------------------------

/// What `openssl`/`date` on the node would have printed for a cert expiring
/// `days` from the mock now.
fn probe_line(path: &str, subject: &str, days: i64, ca: bool) -> String {
    format!(
        "{path}|{subject}|probe enddate|{days}|{}",
        if ca { 1 } else { 0 }
    )
}

#[derive(Default)]
struct MockPool {
    sessions: Mutex<HashMap<String, Arc<MockSession>>>,
}

struct MockSession {
    /// What the pki probe should return (per-node cert table).
    probe_stdout: Mutex<String>,
    /// Every command the node received, for assertion.
    commands: Mutex<Vec<String>>,
    /// Substring of a command that should exit 1 (injected failure).
    fail_on: Mutex<Option<String>>,
}

#[async_trait::async_trait]
impl Session for MockSession {
    async fn exec(&self, command: &str) -> miette::Result<ExecOutput> {
        self.commands.lock().unwrap().push(command.to_string());
        if command.contains("for c in ") {
            return Ok(ExecOutput {
                exit_code: 0,
                stdout: self.probe_stdout.lock().unwrap().clone(),
                stderr: String::new(),
            });
        }
        let code = match self.fail_on.lock().unwrap().as_deref() {
            Some(pat) if command.contains(pat) => 1,
            _ => 0,
        };
        Ok(ExecOutput {
            exit_code: code,
            stdout: "ok".into(),
            stderr: if code == 0 {
                String::new()
            } else {
                // Matches the old-k3s failure shape (unknown subcommand),
                // generic enough for every injected failure.
                "unknown command".into()
            },
        })
    }
    async fn write_file(&self, _p: &str, _c: &[u8]) -> miette::Result<()> {
        Ok(())
    }
    async fn file_sha256(&self, _p: &str) -> miette::Result<Option<String>> {
        Ok(None)
    }
    async fn service_active(&self, _n: &str) -> miette::Result<bool> {
        Ok(true)
    }
}

#[async_trait::async_trait]
impl kurin::SessionPool for MockPool {
    async fn get(&self, node: &str) -> miette::Result<Arc<dyn Session>> {
        Ok(self
            .sessions
            .lock()
            .unwrap()
            .get(node)
            .cloned()
            .expect("node registered in mock pool") as Arc<dyn Session>)
    }
}

/// Wrap the mock pool as the trait object the certs API takes.
fn arc(pool: MockPool) -> Arc<dyn kurin::SessionPool> {
    Arc::new(pool)
}

fn mock_node(pool: &MockPool, node: &str, lines: &[String]) -> Arc<MockSession> {
    let s = Arc::new(MockSession {
        probe_stdout: Mutex::new(lines.join("\n") + "\n"),
        commands: Mutex::new(vec![]),
        fail_on: Mutex::new(None),
    });
    pool.sessions
        .lock()
        .unwrap()
        .insert(node.into(), Arc::clone(&s));
    s
}

fn cluster() -> Cluster {
    kurin_config::parse_str(
        std::path::Path::new("x.toml"),
        r##"version = "v1alpha1"
name = "t"
[ssh]
user = "root"
[ssh.auth]
method = "password"
password = "x"
[nodes.cp1]
address = "10.0.0.1"
role = "control-plane"
[nodes.w1]
address = "10.0.0.2"
role = "worker"
"##,
    )
    .unwrap()
}

fn cluster_k3s() -> Cluster {
    kurin_config::parse_str(
        std::path::Path::new("x.toml"),
        r##"version = "v1alpha1"
name = "t"
[kubernetes]
distribution = "k3s"
[ssh]
user = "root"
[ssh.auth]
method = "password"
password = "x"
[nodes.cp1]
address = "10.0.0.1"
role = "control-plane"
[nodes.w1]
address = "10.0.0.2"
role = "worker"
"##,
    )
    .unwrap()
}

// ---------------------------------------------------------------------------
// check
// ---------------------------------------------------------------------------

#[tokio::test]
async fn check_parses_probe_output_and_days_left() {
    let pool = MockPool::default();
    let cp = mock_node(
        &pool,
        "cp1",
        &[
            probe_line(
                "/etc/kubernetes/pki/apiserver.crt",
                "CN = kube-apiserver",
                300,
                false,
            ),
            probe_line("/etc/kubernetes/pki/ca.crt", "CN = kubernetes", 42, true),
            probe_line("/etc/kubernetes/pki/etcd/server.crt", "CN = cp1", 12, false),
        ],
    );
    // Workers have no pki tree: probe succeeds with empty output.
    let _w = mock_node(&pool, "w1", &[]);

    let reports = certs::check(&cluster(), &arc(pool), 90).await.unwrap();

    assert_eq!(reports.len(), 2);
    let cp_report = reports.iter().find(|r| r.node == "cp1").unwrap();
    assert_eq!(cp_report.certs.len(), 3);

    let ca = cp_report
        .certs
        .iter()
        .find(|c| c.path.ends_with("ca.crt"))
        .unwrap();
    assert!(ca.is_ca);
    assert_eq!(ca.days_left, 42);
    assert_eq!(ca.subject, "CN = kubernetes");

    let apiserver = cp_report
        .certs
        .iter()
        .find(|c| c.path.ends_with("apiserver.crt"))
        .unwrap();
    assert!(!apiserver.is_ca);
    assert_eq!(apiserver.days_left, 300);

    // days_left is whatever the node's epoch arithmetic produced; a second
    // probe with a different table must not leak values across nodes.
    let reports2 = {
        let pool = MockPool::default();
        mock_node(
            &pool,
            "cp1",
            &[probe_line("/etc/kubernetes/pki/x.crt", "CN = x", 4, false)],
        );
        mock_node(&pool, "w1", &[]);
        certs::check(&cluster(), &arc(pool), 0).await.unwrap()
    };
    assert_eq!(reports2[0].certs[0].days_left, 4);

    // The probe command used the documented openssl fields.
    let cmds = cp.commands.lock().unwrap();
    assert!(cmds[0].contains("openssl x509 -in \"$c\" -noout -enddate"));
    assert!(cmds[0].contains("CA:TRUE"));
    assert!(cmds[0].contains("date -u +%s"));
}

#[tokio::test]
async fn check_rejects_malformed_probe_line() {
    let pool = MockPool::default();
    mock_node(&pool, "cp1", &["garbage without pipes".into()]);
    mock_node(&pool, "w1", &[]);
    let err = certs::check(&cluster(), &arc(pool), 90).await.unwrap_err();
    assert!(format!("{err:?}").contains("unparsable cert probe"));
}

// ---------------------------------------------------------------------------
// render
// ---------------------------------------------------------------------------

#[test]
fn render_marks_certs_below_warn_days() {
    let reports = vec![certs::NodeCerts {
        node: "cp1".into(),
        certs: vec![
            certs::CertInfo {
                path: "/etc/kubernetes/pki/apiserver.crt".into(),
                subject: "CN = kube-apiserver".into(),
                not_after_utc: "Oct 26 12:00:00 2027 GMT".into(),
                days_left: 300,
                is_ca: false,
            },
            certs::CertInfo {
                path: "/etc/kubernetes/pki/ca.crt".into(),
                subject: "CN = kubernetes".into(),
                not_after_utc: "Dec 07 12:00:00 2026 GMT".into(),
                days_left: 42,
                is_ca: true,
            },
        ],
    }];
    let _ = certs::render as fn(&[certs::NodeCerts], i64); // stderr sink used by the CLI
    let table = certs::render_table(&reports, 90);
    assert!(table.contains("NODE"));
    assert!(table.contains("cp1"));
    assert!(table.contains("/etc/kubernetes/pki/apiserver.crt"));
    // 42 < 90 -> WARN; 300 stays unmarked.
    let ca_line = table
        .lines()
        .find(|l| l.contains("ca.crt"))
        .expect("ca row rendered");
    assert!(ca_line.contains("WARN"));
    let api_line = table
        .lines()
        .find(|l| l.contains("apiserver.crt"))
        .expect("apiserver row rendered");
    assert!(!api_line.contains("WARN"));
}

// ---------------------------------------------------------------------------
// renew
// ---------------------------------------------------------------------------

#[tokio::test]
async fn renew_skips_healthy_node_without_force() {
    let pool = MockPool::default();
    let cp = mock_node(
        &pool,
        "cp1",
        &[probe_line(
            "/etc/kubernetes/pki/apiserver.crt",
            "CN = kube-apiserver",
            200,
            false,
        )],
    );

    certs::renew(&cluster(), &arc(pool), false).await.unwrap();

    let cmds = cp.commands.lock().unwrap();
    assert!(
        !cmds.iter().any(|c| c.contains("kubeadm certs renew all")),
        "renew must not run on a node with >89 days left: {cmds:?}"
    );
    assert_eq!(cmds.len(), 1, "only the pre-check probe should run");
}

#[tokio::test]
async fn renew_renews_expiring_node_and_gates_health() {
    let pool = MockPool::default();
    let cp = mock_node(
        &pool,
        "cp1",
        &[probe_line(
            "/etc/kubernetes/pki/ca.crt",
            "CN = kubernetes",
            30,
            true,
        )],
    );

    certs::renew(&cluster(), &arc(pool), false).await.unwrap();

    let cmds = cp.commands.lock().unwrap();
    let renew = cmds
        .iter()
        .position(|c| c.contains("kubeadm certs renew all"))
        .expect("kubeadm certs renew all executed");
    let restart = cmds
        .iter()
        .position(|c| c.contains("/etc/kubernetes/manifests/*.yaml"))
        .expect("static pod manifests cycled");
    let ready = cmds
        .iter()
        .position(|c| c.contains("--server https://127.0.0.1:6443") && c.contains("/readyz"))
        .expect("apiserver health gate polled");
    assert!(renew < restart, "renew precedes pod restart");
    assert!(restart < ready, "health gate follows restart");
    assert!(
        cmds[restart].contains("mv") && cmds[restart].contains("sleep"),
        "restart uses staged mv + bounded sleep: {}",
        cmds[restart]
    );
}

#[tokio::test]
async fn renew_force_skips_precheck() {
    let pool = MockPool::default();
    let cp = mock_node(
        &pool,
        "cp1",
        &[probe_line(
            "/etc/kubernetes/pki/apiserver.crt",
            "CN = kube-apiserver",
            200,
            false,
        )],
    );

    certs::renew(&cluster(), &arc(pool), true).await.unwrap();

    let cmds = cp.commands.lock().unwrap();
    assert!(
        !cmds
            .iter()
            .any(|c| c.contains("for c in /etc/kubernetes/pki")),
        "force must skip the pre-check probe: {cmds:?}"
    );
    assert!(cmds.iter().any(|c| c.contains("kubeadm certs renew all")));
}

#[tokio::test]
async fn renew_aborts_when_static_pod_restart_fails() {
    let pool = MockPool::default();
    let cp = mock_node(
        &pool,
        "cp1",
        &[probe_line(
            "/etc/kubernetes/pki/ca.crt",
            "CN = kubernetes",
            30,
            true,
        )],
    );
    // Let renew succeed, fail the manifest-cycle step.
    *cp.fail_on.lock().unwrap() = Some("/etc/kubernetes/manifests/*.yaml".into());

    let err = certs::renew(&cluster(), &arc(pool), false)
        .await
        .unwrap_err();
    assert!(format!("{err:?}").contains("static pod restart exited 1"));
}

// ---------------------------------------------------------------------------
// k3s distribution
// ---------------------------------------------------------------------------

#[tokio::test]
async fn check_k3s_probes_rancher_tls_tree() {
    let pool = MockPool::default();
    let cp = mock_node(
        &pool,
        "cp1",
        &[probe_line(
            "/var/lib/rancher/k3s/server/tls/client-admin.crt",
            "CN = system:admin",
            300,
            false,
        )],
    );
    // Agents have no tls tree: empty probe output.
    let _w = mock_node(&pool, "w1", &[]);

    let reports = certs::check(&cluster_k3s(), &arc(pool), 90).await.unwrap();

    assert_eq!(reports.len(), 2);
    let cp_report = reports.iter().find(|r| r.node == "cp1").unwrap();
    assert_eq!(cp_report.certs.len(), 1);
    assert_eq!(cp_report.certs[0].days_left, 300);

    let cmds = cp.commands.lock().unwrap();
    assert!(
        cmds[0].contains("/var/lib/rancher/k3s/server/tls/*.crt"),
        "k3s probe must walk the rancher tls tree: {}",
        cmds[0]
    );
    assert!(
        !cmds[0].contains("/etc/kubernetes/pki"),
        "k3s probe must not touch the kubeadm pki: {}",
        cmds[0]
    );
}

#[tokio::test]
async fn renew_k3s_rotates_restarts_and_gates() {
    let pool = MockPool::default();
    let cp = mock_node(
        &pool,
        "cp1",
        &[probe_line(
            "/var/lib/rancher/k3s/server/tls/server-ca.crt",
            "CN = k3s server-ca",
            30,
            true,
        )],
    );

    certs::renew(&cluster_k3s(), &arc(pool), false)
        .await
        .unwrap();

    let cmds = cp.commands.lock().unwrap();
    let rotate = cmds
        .iter()
        .position(|c| c == "k3s certificate rotate")
        .expect("k3s certificate rotate executed");
    let restart = cmds
        .iter()
        .position(|c| c == "systemctl restart k3s")
        .expect("k3s.service restarted");
    let ready = cmds
        .iter()
        .position(|c| {
            c.contains("k3s kubectl")
                && c.contains("/etc/rancher/k3s/k3s.yaml")
                && c.contains("/readyz")
        })
        .expect("readyz health gate polled through k3s kubectl");
    assert!(rotate < restart, "rotate precedes restart");
    assert!(restart < ready, "health gate follows restart");
    assert!(
        !cmds.iter().any(|c| c.contains("kubeadm")),
        "k3s renew must not run kubeadm: {cmds:?}"
    );
}

#[tokio::test]
async fn renew_k3s_too_old_reports_upgrade_guidance() {
    let pool = MockPool::default();
    let cp = mock_node(
        &pool,
        "cp1",
        &[probe_line(
            "/var/lib/rancher/k3s/server/tls/server-ca.crt",
            "CN = k3s server-ca",
            30,
            true,
        )],
    );
    // Old k3s: `certificate rotate` is unknown (stderr contains "unknown").
    *cp.fail_on.lock().unwrap() = Some("k3s certificate rotate".into());

    let err = certs::renew(&cluster_k3s(), &arc(pool), false)
        .await
        .unwrap_err();
    let msg = format!("{err:?}");
    assert!(
        msg.contains("k3s too old for rotate; upgrade k3s"),
        "upgrade guidance expected: {msg}"
    );

    let cmds = cp.commands.lock().unwrap();
    assert!(
        !cmds.iter().any(|c| c == "systemctl restart k3s"),
        "failed rotate must not restart k3s: {cmds:?}"
    );
}
