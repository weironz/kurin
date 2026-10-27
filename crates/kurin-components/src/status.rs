//! Cluster health probe (`kurin status`): node readiness, non-running pods,
//! etcd health and certificate freshness.
//!
//! Everything is collected through [`kurin_kube::ClusterApi`], which prefers
//! the SSH-tunneled kube-rs backend and falls back to kubectl over SSH. On
//! HA clusters kubectl pins `--server https://127.0.0.1:6443` so a broken
//! VIP cannot blind the probe. etcd health: kubeadm counts ready etcd pods,
//! k3s runs its etcd outside pods and is checked via `/healthz/etcd`.

use std::sync::Arc;

use kurin_config::{Cluster, Distribution};
use miette::Result;
use serde_json::Value;

use kurin_core::executor::SessionPool;
use kurin_kube::{connect, Gvk, ALL_NAMESPACES};

/// One row of the node table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeStatus {
    pub name: String,
    pub ready: bool,
    pub kubelet_version: String,
    pub roles: String,
}

/// Aggregated probe result; `healthy` gates the CLI exit code.
#[derive(Debug, Clone)]
pub struct StatusReport {
    pub healthy: bool,
    pub nodes: Vec<NodeStatus>,
    /// `ns/name` of pods that are neither Running nor Succeeded.
    pub pods_not_running: Vec<String>,
    pub etcd_ok: bool,
    /// Smallest days-left across control-plane certificates (None if unknown).
    pub cert_min_days: Option<i64>,
}

fn parse_nodes(items: &[Value]) -> Vec<NodeStatus> {
    items
        .iter()
        .map(|item| {
            let name = item
                .pointer("/metadata/name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let ready = item
                .pointer("/status/conditions")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .any(|c| {
                    c.get("type").and_then(Value::as_str) == Some("Ready")
                        && c.get("status").and_then(Value::as_str) == Some("True")
                });
            let kubelet_version = item
                .pointer("/status/nodeInfo/kubeletVersion")
                .and_then(Value::as_str)
                .unwrap_or("?")
                .to_string();
            let empty = serde_json::Map::new();
            let labels = item
                .pointer("/metadata/labels")
                .and_then(Value::as_object)
                .unwrap_or(&empty);
            let mut roles: Vec<&str> = labels
                .keys()
                .filter_map(|k| k.strip_prefix("node-role.kubernetes.io/"))
                .filter(|r| !r.is_empty())
                .collect();
            roles.sort_unstable();
            let roles = if roles.is_empty() {
                "worker".to_string()
            } else {
                roles.join(",")
            };
            NodeStatus {
                name,
                ready,
                kubelet_version,
                roles,
            }
        })
        .collect()
}

fn parse_pods_not_running(items: &[Value]) -> Vec<String> {
    items
        .iter()
        .filter(|item| {
            !matches!(
                item.pointer("/status/phase").and_then(Value::as_str),
                Some("Running") | Some("Succeeded")
            )
        })
        .map(|item| {
            format!(
                "{}/{}",
                item.pointer("/metadata/namespace")
                    .and_then(Value::as_str)
                    .unwrap_or("?"),
                item.pointer("/metadata/name")
                    .and_then(Value::as_str)
                    .unwrap_or("?"),
            )
        })
        .collect()
}

/// Ready etcd pods (`component=etcd` in kube-system).
fn parse_etcd_ready(items: &[Value]) -> usize {
    items
        .iter()
        .filter(|item| {
            match item
                .pointer("/status/containerStatuses")
                .and_then(Value::as_array)
            {
                Some(cs) if !cs.is_empty() => cs
                    .iter()
                    .all(|c| c.get("ready").and_then(Value::as_bool) == Some(true)),
                _ => false,
            }
        })
        .count()
}

/// Days until the earliest `notAfter` among all control-plane certificates
/// (distribution-specific roots; agent-only nodes report nothing).
///
/// Delegates to `crate::certs::check`, which probes every node and validates
/// the table remotely-computed expiry days.
pub(crate) async fn cert_min_days(
    cluster: &Cluster,
    pool: &Arc<dyn SessionPool>,
) -> Result<Option<i64>> {
    let reports = crate::certs::check(cluster, pool, 0).await?;
    Ok(reports
        .iter()
        .filter_map(|r| r.certs.iter().map(|c| c.days_left).min())
        .min())
}

/// Probe the cluster through the control-plane anchors.
pub async fn collect(cluster: &Cluster, pool: &Arc<dyn SessionPool>) -> Result<StatusReport> {
    let cp: Vec<_> = cluster.control_plane().collect();
    if cp.is_empty() {
        return Err(miette::miette!("cluster has no control-plane node"));
    }
    let anchors: Vec<String> = cp.iter().map(|n| n.name.clone()).collect();
    let mut log = |msg: &str| eprintln!("{msg}");
    let connected = connect(
        pool,
        &anchors,
        cluster.distribution.into(),
        None,
        cp.len() >= 2,
        &mut log,
    )
    .await?;
    let api = connected.api;

    let nodes = parse_nodes(&api.list_json(&Gvk::core("v1", "Node"), None, None).await?);
    let pods_not_running = parse_pods_not_running(
        &api.list_json(&Gvk::core("v1", "Pod"), Some(ALL_NAMESPACES), None)
            .await?,
    );

    let etcd_ok = if cluster.distribution == Distribution::K3s {
        // k3s runs etcd as part of the server process, not in a pod: ask the
        // apiserver directly (any non-5xx answer means etcd is serving).
        api.get_raw("/healthz/etcd").await.is_ok()
    } else if cp.len() >= 2 {
        let etcd = api
            .list_json(
                &Gvk::core("v1", "Pod"),
                Some("kube-system"),
                Some("component=etcd"),
            )
            .await?;
        parse_etcd_ready(&etcd) == cp.len()
    } else {
        api.get_raw("/healthz")
            .await
            .map(|body| body.trim() == "ok")
            .unwrap_or(false)
    };

    let cert_min_days = cert_min_days(cluster, pool).await?;

    let healthy = nodes.iter().all(|n| n.ready)
        && pods_not_running.is_empty()
        && etcd_ok
        && cert_min_days.unwrap_or(i64::MAX) >= 0;

    Ok(StatusReport {
        healthy,
        nodes,
        pods_not_running,
        etcd_ok,
        cert_min_days,
    })
}

/// Human-readable summary to stderr (stdout stays machine-clean).
pub fn render(r: &StatusReport) {
    eprintln!("CLUSTER STATUS");
    eprintln!("{:<24} {:<6} {:<14} ROLES", "NAME", "READY", "VERSION");
    for n in &r.nodes {
        eprintln!(
            "{:<24} {:<6} {:<14} {}",
            n.name,
            if n.ready { "True" } else { "False" },
            n.kubelet_version,
            n.roles
        );
    }
    if !r.pods_not_running.is_empty() {
        eprintln!("\nPODS NOT RUNNING");
        for p in &r.pods_not_running {
            eprintln!("  {p}");
        }
    }
    eprintln!(
        "\netcd: {}",
        if r.etcd_ok { "healthy" } else { "UNHEALTHY" }
    );
    match r.cert_min_days {
        Some(d) => eprintln!("certificates: earliest expiry in {d} day(s)"),
        None => eprintln!("certificates: expiry unknown"),
    }
    eprintln!(
        "overall: {}",
        if r.healthy { "healthy" } else { "UNHEALTHY" }
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(distribution: kurin_config::Distribution, ha: bool) -> Cluster {
        let dist = match distribution {
            kurin_config::Distribution::K3s => "k3s",
            kurin_config::Distribution::K8s => "k8s",
        };
        let network = if dist == "k3s" {
            "[network]\ncni = { kind = \"none\" }\n"
        } else {
            ""
        };
        let mut toml = format!(
            r#"version = "v1alpha1"
name = "t"
[kubernetes]
distribution = "{dist}"
{network}[ssh]
user = "root"
[ssh.auth]
method = "password"
password = "x"
[nodes.cp1]
address = "10.0.0.1"
role = "control-plane"
"#
        );
        if ha {
            toml.push_str("[nodes.cp2]\naddress = \"10.0.0.2\"\nrole = \"control-plane\"\n");
        }
        kurin_config::parse_str(std::path::Path::new("x.toml"), &toml).unwrap()
    }

    #[test]
    fn distribution_maps_to_kubectl_flavor() {
        assert_eq!(
            kurin_kube::Flavor::from(parse(kurin_config::Distribution::K8s, false).distribution),
            kurin_kube::Flavor::Kubeadm
        );
        assert_eq!(
            kurin_kube::Flavor::from(parse(kurin_config::Distribution::K3s, false).distribution),
            kurin_kube::Flavor::K3s
        );
    }

    #[test]
    fn parses_node_rows_from_a_list() {
        let items = vec![serde_json::json!({
            "metadata": {"name": "cp1", "labels": {"node-role.kubernetes.io/control-plane": ""}},
            "status": {
                "conditions": [{"type": "Ready", "status": "True"}],
                "nodeInfo": {"kubeletVersion": "v1.33.2"}
            }
        })];
        let nodes = parse_nodes(&items);
        assert_eq!(nodes.len(), 1);
        assert!(nodes[0].ready);
        assert_eq!(nodes[0].roles, "control-plane");
    }
}
