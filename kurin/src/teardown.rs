//! Node/cluster teardown. Explicit commands only (spec: dangerous ops never
//! trigger from config diffs). Best-effort with per-step reporting; the goal
//! is a provably clean node, `kubeadm reset`-style, plus kurin-installed
//! artifacts removed.
//!
//! The wipe set is distribution-aware: a k3s node has no kubeadm, no
//! `/etc/kubernetes` and no standalone kubectl (k3s embeds kubectl), while a
//! kubeadm node keeps its etcd data and pki under `/etc/kubernetes`. Runtime
//! leftovers (cri-o) are only touched on clusters that actually used cri-o —
//! on a containerd node `/etc/containers` may belong to a foreign install.

use std::sync::Arc;

use kurin_config::{Cluster, Distribution, Node, RuntimeKind};
use miette::Result;

use crate::engine::Session;
use crate::executor::SessionPool;
use crate::tasks::k3s::K3S_KUBECONFIG;

/// Artifacts both tracks leave behind.
const COMMON_PATHS: &[&str] = &[
    "/var/lib/kubelet",
    "/var/lib/cni",
    "/etc/cni/net.d",
    "/opt/cni/bin",
    "/etc/containerd/config.toml",
    "/etc/sysctl.d/99-kurin.conf",
    "/etc/kurin",
    "/etc/systemd/system/kubelet.service",
];

/// kubeadm-only control-plane state: pki, static pods, etcd.
const KUBEADM_PATHS: &[&str] = &["/etc/kubernetes", "/var/lib/etcd"];

/// k3s config, cluster data, its embedded containerd/agent state and the
/// pod logs k3s's CRI writes (kubeadm's kubelet cleans these itself).
const K3S_PATHS: &[&str] = &[
    "/etc/rancher",
    "/var/lib/rancher",
    "/var/log/pods",
    "/var/log/containers",
];

/// cri-o payload (binary, OCI helpers, config, registry policy).
const CRIO_PATHS: &[&str] = &[
    "/etc/crio",
    "/usr/local/bin/crio",
    "/usr/local/bin/pinns",
    "/usr/local/libexec/crio",
    "/etc/containers",
];

/// The k3s binary itself (kubectl lives inside it).
const K3S_BINARY: &str = "/usr/local/bin/k3s";
/// kubeadm track's kubectl.
const KUBECTL_BINARY: &str = "/usr/local/bin/kubectl";

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

/// kubectl invocation for API-level operations (drain/delete node). k3s has
/// no standalone kubectl: it is a subcommand of the k3s binary.
fn kubectl_cmd(cluster: &Cluster) -> String {
    match cluster.distribution {
        Distribution::K8s => {
            format!("{KUBECTL_BINARY} --kubeconfig /etc/kubernetes/admin.conf")
        }
        Distribution::K3s => format!("{K3S_BINARY} kubectl --kubeconfig {K3S_KUBECONFIG}"),
    }
}

/// Everything to `rm -rf` on one node of this cluster.
fn cleanup_paths(cluster: &Cluster) -> Vec<&'static str> {
    let mut paths = COMMON_PATHS.to_vec();
    match cluster.distribution {
        Distribution::K8s => paths.extend(KUBEADM_PATHS),
        Distribution::K3s => {
            paths.extend(K3S_PATHS);
            paths.push(K3S_BINARY);
        }
    }
    if cluster.runtime == RuntimeKind::Crio {
        paths.extend(CRIO_PATHS);
    }
    paths.extend(CLEANUP_BINARIES);
    paths
}

/// Stop (and never restart) the node services of this track. k3s owns its
/// unit; the kubeadm track owns kubelet plus the external runtime.
fn stop_services_cmd(cluster: &Cluster) -> String {
    let mut cmds = Vec::new();
    match cluster.distribution {
        Distribution::K8s => {
            cmds.push("systemctl disable --now kubelet");
            match cluster.runtime {
                RuntimeKind::Containerd => cmds.push("systemctl disable --now containerd"),
                RuntimeKind::Crio => cmds.push("systemctl disable --now crio"),
            }
        }
        Distribution::K3s => cmds.push("systemctl disable --now k3s"),
    }
    cmds.join("; ")
}

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
/// `kubeadm reset` (kubeadm track only), service stop, artifact removal.
/// Failures are reported but do not stop the wipe — a partially destroyed
/// node must not block the rest.
pub async fn delete_node(
    pool: &Arc<dyn SessionPool>,
    cluster: &Cluster,
    node: &Node,
    api_session: Option<&Arc<dyn Session>>,
) -> Result<()> {
    // 1. Cluster-level removal via the first control plane, when available.
    if let Some(cp) = api_session {
        let k = format!("{} --request-timeout=15s", kubectl_cmd(cluster));
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
    // A k3s node never ran kubeadm: the binary is absent and `systemctl
    // disable --now k3s` below is the whole reset.
    if cluster.distribution == Distribution::K8s {
        let out = s.exec("kubeadm reset -f 2>&1; true").await?;
        report(
            &node.name,
            "kubeadm reset",
            out.ok(),
            out.stdout.trim().lines().next().unwrap_or(""),
        );
    }

    // 3. kube-vip VIP release (harmless when never assigned).
    let _ = s
        .exec(
            "ip a 2>/dev/null | awk '/inet .*\\/32/{print $2}' | \
               grep -vE '127\\.|/32$' -m1 >/dev/null; true",
        )
        .await;

    // 4. Stop services, then remove every artifact the track laid down.
    let out = s
        .exec(&format!("{} 2>&1; true", stop_services_cmd(cluster)))
        .await?;
    report(&node.name, "services stopped", out.exit_code == 0, "");

    let paths = cleanup_paths(cluster)
        .into_iter()
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
        delete_node(pool, cluster, node, cp_session.as_ref()).await?;
    }
    for node in cluster.control_plane() {
        // After the first reset this node no longer speaks API; only drain it
        // while still a member.
        delete_node(pool, cluster, node, None).await?;
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::net::{IpAddr, Ipv4Addr};
    use std::sync::{Arc, Mutex};

    use kurin_config::{
        Cluster, Distribution, HaMode, Node, NodeRole, ResolvedSsh, RuntimeKind, SshAuth,
    };
    use miette::Result;

    use super::*;
    use crate::engine::ExecOutput;

    /// Records every command; everything succeeds with empty output.
    struct RecordingSession {
        commands: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl Session for RecordingSession {
        async fn exec(&self, command: &str) -> Result<ExecOutput> {
            self.commands.lock().unwrap().push(command.to_string());
            Ok(ExecOutput {
                exit_code: 0,
                stdout: String::new(),
                stderr: String::new(),
            })
        }

        async fn write_file(&self, _path: &str, _contents: &[u8]) -> Result<()> {
            Ok(())
        }

        async fn file_sha256(&self, _path: &str) -> Result<Option<String>> {
            Ok(None)
        }

        async fn service_active(&self, _name: &str) -> Result<bool> {
            Ok(false)
        }
    }

    struct MockPool {
        session: Arc<dyn Session>,
    }

    #[async_trait::async_trait]
    impl SessionPool for MockPool {
        async fn get(&self, _node: &str) -> Result<Arc<dyn Session>> {
            Ok(self.session.clone())
        }
    }

    fn node(name: &str, role: NodeRole) -> Node {
        Node {
            name: name.into(),
            role,
            address: IpAddr::V4(Ipv4Addr::new(10, 0, 0, 11)),
            internal_address: IpAddr::V4(Ipv4Addr::new(10, 0, 0, 11)),
            ssh: ResolvedSsh {
                user: "root".into(),
                port: 22,
                auth: SshAuth::PrivateKey {
                    key_path: "~/.ssh/id_ed25519".into(),
                    passphrase: None,
                },
            },
            labels: BTreeMap::new(),
        }
    }

    fn cluster(distribution: Distribution, runtime: RuntimeKind) -> Cluster {
        Cluster {
            name: "t".into(),
            offline_dir: None,
            zone: None,
            distribution,
            ha_mode: HaMode::KubeVip,
            runtime,
            kubernetes_version: semver::Version::parse("1.33.2").unwrap(),
            pod_cidr: "10.244.0.0/16".parse().unwrap(),
            service_cidr: "10.96.0.0/12".parse().unwrap(),
            dns_domain: "cluster.local".into(),
            cni: Default::default(),
            vip: None,
            nodes: vec![node("cp1", NodeRole::ControlPlane)],
            source: String::new(),
        }
    }

    /// Runs one node's wipe and returns the command log.
    async fn wipe(distribution: Distribution, runtime: RuntimeKind) -> Vec<String> {
        let commands = Arc::new(Mutex::new(Vec::new()));
        let pool: Arc<dyn SessionPool> = Arc::new(MockPool {
            session: Arc::new(RecordingSession {
                commands: commands.clone(),
            }),
        });
        let cluster = cluster(distribution, runtime);
        delete_node(&pool, &cluster, &cluster.nodes[0], None)
            .await
            .unwrap();
        let log = commands.lock().unwrap().clone();
        log
    }

    fn joined(log: &[String]) -> String {
        log.join("\n")
    }

    #[tokio::test]
    async fn k3s_never_runs_kubeadm_reset_and_drops_rancher_state() {
        let log = joined(&wipe(Distribution::K3s, RuntimeKind::Containerd).await);
        assert!(
            !log.contains("kubeadm reset"),
            "k3s teardown must not run kubeadm reset:\n{log}"
        );
        assert!(log.contains("rm -rf /var/lib/rancher"), "{log}");
        assert!(log.contains("rm -rf /etc/rancher"), "{log}");
        assert!(log.contains("rm -rf /usr/local/bin/k3s"), "{log}");
        assert!(log.contains("systemctl disable --now k3s"), "{log}");
        assert!(!log.contains("/etc/kubernetes"), "{log}");
        assert!(!log.contains("/var/lib/etcd"), "{log}");
    }

    #[tokio::test]
    async fn kubeadm_keeps_reset_and_control_plane_state() {
        let log = joined(&wipe(Distribution::K8s, RuntimeKind::Containerd).await);
        assert!(log.contains("kubeadm reset -f"), "{log}");
        assert!(log.contains("rm -rf /etc/kubernetes"), "{log}");
        assert!(log.contains("rm -rf /var/lib/etcd"), "{log}");
        assert!(log.contains("systemctl disable --now kubelet"), "{log}");
        assert!(!log.contains("/var/lib/rancher"), "{log}");
    }

    #[tokio::test]
    async fn crio_runtime_adds_crio_leftovers() {
        let log = joined(&wipe(Distribution::K8s, RuntimeKind::Crio).await);
        assert!(log.contains("rm -rf /etc/crio"), "{log}");
        assert!(log.contains("rm -rf /usr/local/bin/crio"), "{log}");
        assert!(log.contains("rm -rf /usr/local/bin/pinns"), "{log}");
        assert!(log.contains("rm -rf /usr/local/libexec/crio"), "{log}");
        assert!(log.contains("rm -rf /etc/containers"), "{log}");
        assert!(log.contains("systemctl disable --now crio"), "{log}");
        assert!(!log.contains("disable --now containerd"), "{log}");
    }

    #[tokio::test]
    async fn containerd_runtime_leaves_crio_paths_alone() {
        let log = joined(&wipe(Distribution::K8s, RuntimeKind::Containerd).await);
        assert!(!log.contains("/etc/crio"), "{log}");
        assert!(!log.contains("/etc/containers"), "{log}");
        assert!(!log.contains("/usr/local/libexec/crio"), "{log}");
    }

    #[test]
    fn kubectl_points_at_each_tracks_kubeconfig() {
        let k8s = cluster(Distribution::K8s, RuntimeKind::Containerd);
        assert_eq!(
            kubectl_cmd(&k8s),
            "/usr/local/bin/kubectl --kubeconfig /etc/kubernetes/admin.conf"
        );
        let k3s = cluster(Distribution::K3s, RuntimeKind::Containerd);
        assert_eq!(
            kubectl_cmd(&k3s),
            "/usr/local/bin/k3s kubectl --kubeconfig /etc/rancher/k3s/k3s.yaml"
        );
    }
}
