//! `cni/*`: one component per `network.cni` choice. The kubeadm track
//! applies the plugin through the first control plane's kubectl; the k3s
//! track embeds or omits the CNI (its task set owns the flannel backend),
//! so these components plan nothing there. The k3s distribution's own
//! CNI constraints (flannel or none only) stay in `distro/k3s`.

use std::sync::Arc;

use kurin_config::{Cluster, CniConfig, Distribution};
use kurin_core::arch::Arch;
use kurin_core::plan::{OpKind, PlanBuilder};
use miette::Result;

use super::{ArtifactRef, Capabilities, Component, ComponentKind, Diagnostics, Offline, ALL_ZONES};
use crate::tasks;

/// Arch the pinned plugin payloads resolve for: the control-plane node
/// the plugin is applied through (`facts` fall back to the host arch in
/// `--dry-run`, same as the dispatch this replaces).
fn cp_arch(b: &PlanBuilder<'_>) -> Result<Arch> {
    match b.nodes().first_cp() {
        Some(cp) => b.facts().arch(&cp.name),
        None => Arch::host(),
    }
}

/// `cni/calico`: the tigera-operator manifest plus an inline Installation
/// CR, applied through the first control plane.
pub struct Calico;

impl Component for Calico {
    fn id(&self) -> &'static str {
        "cni/calico"
    }

    fn kind(&self) -> ComponentKind {
        ComponentKind::Cni
    }

    fn enabled(&self, spec: &Cluster) -> bool {
        spec.cni == CniConfig::Calico
    }

    fn validate(&self, _spec: &Cluster, _d: &mut Diagnostics) {
        // The k3s conflict is the k3s distribution's rule (distro/k3s).
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            // k3s rejects every non-flannel CNI (distro/k3s's rule).
            distributions: &[Distribution::K8s],
            archs: &[Arch::Amd64, Arch::Arm64],
            offline: Offline::Bundle,
            zones: ALL_ZONES,
            ops: &[OpKind::Create],
        }
    }

    fn artifacts(&self, _spec: &Cluster, arch: Arch) -> Vec<ArtifactRef> {
        // The operator manifest plus the fleet the Installation CR
        // pulls; image order matches the offline import set.
        [
            "calico-operator",
            "calico-node",
            "calico-cni",
            "calico-pod2daemon-flexvol",
            "calico-kube-controllers",
            "calico-typha",
            "calico-csi",
            "calico-node-driver-registrar",
            "tigera-operator",
        ]
        .into_iter()
        .map(|name| ArtifactRef::of(name, arch))
        .collect()
    }

    fn plan(&self, b: &mut PlanBuilder<'_>) -> Result<()> {
        if b.cluster().distribution != Distribution::K8s {
            return Ok(());
        }
        let task = Arc::new(tasks::cni::calico::CalicoOperator::new(
            b.cluster(),
            cp_arch(b)?,
            b.run(),
        )) as Arc<dyn kurin_core::engine::Task>;
        let phase = b.phase("cni");
        b.add(phase, task);
        Ok(())
    }
}

/// `cni/flannel`: upstream kube-flannel.yml, CIDR-patched to the
/// cluster's pod subnet.
pub struct Flannel;

impl Component for Flannel {
    fn id(&self) -> &'static str {
        "cni/flannel"
    }

    fn kind(&self) -> ComponentKind {
        ComponentKind::Cni
    }

    fn enabled(&self, spec: &Cluster) -> bool {
        spec.cni == CniConfig::Flannel
    }

    fn validate(&self, _spec: &Cluster, _d: &mut Diagnostics) {}

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            distributions: &[Distribution::K8s, Distribution::K3s],
            archs: &[Arch::Amd64, Arch::Arm64],
            offline: Offline::Bundle,
            zones: ALL_ZONES,
            ops: &[OpKind::Create],
        }
    }

    fn artifacts(&self, _spec: &Cluster, arch: Arch) -> Vec<ArtifactRef> {
        vec![
            ArtifactRef::of("flannel", arch),
            ArtifactRef::of("flannel-image", arch),
            ArtifactRef::of("flannel-cni-plugin", arch),
        ]
    }

    fn plan(&self, b: &mut PlanBuilder<'_>) -> Result<()> {
        // k3s embeds its own flannel backend; this is the kubeadm task.
        if b.cluster().distribution != Distribution::K8s {
            return Ok(());
        }
        let task = Arc::new(tasks::cni::flannel::Flannel::new(
            b.cluster(),
            cp_arch(b)?,
            b.run(),
        )) as Arc<dyn kurin_core::engine::Task>;
        let phase = b.phase("cni");
        b.add(phase, task);
        Ok(())
    }
}

/// `cni/cilium`: the pinned Helm chart, applied through `HelmRunner`
/// with the readiness tracker watching the fleet (WP-48).
pub struct Cilium;

impl Component for Cilium {
    fn id(&self) -> &'static str {
        "cni/cilium"
    }

    fn kind(&self) -> ComponentKind {
        ComponentKind::Cni
    }

    fn enabled(&self, spec: &Cluster) -> bool {
        spec.cni == CniConfig::Cilium
    }

    fn validate(&self, spec: &Cluster, d: &mut Diagnostics) {
        // Cilium 1.20.4's official matrix is Kubernetes 1.31-1.36; 1.37
        // changed the PartialObjectMetadata watch path it relies on.
        if !(31..=36).contains(&spec.kubernetes_version.minor) {
            d.error(format!(
                "Cilium 1.20.4 supports Kubernetes 1.31-1.36; {} is newer. \
                 Use `--kubernetes 1.36.6` (or a base package for 1.36.x), \
                 or wait for a Cilium release that supports 1.37.",
                spec.kubernetes_version
            ));
        }
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            distributions: &[Distribution::K8s],
            archs: &[Arch::Amd64, Arch::Arm64],
            offline: Offline::Bundle,
            zones: ALL_ZONES,
            ops: &[OpKind::Create],
        }
    }

    fn artifacts(&self, _spec: &Cluster, arch: Arch) -> Vec<ArtifactRef> {
        // The chart archive joins the images it renders: both travel the
        // payload paths (online cache, offline bundle, base package)
        // so the apply path never needs the network in air-gapped runs.
        vec![
            ArtifactRef::of("cilium-chart", arch),
            ArtifactRef::of("cilium-image", arch),
            ArtifactRef::of("cilium-operator-generic", arch),
            ArtifactRef::of("cilium-envoy", arch),
        ]
    }

    fn plan(&self, b: &mut PlanBuilder<'_>) -> Result<()> {
        if b.cluster().distribution != Distribution::K8s {
            return Ok(());
        }
        let task = tasks::cni::cilium::plugin(b.cluster(), b.run())?;
        let phase = b.phase("cni");
        b.add(phase, task);
        Ok(())
    }
}

/// `cni/kube-ovn`: the official pinned Helm chart; the cluster's CIDRs
/// ride as chart values (WP-48).
pub struct KubeOvn;

impl Component for KubeOvn {
    fn id(&self) -> &'static str {
        "cni/kube-ovn"
    }

    fn kind(&self) -> ComponentKind {
        ComponentKind::Cni
    }

    fn enabled(&self, spec: &Cluster) -> bool {
        spec.cni == CniConfig::KubeOvn
    }

    fn validate(&self, _spec: &Cluster, _d: &mut Diagnostics) {}

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            distributions: &[Distribution::K8s],
            archs: &[Arch::Amd64, Arch::Arm64],
            offline: Offline::Bundle,
            zones: ALL_ZONES,
            ops: &[OpKind::Create],
        }
    }

    fn artifacts(&self, _spec: &Cluster, arch: Arch) -> Vec<ArtifactRef> {
        // The official chart archive plus the two images its rendered
        // output references; all pinned in the artifact manifest, so
        // the offline bundle and the base package carry them.
        vec![
            ArtifactRef::of("kube-ovn-chart", arch),
            ArtifactRef::of("kube-ovn-image", arch),
            ArtifactRef::of("kube-ovn-vpc-nat-gateway", arch),
        ]
    }

    fn plan(&self, b: &mut PlanBuilder<'_>) -> Result<()> {
        if b.cluster().distribution != Distribution::K8s {
            return Ok(());
        }
        let task = Arc::new(tasks::cni::kube_ovn::KubeOvn::new(b.cluster(), b.run()))
            as Arc<dyn kurin_core::engine::Task>;
        let phase = b.phase("cni");
        b.add(phase, task);
        Ok(())
    }
}

/// `cni/none`: the operator installs their own plugin; no task work.
pub struct NoneCni;

impl Component for NoneCni {
    fn id(&self) -> &'static str {
        "cni/none"
    }

    fn kind(&self) -> ComponentKind {
        ComponentKind::Cni
    }

    fn enabled(&self, spec: &Cluster) -> bool {
        spec.cni == CniConfig::None
    }

    fn validate(&self, _spec: &Cluster, _d: &mut Diagnostics) {}

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            distributions: &[Distribution::K8s, Distribution::K3s],
            archs: &[Arch::Amd64, Arch::Arm64],
            offline: Offline::Bundle,
            zones: ALL_ZONES,
            ops: &[OpKind::Create],
        }
    }

    fn artifacts(&self, _spec: &Cluster, _arch: Arch) -> Vec<ArtifactRef> {
        Vec::new()
    }

    fn plan(&self, b: &mut PlanBuilder<'_>) -> Result<()> {
        if b.cluster().distribution != Distribution::K8s {
            return Ok(());
        }
        let task =
            Arc::new(tasks::cni::NoCni::new(b.cluster())) as Arc<dyn kurin_core::engine::Task>;
        let phase = b.phase("cni");
        b.add(phase, task);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cluster(toml: &str) -> Cluster {
        kurin_config::parse_str(std::path::Path::new("cni.toml"), toml).expect("cluster parses")
    }

    /// One kubeadm cluster with the given CNI.
    fn spec(cni: &str) -> Cluster {
        cluster(&format!(
            r#"version = "v1alpha1"
name = "cnicomp"
[kubernetes]
version = "1.33.1"
[network]
pod-cidr = "10.233.64.0/18"
service-cidr = "10.233.0.0/18"
[network.cni]
kind = "{cni}"
[ssh]
user = "root"
[ssh.auth]
method = "password"
password = "fake"
[nodes.cp1]
address = "10.99.0.11"
role = "control-plane"
[nodes.w1]
address = "10.99.0.21"
role = "worker"
"#
        ))
    }

    #[test]
    fn components_enable_exactly_for_their_cni_choice() {
        let reg = super::super::registry();
        for (cni, id) in [
            ("calico", "cni/calico"),
            ("flannel", "cni/flannel"),
            ("cilium", "cni/cilium"),
            ("kube-ovn", "cni/kube-ovn"),
            ("none", "cni/none"),
        ] {
            let c = spec(cni);
            for component in reg.iter() {
                if component.kind() != ComponentKind::Cni {
                    continue;
                }
                assert_eq!(
                    component.enabled(&c),
                    component.id() == id,
                    "{} x {}",
                    cni,
                    component.id()
                );
            }
        }
    }

    #[test]
    fn cilium_rejects_kubernetes_outside_its_support_matrix() {
        let mut c = spec("cilium");
        c.kubernetes_version = semver::Version::parse("1.37.2").unwrap();
        let mut d = Diagnostics::new("");
        Cilium.validate(&c, &mut d);
        let err = d.first_error().expect("1.37 is rejected");
        assert!(err.to_string().contains("1.31-1.36"), "{err}");

        c.kubernetes_version = semver::Version::parse("1.36.6").unwrap();
        let mut d = Diagnostics::new("");
        Cilium.validate(&c, &mut d);
        assert!(d.is_clean(), "1.36.6 is supported");
    }

    #[test]
    fn every_cni_accepts_offline_installs() {
        // WP-37: all CNIs are air-gap ready; the offline bundle carries
        // their images through `artifacts()`.
        for cni in ["calico", "flannel", "cilium", "kube-ovn", "none"] {
            let mut c = spec(cni);
            c.packages.offline_dir = Some(std::path::PathBuf::from("/tmp/does-not-matter"));
            let mut d = Diagnostics::new("");
            for component in super::super::registry().iter() {
                if component.enabled(&c) {
                    component.validate(&c, &mut d);
                }
            }
            assert_eq!(d.first_error().map(|e| e.to_string()), None, "{cni}");
        }
    }

    #[test]
    fn image_artifacts_match_the_offline_import_order() {
        // The import set is derived from these declarations; keep the
        // order the frozen imports expect (control-plane set first, then
        // the plugin fleet).
        for (id, images) in [
            (
                "cni/calico",
                vec![
                    "calico-node",
                    "calico-cni",
                    "calico-pod2daemon-flexvol",
                    "calico-kube-controllers",
                    "calico-typha",
                    "calico-csi",
                    "calico-node-driver-registrar",
                    "tigera-operator",
                ],
            ),
            ("cni/flannel", vec!["flannel-image", "flannel-cni-plugin"]),
            (
                "cni/cilium",
                vec!["cilium-image", "cilium-operator-generic", "cilium-envoy"],
            ),
            (
                "cni/kube-ovn",
                vec!["kube-ovn-image", "kube-ovn-vpc-nat-gateway"],
            ),
            ("cni/none", vec![]),
        ] {
            let component = super::super::registry().get(id).expect(id);
            let refs = component.artifacts(&spec("calico"), Arch::Amd64);
            let names: Vec<&str> = refs
                .iter()
                .filter(|r| r.kind == kurin_artifact::artifact::ArtifactKind::Image)
                .map(|r| r.name.as_str())
                .collect();
            assert_eq!(names, images, "{id}");
        }
    }

    #[test]
    fn the_selected_cni_task_lands_in_the_cni_phase() {
        for cni in ["calico", "flannel", "cilium", "kube-ovn", "none"] {
            let plan = crate::test_util::plan_for(&spec(cni));
            let ids = crate::test_util::phase_ids(&plan, "cni");
            assert_eq!(ids.len(), 1, "{cni}: {ids:?}");
        }
    }

    #[test]
    fn k3s_track_plans_no_cni_task() {
        // k3s embeds its own flannel backend; the kubeadm-track task
        // never appears on the k3s plan.
        let toml = r#"version = "v1alpha1"
name = "cni-k3s"
[kubernetes]
version = "1.33.1"
distribution = "k3s"
[network]
pod-cidr = "10.233.64.0/18"
service-cidr = "10.233.0.0/18"
[network.cni]
kind = "flannel"
[ssh]
user = "root"
[ssh.auth]
method = "password"
password = "fake"
[nodes.s1]
address = "10.99.0.11"
role = "control-plane"
[nodes.a1]
address = "10.99.0.21"
role = "worker"
"#;
        let c = cluster(toml);
        let plan = crate::test_util::plan_for(&c);
        assert!(crate::test_util::phase_ids(&plan, "cni").is_empty());
    }
}
