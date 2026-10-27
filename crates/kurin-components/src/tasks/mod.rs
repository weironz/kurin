pub mod bootstrap;
pub mod cni;
pub mod cri_o;
pub mod external_lb;
pub mod images;
pub mod install;
pub mod join;
pub mod k3s;
pub mod kernel_deps;
pub mod os_baseline;
pub mod post;
pub mod preflight;
pub mod prepare;
pub mod registry;
pub mod storage;

use kurin_config::v1alpha2::Delivery;
use kurin_config::Cluster;
use kurin_core::run::ArtifactSources;

/// The delivery strategy a run actually uses (04 §5.5): an explicit
/// `[packages] delivery` wins; `auto` picks `registry` when the cluster
/// configures one (a `role = "registry"` node or `kind = "external"`),
/// `preload` otherwise.
pub fn effective_delivery(cluster: &Cluster, sources: &ArtifactSources) -> Delivery {
    match sources.delivery {
        Delivery::Auto => {
            if cluster.registry_kind().is_some()
                || cluster
                    .nodes
                    .iter()
                    .any(|n| n.role == kurin_config::NodeRole::Registry)
            {
                Delivery::Registry
            } else {
                Delivery::Preload
            }
        }
        other => other,
    }
}
