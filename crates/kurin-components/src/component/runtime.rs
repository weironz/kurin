//! `runtime/*`: the container runtime layer of the kubeadm track.

use std::sync::Arc;

use kurin_config::{Cluster, Distribution, RuntimeKind};
use kurin_core::arch::Arch;
use kurin_core::plan::{OpKind, PlanBuilder};
use miette::Result;

use super::{
    tracks, ArtifactRef, Capabilities, Component, ComponentKind, Diagnostics, Offline, ALL_ZONES,
};
use crate::tasks;

/// `runtime/containerd`: the standalone containerd install — config and
/// systemd unit — for kubeadm clusters on the default runtime. k3s
/// embeds its own containerd, so the component is not enabled there.
pub struct Containerd;

impl Component for Containerd {
    fn id(&self) -> &'static str {
        "runtime/containerd"
    }

    fn kind(&self) -> ComponentKind {
        ComponentKind::Runtime
    }

    fn enabled(&self, spec: &Cluster) -> bool {
        spec.runtime == RuntimeKind::Containerd && spec.distribution != Distribution::K3s
    }

    fn validate(&self, _spec: &Cluster, _d: &mut Diagnostics) {
        // The k3s conflict is the k3s distribution's rule (distro/k3s);
        // containerd itself has no constraints beyond the structural ones.
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
        vec![
            ArtifactRef::of("containerd", arch),
            ArtifactRef::of("runc", arch),
            // The sandbox image the rendered config.toml pins.
            ArtifactRef::of("pause", arch),
        ]
    }

    fn plan(&self, b: &mut PlanBuilder<'_>) -> Result<()> {
        let prepare = b.phase("prepare");
        for node in b.nodes().k8s().collect::<Vec<_>>() {
            let config = b.add_track(
                prepare,
                Arc::new(tasks::prepare::ContainerdConfig::new(
                    node,
                    kurin_artifact::version::PAUSE_IMAGE,
                )),
                tracks::OS_K8S_MEMBER,
            );
            let unit = b.add_track(
                prepare,
                Arc::new(tasks::prepare::ContainerdUnit::new(node)),
                tracks::OS_K8S_MEMBER,
            );
            // The config task restarts containerd; the unit must exist first
            // (a fresh host has no containerd.service until this task runs).
            b.after(config, unit);
        }
        Ok(())
    }
}

/// `runtime/crio`: the CRI-O static package install, drop-in config and
/// unit — the alternative container runtime for kubeadm clusters. The
/// pinned package resolves per node arch, and the sweep runs after every
/// member's os band (the layout the frozen snapshots pin).
pub struct Crio;

impl Component for Crio {
    fn id(&self) -> &'static str {
        "runtime/crio"
    }

    fn kind(&self) -> ComponentKind {
        ComponentKind::Runtime
    }

    fn enabled(&self, spec: &Cluster) -> bool {
        spec.runtime == RuntimeKind::Crio && spec.distribution != Distribution::K3s
    }

    fn validate(&self, _spec: &Cluster, _d: &mut Diagnostics) {
        // Nothing beyond the k3s distribution rule (distro/k3s).
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
        vec![
            ArtifactRef::of("crio", arch),
            ArtifactRef::of("pause", arch),
        ]
    }

    fn plan(&self, b: &mut PlanBuilder<'_>) -> Result<()> {
        // The task list is runtime-owned (cri-o resolves its pinned
        // package per node arch from the facts table).
        for (stage, task) in tasks::cri_o::runtime_tasks(b.cluster(), b.facts(), b.run())? {
            let phase = b.phase(stage.label());
            b.add_track(phase, task, tracks::LATE_UNITS);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cluster_toml(runtime: &str) -> Cluster {
        let toml = format!(
            r#"version = "v1alpha1"
name = "rt"
[kubernetes]
version = "1.33.1"
runtime = "{runtime}"
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
        kurin_config::parse_str(std::path::Path::new("rt.toml"), &toml).expect("cluster parses")
    }

    #[test]
    fn runtimes_enable_by_spec_choice_and_distribution() {
        let containerd = cluster_toml("containerd");
        let crio = cluster_toml("crio");

        let reg = super::super::registry();
        assert!(reg.get("runtime/containerd").unwrap().enabled(&containerd));
        assert!(!reg.get("runtime/crio").unwrap().enabled(&containerd));
        assert!(reg.get("runtime/crio").unwrap().enabled(&crio));
        assert!(!reg.get("runtime/containerd").unwrap().enabled(&crio));

        // k3s embeds its own containerd: neither runtime is enabled.
        let mut k3s = cluster_toml("containerd");
        k3s.distribution = Distribution::K3s;
        k3s.cni = kurin_config::CniConfig::Flannel;
        assert!(!reg.get("runtime/containerd").unwrap().enabled(&k3s));
        assert!(!reg.get("runtime/crio").unwrap().enabled(&k3s));
    }

    #[test]
    fn runtimes_declare_no_validation_rules() {
        for spec in [cluster_toml("containerd"), cluster_toml("crio")] {
            for id in ["runtime/containerd", "runtime/crio"] {
                let comp = super::super::registry().get(id).expect(id);
                let mut d = Diagnostics::new("");
                comp.validate(&spec, &mut d);
                assert!(d.is_clean(), "{id} must not add rules");
            }
        }
    }

    #[test]
    fn runtimes_declare_their_pinned_artifacts() {
        let spec = cluster_toml("containerd");
        let containerd: Vec<String> = super::super::registry()
            .get("runtime/containerd")
            .unwrap()
            .artifacts(&spec, Arch::Amd64)
            .iter()
            .map(|a| a.name.clone())
            .collect();
        assert_eq!(containerd, ["containerd", "runc", "pause"]);

        let crio: Vec<String> = super::super::registry()
            .get("runtime/crio")
            .unwrap()
            .artifacts(&spec, Arch::Amd64)
            .iter()
            .map(|a| a.name.clone())
            .collect();
        assert_eq!(crio, ["crio", "pause"]);
    }

    #[test]
    fn crio_units_sweep_after_every_members_band() {
        // The frozen snapshot layout: per-node os+runtime band first,
        // then the cri-o sweep (install/config/unit per node).
        let spec = cluster_toml("crio");
        let plan = crate::test_util::plan_for(&spec);
        assert_eq!(
            crate::test_util::phase_ids(&plan, "prepare"),
            [
                "prepare/os-baseline@cp1",
                "prepare/kernel-deps@cp1",
                "prepare/sysctl@cp1",
                "prepare/kubelet-unit@cp1",
                "prepare/os-baseline@w1",
                "prepare/kernel-deps@w1",
                "prepare/sysctl@w1",
                "prepare/kubelet-unit@w1",
                "prepare/crio-install@cp1",
                "prepare/crio-config@cp1",
                "prepare/crio-unit@cp1",
                "prepare/crio-install@w1",
                "prepare/crio-config@w1",
                "prepare/crio-unit@w1",
            ]
        );
    }
}
