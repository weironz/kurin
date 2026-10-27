//! `os/*`: node OS convergence — the track every node enters before any
//! Kubernetes piece is configured.

use std::sync::Arc;

use kurin_config::{Cluster, Distribution, Node, NodeRole};
use kurin_core::arch::Arch;
use kurin_core::plan::{OpKind, PlanBuilder};
use miette::Result;

use super::{
    tracks, ArtifactRef, Capabilities, Component, ComponentKind, Diagnostics, Offline, ALL_ZONES,
};
use crate::tasks;

/// `os/baseline`: the entry of the per-node track — preflight probes, the
/// pinned artifact install and the baseline convergence (swap,
/// firewalld, SELinux). Registry nodes stop here: they never join the
/// k8s member band.
pub struct OsBaseline;

impl Component for OsBaseline {
    fn id(&self) -> &'static str {
        "os/baseline"
    }

    fn kind(&self) -> ComponentKind {
        ComponentKind::Os
    }

    fn enabled(&self, _spec: &Cluster) -> bool {
        // Every node enters the os track, on both distributions.
        true
    }

    fn validate(&self, _spec: &Cluster, _d: &mut Diagnostics) {
        // No component-specific rules: node names, arch domains and
        // address uniqueness are structural and stay in kurin-config.
    }

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
        // The baseline's artifact sweep installs the pinned
        // /opt/cni/bin plugin toolset on every node (flannel's CNI
        // config and operator-provided plugins consume it; calico,
        // cilium and kube-ovn ship their own binaries). Kernel deps
        // come from the distribution package layer, not the version
        // table.
        vec![ArtifactRef::of("cni-plugins", arch)]
    }

    fn plan(&self, b: &mut PlanBuilder<'_>) -> Result<()> {
        // Registry nodes run the os track only on the kubeadm branch —
        // the k3s branch historically converges k8s members alone (its
        // task set never included registry nodes).
        if b.cluster().distribution != Distribution::K3s {
            for node in b.nodes().role(NodeRole::Registry).collect::<Vec<_>>() {
                Self::plan_node(b, node, tracks::OS_REGISTRY_NODE)?;
            }
        }
        for node in b.nodes().k8s().collect::<Vec<_>>() {
            Self::plan_node(b, node, tracks::OS_K8S_MEMBER)?;
        }
        Ok(())
    }
}

impl OsBaseline {
    /// One node's os track: preflight probe, pinned artifact install and
    /// baseline convergence.
    fn plan_node(b: &mut PlanBuilder<'_>, node: &Node, track: u32) -> Result<()> {
        let arch = b.facts().arch(&node.name)?;
        let preflight = b.phase("preflight");
        b.add_track(
            preflight,
            Arc::new(tasks::preflight::Preflight::new(node)),
            track,
        );
        let artifacts = b.phase("artifacts");
        let delivery = tasks::effective_delivery(b.cluster(), b.run().sources.as_ref());
        let has_curl = b.facts().get(&node.name).and_then(|facts| facts.curl) != Some(false);
        let registry_fetch = delivery == kurin_config::v1alpha2::Delivery::Registry && has_curl;
        let mut fetch = None;
        if registry_fetch {
            let cluster = b.cluster();
            let endpoint = cluster
                .registry_endpoint()
                .ok_or_else(|| miette::miette!("registry endpoint missing"))?;
            let prefix = if cluster.registry_kind() == Some(kurin_config::RegistryKind::Harbor) {
                "library/".to_string()
            } else {
                cluster.registry_repo_prefix()
            };
            let username =
                cluster.registry_username().map(str::to_string).or_else(|| {
                    match cluster.registry_kind() {
                        Some(kurin_config::RegistryKind::Harbor) => Some("admin".into()),
                        Some(kurin_config::RegistryKind::External) => None,
                        _ => Some("kurin".into()),
                    }
                });
            let password = b
                .run()
                .secrets()
                .registry_password
                .as_ref()
                .map(|p| secrecy::ExposeSecret::expose_secret(p).to_string());
            let use_ca = !cluster.registry_is_external() || cluster.registry_ca_file().is_some();
            let task = tasks::install::FetchFromRegistry::new(
                node,
                arch,
                cluster.runtime,
                &endpoint,
                &prefix,
                username.as_deref(),
                password.as_deref(),
                use_ca,
                b.run(),
            );
            fetch = Some(b.add_track(artifacts, Arc::new(task), track));
        }
        let install = b.add_track(
            artifacts,
            Arc::new(tasks::install::ArtifactsInstall::new(
                node,
                arch,
                b.cluster().runtime,
                b.run(),
                registry_fetch,
            )),
            track,
        );
        if let Some(fetch) = fetch {
            b.after(install, fetch);
        }
        let prepare = b.phase("prepare");
        b.add_track(
            prepare,
            Arc::new(tasks::os_baseline::OsBaseline::new(node)),
            track,
        );
        Ok(())
    }
}

/// `os/sysctl`: the Kubernetes kernel sysctls (bridge-nf-call-iptables,
/// ip_forward) plus the br_netfilter module — k8s members only, on both
/// distributions.
pub struct Sysctl;

impl Component for Sysctl {
    fn id(&self) -> &'static str {
        "os/sysctl"
    }

    fn kind(&self) -> ComponentKind {
        ComponentKind::Os
    }

    fn enabled(&self, _spec: &Cluster) -> bool {
        // Every k8s member needs the sysctls, whatever the distribution.
        true
    }

    fn validate(&self, _spec: &Cluster, _d: &mut Diagnostics) {
        // The sysctl set is unconditional; nothing to reject.
    }

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
        // sysctl + modprobe only; nothing from the version table.
        Vec::new()
    }

    fn plan(&self, b: &mut PlanBuilder<'_>) -> Result<()> {
        let prepare = b.phase("prepare");
        for node in b.nodes().k8s().collect::<Vec<_>>() {
            b.add_track(
                prepare,
                Arc::new(tasks::prepare::Sysctl::new(node)),
                tracks::OS_K8S_MEMBER,
            );
        }
        Ok(())
    }
}

/// `os/kernel-deps`: kernel-space tools kubeadm-style clusters require
/// and that cannot ship as portable static binaries (conntrack, socat
/// and friends) — k8s members only.
pub struct KernelDeps;

impl Component for KernelDeps {
    fn id(&self) -> &'static str {
        "os/kernel-deps"
    }

    fn kind(&self) -> ComponentKind {
        ComponentKind::Os
    }

    fn enabled(&self, _spec: &Cluster) -> bool {
        // Both distributions' kubelets need the conntrack/socat pair.
        true
    }

    fn validate(&self, _spec: &Cluster, _d: &mut Diagnostics) {
        // Package names differ per OS family; precheck reports the gaps.
    }

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
        // OS packages, not version-table artifacts: offline bundles carry
        // the .deb payloads, online installs fall back to apt/dnf.
        Vec::new()
    }

    fn plan(&self, b: &mut PlanBuilder<'_>) -> Result<()> {
        let prepare = b.phase("prepare");
        for node in b.nodes().k8s().collect::<Vec<_>>() {
            b.add_track(
                prepare,
                Arc::new(tasks::kernel_deps::KernelDeps::new(node)),
                tracks::OS_K8S_MEMBER,
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn component_toml(extra: &str) -> Cluster {
        let toml = format!(
            r#"version = "v1alpha1"
name = "oscomp"
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
        kurin_config::parse_str(std::path::Path::new("os.toml"), &toml).expect("cluster parses")
    }

    #[test]
    fn os_components_are_always_enabled() {
        let c = component_toml("");
        for id in ["os/baseline", "os/sysctl", "os/kernel-deps"] {
            let comp = super::super::registry().get(id).expect(id);
            assert!(comp.enabled(&c), "{id} must be enabled");
        }
    }

    #[test]
    fn os_components_declare_no_rules() {
        let c = component_toml("");
        for id in ["os/baseline", "os/sysctl", "os/kernel-deps"] {
            let comp = super::super::registry().get(id).expect(id);
            let mut d = Diagnostics::new("");
            comp.validate(&c, &mut d);
            assert!(d.is_clean(), "{id} must not add rules");
        }
    }

    #[test]
    fn baseline_converges_registry_nodes_only_on_the_kubeadm_track() {
        let toml = r#"version = "v1alpha1"
name = "withreg"
[kubernetes]
version = "1.33.1"
[network]
pod-cidr = "10.233.64.0/18"
service-cidr = "10.233.0.0/18"
[ssh]
user = "root"
[ssh.auth]
method = "password"
password = "fake"
[registry]
kind = "zot"
[nodes.cp1]
address = "10.99.0.11"
role = "control-plane"
[nodes.w1]
address = "10.99.0.21"
role = "worker"
[nodes.reg1]
address = "10.99.0.31"
role = "registry"
"#;
        let kubeadm = kurin_config::parse_str(std::path::Path::new("reg.toml"), toml)
            .expect("cluster parses");
        // kubeadm: the registry node joins the os track ahead of the members.
        let plan = crate::test_util::plan_for(&kubeadm);
        assert_eq!(
            crate::test_util::phase_ids(&plan, "preflight"),
            ["preflight/reg1", "preflight/cp1", "preflight/w1"]
        );

        // k3s: registry nodes are not part of the os track (historical
        // k3s branch shape — its task set only covers k8s members).
        let k3s = toml.replace(
            "version = \"1.33.1\"",
            "version = \"1.33.1\"\ndistribution = \"k3s\"",
        ) + "\n[network.cni]\nkind = \"flannel\"\n";
        let k3s = kurin_config::parse_str(std::path::Path::new("reg-k3s.toml"), &k3s)
            .expect("k3s parses");
        let plan = crate::test_util::plan_for(&k3s);
        assert_eq!(
            crate::test_util::phase_ids(&plan, "preflight"),
            ["preflight/cp1", "preflight/w1"]
        );
    }

    #[test]
    fn sysctl_and_kernel_deps_cover_k8s_members_only() {
        let c = component_toml("");
        let plan = crate::test_util::plan_for(&c);
        assert_eq!(
            crate::test_util::phase_ids(&plan, "prepare"),
            [
                "prepare/os-baseline@cp1",
                "prepare/sysctl@cp1",
                "prepare/kernel-deps@cp1",
                "prepare/containerd-config@cp1",
                "prepare/containerd-unit@cp1",
                "prepare/kubelet-unit@cp1",
                "prepare/os-baseline@w1",
                "prepare/sysctl@w1",
                "prepare/kernel-deps@w1",
                "prepare/containerd-config@w1",
                "prepare/containerd-unit@w1",
                "prepare/kubelet-unit@w1",
            ]
        );
    }
}
