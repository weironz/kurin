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
pub mod os_repo;
pub mod post;
pub mod preflight;
pub mod prepare;
pub mod registry;

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

/// True when the delivery strategy serves images from a registry the
/// runtime mirror points at: a node-hosted `[registry]` (04 §5.2) or the
/// controller-side seed registry tunnelled to each node's loopback
/// (04 §5.3, WP-77). Image import tasks become no-ops in both cases.
pub fn mirror_delivery(cluster: &Cluster, sources: &ArtifactSources) -> bool {
    matches!(
        effective_delivery(cluster, sources),
        Delivery::Registry | Delivery::Seed
    )
}
