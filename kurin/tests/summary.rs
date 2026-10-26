//! Summary track integration tests: the Post-stage summary task against a
//! mock session pool. The cert column is probed through the same wire
//! contract as `kurin certs` (`path|subject|not_after|days_left|is_ca`
//! lines), so the mocks replay probe output an old cluster would return.

use std::sync::Arc;

use kurin::{
    engine::{Drift, ExecOutput, Outcome, Session, Task},
    executor::SessionPool,
    tasks::post::Summary,
};
use miette::Result;

/// Replays the `certs` PROBE (`sh -eu -c 'for c in /etc/kubernetes/pki/...'`)
/// output for one node. Control-plane nodes answer with a multi-cert table;
/// workers have no pki tree and answer empty.
struct CertProbeSession {
    node: String,
}

fn probe_table(node: &str) -> &'static str {
    match node {
        // Two certs: the CA outlives the serving cert, so the earliest
        // expiry (360d) is what the summary must pick for master01.
        "master01" => concat!(
            "/etc/kubernetes/pki/apiserver.crt|kube-apiserver|Oct 26 12:00:00 2027 GMT|360|0\n",
            "/etc/kubernetes/pki/ca.crt|kubernetes-ca|Oct 26 12:00:00 2027 GMT|365|1\n",
        ),
        // An aged cluster: serving cert below the 30d warn threshold.
        "master02" => concat!(
            "/etc/kubernetes/pki/apiserver.crt|kube-apiserver|Nov 26 12:00:00 2026 GMT|29|0\n",
            "/etc/kubernetes/pki/etcd/server.crt|etcd-server|Oct 26 12:00:00 2027 GMT|400|0\n",
        ),
        _ => "",
    }
}
#[async_trait::async_trait]
impl Session for CertProbeSession {
    async fn exec(&self, command: &str) -> Result<ExecOutput> {
        if command.contains("openssl x509") {
            return Ok(ExecOutput {
                exit_code: 0,
                stdout: probe_table(&self.node).into(),
                stderr: String::new(),
            });
        }
        // Summary never issues anything but the cert probe.
        panic!("unexpected command on {}: {command}", self.node);
    }
    async fn write_file(&self, _path: &str, _contents: &[u8]) -> Result<()> {
        panic!("summary never writes files");
    }
    async fn file_sha256(&self, _path: &str) -> Result<Option<String>> {
        panic!("summary never hashes files");
    }
    async fn service_active(&self, _name: &str) -> Result<bool> {
        panic!("summary never checks services");
    }
}

struct MockPool;

#[async_trait::async_trait]
impl SessionPool for MockPool {
    async fn get(&self, node: &str) -> Result<Arc<dyn Session>> {
        Ok(Arc::new(CertProbeSession {
            node: node.to_string(),
        }))
    }
}

fn cluster() -> kurin_config::Cluster {
    kurin_config::parse_str(
        std::path::Path::new("summary.toml"),
        r##"version = "v1alpha1"
name = "kfc"
[ssh]
user = "root"
[ssh.auth]
method = "password"
password = "x"
[nodes.master01]
address = "192.168.10.11"
role = "control-plane"
[nodes.master02]
address = "192.168.10.12"
role = "control-plane"
[nodes.worker01]
address = "192.168.10.21"
"##,
    )
    .expect("parse cluster")
}

#[tokio::test(flavor = "multi_thread")]
async fn collect_merges_earliest_cert_expiry_per_control_plane() {
    let summary = Summary::new(&cluster(), Arc::new(MockPool));
    let report = summary.collect().await;

    let by_name = |name: &str| {
        report
            .nodes
            .iter()
            .find(|n| n.name == name)
            .unwrap_or_else(|| panic!("no row for {name}"))
    };
    // Earliest, not the first line: CA (365) must not mask apiserver (360).
    assert_eq!(by_name("master01").cert_days_left, Some(360));
    // The etcd cert (400d) must not mask the aged apiserver cert (29d).
    assert_eq!(by_name("master02").cert_days_left, Some(29));
    // Workers have no pki tree: the column stays empty.
    assert_eq!(by_name("worker01").cert_days_left, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn table_marks_aged_cluster_certs() {
    let summary = Summary::new(&cluster(), Arc::new(MockPool));
    let table = summary.collect().await.to_table();

    assert!(table.contains("CLUSTER SUMMARY"), "{table}");
    assert!(table.contains("cluster: kfc"), "{table}");
    assert!(table.contains("v1.33.1"), "{table}");
    assert!(
        table.contains("3 node(s): 2 control-plane, 1 worker"),
        "{table}"
    );
    let cols = |name: &str| -> Vec<&str> {
        table
            .lines()
            .find(|l| l.starts_with(name))
            .unwrap_or_else(|| panic!("no row for {name}: {table}"))
            .split_whitespace()
            .collect()
    };
    // Healthy control plane: no mark.
    assert_eq!(
        cols("master01"),
        ["master01", "control-plane", "v1.33.1", "v1.33.1", "360d"],
        "{table}"
    );
    // master02 is below the 30-day threshold.
    assert_eq!(
        cols("master02"),
        ["master02", "control-plane", "v1.33.1", "v1.33.1", "29d", "WARN"],
        "{table}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn apply_is_a_verified_report() {
    let summary = Summary::new(&cluster(), Arc::new(MockPool));
    // The executor hands the task its anchor node's session; Summary
    // ignores it and probes through the pool instead.
    let session = MockPool.get("master01").await.unwrap();
    assert!(matches!(
        summary.check(session.as_ref()).await.unwrap(),
        Drift::Drifted(_)
    ));
    assert!(matches!(
        summary.apply(session.as_ref()).await.unwrap(),
        Outcome::Verified
    ));
}
