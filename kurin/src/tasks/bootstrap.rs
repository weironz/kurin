//! bootstrap/*: kubeadm init on the first control-plane node, with the
//! rendered ClusterConfiguration shipped to /etc/kurin/kubeadm-init.yaml.

use kurin_config::{Cluster, Node};
use miette::Result;

use crate::engine::{Drift, Outcome, Session, Stage, Task};
use crate::templates;
use crate::tasks::prepare::sha256_hex;

pub const KUBEADM_DIR: &str = "/etc/kurin";

pub struct KubeadmInit {
    node_name: String,
    rendered: String,
}

impl KubeadmInit {
    pub fn new(cluster: &Cluster, first: &Node) -> Self {
        let advertise = first.internal_address.to_string();
        let endpoint = cluster
            .vip
            .map(|v| format!("{v}:6443"))
            .unwrap_or_else(|| format!("{advertise}:6443"));
        let rendered = templates::render(
            "kubeadm/kubeadm-init.yaml.j2",
            minijinja::context! {
                cluster => cluster.name,
                kubernetes_version => cluster.kubernetes_version.to_string(),
                pod_cidr => cluster.pod_cidr.to_string(),
                service_cidr => cluster.service_cidr.to_string(),
                dns_domain => cluster.dns_domain,
                advertise_address => advertise,
                control_plane_endpoint => endpoint,
            },
        )
        .expect("kubeadm init template renders");
        Self {
            node_name: first.name.clone(),
            rendered,
        }
    }

    fn config_path() -> String {
        format!("{KUBEADM_DIR}/kubeadm-init.yaml")
    }
}

#[async_trait::async_trait]
impl Task for KubeadmInit {
    fn id(&self) -> String {
        format!("bootstrap/kubeadm-init@{}", self.node_name)
    }
    fn stage(&self) -> Stage {
        Stage::Bootstrap
    }
    fn node(&self) -> &str {
        &self.node_name
    }
    fn describe(&self) -> String {
        format!(
            "kubeadm init --config {} on {}",
            Self::config_path(),
            self.node_name
        )
    }

    async fn check(&self, session: &dyn Session) -> Result<Drift> {
        // kubeadm's marker: admin.conf exists => init already ran successfully.
        match session.file_sha256("/etc/kubernetes/admin.conf").await? {
            Some(_) => Ok(Drift::InSync),
            None => Ok(Drift::Drifted(
                "node has not been initialized (no /etc/kubernetes/admin.conf)".into(),
            )),
        }
    }

    async fn apply(&self, session: &dyn Session) -> miette::Result<Outcome> {
        let dir = Self::config_path().rsplit_once('/').map(|(d, _)| d).unwrap_or(KUBEADM_DIR).to_string();
        session.exec(&format!("mkdir -p {dir}")).await?;
        session
            .write_file(&Self::config_path(), self.rendered.as_bytes())
            .await?;
        // Upload-phase succeeded; init itself is the risky network operation.
        // kubeadm is rerun-safe on the same inputs; the dry run documents
        // this as the one task where an Unknown outcome re-checks admin.conf.
        let out = session
            .exec(&format!(
                "kubeadm init --config {} --upload-certs",
                Self::config_path()
            ))
            .await?;
        match session.file_sha256("/etc/kubernetes/admin.conf").await {
            Err(e) => {
                // Probe failed right after apply; the op outcome is unknown.
                return Ok(Outcome::Unknown(format!(
                    "init exited, but admin.conf probe errored: {e}"
                )));
            }
            Ok(None) => {
                return Err(init_failure(&out, &self.node_name));
            }
            Ok(Some(_)) => return Ok(Outcome::Verified),
        };
    }
}

fn init_failure(out: &crate::engine::ExecOutput, node: &str) -> miette::Report {
    miette::miette!(
        "kubeadm init on {node} left no admin.conf (exit {}): {}",
        out.exit_code,
        out.stderr.lines().last().unwrap_or("")
    )
}

/// Path of the kube-vip static pod manifest watched by kubelet.
pub const KUBEVIP_MANIFEST: &str = "/etc/kubernetes/manifests/kube-vip.yaml";

/// Default NIC carrying the control-plane VIP. Overridden in M2 via config.
pub const DEFAULT_VIP_IFACE: &str = "eth0";

/// HA control-plane VIP manager, shipped as a kubelet static pod. Only
/// planned when the cluster declares a VIP.
pub struct KubeVip {
    node_name: String,
    rendered: String,
}

impl KubeVip {
    pub fn new(cluster: &Cluster, node: &Node) -> Option<Self> {
        let vip = cluster.vip?;
        let rendered = templates::render(
            "kubevip/kube-vip.yaml.j2",
            minijinja::context! {
                vip => vip.to_string(),
                pod_cidr => cluster.pod_cidr.to_string(),
                vip_interface => DEFAULT_VIP_IFACE,
                image => crate::version::KUBE_VIP_VERSION,
            },
        )
        .expect("embedded kube-vip template renders");
        Some(Self {
            node_name: node.name.clone(),
            rendered,
        })
    }
}

#[async_trait::async_trait]
impl Task for KubeVip {
    fn id(&self) -> String {
        format!("bootstrap/kube-vip@{}", self.node_name)
    }
    fn stage(&self) -> Stage {
        Stage::Bootstrap
    }
    fn node(&self) -> &str {
        &self.node_name
    }
    fn describe(&self) -> String {
        format!("install kube-vip static pod {KUBEVIP_MANIFEST}")
    }

    async fn check(&self, session: &dyn Session) -> Result<Drift> {
        let want = sha256_hex(self.rendered.as_bytes());
        match session.file_sha256(KUBEVIP_MANIFEST).await? {
            Some(have) if have == want => Ok(Drift::InSync),
            _ => Ok(Drift::Drifted("kube-vip manifest differs from rendered".into())),
        }
    }

    async fn apply(&self, session: &dyn Session) -> miette::Result<Outcome> {
        session
            .write_file(KUBEVIP_MANIFEST, self.rendered.as_bytes())
            .await?;
        let want = sha256_hex(self.rendered.as_bytes());
        match session.file_sha256(KUBEVIP_MANIFEST).await? {
            Some(have) if have == want => Ok(Outcome::Verified),
            _ => Err(miette::miette!(
                "kube-vip manifest on {} does not match rendered output",
                self.node_name
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    use kurin_config::{CniConfig, NodeRole, ResolvedSsh, SshAuth};

    fn node(name: &str, ip: &str) -> Node {
        Node {
            name: name.into(),
            role: NodeRole::ControlPlane,
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
        Cluster {
            name: "test".into(),
            kubernetes_version: "1.33.1".parse().unwrap(),
            pod_cidr: "10.233.64.0/18".parse().unwrap(),
            service_cidr: "10.233.0.0/18".parse().unwrap(),
            dns_domain: "cluster.local".into(),
            cni: CniConfig::Calico,
            vip: vip.map(|v| v.parse().unwrap()),
            nodes: vec![node("m1", "192.168.10.11")],
            source: String::new(),
        }
    }

    #[test]
    fn kubeadm_init_renders_network_and_endpoints() {
        let c = cluster(Some("192.168.10.100"));
        let t = KubeadmInit::new(&c, &c.nodes[0]);
        assert!(t.rendered.contains("podSubnet: 10.233.64.0/18"));
        assert!(t.rendered.contains("serviceSubnet: 10.233.0.0/18"));
        assert!(t.rendered.contains("controlPlaneEndpoint: 192.168.10.100:6443"));
        assert!(t.rendered.contains("advertiseAddress: 192.168.10.11"));
    }

    #[test]
    fn kube_vip_renders_vip_interface_and_image() {
        let c = cluster(Some("192.168.10.100"));
        let t = KubeVip::new(&c, &c.nodes[0]).expect("VIP configured");
        assert!(t.rendered.contains("value: \"192.168.10.100\""));
        assert!(t.rendered.contains("value: \"eth0\""));
        assert!(t.rendered.contains("10.233.64.0/18"));
        assert!(t
            .rendered
            .contains(&format!("ghcr.io/kube-vip/kube-vip:{}", crate::version::KUBE_VIP_VERSION)));
        assert!(t.rendered.contains("hostNetwork: true"));
    }

    #[test]
    fn kube_vip_absent_without_vip() {
        let c = cluster(None);
        assert!(KubeVip::new(&c, &c.nodes[0]).is_none());
    }
}