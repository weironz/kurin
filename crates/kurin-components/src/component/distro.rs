//! `distro/*`: the Kubernetes distributions — the components that tie
//! the os and runtime tracks into a cluster.

use kurin_config::{Cluster, CniConfig, Distribution, RuntimeKind};
use kurin_core::arch::Arch;
use kurin_core::engine::Stage;
use kurin_core::plan::{OpKind, PlanBuilder};
use miette::Result;

use super::{
    tracks, ArtifactRef, Capabilities, Component, ComponentKind, Diagnostics, Offline, ALL_ZONES,
};
use crate::tasks;
use std::sync::Arc;

/// `distro/kubeadm`: upstream Kubernetes on the kubeadm track — kubelet
/// units, the image preload, `kubeadm init`, the API-readiness gate and
/// the joins.
pub struct Kubeadm;

impl Component for Kubeadm {
    fn id(&self) -> &'static str {
        "distro/kubeadm"
    }

    fn kind(&self) -> ComponentKind {
        ComponentKind::Distribution
    }

    fn enabled(&self, spec: &Cluster) -> bool {
        spec.distribution == Distribution::K8s
    }

    fn validate(&self, _spec: &Cluster, _d: &mut Diagnostics) {
        // No kubeadm-specific rules: cross-distribution conflicts are the
        // k3s component's to declare.
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
        // Node binaries and the control-plane image set kubeadm pulls.
        // CNI fleets, registry payloads and the kube-vip image belong to
        // their own components (WP-16).
        [
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
        ]
        .into_iter()
        .map(|name| ArtifactRef::of(name, arch))
        .collect()
    }

    fn plan(&self, b: &mut PlanBuilder<'_>) -> Result<()> {
        // The kubelet unit sits inside every member's os+runtime band.
        let version = b.cluster().kubernetes_version.to_string();
        let prepare = b.phase("prepare");
        for node in b.nodes().k8s().collect::<Vec<_>>() {
            b.add_track(
                prepare,
                Arc::new(tasks::prepare::KubeletUnit::new(
                    node,
                    kurin_artifact::version::PAUSE_IMAGE,
                    &version,
                )),
                tracks::OS_K8S_MEMBER,
            );
        }

        // Image preload per member (offline bundles seed the runtime;
        // online the task is a no-op that reports InSync).
        let images = b.phase("images");
        for node in b.nodes().k8s().collect::<Vec<_>>() {
            let arch = b.facts().arch(&node.name)?;
            b.add(
                images,
                Arc::new(tasks::images::ImagesImport::new(
                    node,
                    arch,
                    b.cluster(),
                    b.run(),
                )),
            );
        }

        // Bootstrap: init on the first control plane, then wait for the
        // (VIP-backed) API to serve before any join. Legacy phase-index
        // quirk preserved: init lands in `vip`, the readiness gate in
        // `bootstrap` (WP-11/12 fix the semantics).
        if let Some(first) = b.nodes().first_cp() {
            let vip = b.phase("vip");
            b.add(
                vip,
                Arc::new(tasks::bootstrap::KubeadmInit::new(
                    b.cluster(),
                    first,
                    b.run(),
                )),
            );
            let bootstrap = b.phase("bootstrap");
            b.add(
                bootstrap,
                Arc::new(tasks::bootstrap::ApiReady::new(b.cluster(), first)),
            );
        }

        // Joins: every member except the first control plane. Legacy
        // phase-index quirk preserved: they land in `api-ready`, not
        // `join`.
        let first_cp = b.nodes().first_cp().map(|n| n.name.clone());
        let api_ready = b.phase("api-ready");
        for node in b.nodes().k8s().collect::<Vec<_>>() {
            if first_cp.as_deref() == Some(node.name.as_str()) {
                continue;
            }
            b.add(
                api_ready,
                Arc::new(tasks::join::KubeadmJoin::new(b.cluster(), node, b.run())),
            );
        }
        Ok(())
    }
}

/// `distro/k3s`: the k3s distribution — its own unit, bootstrap, join
/// and kubeconfig, plus the k3s flavors of kube-vip and the external LB.
pub struct K3s;

impl Component for K3s {
    fn id(&self) -> &'static str {
        "distro/k3s"
    }

    fn kind(&self) -> ComponentKind {
        ComponentKind::Distribution
    }

    fn enabled(&self, spec: &Cluster) -> bool {
        spec.distribution == Distribution::K3s
    }

    fn validate(&self, spec: &Cluster, d: &mut Diagnostics) {
        // Moved from kurin-config's validate (WP-15); the wording is
        // unchanged — diagnostic snapshots guard it.
        if spec.runtime == RuntimeKind::Crio {
            d.error(
                "k3s embeds containerd; kubernetes.runtime = \"crio\" needs distribution = \"k8s\"",
            );
        }
        // K3s embeds containerd and (in this build) ships its own
        // flannel. Calico/Cilium with k3s needs --flannel-backend=none
        // wiring that this build does not do.
        if !matches!(spec.cni, CniConfig::Flannel | CniConfig::None) {
            d.error(
                "kubernetes.distribution = \"k3s\" supports network.cni = \"flannel\" or \"none\" only",
            );
        }
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            distributions: &[Distribution::K3s],
            archs: &[Arch::Amd64, Arch::Arm64],
            offline: Offline::Bundle,
            zones: ALL_ZONES,
            ops: &[OpKind::Create],
        }
    }

    fn artifacts(&self, spec: &Cluster, arch: Arch) -> Vec<ArtifactRef> {
        let mut refs = vec![ArtifactRef::of("k3s", arch)];
        // The air-gap tarball only matters when the install runs from a
        // bundle; the env-zone override is resolved at run time and is
        // invisible to a static declaration.
        if spec.offline_dir.is_some() || spec.zone.as_deref() == Some("cn") {
            refs.push(ArtifactRef::of("k3s-airgap", arch));
        }
        refs
    }

    fn plan(&self, b: &mut PlanBuilder<'_>) -> Result<()> {
        // The k3s task set owns its stage assignment (including its
        // kube-vip and external-LB flavors); its prepare tasks sweep
        // after every member's os band.
        for (stage, task) in tasks::k3s::stage_tasks(b.cluster(), b.run())? {
            let phase = b.phase(stage.label());
            match stage {
                Stage::Prepare => {
                    b.add_track(phase, task, tracks::LATE_UNITS);
                }
                _ => {
                    b.add(phase, task);
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kubeadm_toml(extra: &str) -> Cluster {
        let toml = format!(
            r#"version = "v1alpha1"
name = "kd"
[kubernetes]
version = "1.33.1"
{extra}
[network]
pod-cidr = "10.233.64.0/18"
service-cidr = "10.233.0.0/18"
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
        );
        kurin_config::parse_str(std::path::Path::new("kd.toml"), &toml).expect("cluster parses")
    }

    fn k3s_toml(kubernetes_extra: &str, cni: &str) -> Cluster {
        let toml = format!(
            r#"version = "v1alpha1"
name = "k3"
[kubernetes]
version = "1.33.1"
distribution = "k3s"
{kubernetes_extra}
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
[nodes.s1]
address = "10.99.0.11"
role = "control-plane"
[nodes.a1]
address = "10.99.0.21"
role = "worker"
"#
        );
        kurin_config::parse_str(std::path::Path::new("k3.toml"), &toml).expect("cluster parses")
    }

    #[test]
    fn distros_enable_exclusively_by_distribution() {
        let reg = super::super::registry();
        let k8s = kubeadm_toml("");
        assert!(reg.get("distro/kubeadm").unwrap().enabled(&k8s));
        assert!(!reg.get("distro/k3s").unwrap().enabled(&k8s));

        let k3s = k3s_toml("", "flannel");
        assert!(!reg.get("distro/kubeadm").unwrap().enabled(&k3s));
        assert!(reg.get("distro/k3s").unwrap().enabled(&k3s));
    }

    #[test]
    fn k3s_rejects_crio_with_the_previous_error_text() {
        // The rule moved from kurin-config (WP-15); the text is frozen.
        let spec = k3s_toml("runtime = \"crio\"", "flannel");
        let mut d = Diagnostics::new("");
        K3s.validate(&spec, &mut d);
        assert_eq!(
            d.first_error().map(|e| e.to_string()),
            Some(
                "invalid configuration: k3s embeds containerd; kubernetes.runtime = \"crio\" needs distribution = \"k8s\""
                    .to_string()
            )
        );
    }

    #[test]
    fn k3s_rejects_non_embedded_cnis_with_the_previous_error_text() {
        for cni in ["calico", "cilium", "kube-ovn"] {
            let spec = k3s_toml("", cni);
            let mut d = Diagnostics::new("");
            K3s.validate(&spec, &mut d);
            assert_eq!(
                d.first_error().map(|e| e.to_string()),
                Some(
                    "invalid configuration: kubernetes.distribution = \"k3s\" supports network.cni = \"flannel\" or \"none\" only"
                        .to_string()
                ),
                "cni {cni}"
            );
        }
    }

    #[test]
    fn k3s_accepts_flannel_and_none() {
        for cni in ["flannel", "none"] {
            let spec = k3s_toml("", cni);
            let mut d = Diagnostics::new("");
            K3s.validate(&spec, &mut d);
            assert!(d.is_clean(), "cni {cni} must be accepted");
        }
        // kubeadm specs never run the k3s rules (component not enabled).
        let spec = kubeadm_toml("runtime = \"crio\"");
        let mut d = Diagnostics::new("");
        Kubeadm.validate(&spec, &mut d);
        assert!(d.is_clean());
    }

    #[test]
    fn aggregated_validate_reports_the_first_k3s_rule() {
        // k3s + crio + calico: the runtime conflict wins (rule order
        // inside the component), matching the old first-error behavior.
        let spec = k3s_toml("runtime = \"crio\"", "calico");
        let err = super::super::validate(&spec).expect_err("must be rejected");
        assert_eq!(
            err.to_string(),
            "invalid configuration: k3s embeds containerd; kubernetes.runtime = \"crio\" needs distribution = \"k8s\""
        );

        let ok = k3s_toml("", "flannel");
        super::super::validate(&ok).expect("valid k3s spec passes");
    }

    #[test]
    fn kubeadm_declares_its_node_binaries_and_control_plane_images() {
        let spec = kubeadm_toml("");
        let names: Vec<String> = Kubeadm
            .artifacts(&spec, Arch::Amd64)
            .iter()
            .map(|a| a.name.clone())
            .collect();
        assert_eq!(
            names,
            [
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
            ]
        );
    }

    #[test]
    fn k3s_declares_the_airgap_tarball_only_when_needed() {
        let online = k3s_toml("", "flannel");
        assert_eq!(online.offline_dir, None);
        assert_eq!(K3s.artifacts(&online, Arch::Arm64).len(), 1);

        let mut offline = online;
        offline.offline_dir = Some(std::path::PathBuf::from("/tmp/does-not-matter"));
        let names: Vec<String> = K3s
            .artifacts(&offline, Arch::Arm64)
            .iter()
            .map(|a| a.name.clone())
            .collect();
        assert_eq!(names, ["k3s", "k3s-airgap"]);
    }

    #[test]
    fn kubeadm_plan_spans_images_bootstrap_and_joins() {
        let spec = kubeadm_toml("");
        let plan = crate::test_util::plan_for(&spec);
        assert_eq!(
            crate::test_util::phase_ids(&plan, "images"),
            ["images/import@cp1", "images/import@w1"]
        );
        assert_eq!(
            crate::test_util::phase_ids(&plan, "vip"),
            ["bootstrap/kubeadm-init@cp1"]
        );
        assert_eq!(
            crate::test_util::phase_ids(&plan, "bootstrap"),
            ["bootstrap/api-ready@cp1"]
        );
        // Legacy quirk: joins land in `api-ready`, `join` stays empty.
        assert_eq!(
            crate::test_util::phase_ids(&plan, "api-ready"),
            ["join/kubeadm@w1"]
        );
        assert!(crate::test_util::phase_ids(&plan, "join").is_empty());
    }

    #[test]
    fn k3s_plan_uses_its_own_stages() {
        let spec = k3s_toml("", "flannel");
        let plan = crate::test_util::plan_for(&spec);
        assert_eq!(
            crate::test_util::phase_ids(&plan, "prepare"),
            [
                "prepare/os-baseline@a1",
                "prepare/sysctl@a1",
                "prepare/kernel-deps@a1",
                "prepare/os-baseline@s1",
                "prepare/sysctl@s1",
                "prepare/kernel-deps@s1",
                "prepare/k3s-unit@a1",
                "prepare/k3s-unit@s1",
            ]
        );
        assert_eq!(
            crate::test_util::phase_ids(&plan, "bootstrap"),
            ["bootstrap/k3s-init@s1"]
        );
        assert_eq!(
            crate::test_util::phase_ids(&plan, "api-ready"),
            ["api-ready/k3s@s1"]
        );
        assert_eq!(crate::test_util::phase_ids(&plan, "join"), ["join/k3s@a1"]);
        assert_eq!(
            crate::test_util::phase_ids(&plan, "post"),
            ["post/k3s-kubeconfig@s1"]
        );
        // The k3s track has no image preload.
        assert!(crate::test_util::phase_ids(&plan, "images").is_empty());
    }
}
