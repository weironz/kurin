//! Cluster health probe (`kurin status`): node readiness, non-running pods,
//! etcd health and certificate freshness.
//!
//! Everything is collected through [`kurin_kube::ClusterApi`], which prefers
//! the SSH-tunneled kube-rs backend and falls back to kubectl over SSH. On
//! HA clusters kubectl pins `--server https://127.0.0.1:6443` so a broken
//! VIP cannot blind the probe. etcd health: kubeadm counts ready etcd pods,
//! k3s runs its etcd outside pods and is checked via `/healthz/etcd`.

use std::io::IsTerminal;
use std::sync::Arc;

use kurin_config::{Cluster, Distribution};
use miette::Result;
use serde::Serialize;
use serde_json::Value;

use kurin_core::executor::SessionPool;
use kurin_kube::{connect, Gvk, ALL_NAMESPACES};

/// One row of the node table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NodeStatus {
    pub name: String,
    pub ready: bool,
    pub kubelet_version: String,
    pub roles: String,
    pub internal_ip: String,
    pub container_runtime: String,
    pub kernel_version: String,
}

/// Aggregated probe result; `healthy` gates the CLI exit code.
#[derive(Debug, Clone, Serialize)]
pub struct StatusReport {
    pub healthy: bool,
    pub nodes: Vec<NodeStatus>,
    /// `ns/name` of pods that are neither Running nor Succeeded.
    pub pods_not_running: Vec<String>,
    pub etcd_ok: bool,
    /// Smallest days-left across control-plane certificates (None if unknown).
    pub cert_min_days: Option<i64>,
    /// Next `kurin-cert-renew.timer` elapse on the first control plane,
    /// exactly as systemd printed it (`Wed 2026-10-29 06:00:00 UTC`);
    /// `None` when the renewal timer is not installed.
    pub cert_next_renewal: Option<String>,
    /// Version kurin recorded in `kube-system/kurin-state` at the last
    /// mutation (`v1.33.2`); `None` on clusters without a record.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_version: Option<String>,
    /// Version the apiserver reports now (`/version` `gitVersion`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed_version: Option<String>,
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
            let internal_ip = item
                .pointer("/status/addresses")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .find(|a| a.get("type").and_then(Value::as_str) == Some("InternalIP"))
                .and_then(|a| a.get("address"))
                .and_then(Value::as_str)
                .unwrap_or("?")
                .to_string();
            let container_runtime = item
                .pointer("/status/nodeInfo/containerRuntimeVersion")
                .and_then(Value::as_str)
                .unwrap_or("?")
                .to_string();
            let kernel_version = item
                .pointer("/status/nodeInfo/kernelVersion")
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
                internal_ip,
                container_runtime,
                kernel_version,
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

/// Knobs for a status probe.
#[derive(Debug, Clone, Copy)]
pub struct StatusOpts {
    /// Also probe certificate expiry. The core health data (nodes, pods,
    /// etcd) is independent, so callers that poll certificates less often
    /// can skip the extra SSH round trips.
    pub certs: bool,
}

impl Default for StatusOpts {
    fn default() -> Self {
        StatusOpts { certs: true }
    }
}

/// Probe the cluster through the control-plane anchors.
///
/// The anchors are tried in config order: when the first control plane is
/// unreachable, the next one is used. On HA clusters kubectl pins
/// `--server https://127.0.0.1:6443` so a broken VIP cannot blind the probe.
pub async fn collect(
    cluster: &Cluster,
    pool: &Arc<dyn SessionPool>,
    opts: StatusOpts,
) -> Result<StatusReport> {
    let cp: Vec<_> = cluster.control_plane().collect();
    if cp.is_empty() {
        return Err(miette::miette!("cluster has no control-plane node"));
    }
    let mut log = |msg: &str| eprintln!("{msg}");
    let mut connected = None;
    let mut last_err = None;
    for node in &cp {
        match connect(
            pool,
            std::slice::from_ref(&node.name),
            cluster.distribution.into(),
            None,
            cp.len() >= 2,
            &mut log,
        )
        .await
        {
            Ok(c) => {
                connected = Some(c);
                break;
            }
            Err(err) => {
                log(&format!(
                    "status: control plane {} unreachable ({err}); trying the next one",
                    node.name
                ));
                last_err = Some(err);
            }
        }
    }
    let connected = connected.ok_or_else(|| {
        last_err.unwrap_or_else(|| miette::miette!("cluster has no control-plane node"))
    })?;
    let api = connected.api;

    // Expected vs observed version (ADR-09): the record says what kurin
    // installed; `/version` says what the apiserver runs now.
    let expected_version = match crate::tasks::post::read_state(api.as_ref()).await {
        Ok(Some(record)) => record.kubernetes_version().map(|v| format!("v{v}")),
        Ok(None) => None,
        Err(err) => {
            log(&format!("status: kurin-state record unreadable ({err})"));
            None
        }
    };
    let observed_version = api
        .get_raw("/version")
        .await
        .ok()
        .and_then(|body| parse_git_version(&body));

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

    let cert_min_days = if opts.certs {
        cert_min_days(cluster, pool).await?
    } else {
        None
    };

    // The renewal timer is one cheap `systemctl show` on the first
    // control plane (not the certificate walk above), so it is always
    // probed; an unreachable node degrades to `None`.
    let cert_next_renewal = match crate::certs::next_renewal(cluster, pool).await {
        Ok(next) => next,
        Err(err) => {
            log(&format!("status: renewal timer unreadable ({err})"));
            None
        }
    };

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
        cert_next_renewal,
        expected_version,
        observed_version,
    })
}

/// `gitVersion` from an apiserver `/version` body (`v1.33.2`).
fn parse_git_version(body: &str) -> Option<String> {
    serde_json::from_str::<Value>(body)
        .ok()?
        .get("gitVersion")?
        .as_str()
        .map(str::to_string)
}

/// Human-readable summary to stderr (stdout stays machine-clean).
pub fn render(r: &StatusReport) {
    let color = std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none();
    render_with_color(r, color);
}

fn paint(color: bool, code: &str, text: &str) -> String {
    if color {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

/// The node table, one string per line (header first).
fn table_lines(r: &StatusReport, color: bool) -> Vec<String> {
    let name_w = r
        .nodes
        .iter()
        .map(|n| n.name.len())
        .max()
        .unwrap_or(4)
        .max(4);
    let role_w = r
        .nodes
        .iter()
        .map(|n| n.roles.len())
        .max()
        .unwrap_or(4)
        .max(4);
    let ip_w = r
        .nodes
        .iter()
        .map(|n| n.internal_ip.len())
        .max()
        .unwrap_or(11)
        .max(11);
    let runtime_w = r
        .nodes
        .iter()
        .map(|n| n.container_runtime.len())
        .max()
        .unwrap_or(7)
        .max(7);
    let mut out = vec![
        "CLUSTER STATUS".to_string(),
        format!(
            "{:<name_w$} {:<role_w$} {:<ip_w$} {:<8} {:<runtime_w$} KERNEL",
            "NODE", "ROLE", "INTERNAL-IP", "STATUS", "RUNTIME"
        ),
    ];
    for n in &r.nodes {
        let status = if n.ready { "Ready" } else { "NotReady" };
        let status = format!("{status:<8}");
        let status = if n.ready {
            paint(color, "32", &status)
        } else {
            paint(color, "31", &status)
        };
        let role = format!("{:<role_w$}", n.roles);
        let role = paint(color, "2", &role);
        out.push(format!(
            "{:<name_w$} {role} {:<ip_w$} {status} {:<runtime_w$} {}",
            n.name, n.internal_ip, n.container_runtime, n.kernel_version
        ));
    }
    out
}

/// Testable/color-controlled variant of [`render`].
pub fn render_with_color(r: &StatusReport, color: bool) {
    for line in table_lines(r, color) {
        eprintln!("{line}");
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
    match (&r.expected_version, &r.observed_version) {
        (Some(expected), Some(observed)) if expected != observed => {
            eprintln!("version: expected {expected} (kurin-state), observed {observed} (MISMATCH)")
        }
        (Some(expected), Some(observed)) => {
            eprintln!("version: {observed} (matches kurin-state {expected})")
        }
        (Some(expected), None) => eprintln!("version: expected {expected} (kurin-state)"),
        (None, Some(observed)) => eprintln!("version: {observed} (no kurin-state record)"),
        (None, None) => eprintln!("version: unknown (no kurin-state record)"),
    }
    match r.cert_min_days {
        Some(d) => eprintln!("certificates: earliest expiry in {d} day(s)"),
        None => eprintln!("certificates: expiry unknown"),
    }
    match r.cert_next_renewal.as_deref() {
        Some(ts) => eprintln!(
            "certificates: next auto-renewal {ts} ({})",
            crate::certs::SCHEDULE_TIMER_NAME
        ),
        None => eprintln!(
            "certificates: auto-renewal not scheduled (no {})",
            crate::certs::SCHEDULE_TIMER_NAME
        ),
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

    #[test]
    fn table_lists_runtime_kernel_and_internal_ip() {
        let report = StatusReport {
            healthy: true,
            nodes: vec![
                NodeStatus {
                    name: "master01".into(),
                    ready: true,
                    kubelet_version: "v1.33.1".into(),
                    roles: "control-plane".into(),
                    internal_ip: "192.168.1.11".into(),
                    container_runtime: "containerd://1.7.22".into(),
                    kernel_version: "6.8.0-45-generic".into(),
                },
                NodeStatus {
                    name: "worker01".into(),
                    ready: false,
                    kubelet_version: "v1.33.1".into(),
                    roles: "worker".into(),
                    internal_ip: "192.168.1.21".into(),
                    container_runtime: "containerd://1.7.22".into(),
                    kernel_version: "6.8.0-45-generic".into(),
                },
            ],
            pods_not_running: vec![],
            etcd_ok: true,
            cert_min_days: None,
            cert_next_renewal: None,
            expected_version: None,
            observed_version: None,
        };
        let lines = table_lines(&report, false);
        assert_eq!(lines[0], "CLUSTER STATUS");
        assert!(lines[1].contains("NODE"), "{}", lines[1]);
        assert!(lines[1].contains("INTERNAL-IP"), "{}", lines[1]);
        assert!(lines[1].contains("RUNTIME"), "{}", lines[1]);
        assert!(lines[1].contains("KERNEL"), "{}", lines[1]);
        assert!(lines[2].contains("master01"), "{}", lines[2]);
        assert!(lines[2].contains("192.168.1.11"), "{}", lines[2]);
        assert!(lines[2].contains("containerd://1.7.22"), "{}", lines[2]);
        assert!(lines[2].contains("6.8.0-45-generic"), "{}", lines[2]);
        assert!(lines[3].contains("NotReady"), "{}", lines[3]);
    }
}
