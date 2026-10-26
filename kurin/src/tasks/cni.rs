//! cni/*: one plugin per file; plan dispatches from `cluster.cni`.

pub mod calico;
pub mod flannel;

use std::sync::Arc;

use kurin_config::Cluster;
use miette::Result;

use crate::engine::Task;

/// Task implementing the configured CNI. Plugin modules keep their pinned
/// manifest handling internally; this is the only place that branches.
pub fn plugin_task(cluster: &Cluster) -> Result<Arc<dyn Task>> {
    match &cluster.cni {
        kurin_config::CniConfig::Calico => Ok(Arc::new(calico::CalicoOperator::new(cluster))),
        kurin_config::CniConfig::Flannel => Ok(Arc::new(flannel::Flannel::new(cluster))),
        // Wave2 track-cilium lands the module; wired by the integrator.
        kurin_config::CniConfig::Cilium => Err(miette::miette!(
            "network.cni = \"cilium\" needs the wave2 cilium module (not in this build)"
        )),
        kurin_config::CniConfig::None => Ok(Arc::new(NoCni::new(cluster))),
    }
}

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
    fn stage(&self) -> crate::engine::Stage {
        crate::engine::Stage::Cni
    }
    fn node(&self) -> &str {
        &self.node_name
    }
    fn describe(&self) -> String {
        "no CNI installed (cni = none)".into()
    }
    async fn check(&self, _s: &dyn crate::engine::Session) -> Result<crate::engine::Drift> {
        Ok(crate::engine::Drift::InSync)
    }
    async fn apply(&self, _s: &dyn crate::engine::Session) -> Result<crate::engine::Outcome> {
        Ok(crate::engine::Outcome::Verified)
    }
}
