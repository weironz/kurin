//! WP-19: the in-cluster state record (`kube-system/ConfigMap kurin-state`).
//!
//! Covers the record's serialization (the exact ConfigMap `data` keys), the
//! redaction of `spec.toml`, the missing-record fallback and the add-nodes
//! version preference. No network: the API is a mock.

use kurin::ops::add_nodes::running_version;
use kurin::ops::{read_state, ClusterStateRecord};
use kurin_kube::{
    ApplyOpts, ApplyReport, ClusterApi, DeleteOpts, DynamicObject, Gvk, LogOpts, ObjRef,
};
use miette::Result;
use serde_json::Value;

const FAKE_SSH_PASSWORD: &str = "fake-ssh-password-not-real";
const FAKE_BECOME_PASSWORD: &str = "fake-become-password-not-real";
const FAKE_REGISTRY_PASSWORD: &str = "fake-registry-password-not-real";
const FAKE_PASSPHRASE: &str = "fake-passphrase-not-real";

fn config() -> String {
    format!(
        r#"version = "v1alpha1"
name = "state-test"
[kubernetes]
version = "1.33.1"
[network]
pod-cidr = "10.233.64.0/18"
service-cidr = "10.233.0.0/18"
[network.cni]
kind = "calico"
[ssh]
user = "root"
become-password = "{FAKE_BECOME_PASSWORD}"
[ssh.auth]
method = "password"
password = "{FAKE_SSH_PASSWORD}"
[registry]
kind = "zot"
port = 5000
password = "{FAKE_REGISTRY_PASSWORD}"
[nodes.cp1]
address = "10.99.0.11"
role = "control-plane"
[nodes.w1]
address = "10.99.0.21"
role = "worker"
[nodes.reg1]
address = "10.99.0.31"
role = "registry"
"#
    )
}

fn cluster() -> kurin_config::Cluster {
    kurin_config::parse_str(std::path::Path::new("state.toml"), &config()).expect("config parses")
}

fn cluster_with_passphrase() -> kurin_config::Cluster {
    let toml = config().replace(
        "method = \"password\"\npassword = \"fake-ssh-password-not-real\"",
        &format!("method = \"private_key\"\nkey-path = \"/root/.ssh/id_ed25519\"\npassphrase = \"{FAKE_PASSPHRASE}\""),
    );
    kurin_config::parse_str(std::path::Path::new("state.toml"), &toml).expect("config parses")
}

fn record(cluster: &kurin_config::Cluster) -> ClusterStateRecord {
    ClusterStateRecord::build(
        cluster,
        "op-20261028-000001",
        "create",
        "2026-10-28 12:00:00.0 +00:00:00",
        None,
    )
}

#[test]
fn record_serialization_snapshot() {
    let data = record(&cluster()).to_configmap_data();
    insta::assert_yaml_snapshot!("kurin-state-configmap-data", data);
}

#[test]
fn spec_toml_is_redacted() {
    for cluster in [cluster(), cluster_with_passphrase()] {
        let spec = record(&cluster).spec_toml;
        for secret in [
            FAKE_SSH_PASSWORD,
            FAKE_BECOME_PASSWORD,
            FAKE_REGISTRY_PASSWORD,
            FAKE_PASSPHRASE,
        ] {
            assert!(
                !spec.contains(secret),
                "secret {secret:?} leaked into spec.toml:\n{spec}"
            );
        }
        assert!(spec.contains("******"), "redaction marker missing:\n{spec}");
        // Non-secret fields survive.
        assert!(spec.contains("state-test"));
        assert!(spec.contains("1.33.1"));
    }
}

#[test]
fn history_keeps_the_last_twenty_operations() {
    let cluster = cluster();
    let mut previous = record(&cluster);
    // 25 older entries, then the new op must push the oldest out.
    previous.history = (0..25)
        .map(|i| kurin::tasks::post::HistoryEntry {
            op_id: format!("old-{i}"),
            op_kind: "create".into(),
            finished_at: "2026-01-01 00:00:00.0 +00:00:00".into(),
            kurin_version: "0.3.0".into(),
        })
        .collect();
    let next = ClusterStateRecord::build(
        &cluster,
        "op-new",
        "add-nodes",
        "2026-10-28 13:00:00.0 +00:00:00",
        Some(&previous),
    );
    assert_eq!(next.history.len(), 20);
    assert_eq!(next.history.last().unwrap().op_id, "op-new");
    assert_eq!(next.history.first().unwrap().op_id, "old-6");
    assert_eq!(next.op_kind, "add-nodes");
}

/// Mock API: answers `get_json` with a canned ConfigMap (or nothing).
struct MockApi {
    configmap: Option<Value>,
}

#[async_trait::async_trait]
impl ClusterApi for MockApi {
    async fn apply(&self, _objs: &[DynamicObject], _opts: ApplyOpts) -> Result<ApplyReport> {
        Ok(ApplyReport::default())
    }
    async fn delete(&self, _refs: &[ObjRef], _opts: DeleteOpts) -> Result<()> {
        Ok(())
    }
    async fn get_json(&self, _r: &ObjRef) -> Result<Option<Value>> {
        Ok(self.configmap.clone())
    }
    async fn list_json(
        &self,
        _gvk: &Gvk,
        _ns: Option<&str>,
        _selector: Option<&str>,
    ) -> Result<Vec<Value>> {
        Ok(Vec::new())
    }
    async fn get_raw(&self, _path: &str) -> Result<String> {
        Ok(String::new())
    }
    async fn pod_logs(
        &self,
        _pod: &ObjRef,
        _opts: LogOpts,
        _sink: &mut (dyn for<'a> FnMut(&'a str) + Send),
    ) -> Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn missing_record_is_none() {
    let api = MockApi { configmap: None };
    assert!(read_state(&api).await.unwrap().is_none());
}

#[tokio::test]
async fn record_round_trips_through_the_configmap() {
    let record = record(&cluster());
    let object = serde_json::to_value(record.configmap()).unwrap();
    assert_eq!(
        object
            .pointer("/metadata/namespace")
            .and_then(Value::as_str),
        Some("kube-system")
    );
    assert_eq!(
        object.pointer("/metadata/name").and_then(Value::as_str),
        Some("kurin-state")
    );
    let api = MockApi {
        configmap: Some(object),
    };
    let read = read_state(&api).await.unwrap().expect("record present");
    assert_eq!(read, record);
}

#[test]
fn add_nodes_prefers_the_record_version() {
    let record = record(&cluster());
    // The record says 1.33.1; a stale live probe must not win.
    assert_eq!(
        running_version(Some(&record), Some("v1.33.2")).as_deref(),
        Some("1.33.1")
    );
}

#[test]
fn add_nodes_falls_back_to_the_probe() {
    assert_eq!(
        running_version(None, Some("v1.33.2")).as_deref(),
        Some("1.33.2")
    );
    assert_eq!(running_version(None, None), None);
}
