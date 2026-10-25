//! join/*: kubeadm join for workers and additional control-plane members.

use kurin_config::{Cluster, Node};
use miette::Result;

use crate::engine::{Drift, Outcome, Session, Stage, Task};

pub struct KubeadmJoin {
    node_name: String,
    is_control_plane: bool,
    first_master: String,
}

impl KubeadmJoin {
    pub fn new(cluster: &Cluster, node: &Node) -> Self {
        Self {
            node_name: node.name.clone(),
            is_control_plane: node.role == kurin_config::NodeRole::ControlPlane,
            first_master: cluster
                .control_plane()
                .next()
                .map(|n| n.internal_address.to_string())
                .unwrap_or_default(),
        }
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
        format!("kubeadm join ({role}) -> {}", self.first_master)
    }

    async fn check(&self, session: &dyn Session) -> Result<Drift> {
        // kubelet.conf is written by a successful join.
        match session.file_sha256("/etc/kubernetes/kubelet.conf").await? {
            Some(_) => Ok(Drift::InSync),
            None => Ok(Drift::Drifted("node has not joined the cluster".into())),
        }
    }

    async fn apply(&self, session: &dyn Session) -> miette::Result<Outcome> {
        // The join command is obtained from the control-plane by the executor
        // (needs a session to another node); W1 wires SessionPool fan-in.
        // Contract: any transport error after kubeadm starts => Unknown.
        let out = session.exec("kubeadm join --dry-run >/dev/null 2>&1 || true").await?;
        if out.ok() {
            Ok(Outcome::Unknown(
                "join requires the bootstrap token from the control-plane session; \
                 executor wiring lands in W1"
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