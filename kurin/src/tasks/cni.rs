//! cni/*: apply the tigera-operator manifest + inline Installation CR.
//! Runs on the control-plane node via kubectl, not on every node.
//!
//! The Installation CR is rendered from a template; only the pinned operator
//! manifest URL and file-path plumbing are assembled here.

use kurin_config::Cluster;
use miette::Result;

use crate::engine::{Drift, Outcome, Session, Stage, Task};
use crate::templates;

/// Staged on the control-plane node, then applied and removed.
const INSTALLATION_STAGING_PATH: &str = "/tmp/kurin-calico-installation.yaml";

pub struct CalicoOperator {
    node_name: String,
    pod_cidr: String,
}

impl CalicoOperator {
    pub fn new(cluster: &Cluster) -> Self {
        Self {
            // Applied through the first control-plane session.
            node_name: cluster
                .control_plane()
                .next()
                .map(|n| n.name.clone())
                .unwrap_or_default(),
            pod_cidr: cluster.pod_cidr.to_string(),
        }
    }

    fn installation_cr(&self) -> String {
        templates::render(
            "cni/calico-installation.yaml.j2",
            minijinja::context! { pod_cidr => self.pod_cidr },
        )
        .expect("embedded calico Installation template renders")
    }

    fn operator_url() -> String {
        format!(
            "https://raw.githubusercontent.com/projectcalico/calico/{}/manifests/tigera-operator.yaml",
            crate::version::CALICO_VERSION
        )
    }
}

#[async_trait::async_trait]
impl Task for CalicoOperator {
    fn id(&self) -> String {
        format!("cni/calico-operator@{}", self.node_name)
    }
    fn stage(&self) -> Stage {
        Stage::Cni
    }
    fn node(&self) -> &str {
        &self.node_name
    }
    fn describe(&self) -> String {
        "kubectl apply tigera-operator manifest + Installation CR".into()
    }

    async fn check(&self, session: &dyn Session) -> Result<Drift> {
        let out = session
            .exec("kubectl get installation default -o name 2>/dev/null")
            .await?;
        if out.ok() && out.stdout.trim() == "installation.operator.tigera.io/default" {
            Ok(Drift::InSync)
        } else {
            Ok(Drift::Drifted("calico Installation CR not present".into()))
        }
    }

    async fn apply(&self, session: &dyn Session) -> miette::Result<Outcome> {
        session
            .write_file(INSTALLATION_STAGING_PATH, self.installation_cr().as_bytes())
            .await?;
        let out = session
            .exec(&format!(
                "kubectl apply -f {} && kubectl apply -f {} && rm -f {}",
                Self::operator_url(),
                INSTALLATION_STAGING_PATH,
                INSTALLATION_STAGING_PATH
            ))
            .await?;
        if out.ok() {
            Ok(Outcome::Verified)
        } else {
            Err(miette::miette!(
                "calico apply failed: {}",
                out.stderr.lines().last().unwrap_or("")
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    use kurin_config::{CniConfig, Node, NodeRole, ResolvedSsh, SshAuth};

    fn cluster(pod_cidr: &str) -> Cluster {
        let node = Node {
            name: "m1".into(),
            role: NodeRole::ControlPlane,
            address: "192.168.10.11".parse().unwrap(),
            internal_address: "192.168.10.11".parse().unwrap(),
            ssh: ResolvedSsh {
                user: "root".into(),
                port: 22,
                auth: SshAuth::Password { password: "x".into() },
            },
            labels: BTreeMap::new(),
        };
        Cluster {
            name: "test".into(),
            kubernetes_version: "1.33.1".parse().unwrap(),
            pod_cidr: pod_cidr.parse().unwrap(),
            service_cidr: "10.233.0.0/18".parse().unwrap(),
            dns_domain: "cluster.local".into(),
            cni: CniConfig::Calico,
            vip: None,
            nodes: vec![node],
            source: String::new(),
        }
    }

    #[test]
    fn installation_cr_renders_pod_cidr_and_encapsulation() {
        let cr = CalicoOperator::new(&cluster("10.244.0.0/16")).installation_cr();
        assert!(cr.contains("kind: Installation"));
        assert!(cr.contains("cidr: 10.244.0.0/16"));
        assert!(cr.contains("encapsulation: VXLANCrossSubnet"));
        // The IP pool is nested under calicoNetwork.ipPools with correct indent.
        assert!(cr.contains("  calicoNetwork:\n    ipPools:\n    - cidr: 10.244.0.0/16"));
    }

    #[test]
    fn operator_url_is_version_pinned() {
        let url = CalicoOperator::operator_url();
        assert!(url.contains(crate::version::CALICO_VERSION));
        assert!(url.ends_with("/manifests/tigera-operator.yaml"));
    }
}