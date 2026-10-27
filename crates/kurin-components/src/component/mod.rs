//! Built-in first-layer components (02 §3.8, ADR-07): the unified entry
//! point for every piece that converges a node's OS or bootstraps the
//! cluster. Each component owns its validation rules, its capability
//! declaration, its artifact list and its plan contribution; the planner
//! consults the explicit [`registry()`] — there is no dynamic loading.
//!
//! WP-15 migrated the os, runtime and distro components; WP-16 adds ha,
//! cni, registry, storage (an addon-layer candidate, see WP-47) and the
//! post-install reporting. `capabilities()` aggregated over the registry
//! is the single source of truth behind `kurin capabilities --json` and
//! `just docs-matrix`.
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
use kurin_core::zone::Zone;
use miette::Result;

pub mod cni;
pub mod distro;
pub mod ha;
pub mod os;
pub mod post;
pub mod registry;
pub mod runtime;
pub mod storage;

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
    /// CNI plugin.
    Cni,
    /// API endpoint HA.
    Ha,
    /// Node-hosted image registry.
    Registry,
    /// Dynamic storage provisioning (first-layer until WP-47 moves it
    /// to the addon layer).
    Storage,
    /// Post-install reporting (kubeconfig, summary, labels).
    Post,
    /// Declarative addon engine (WP-40+).
    AddonEngine,
}

impl ComponentKind {
    /// Stable lowercase name — the key the capability table and the
    /// docs-matrix generator group by.
    pub fn as_str(self) -> &'static str {
        match self {
            ComponentKind::Os => "os",
            ComponentKind::Runtime => "runtime",
            ComponentKind::Distribution => "distribution",
            ComponentKind::Cni => "cni",
            ComponentKind::Ha => "ha",
            ComponentKind::Registry => "registry",
            ComponentKind::Storage => "storage",
            ComponentKind::Post => "post",
            ComponentKind::AddonEngine => "addon-engine",
        }
    }
}

/// The download zones every built-in component works in (the CN mirror
/// table covers the whole manifest; `scripts/check-mirrors.sh` guards
/// the parity).
pub const ALL_ZONES: &[Zone] = &[Zone::Global, Zone::Cn];

/// Offline-bundle readiness of a component — the three honest states the
/// capability table distinguishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Offline {
    /// Everything the component needs ships in the offline bundle.
    Bundle,
    /// Validation rejects offline installs for this component (honest
    /// failure instead of a half-installed cluster).
    Rejected,
    /// Not bundle-managed: a dependency (distro package, runtime image
    /// pull) needs the network even on an offline install.
    OnlineOnly,
}

impl Offline {
    /// Stable lowercase name — the value `kurin capabilities --json`
    /// emits and the docs-matrix generator maps to a cell.
    pub fn as_str(self) -> &'static str {
        match self {
            Offline::Bundle => "bundle",
            Offline::Rejected => "rejected",
            Offline::OnlineOnly => "online-only",
        }
    }
}

/// What a component supports. Aggregated over the registry this becomes
/// the single source of truth for config validation, the docs matrix and
/// the Web capability table.
#[derive(Debug, Clone)]
pub struct Capabilities {
    /// Distributions the component runs on.
    pub distributions: &'static [Distribution],
    /// Architectures with pinned artifacts.
    pub archs: &'static [Arch],
    /// Offline-bundle readiness (see [`Offline`]).
    pub offline: Offline,
    /// Download zones the component works in.
    pub zones: &'static [Zone],
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
/// the item order is the consultation order — the os track first, then
/// the runtime, then the distribution that ties them together, then the
/// kubeadm-track areas (ha, cni, registry, storage, post) in the order
/// their `plan_*` functions used to run.
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
            Arc::new(ha::KubeVip),
            Arc::new(ha::KurinLb),
            Arc::new(ha::External),
            Arc::new(cni::Calico),
            Arc::new(cni::Flannel),
            Arc::new(cni::Cilium),
            Arc::new(cni::KubeOvn),
            Arc::new(cni::NoneCni),
            Arc::new(registry::Zot),
            Arc::new(registry::DockerRegistry),
            Arc::new(registry::Harbor),
            Arc::new(storage::LocalPath),
            Arc::new(storage::Nfs),
            Arc::new(post::Kubeconfig),
            Arc::new(post::Summary),
            Arc::new(post::Labels),
            Arc::new(post::StateRecord),
        ],
    });
    &REGISTRY
}

/// One row of the capability table: what a built-in component supports.
/// Aggregated over the registry this is what `kurin capabilities --json`
/// emits, `just docs-matrix` renders and the Web UI will consume.
#[derive(Debug, serde::Serialize)]
pub struct CapabilityRow {
    /// Component id (`<kind>/<name>`).
    pub id: &'static str,
    /// Layer the component belongs to.
    pub kind: &'static str,
    /// Distributions the component runs on.
    pub distributions: Vec<String>,
    /// Architectures with pinned artifacts.
    pub archs: Vec<String>,
    /// Offline-bundle readiness (`bundle` | `rejected` | `online-only`).
    pub offline: &'static str,
    /// Download zones the component works in.
    pub zones: Vec<String>,
    /// Operations the component contributes tasks to.
    pub ops: Vec<String>,
}

/// The capability table, one row per registered component in
/// consultation order.
pub fn capabilities_report() -> Vec<CapabilityRow> {
    registry()
        .iter()
        .map(|component| {
            let caps = component.capabilities();
            CapabilityRow {
                id: component.id(),
                kind: component.kind().as_str(),
                distributions: caps
                    .distributions
                    .iter()
                    .map(|d| match d {
                        Distribution::K8s => "k8s".to_string(),
                        Distribution::K3s => "k3s".to_string(),
                    })
                    .collect(),
                archs: caps.archs.iter().map(|a| a.as_str().to_string()).collect(),
                offline: caps.offline.as_str(),
                zones: caps
                    .zones
                    .iter()
                    .map(|z| match z {
                        Zone::Global => "global".to_string(),
                        Zone::Cn => "cn".to_string(),
                    })
                    .collect(),
                ops: caps.ops.iter().map(|o| o.as_str().to_string()).collect(),
            }
        })
        .collect()
}

/// The synthetic spec bundle aggregation resolves against: the bundle
/// exists for air-gapped installs, so spec-conditional extras (the k3s
/// air-gap tarball) are in.
fn bundle_spec() -> Cluster {
    Cluster {
        name: "kurin-bundle".into(),
        offline_dir: Some(std::path::PathBuf::new()),
        zone: None,
        distribution: Distribution::K8s,
        ha_mode: kurin_config::HaMode::KubeVip,
        runtime: kurin_config::RuntimeKind::Containerd,
        kubernetes_version: kurin_artifact::version::KUBERNETES_VERSION
            .parse()
            .expect("the pinned kubernetes version parses"),
        pod_cidr: "10.233.64.0/18"
            .parse()
            .expect("the pod cidr literal parses"),
        service_cidr: "10.233.0.0/18"
            .parse()
            .expect("the service cidr literal parses"),
        dns_domain: "cluster.local".into(),
        cni: kurin_config::CniConfig::Calico,
        vip: None,
        nodes: Vec::new(),
        registry_kind: None,
        registry_port: 5000,
        registry_username: None,
        registry_password: None,
        registry_data_dir: "/var/lib/kurin/registry".into(),
        storage_kind: None,
        storage_default: false,
        storage_server: None,
        storage_path: None,
        source: String::new(),
    }
}

/// The artifact set the offline bundle carries: the union of every
/// registered component's artifacts (enabled or not — the bundle is
/// cluster-agnostic) for one arch, de-duplicated by name. `kurin
/// artifact` builds from this list instead of the raw manifest; the
/// parity test in `kurin/tests/capabilities.rs` pins the two together.
pub fn bundle_artifacts(arch: Arch) -> Vec<ArtifactRef> {
    let spec = bundle_spec();
    let mut out: Vec<ArtifactRef> = Vec::new();
    for component in registry().iter() {
        for r in component.artifacts(&spec, arch) {
            if !out.iter().any(|e| e.name == r.name) {
                out.push(r);
            }
        }
    }
    out
}

/// Image artifacts the bootstrap track must preload on every member:
/// what kubelet pulls before any node is Ready — the runtime's sandbox
/// image, the distribution's control-plane set and the selected CNI's
/// fleet. Storage and post components apply after the cluster is up and
/// pull on demand, so they stay out (matching the hand-maintained set
/// this replaces). Both `images/import` and the registry seed resolve
/// their payloads from this.
pub fn bootstrap_images(cluster: &Cluster, arch: Arch) -> Vec<ArtifactRef> {
    let mut out: Vec<ArtifactRef> = Vec::new();
    for component in registry().iter() {
        if !matches!(
            component.kind(),
            ComponentKind::Runtime | ComponentKind::Distribution | ComponentKind::Cni
        ) || !component.enabled(cluster)
        {
            continue;
        }
        for r in component.artifacts(cluster, arch) {
            if r.kind == ArtifactKind::Image && !out.iter().any(|e| e.name == r.name) {
                out.push(r);
            }
        }
    }
    out
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
    fn registry_lists_the_built_in_components_in_consultation_order() {
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
                "ha/kube-vip",
                "ha/kurin-lb",
                "ha/external",
                "cni/calico",
                "cni/flannel",
                "cni/cilium",
                "cni/kube-ovn",
                "cni/none",
                "registry/zot",
                "registry/docker-registry",
                "registry/harbor",
                "storage/local-path",
                "storage/nfs",
                "post/kubeconfig",
                "post/summary",
                "post/labels",
                "post/state-record",
            ]
        );
        assert_eq!(
            registry().get("cni/calico").map(|c| c.id()),
            Some("cni/calico")
        );
    }

    #[test]
    fn the_capability_report_covers_every_registered_component() {
        let report = capabilities_report();
        assert_eq!(report.len(), registry().iter().count());
        assert_eq!(report[0].id, "os/baseline");
        assert_eq!(report[0].kind, "os");
        // The cni rows are the docs-matrix inputs: their distributions
        // and offline readiness drive the generated cells.
        let calico = report.iter().find(|r| r.id == "cni/calico").unwrap();
        assert_eq!(calico.distributions, ["k8s"]);
        assert_eq!(calico.offline, "bundle");
        let kube_ovn = report.iter().find(|r| r.id == "cni/kube-ovn").unwrap();
        assert_eq!(kube_ovn.offline, "rejected");
        let kube_vip = report.iter().find(|r| r.id == "ha/kube-vip").unwrap();
        assert_eq!(kube_vip.offline, "online-only");
        assert_eq!(kube_vip.distributions, ["k8s", "k3s"]);
        assert_eq!(kube_vip.zones, ["global", "cn"]);
    }

    #[test]
    fn the_bundle_aggregation_reaches_every_manifest_entry() {
        // The parity test in kurin/tests/capabilities.rs pins the exact
        // set; here the cheap invariant: every manifest name is claimed
        // by some component, so no payload can disappear from bundles.
        let manifest = ArtifactManifest::embedded().expect("the embedded manifest parses");
        let aggregated: std::collections::BTreeSet<String> = bundle_artifacts(Arch::Amd64)
            .into_iter()
            .map(|r| r.name)
            .collect();
        for entry in manifest.artifacts() {
            assert!(
                aggregated.contains(&entry.name),
                "{} is not declared by any component",
                entry.name
            );
        }
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
            "cni-plugins",
            "calico-operator",
            "calico-node",
            "calico-cni",
            "calico-pod2daemon-flexvol",
            "calico-kube-controllers",
            "calico-typha",
            "calico-csi",
            "calico-node-driver-registrar",
            "tigera-operator",
            "flannel",
            "flannel-image",
            "flannel-cni-plugin",
            "cilium-cli",
            "cilium-image",
            "cilium-operator-generic",
            "cilium-envoy",
            "zot",
            "docker-registry",
            "harbor-installer",
            "local-path-provisioner",
            "busybox",
            "nfs-subdir-provisioner",
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
