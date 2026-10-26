//! Cluster health probe (`kurin status`): node readiness, non-running pods,
//! etcd health and certificate freshness.
//!
//! Everything is collected through the first control-plane node's session:
//! kubectl there always uses `/etc/kubernetes/admin.conf`, and on HA clusters
//! `--server https://127.0.0.1:6443` (the local apiserver, so a broken VIP
//! cannot blind the probe).

use std::sync::Arc;

use kurin_config::Cluster;
use miette::{IntoDiagnostic, Result, WrapErr};
use serde_json::Value;

use crate::engine::Session;
use crate::executor::SessionPool;

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

/// Base kubectl invocation; HA clusters pin the local apiserver.
fn kubectl_base(cluster: &Cluster) -> String {
    let ha = cluster.control_plane().nth(1).is_some();
    let mut cmd = String::from("kubectl --kubeconfig /etc/kubernetes/admin.conf");
    if ha {
        cmd.push_str(" --server https://127.0.0.1:6443");
    }
    cmd
}

async fn kubectl_json(s: &Arc<dyn Session>, base: &str, args: &str) -> Result<Value> {
    let cmd = format!("{base} {args}");
    let out = s.exec(&cmd).await.wrap_err_with(|| format!("cannot run `{cmd}`"))?;
    if !out.ok() {
        miette::bail!(
            "`{cmd}` exited {}: {}",
            out.exit_code,
            out.stderr.trim()
        );
    }
    serde_json::from_str(&out.stdout)
        .into_diagnostic()
        .wrap_err_with(|| format!("`{cmd}` returned invalid JSON"))
}

fn parse_nodes(v: &Value) -> Vec<NodeStatus> {
    v.get("items")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
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

fn parse_pods_not_running(v: &Value) -> Vec<String> {
    v.get("items")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
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
fn parse_etcd_ready(v: &Value) -> usize {
    v.get("items")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|item| match item
            .pointer("/status/containerStatuses")
            .and_then(Value::as_array)
        {
            Some(cs) if !cs.is_empty() => cs
                .iter()
                .all(|c| c.get("ready").and_then(Value::as_bool) == Some(true)),
            _ => false,
        })
        .count()
}

/// Days until the earliest `notAfter` under `/etc/kubernetes/pki` on every
/// control-plane node.
///
/// Local implementation: track A's `crate::certs::check` is not exported yet
/// (lib.rs is the integration owner's file), so this reads the same paths with
/// `openssl` + `date` remotely and needs no extra crate. When `certs` is wired
/// in, this should delegate to `crate::certs::check(..)` instead.
pub(crate) async fn cert_min_days(
    cluster: &Cluster,
    pool: &Arc<dyn SessionPool>,
) -> Result<Option<i64>> {
    // One `<days>` line per certificate found; `0`/`-1` fallback when the
    // date cannot be parsed but openssl -checkend can judge validity.
    const SCRIPT: &str = r#"now=$(date +%s)
for f in $(grep -rsl -- 'BEGIN CERTIFICATE' /etc/kubernetes/pki 2>/dev/null || true); do
  end=$(openssl x509 -in "$f" -noout -enddate 2>/dev/null | cut -d= -f2-)
  [ -n "$end" ] || continue
  ts=$(date -d "$end" +%s 2>/dev/null || echo "")
  if [ -n "$ts" ]; then
    echo $(( (ts - now) / 86400 ))
  elif openssl x509 -in "$f" -noout -checkend 0 >/dev/null 2>&1; then
    echo 0
  else
    echo -1
  fi
done
exit 0"#;
    let mut min: Option<i64> = None;
    for node in cluster.control_plane() {
        let s = pool.get(&node.name).await?;
        let out = s
            .exec(SCRIPT)
            .await
            .wrap_err_with(|| format!("cert probe failed on {}", node.name))?;
        // A missing pki dir yields no lines: node not (yet) a control plane.
        if !out.ok() {
            return Err(miette::miette!(
                "cert probe on {} exited {}: {}",
                node.name,
                out.exit_code,
                out.stderr.trim()
            ));
        }
        for line in out.stdout.lines() {
            if let Ok(days) = line.trim().parse::<i64>() {
                min = Some(min.map_or(days, |m| m.min(days)));
            }
        }
    }
    Ok(min)
}

/// Probe the cluster through the first control-plane session.
pub async fn collect(cluster: &Cluster, pool: &Arc<dyn SessionPool>) -> Result<StatusReport> {
    let cp: Vec<_> = cluster.control_plane().collect();
    let first = cp
        .first()
        .ok_or_else(|| miette::miette!("cluster has no control-plane node"))?;
    let s = pool.get(&first.name).await?;
    let base = kubectl_base(cluster);

    let nodes = parse_nodes(&kubectl_json(&s, &base, "get nodes -o json").await?);
    let pods_not_running =
        parse_pods_not_running(&kubectl_json(&s, &base, "get pods -A -o json").await?);

    let etcd_ok = if cp.len() >= 2 {
        let etcd = kubectl_json(
            &s,
            &base,
            "-n kube-system get pods -l component=etcd -o json",
        )
        .await?;
        parse_etcd_ready(&etcd) == cp.len()
    } else {
        let cmd = format!("{base} get --raw /healthz");
        let out = s
            .exec(&cmd)
            .await
            .wrap_err_with(|| format!("cannot run `{cmd}`"))?;
        out.ok() && out.stdout.trim() == "ok"
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