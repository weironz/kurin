//! cni/*: one plugin per file; plan dispatches from `cluster.cni`.

pub mod calico;
pub mod cilium;
pub mod flannel;
pub mod kube_ovn;

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
        kurin_config::CniConfig::Cilium => cilium::plugin(cluster),
        kurin_config::CniConfig::KubeOvn => Ok(Arc::new(kube_ovn::KubeOvn::new(cluster))),
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
