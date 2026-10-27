//! cni/*: one plugin per file; the `cni/*` components in
//! `crate::component` each construct their plugin's task.

pub mod calico;
pub mod chart;
pub mod cilium;
pub mod flannel;
pub mod kube_ovn;

use kurin_config::Cluster;
use kurin_core::engine::Task;
use miette::Result;

/// `cni = "none"`: the operator installs their own plugin. No task work.
pub struct NoCni {
    node_name: String,
}

impl NoCni {
    pub fn new(cluster: &Cluster) -> Self {
        Self {
            node_name: cluster
                .control_plane()
                .next()
                .map(|n| n.name.clone())
                .unwrap_or_default(),
        }
    }
}

#[async_trait::async_trait]
impl Task for NoCni {
    fn id(&self) -> String {
        format!("cni/none@{}", self.node_name)
    }
    fn stage(&self) -> kurin_core::engine::Stage {
        kurin_core::engine::Stage::Cni
    }
    fn node(&self) -> &str {
        &self.node_name
    }
    fn describe(&self) -> String {
        "no CNI installed (cni = none)".into()
    }
    async fn check(
        &self,
        _s: &dyn kurin_core::engine::Session,
    ) -> Result<kurin_core::engine::Drift> {
        Ok(kurin_core::engine::Drift::InSync)
    }
    async fn apply(
        &self,
        _s: &dyn kurin_core::engine::Session,
    ) -> Result<kurin_core::engine::Outcome> {
        Ok(kurin_core::engine::Outcome::Verified)
    }
}
