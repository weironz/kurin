//! UI §6.9: `status::collect`'s multi-control-plane fallback.
//!
//! The anchors are tried in config order: when the first control plane
//! is unreachable, the next one is used; only when every control plane
//! fails is the cluster judged unreachable. The sessions here answer
//! `kubectl` commands with canned JSON (the backend `connect` falls back
//! to when sshd refuses TCP forwarding), so the whole production path —
//! pool, anchor loop, kubectl backend, report parsing — runs without SSH.

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use kurin_components::status::{collect, NodeStatus, StatusOpts};
use kurin_core::engine::{Cmd, ExecOutput, Session};
use kurin_core::executor::SessionPool;
use kurin_kube::ALL_NAMESPACES;

const CLUSTER: &str = r#"
version = "v1alpha1"
name = "ha-fallback"

[kubernetes]
version = "1.33.1"

[network]
pod-cidr = "10.233.64.0/18"
service-cidr = "10.233.0.0/18"

[network.cni]
kind = "calico"

[ssh]
user = "root"
port = 22

[ssh.auth]
method = "private_key"
key-path = "/root/.ssh/id_ed25519"

[nodes.master01]
address = "192.168.10.11"
role = "control-plane"

[nodes.master02]
address = "192.168.10.12"
role = "control-plane"

[nodes.worker01]
address = "192.168.10.21"
role = "worker"
"#;

/// Two ready control-plane nodes and one ready worker.
const NODES: &str = r#"{"items": [
  {"metadata": {"name": "master01",
      "labels": {"node-role.kubernetes.io/control-plane": ""}},
   "status": {"conditions": [{"type": "Ready", "status": "True"}],
      "nodeInfo": {"kubeletVersion": "v1.33.1"}}},
  {"metadata": {"name": "master02",
      "labels": {"node-role.kubernetes.io/control-plane": ""}},
   "status": {"conditions": [{"type": "Ready", "status": "True"}],
      "nodeInfo": {"kubeletVersion": "v1.33.1"}}},
  {"metadata": {"name": "worker01"},
   "status": {"conditions": [{"type": "Ready", "status": "True"}],
      "nodeInfo": {"kubeletVersion": "v1.33.1"}}}
]}"#;

/// Two ready etcd pods (`component=etcd`), matching the control-plane
/// count so the kubeadm etcd check passes.
const ETCD_PODS: &str = r#"{"items": [
  {"metadata": {"name": "etcd-master01"},
   "status": {"containerStatuses": [{"ready": true}]}},
  {"metadata": {"name": "etcd-master02"},
   "status": {"containerStatuses": [{"ready": true}]}}
]}"#;

/// A session that answers kubectl commands with canned JSON. The default
/// `forward_local` refuses forwarding, so `connect` falls back to the
/// kubectl backend — the same path a cluster with sshd `AllowTcpForwarding
/// no` takes.
struct KubectlSession;

#[async_trait]
impl Session for KubectlSession {
    async fn exec(&self, command: &str) -> miette::Result<ExecOutput> {
        self.exec_cmd(&Cmd::probe(command)).await
    }

    async fn exec_cmd(&self, cmd: &Cmd) -> miette::Result<ExecOutput> {
        let script = cmd.script.as_str();
        let stdout = if script.contains("get --raw /version") {
            r#"{"gitVersion": "v1.33.1"}"#.to_string()
        } else if script.contains("get --raw /healthz") {
            "ok".to_string()
        } else if script.contains("get pods -n kube-system -l component=etcd") {
            ETCD_PODS.to_string()
        } else if script.contains("get nodes") {
            NODES.to_string()
        } else if script.contains("get pods") {
            r#"{"items": []}"#.to_string()
        } else {
            // No kurin-state ConfigMap: `get_json` treats empty output as
            // "not found".
            String::new()
        };
        Ok(ExecOutput {
            exit_code: 0,
            stdout,
            stderr: String::new(),
        })
    }

    async fn write_file(&self, _path: &str, _contents: &[u8]) -> miette::Result<()> {
        Ok(())
    }

    async fn file_sha256(&self, _path: &str) -> miette::Result<Option<String>> {
        Ok(None)
    }

    async fn service_active(&self, _name: &str) -> miette::Result<bool> {
        Ok(true)
    }
}

/// A pool whose `get` fails for the named nodes — the SSH-level view of
/// "this control plane is unreachable".
struct Pool {
    unreachable: HashSet<&'static str>,
}

#[async_trait]
impl SessionPool for Pool {
    async fn get(&self, node: &str) -> miette::Result<Arc<dyn Session>> {
        if self.unreachable.contains(node) {
            Err(miette::miette!(
                "ssh: node {node} unreachable (dial timeout)"
            ))
        } else {
            Ok(Arc::new(KubectlSession))
        }
    }
}

fn spec() -> kurin_config::Cluster {
    kurin_config::parse_str(std::path::Path::new("cluster.toml"), CLUSTER).expect("parse")
}

#[tokio::test]
async fn the_second_control_plane_takes_over_when_the_first_is_unreachable() {
    let spec = spec();
    // `master01` is down; the fallback must serve the report through
    // `master02` (UI §6.9).
    let pool: Arc<dyn SessionPool> = Arc::new(Pool {
        unreachable: HashSet::from(["master01"]),
    });

    let report = collect(&spec, &pool, StatusOpts { certs: false })
        .await
        .expect("collect via the second control plane");

    assert!(report.healthy, "the report itself should be healthy");
    assert_eq!(
        report
            .nodes
            .iter()
            .map(|node| (node.name.as_str(), node.ready))
            .collect::<Vec<_>>(),
        vec![("master01", true), ("master02", true), ("worker01", true),]
    );
    assert_eq!(
        report.nodes[0],
        NodeStatus {
            name: "master01".into(),
            ready: true,
            kubelet_version: "v1.33.1".into(),
            roles: "control-plane".into()
        }
    );
    assert!(report.etcd_ok);
    assert!(report.pods_not_running.is_empty());
    assert_eq!(report.observed_version.as_deref(), Some("v1.33.1"));
}

#[tokio::test]
async fn every_control_plane_failing_means_unreachable() {
    let spec = spec();
    let pool: Arc<dyn SessionPool> = Arc::new(Pool {
        unreachable: HashSet::from(["master01", "master02"]),
    });

    let error = collect(&spec, &pool, StatusOpts { certs: false })
        .await
        .expect_err("all control planes down");
    let message = format!("{error:#}");
    assert!(
        message.contains("unreachable"),
        "the error should name the failure: {message}"
    );
}

/// The worker never matters for reachability: only control-plane nodes
/// are anchors (regression guard for the anchor selection itself).
#[tokio::test]
async fn an_unreachable_worker_does_not_block_collection() {
    let spec = spec();
    let pool: Arc<dyn SessionPool> = Arc::new(Pool {
        unreachable: HashSet::from(["worker01"]),
    });
    let report = collect(&spec, &pool, StatusOpts { certs: false })
        .await
        .expect("collect ignores worker reachability");
    assert_eq!(report.nodes.len(), 3);
    assert!(report.healthy);
    // Sanity: the pod listing runs across all namespaces.
    assert_eq!(ALL_NAMESPACES, "*");
}
