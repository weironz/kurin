//! Built-in first-layer components (02 §3.8, ADR-07): the unified entry
//! point for every piece that converges a node's OS or bootstraps the
//! cluster. Each component owns its validation rules, its capability
//! declaration, its artifact list and its plan contribution; the planner
//! consults the explicit [`registry()`] — there is no dynamic loading.
//!
//! WP-15 migrates the os, runtime and distro components; ha, cni,
//! registry, storage and post stay as planner functions until WP-16.
//!
//! The frozen plan snapshots pin a node-major layout for the per-node
//! tracks (all of one node's os and runtime tasks before the next
//! node's), which a flat component loop cannot express on its own —
//! [`tracks`] positions and [`kurin_core::plan::PlanBuilder::add_track`]
//! carry that layout.

use std::sync::{Arc, LazyLock};

use kurin_artifact::artifact::{ArtifactKind, ArtifactManifest};
use kurin_config::{Cluster, ConfigError, Distribution};
use kurin_core::arch::Arch;
use kurin_core::plan::OpKind;
use kurin_core::plan::PlanBuilder;
use miette::Result;

pub mod distro;
pub mod os;
pub mod runtime;

/// The layer a built-in component belongs to (03 §3): pieces that run on
/// the node itself, grouped by what they own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComponentKind {
    /// Node OS convergence (baseline, sysctl, kernel deps).
    Os,
    /// Container runtime.
    Runtime,
    /// Kubernetes distribution.
    Distribution,
    /// CNI plugin (WP-16).
    Cni,
    /// API endpoint HA (WP-16).
    Ha,
    /// Node-hosted image registry (WP-16).
    Registry,
    /// Declarative addon engine (WP-40+).
    AddonEngine,
}

/// What a component supports. Aggregated over the registry this becomes
/// the single source of truth for config validation, the docs matrix and
/// the Web capability table (WP-16).
#[derive(Debug, Clone)]
pub struct Capabilities {
    /// Distributions the component runs on.
    pub distributions: &'static [Distribution],
    /// Architectures with pinned artifacts.
    pub archs: &'static [Arch],
    /// Whether the component works from the offline bundle.
    pub offline: bool,
    /// Operations the component contributes tasks to.
    pub ops: &'static [OpKind],
}

/// One pinned artifact a component requires: `name` keys an entry of the
/// embedded artifact manifest, so kinds and versions come from the same
/// table the installer uses (WP-16 aggregates these into the bundle).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactRef {
    /// Manifest entry name (e.g. `containerd`, `etcd`).
    pub name: String,
    /// Binary, container image or plain manifest.
    pub kind: ArtifactKind,
    /// Pinned version from the manifest entry.
    pub version: String,
    /// Architecture this ref resolves for.
    pub arch: Arch,
}

impl ArtifactRef {
    /// Resolve `name` for `arch` from the embedded manifest.
    fn of(name: &str, arch: Arch) -> ArtifactRef {
        let entry = embedded_manifest()
            .get(name)
            .unwrap_or_else(|| panic!("artifact {name:?} is not in the embedded manifest"));
        ArtifactRef {
            name: entry.name.clone(),
            kind: entry.kind,
            version: entry.version.clone(),
            arch,
        }
    }
}

/// The embedded artifact manifest, parsed once (compile-time JSON; the
/// version-table tests keep it valid).
fn embedded_manifest() -> &'static ArtifactManifest {
    static MANIFEST: LazyLock<ArtifactManifest> =
        LazyLock::new(|| ArtifactManifest::embedded().expect("the embedded manifest parses"));
    &MANIFEST
}

/// Validation findings from one component pass: components append hints;
/// the first becomes the error the config loader reports — the same
/// first-error-wins behavior these rules had inside `kurin-config`.
pub struct Diagnostics {
    source: String,
    hints: Vec<String>,
}

impl Diagnostics {
    /// Collect findings for a spec loaded from `source` (the config text,
    /// so errors render with span context).
    pub fn new(source: impl Into<String>) -> Self {
        Diagnostics {
            source: source.into(),
            hints: Vec::new(),
        }
    }

    /// Record a rejection; the hint is the user-facing message and must
    /// keep the wording the rule had in `kurin-config` (diagnostic
    /// snapshots guard it).
    pub fn error(&mut self, hint: impl Into<String>) {
        self.hints.push(hint.into());
    }

    /// True when no component found a problem.
    pub fn is_clean(&self) -> bool {
        self.hints.is_empty()
    }

    /// First finding as the loader-style [`ConfigError`].
    pub fn first_error(&self) -> Option<ConfigError> {
        self.hints
            .first()
            .map(|hint| ConfigError::invalid(self.source.clone(), hint.clone()))
    }
}

/// Per-node track positions for [`kurin_core::plan::PlanBuilder::add_track`]:
/// the frozen plan snapshots pin a node-major layout for the per-node
/// sweeps, which a flat component loop cannot express on its own. The
/// numbers only ever compare within one phase.
pub mod tracks {
    /// `vip` phase: kube-vip static pods per control-plane node, ahead of
    /// the distribution's init task.
    pub const KUBE_VIP: u32 = 0;
    /// `preflight`/`artifacts`/`prepare`: registry nodes' os sweep — they
    /// never join the k8s member band.
    pub const OS_REGISTRY_NODE: u32 = 1;
    /// `preflight`/`artifacts`/`prepare`: every k8s member's os+runtime
    /// band, node-major.
    pub const OS_K8S_MEMBER: u32 = 2;
    /// `prepare`: cri-o and k3s unit sweep, after every member's band.
    pub const LATE_UNITS: u32 = 3;
}

/// A built-in component: one installable piece of the node/bootstrap
/// track behind a unified entry point. The planner calls [`plan`](Component::plan)
/// on every enabled component; the config loader runs
/// [`validate`](Component::validate) on every enabled component.
pub trait Component: Send + Sync {
    /// Stable id, `<kind>/<name>` (e.g. `runtime/containerd`).
    fn id(&self) -> &'static str;

    /// Which layer the component belongs to.
    fn kind(&self) -> ComponentKind;

    /// Whether the component is active for `spec`. Inactive components
    /// contribute neither validation nor tasks.
    fn enabled(&self, spec: &Cluster) -> bool;

    /// Component-specific validation rules. Append to `d`; the hint text
    /// must stay identical to the rule's previous wording in
    /// `kurin-config` — diagnostic snapshots guard it.
    fn validate(&self, spec: &Cluster, d: &mut Diagnostics);

    /// What the component supports (distributions, archs, offline, ops).
    fn capabilities(&self) -> Capabilities;

    /// Artifacts the component needs for `spec` at `arch` — the basis of
    /// the offline-bundle aggregation (WP-16).
    fn artifacts(&self, spec: &Cluster, arch: Arch) -> Vec<ArtifactRef>;

    /// Contribute tasks for the builder's operation; components that do
    /// not support the operation add nothing.
    fn plan(&self, b: &mut PlanBuilder<'_>) -> Result<()>;
}

/// The explicit registry of built-in components: no dynamic loading, and
/// the item order is the consultation order (os track first, then the
/// runtime, then the distribution that ties them together).
pub struct ComponentRegistry {
    items: Vec<Arc<dyn Component>>,
}

impl ComponentRegistry {
    /// The registered components, in consultation order.
    pub fn iter(&self) -> std::slice::Iter<'_, Arc<dyn Component>> {
        self.items.iter()
    }

    /// Look up a component by id (e.g. `os/baseline`).
    pub fn get(&self, id: &str) -> Option<&Arc<dyn Component>> {
        self.items.iter().find(|c| c.id() == id)
    }
}

/// The built-in components in consultation order.
pub fn registry() -> &'static ComponentRegistry {
    static REGISTRY: LazyLock<ComponentRegistry> = LazyLock::new(|| ComponentRegistry {
        items: vec![
            Arc::new(os::OsBaseline),
            Arc::new(os::Sysctl),
            Arc::new(os::KernelDeps),
            Arc::new(runtime::Containerd),
            Arc::new(runtime::Crio),
            Arc::new(distro::Kubeadm),
            Arc::new(distro::K3s),
        ],
    });
    &REGISTRY
}

/// Run every enabled component's validation rules over `cluster`;
/// returns the first finding as the loader-style config error.
pub fn validate(cluster: &Cluster) -> kurin_config::Result<()> {
    let mut d = Diagnostics::new(cluster.source.clone());
    for component in registry().iter() {
        if component.enabled(cluster) {
            component.validate(cluster, &mut d);
        }
    }
    match d.first_error() {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_lists_the_seven_wp15_components_in_consultation_order() {
        let ids: Vec<&'static str> = registry().iter().map(|c| c.id()).collect();
        assert_eq!(
            ids,
            [
                "os/baseline",
                "os/sysctl",
                "os/kernel-deps",
                "runtime/containerd",
                "runtime/crio",
                "distro/kubeadm",
                "distro/k3s",
            ]
        );
        assert_eq!(
            registry().get("runtime/crio").map(|c| c.id()),
            Some("runtime/crio")
        );
        assert!(registry().get("cni/calico").is_none(), "WP-16 territory");
    }

    #[test]
    fn declared_artifacts_all_resolve_in_the_embedded_manifest() {
        // Every name any component declares must exist in the embedded
        // manifest for both arches — `ArtifactRef::of` panics otherwise.
        let names = [
            "containerd",
            "runc",
            "pause",
            "crio",
            "kubeadm",
            "kubelet",
            "kubectl",
            "crictl",
            "kube-apiserver",
            "kube-controller-manager",
            "kube-scheduler",
            "kube-proxy",
            "coredns",
            "etcd",
            "k3s",
            "k3s-airgap",
        ];
        for arch in [Arch::Amd64, Arch::Arm64] {
            for name in names {
                let r = ArtifactRef::of(name, arch);
                assert_eq!(r.name, name);
                assert!(!r.version.is_empty(), "{name} has a version");
            }
        }
    }

    #[test]
    fn diagnostics_keeps_the_first_finding_in_loader_shape() {
        let mut d = Diagnostics::new("source text");
        assert!(d.is_clean());
        d.error("first");
        d.error("second");
        let err = d.first_error().expect("a finding");
        match err {
            ConfigError::Invalid { hint, text, .. } => {
                assert_eq!(hint, "first");
                assert_eq!(text, "source text");
            }
            other => panic!("unexpected error shape: {other:?}"),
        }
    }
}
