//! cni/*: one plugin per file; plan dispatches from `cluster.cni`.

pub mod calico;
pub mod cilium;
pub mod flannel;
pub mod kube_ovn;

use std::sync::Arc;

use kurin_config::Cluster;
use kurin_core::facts::FactsTable;
use miette::Result;

use kurin_core::engine::Task;
use kurin_core::run::RunContext;

/// Task implementing the configured CNI. Plugin modules keep their pinned
/// manifest handling internally; this is the only place that branches.
/// Manifest-based plugins resolve their pinned payload for the arch of
/// the control-plane node they apply through.
pub fn plugin_task(
    cluster: &Cluster,
    facts: &FactsTable,
    run: &Arc<RunContext>,
) -> Result<Arc<dyn Task>> {
    let cp_arch = match cluster.control_plane().next() {
        Some(cp) => facts.arch(&cp.name)?,
        None => kurin_core::arch::Arch::host()?,
    };
    match &cluster.cni {
        kurin_config::CniConfig::Calico => {
            Ok(Arc::new(calico::CalicoOperator::new(cluster, cp_arch, run)))
        }
        kurin_config::CniConfig::Flannel => {
            Ok(Arc::new(flannel::Flannel::new(cluster, cp_arch, run)))
        }
        kurin_config::CniConfig::Cilium => cilium::plugin(cluster, run),
        kurin_config::CniConfig::KubeOvn => Ok(Arc::new(kube_ovn::KubeOvn::new(cluster, run))),
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
