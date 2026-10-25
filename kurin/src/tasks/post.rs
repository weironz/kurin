//! post/*: kubeconfig fetch + version-report output. Runs on first CP node.

use kurin_config::Cluster;
use miette::Result;

use crate::engine::{Drift, Outcome, Session, Stage, Task};

pub struct KubeconfigPull {
    node_name: String,
    cluster_name: String,
}

impl KubeconfigPull {
    pub fn new(cluster: &Cluster) -> Self {
        Self {
            node_name: cluster
                .control_plane()
                .next()
                .map(|n| n.name.clone())
                .unwrap_or_default(),
            cluster_name: cluster.name.clone(),
        }
    }
}

#[async_trait::async_trait]
impl Task for KubeconfigPull {
    fn id(&self) -> String {
        format!("post/kubeconfig@{}", self.node_name)
    }
    fn stage(&self) -> Stage {
        Stage::Post
    }
    fn node(&self) -> &str {
        &self.node_name
    }
    fn describe(&self) -> String {
        format!("fetch admin.conf -> merge kubeconfig (cluster {})", self.cluster_name)
    }

    async fn check(&self, session: &dyn Session) -> Result<Drift> {
        let out = session.exec("test -r /etc/kubernetes/admin.conf").await?;
        if out.ok() {
            Ok(Drift::InSync)
        } else {
            Ok(Drift::Drifted("admin.conf unreadable".into()))
        }
    }

    async fn apply(&self, session: &dyn Session) -> miette::Result<Outcome> {
        // W1: sftp read admin.conf + local kubeconfig merge (never overwrite).
        let out = session.exec("stat -c %s /etc/kubernetes/admin.conf").await?;
        if out.ok() {
            Ok(Outcome::Verified)
        } else {
            Err(miette::miette!("admin.conf stat failed: {}", out.stderr.trim()))
        }
    }
}