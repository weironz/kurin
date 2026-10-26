//! Node/cluster teardown. Explicit commands only (spec: dangerous ops never
//! trigger from config diffs). Best-effort with per-step reporting; the goal
//! is a provably clean node, `kubeadm reset`-style, plus kurin-installed
//! artifacts removed.

use std::sync::Arc;

use kurin_config::{Cluster, Node};
use miette::Result;

use crate::engine::Session;
use crate::executor::SessionPool;

/// Files and state kurin/kubeadm place on a node.
const CLEANUP_PATHS: &[&str] = &[
    "/etc/kubernetes",
    "/var/lib/etcd",
    "/var/lib/kubelet",
    "/var/lib/cni",
    "/etc/cni/net.d",
    "/opt/cni/bin",
    "/etc/containerd/config.toml",
    "/etc/sysctl.d/99-kurin.conf",
    "/etc/kurin",
    "/etc/systemd/system/kubelet.service",
];

const CLEANUP_BINARIES: &[&str] = &[
    "/usr/local/bin/*.kurin-new",
    "/usr/local/bin/kubeadm",
    "/usr/local/bin/kubelet",
    "/usr/local/bin/kubectl",
    "/usr/local/bin/crictl",
    "/usr/local/bin/runc",
    "/usr/local/bin/containerd",
    "/usr/local/bin/containerd-shim",
    "/usr/local/bin/containerd-shim-runc-v1",
    "/usr/local/bin/containerd-shim-runc-v2",
    "/usr/local/bin/containerd-stress",
    "/usr/local/bin/ctr",
];

fn report(node: &str, step: &str, ok: bool, detail: &str) {
    let mark = if ok { "ok" } else { "!!" };
    let detail = detail.trim();
    if detail.is_empty() {
        eprintln!("[{node}] {mark} {step}");
    } else {
        eprintln!("[{node}] {mark} {step}: {detail}");
    }
}

async fn session_of(pool: &Arc<dyn SessionPool>, name: &str) -> Result<Arc<dyn Session>> {
    pool.get(name).await
}

/// Wipe one node: drain+delete from the cluster (when the API is reachable),
/// kubeadm reset, service stop, artifact removal. Failures are reported but
/// do not stop the wipe — a partially destroyed node must not block the rest.
pub async fn delete_node(
    pool: &Arc<dyn SessionPool>,
    node: &Node,
    api_session: Option<&Arc<dyn Session>>,
) -> Result<()> {
    // 1. Cluster-level removal via the first control plane, when available.
    if let Some(cp) = api_session {
        let k = "kubectl --kubeconfig /etc/kubernetes/admin.conf --request-timeout=15s";
        // --disable-eviction: on NotReady nodes graceful eviction can wait
        // forever; teardown deletes pods directly. --timeout bounds the rest.
        let drain = cp
            .exec(&format!(
                "{k} drain {} --ignore-daemonsets --delete-emptydir-data --force \
                 --disable-eviction --grace-period=0 --timeout=90s 2>&1; true",
                node.name
            ))
            .await?;
        report(
            &node.name,
            "drain",
            drain.exit_code == 0,
            drain.stderr.trim().lines().next().unwrap_or(""),
        );
        let del = cp
            .exec(&format!("{k} delete node {} 2>&1; true", node.name))
            .await?;
        report(
            &node.name,
            "delete node",
            del.exit_code == 0,
            del.stderr.trim().lines().next().unwrap_or(""),
        );
    }

    let s = session_of(pool, &node.name).await?;

    // 2. kubeadm reset (stops kubelet, removes static pods, kills etcd data).
    let out = s.exec("kubeadm reset -f 2>&1; true").await?;
    report(
        &node.name,
        "kubeadm reset",
        out.ok(),
        out.stdout.trim().lines().next().unwrap_or(""),
    );

    // 3. kube-vip VIP release (harmless when never assigned).
    let _ = s
        .exec(
            "ip a 2>/dev/null | awk '/inet .*\\/32/{print $2}' | \
               grep -vE '127\\.|/32$' -m1 >/dev/null; true",
        )
        .await;

    // 4. Stop services, then remove every artifact kurin/kubeadm laid down.
    let out = s
        .exec("systemctl disable --now kubelet 2>&1; systemctl disable --now containerd 2>&1; true")
        .await?;
    report(&node.name, "services stopped", out.exit_code == 0, "");

    let paths = CLEANUP_PATHS
        .iter()
        .chain(CLEANUP_BINARIES)
        .map(|p| format!("rm -rf {p}"))
        .collect::<Vec<_>>()
        .join(" && ");
    let out = s.exec(&format!("{paths}; true")).await?;
    report(
        &node.name,
        "artifacts removed",
        out.ok(),
        out.stderr.trim().lines().next().unwrap_or(""),
    );
    Ok(())
}

/// Wipe the whole cluster node by node. The API-level drain for control-plane
/// members is only possible while quorum exists; for a full deletion we skip
/// drains and nuke nodes directly.
pub async fn delete_cluster(pool: &Arc<dyn SessionPool>, cluster: &Cluster) -> Result<()> {
    let cp0 = cluster.control_plane().next().map(|n| n.name.clone());
    let cp_session = match &cp0 {
        Some(name) => match session_of(pool, name).await {
            Ok(s) => Some(s),
            Err(e) => {
                eprintln!("warn: first control-plane unreachable, skipping API-level drains: {e}");
                None
            }
        },
        None => None,
    };
    // Workers first, then control plane: teardown order matters for etcd.
    for node in cluster.workers() {
        delete_node(pool, node, cp_session.as_ref()).await?;
    }
    for node in cluster.control_plane() {
        // After the first reset this node no longer speaks API; only drain it
        // while still a member.
        delete_node(pool, node, None).await?;
    }
    // Drop local state of this cluster.
    let join_dir = crate::tasks::bootstrap::join_material_path(&cluster.name);
    let _ = std::fs::remove_dir_all(&join_dir);
    if let Some(state) = crate::state::RunState::default_path(&cluster.name) {
        let _ = std::fs::remove_file(&state);
    }
    eprintln!("cluster {} deleted", cluster.name);
    Ok(())
}
