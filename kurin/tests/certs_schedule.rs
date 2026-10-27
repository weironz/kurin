//! Integration tests for the certificate renewal timer (WP-71, mock
//! Session, no network).
//!
//! The mock answers the `systemctl` calls `kurin_components::certs`
//! issues, so the files the schedule installs and the probes it runs
//! are asserted end to end.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use kurin::certs::{self, parse_every};
use kurin::{ExecOutput, Session};
use kurin_config::Cluster;

// ---------------------------------------------------------------------------
// Mocks (shape follows kurin/tests/etcd.rs MockSession)
// ---------------------------------------------------------------------------

struct MockSession {
    /// Canned answer for the `systemctl show` next-elapse probe.
    next_elapse: String,
    /// Every command the node received, in order.
    commands: Mutex<Vec<String>>,
    /// Every (path, contents) written through write_file/write_file_mode.
    files: Mutex<Vec<(String, Vec<u8>)>>,
}

impl MockSession {
    fn new(next_elapse: &str) -> Arc<Self> {
        Arc::new(Self {
            next_elapse: next_elapse.to_string(),
            commands: Mutex::new(vec![]),
            files: Mutex::new(vec![]),
        })
    }

    fn commands(&self) -> Vec<String> {
        self.commands.lock().unwrap().clone()
    }

    fn file(&self, path: &str) -> String {
        self.files
            .lock()
            .unwrap()
            .iter()
            .find(|(p, _)| p == path)
            .map(|(_, c)| String::from_utf8(c.clone()).unwrap())
            .unwrap_or_else(|| panic!("{path} not written"))
    }
}

#[async_trait::async_trait]
impl Session for MockSession {
    async fn exec(&self, command: &str) -> miette::Result<ExecOutput> {
        self.commands.lock().unwrap().push(command.to_string());
        if command.starts_with("systemctl show kurin-cert-renew.timer") {
            return Ok(ExecOutput {
                exit_code: 0,
                stdout: self.next_elapse.clone(),
                stderr: String::new(),
            });
        }
        // daemon-reload / enable / restart all succeed.
        Ok(ExecOutput {
            exit_code: 0,
            stdout: "ok".into(),
            stderr: String::new(),
        })
    }

    async fn write_file(&self, path: &str, contents: &[u8]) -> miette::Result<()> {
        self.files
            .lock()
            .unwrap()
            .push((path.to_string(), contents.to_vec()));
        Ok(())
    }

    async fn file_sha256(&self, _p: &str) -> miette::Result<Option<String>> {
        Ok(None)
    }

    async fn service_active(&self, _n: &str) -> miette::Result<bool> {
        Ok(true)
    }
}

#[derive(Default)]
struct MockPool {
    sessions: Mutex<HashMap<String, Arc<MockSession>>>,
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

fn pool_with(nodes: &[(&str, Arc<MockSession>)]) -> Arc<dyn kurin::SessionPool> {
    let pool = MockPool::default();
    for (name, session) in nodes {
        pool.sessions
            .lock()
            .unwrap()
            .insert(name.to_string(), Arc::clone(session));
    }
    Arc::new(pool)
}

fn cluster_kubeadm() -> Cluster {
    kurin_config::parse_str(
        std::path::Path::new("x.toml"),
        r#"version = "v1alpha1"
name = "t"
[kubernetes]
version = "1.33.1"
[ssh]
user = "root"
[ssh.auth]
method = "password"
password = "x"
[nodes.cp1]
address = "10.0.0.1"
role = "control-plane"
"#,
    )
    .unwrap()
}

fn cluster_k3s() -> Cluster {
    let mut c = cluster_kubeadm();
    c.distribution = kurin_config::Distribution::K3s;
    c
}

// ---------------------------------------------------------------------------
// schedule
// ---------------------------------------------------------------------------

#[tokio::test]
async fn schedule_installs_script_service_and_timer() {
    let session = MockSession::new("");
    let cluster = cluster_kubeadm();
    let pool = pool_with(&[("cp1", session.clone())]);

    let every = parse_every("24h").unwrap();
    let report = certs::schedule(&cluster, &pool, &every, 30).await.unwrap();

    let script = session.file("/usr/local/bin/kurin-cert-renew.sh");
    assert!(script.contains("WARN_DAYS=30"));
    assert!(script.contains("kubeadm certs check-expiration"));
    assert!(script.contains("kubeadm certs renew all"));
    assert!(script.contains("logger -t kurin-cert-renew"));
    // The gate reads the check report's residual times.
    assert!(script.contains("min_residual_days"));

    let service = session.file("/etc/systemd/system/kurin-cert-renew.service");
    assert!(service.contains("ExecStart=/usr/local/bin/kurin-cert-renew.sh"));
    assert!(service.contains("After=kubelet.service"));

    // 24h canonicalizes to the systemd span `1d`.
    let timer = session.file("/etc/systemd/system/kurin-cert-renew.timer");
    assert!(timer.contains("OnBootSec=1d"));
    assert!(timer.contains("OnUnitActiveSec=1d"));
    assert!(timer.contains("Unit=kurin-cert-renew.service"));
    assert!(timer.contains("WantedBy=timers.target"));

    let cmds = session.commands();
    assert!(cmds
        .iter()
        .any(|c| c
            .contains("systemctl daemon-reload && systemctl enable --now kurin-cert-renew.timer")));
    assert!(cmds
        .iter()
        .any(|c| c.contains("systemctl restart kurin-cert-renew.timer")));

    assert_eq!(report.cluster, "t");
    assert_eq!(report.every, "1d");
    assert_eq!(report.warn_days, 30);
    assert_eq!(report.nodes, vec!["cp1".to_string()]);
}

#[tokio::test]
async fn schedule_is_idempotent() {
    let session = MockSession::new("");
    let cluster = cluster_kubeadm();
    let pool = pool_with(&[("cp1", session.clone())]);
    let every = parse_every("12h").unwrap();

    certs::schedule(&cluster, &pool, &every, 30).await.unwrap();
    certs::schedule(&cluster, &pool, &every, 30).await.unwrap();

    // Three files per run, rewritten with the same bytes.
    assert_eq!(session.files.lock().unwrap().len(), 6);
    let enables = session
        .commands()
        .iter()
        .filter(|c| c.contains("systemctl enable --now kurin-cert-renew.timer"))
        .count();
    assert_eq!(enables, 2);
}

#[tokio::test]
async fn schedule_k3s_uses_the_k3s_flow() {
    let session = MockSession::new("");
    let cluster = cluster_k3s();
    let pool = pool_with(&[("cp1", session.clone())]);

    certs::schedule(&cluster, &pool, &parse_every("1h").unwrap(), 7)
        .await
        .unwrap();

    let script = session.file("/usr/local/bin/kurin-cert-renew.sh");
    assert!(script.contains("WARN_DAYS=7"));
    assert!(script.contains("k3s certificate check --output table"));
    assert!(script.contains("k3s certificate rotate"));
    assert!(script.contains("systemctl restart k3s"));
    assert!(!script.contains("kubeadm certs"));

    let service = session.file("/etc/systemd/system/kurin-cert-renew.service");
    assert!(service.contains("After=k3s.service"));

    let timer = session.file("/etc/systemd/system/kurin-cert-renew.timer");
    assert!(timer.contains("OnUnitActiveSec=1h"));
}

// ---------------------------------------------------------------------------
// next renewal (the `systemctl show` probe)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn next_renewal_reads_the_first_control_plane_timer() {
    let session = MockSession::new("NextElapseUSecRealtime=Wed 2026-10-29 06:00:00 UTC\n");
    let pool = pool_with(&[("cp1", session.clone())]);

    let next = certs::next_renewal(&cluster_kubeadm(), &pool)
        .await
        .unwrap();
    assert_eq!(next.as_deref(), Some("Wed 2026-10-29 06:00:00 UTC"));
    let cmds = session.commands();
    let probe = cmds
        .iter()
        .find(|c| c.starts_with("systemctl show kurin-cert-renew.timer"))
        .expect("next-elapse probe executed");
    assert!(probe.contains("--property=NextElapseUSecRealtime"));
    assert!(probe.contains("--property=NextElapseUSecMonotonic"));
}

#[tokio::test]
async fn next_renewal_is_none_without_a_timer() {
    // `systemctl show` prints empty values for a unit that is not loaded.
    let session = MockSession::new("");
    let pool = pool_with(&[("cp1", session)]);

    let next = certs::next_renewal(&cluster_kubeadm(), &pool)
        .await
        .unwrap();

    assert_eq!(next, None);
}
