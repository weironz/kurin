//! join/*: kubeadm join for workers and additional control-plane members.
//! The JoinConfiguration is rendered from templates (no token inside: the
//! executor injects token/CA-hash/certificate-key via command flags) and
//! shipped to /etc/kurin/kubeadm-join.yaml.

use kurin_config::{Cluster, Node};
use miette::Result;

use crate::engine::{Drift, Outcome, Session, Stage, Task};
use crate::templates;
use crate::tasks::bootstrap::KUBEADM_DIR;

const JOIN_CONFIG_NAME: &str = "kubeadm-join.yaml";

pub struct KubeadmJoin {
    node_name: String,
    is_control_plane: bool,
    api_server_endpoint: String,
    rendered: String,
}

impl KubeadmJoin {
    pub fn new(cluster: &Cluster, node: &Node) -> Self {
        let is_control_plane = node.role == kurin_config::NodeRole::ControlPlane;
        // Prefer the HA VIP; otherwise the first master's API server.
        let api_server_endpoint = match cluster.vip {
            Some(vip) => format!("{vip}:6443"),
            None => cluster
                .control_plane()
                .next()
                .map(|n| format!("{}:6443", n.internal_address))
                .unwrap_or_default(),
        };
        let (path, ctx) = if is_control_plane {
            (
                "kubeadm/kubeadm-join-controlplane.yaml.j2",
                minijinja::context! {
                    node_name => node.name.clone(),
                    api_server_endpoint => api_server_endpoint.clone(),
                    advertise_address => node.internal_address.to_string(),
                },
            )
        } else {
            (
                "kubeadm/kubeadm-join-worker.yaml.j2",
                minijinja::context! {
                    node_name => node.name.clone(),
                    api_server_endpoint => api_server_endpoint.clone(),
                },
            )
        };
        let rendered = templates::render(path, ctx).expect("embedded kubeadm join template renders");
        Self {
            node_name: node.name.clone(),
            is_control_plane,
            api_server_endpoint,
            rendered,
        }
    }

    fn config_path() -> String {
        format!("{KUBEADM_DIR}/{JOIN_CONFIG_NAME}")
    }
}

#[async_trait::async_trait]
impl Task for KubeadmJoin {
    fn id(&self) -> String {
        format!("join/kubeadm@{}", self.node_name)
    }
    fn stage(&self) -> Stage {
        Stage::Join
    }
    fn node(&self) -> &str {
        &self.node_name
    }
    fn describe(&self) -> String {
        let role = if self.is_control_plane { "control-plane" } else { "worker" };
        format!("kubeadm join ({role}) -> {}", self.api_server_endpoint)
    }

    async fn check(&self, session: &dyn Session) -> Result<Drift> {
        // kubelet.conf is written by a successful join.
        match session.file_sha256("/etc/kubernetes/kubelet.conf").await? {
            Some(_) => Ok(Drift::InSync),
            None => Ok(Drift::Drifted("node has not joined the cluster".into())),
        }
    }

    async fn apply(&self, session: &dyn Session) -> miette::Result<Outcome> {
        // Ship the rendered JoinConfiguration first; it contains no secret
        // material. The token / CA hash / certificate-key are produced by
        // the control plane at runtime and passed as `kubeadm join` flags by
        // the executor (SessionPool fan-in wiring lands in W1).
        // Contract: any transport error after kubeadm starts => Unknown.
        session.exec(&format!("mkdir -p {KUBEADM_DIR}")).await?;
        session
            .write_file(&Self::config_path(), self.rendered.as_bytes())
            .await?;
        let out = session
            .exec(&format!(
                "kubeadm join --config {} --dry-run >/dev/null 2>&1 || true",
                Self::config_path()
            ))
            .await?;
        if out.ok() {
            Ok(Outcome::Unknown(
                "join config shipped; the bootstrap token from the control-plane session \
                 and the final kubeadm join run are wired by the executor"
                    .into(),
            ))
        } else {
            Ok(Outcome::Unknown(format!(
                "join probe failed on {}",
                self.node_name
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    use kurin_config::{CniConfig, ResolvedSsh, SshAuth};

    fn node(name: &str, ip: &str, role: kurin_config::NodeRole) -> Node {
        Node {
            name: name.into(),
            role,
            address: ip.parse().unwrap(),
            internal_address: ip.parse().unwrap(),
            ssh: ResolvedSsh {
                user: "root".into(),
                port: 22,
                auth: SshAuth::Password { password: "x".into() },
            },
            labels: BTreeMap::new(),
        }
    }

    fn cluster(vip: Option<&str>) -> Cluster {
        use kurin_config::NodeRole::*;
        Cluster {
            name: "test".into(),
            kubernetes_version: "1.33.1".parse().unwrap(),
            pod_cidr: "10.233.64.0/18".parse().unwrap(),
            service_cidr: "10.233.0.0/18".parse().unwrap(),
            dns_domain: "cluster.local".into(),
            cni: CniConfig::Calico,
            vip: vip.map(|v| v.parse().unwrap()),
            nodes: vec![
                node("m1", "192.168.10.11", ControlPlane),
                node("m2", "192.168.10.12", ControlPlane),
                node("w1", "192.168.10.21", Worker),
            ],
            source: String::new(),
        }
    }

    #[test]
    fn worker_join_renders_endpoint_and_node_name() {
        let c = cluster(Some("192.168.10.100"));
        let t = KubeadmJoin::new(&c, &c.nodes[2]);
        assert!(t.rendered.contains("kind: JoinConfiguration"));
        assert!(t.rendered.contains("apiServerEndpoint: 192.168.10.100:6443"));
        assert!(t.rendered.contains("name: w1"));
        assert!(!t.rendered.contains("certificateKey:"), "no secrets in templates");
        // No VIP: falls back to the first control-plane node.
        let c = cluster(None);
        let t = KubeadmJoin::new(&c, &c.nodes[2]);
        assert!(t.rendered.contains("apiServerEndpoint: 192.168.10.11:6443"));
    }

    #[test]
    fn controlplane_join_renders_advertise_address() {
        let c = cluster(Some("192.168.10.100"));
        let t = KubeadmJoin::new(&c, &c.nodes[1]);
        assert!(t.rendered.contains("controlPlane:"));
        assert!(t.rendered.contains("advertiseAddress: 192.168.10.12"));
        assert!(t.rendered.contains("apiServerEndpoint: 192.168.10.100:6443"));
        assert!(t.rendered.contains("name: m2"));
    }
}
