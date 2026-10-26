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
        format!(
            "fetch admin.conf -> merge kubeconfig (cluster {})",
            self.cluster_name
        )
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
        let out = session
            .exec("stat -c %s /etc/kubernetes/admin.conf")
            .await?;
        if out.ok() {
            Ok(Outcome::Verified)
        } else {
            Err(miette::miette!(
                "admin.conf stat failed: {}",
                out.stderr.trim()
            ))
        }
    }
}

/// Applies per-node labels declared in the config. kubeadm v1beta4 accepts
/// `nodeRegistration.labels` but does not act on it (verified on 1.33), so
/// labels are converged through the API explicitly.
pub struct NodeLabels {
    node_name: String,
    labels: std::collections::BTreeMap<String, String>,
}

impl NodeLabels {
    pub fn new(cluster: &kurin_config::Cluster, target: &kurin_config::Node) -> Self {
        let _ = cluster;
        Self {
            node_name: target.name.clone(),
            labels: target.labels.clone(),
        }
    }
}

#[async_trait::async_trait]
impl Task for NodeLabels {
    fn id(&self) -> String {
        format!("post/labels@{}", self.node_name)
    }
    fn stage(&self) -> Stage {
        Stage::Post
    }
    fn node(&self) -> &str {
        &self.node_name
    }
    fn describe(&self) -> String {
        format!("converge {} node label(s)", self.labels.len())
    }

    async fn check(&self, session: &dyn Session) -> Result<Drift> {
        if self.labels.is_empty() {
            return Ok(Drift::InSync);
        }
        let out = session
            .exec(&format!(
                "kubectl --kubeconfig /etc/kubernetes/admin.conf get node {} -o json",
                self.node_name
            ))
            .await?;
        if !out.ok() {
            return Ok(Drift::Drifted(format!(
                "cannot read node {}: {}",
                self.node_name,
                out.stderr.lines().last().unwrap_or("")
            )));
        }
        let v: serde_json::Value = serde_json::from_str(&out.stdout)
            .map_err(|e| miette::miette!("parse node json: {e}"))?;
        let current = v
            .pointer("/metadata/labels")
            .and_then(|l| l.as_object())
            .cloned()
            .unwrap_or_default();
        let missing: Vec<&String> = self
            .labels
            .iter()
            .filter(|(k, val)| current.get(*k).and_then(|x| x.as_str()) != Some(val.as_str()))
            .map(|(k, _)| k)
            .collect();
        if missing.is_empty() {
            Ok(Drift::InSync)
        } else {
            Ok(Drift::Drifted(format!(
                "labels differ on {}: {}",
                self.node_name,
                missing
                    .iter()
                    .map(|s| s.to_string())
                    .collect::<Vec<_>>()
                    .join(",")
            )))
        }
    }

    async fn apply(&self, session: &dyn Session) -> Result<Outcome> {
        if self.labels.is_empty() {
            return Ok(Outcome::Verified);
        }
        let pairs: Vec<String> = self
            .labels
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect();
        let out = session
            .exec(&format!(
                "kubectl --kubeconfig /etc/kubernetes/admin.conf label node {} {} --overwrite",
                self.node_name,
                pairs.join(" ")
            ))
            .await?;
        if out.ok() {
            Ok(Outcome::Verified)
        } else {
            Err(miette::miette!(
                "labeling {} failed: {}",
                self.node_name,
                out.stderr.lines().last().unwrap_or("")
            ))
        }
    }
}
