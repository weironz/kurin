//! `registry/*`: the node-hosted image registries (one component per
//! `[registry]` kind, docs/registry-contracts.md). A registry node is
//! kubeadm-track only — the historical k3s branch never planned
//! registry nodes — so `plan` contributes nothing on the k3s track
//! while `enabled`/`validate` stay kind-based.
//!
//! ONE CA is minted per plan build and shared by install (writes the
//! cert), trust (writes the CA) and seed (curl/ctr against it): each
//! task minting its own CA wrote a different trust anchor than the
//! registry's, and every TLS client failed verification.

use std::sync::Arc;

use kurin_config::{Cluster, Distribution, NodeRole, RegistryKind};
use kurin_core::arch::Arch;
use kurin_core::plan::{OpKind, PlanBuilder};
use miette::Result;

use super::{ArtifactRef, Capabilities, Component, ComponentKind, Diagnostics, Offline, ALL_ZONES};
use crate::tasks;

/// Rules every registry kind shares (moved from kurin-config's validate
/// with WP-16; wording unchanged): a configured registry needs its node,
/// and credentials come in pairs.
fn common_rules(spec: &Cluster, d: &mut Diagnostics) {
    if !spec.nodes.iter().any(|n| n.role == NodeRole::Registry) {
        d.error("[registry] is configured but no node has role = \"registry\"");
    }
    if spec.registry_username.is_some() != spec.registry_password.is_some() {
        d.error("[registry] username and password must be set together");
    }
}

/// The registry install/trust/seed sweep shared by every kind: install
/// on the registry nodes, trust on the Kubernetes members, seed from the
/// first control plane (see docs/registry-contracts.md).
fn plan_registry_track(b: &mut PlanBuilder<'_>) -> Result<()> {
    let ca = tasks::registry::ca_for(b.cluster())?;
    let phase = b.phase("registry");
    for node in &b.cluster().nodes {
        match node.role {
            NodeRole::Registry => {
                let arch = b.facts().arch(&node.name)?;
                let task =
                    tasks::registry::RegistryInstall::new(b.cluster(), node, arch, &ca, b.run())?;
                b.add(phase, Arc::new(task));
            }
            NodeRole::ControlPlane | NodeRole::Worker => {
                let task = tasks::registry::RegistryTrust::new(b.cluster(), node, &ca)?;
                b.add(phase, Arc::new(task));
            }
            NodeRole::Lb => {}
        }
    }
    if let Some(cp1) = b.nodes().first_cp() {
        let arch = b.facts().arch(&cp1.name)?;
        let task = tasks::registry::RegistrySeed::new(b.cluster(), cp1, arch, &ca, b.run())?;
        b.add(phase, Arc::new(task));
    }
    Ok(())
}

/// `registry/zot`: single static binary, OCI-native — the default for
/// `role = "registry"` nodes going forward.
pub struct Zot;

impl Component for Zot {
    fn id(&self) -> &'static str {
        "registry/zot"
    }

    fn kind(&self) -> ComponentKind {
        ComponentKind::Registry
    }

    fn enabled(&self, spec: &Cluster) -> bool {
        spec.registry_kind == Some(RegistryKind::Zot)
    }

    fn validate(&self, spec: &Cluster, d: &mut Diagnostics) {
        common_rules(spec, d);
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
        vec![ArtifactRef::of("zot", arch)]
    }

    fn plan(&self, b: &mut PlanBuilder<'_>) -> Result<()> {
        if b.cluster().distribution != Distribution::K8s {
            return Ok(());
        }
        plan_registry_track(b)
    }
}

/// `registry/docker-registry`: CNCF distribution v3 — the single static
/// binary with a config.yml.
pub struct DockerRegistry;

impl Component for DockerRegistry {
    fn id(&self) -> &'static str {
        "registry/docker-registry"
    }

    fn kind(&self) -> ComponentKind {
        ComponentKind::Registry
    }

    fn enabled(&self, spec: &Cluster) -> bool {
        spec.registry_kind == Some(RegistryKind::DockerRegistry)
    }

    fn validate(&self, spec: &Cluster, d: &mut Diagnostics) {
        common_rules(spec, d);
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
        vec![ArtifactRef::of("docker-registry", arch)]
    }

    fn plan(&self, b: &mut PlanBuilder<'_>) -> Result<()> {
        if b.cluster().distribution != Distribution::K8s {
            return Ok(());
        }
        plan_registry_track(b)
    }
}

/// `registry/harbor`: the offline installer (docker + compose ship
/// inside it); single registry node only — harbor has no shared storage.
pub struct Harbor;

impl Component for Harbor {
    fn id(&self) -> &'static str {
        "registry/harbor"
    }

    fn kind(&self) -> ComponentKind {
        ComponentKind::Registry
    }

    fn enabled(&self, spec: &Cluster) -> bool {
        spec.registry_kind == Some(RegistryKind::Harbor)
    }

    fn validate(&self, spec: &Cluster, d: &mut Diagnostics) {
        // Moved from kurin-config's validate (WP-16); wording and rule
        // order unchanged (kind-specific gates after the shared ones).
        common_rules(spec, d);
        if spec
            .nodes
            .iter()
            .filter(|n| n.role == NodeRole::Registry)
            .count()
            > 1
        {
            d.error(
                "[registry] kind = \"harbor\" supports exactly one registry node (no shared storage)",
            );
        }
        if spec
            .registry_username
            .as_deref()
            .is_some_and(|u| u != "admin")
        {
            d.error(
                "[registry] kind = \"harbor\" can only authenticate as \"admin\" (its single admin account)",
            );
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
        vec![ArtifactRef::of("harbor-installer", arch)]
    }

    fn plan(&self, b: &mut PlanBuilder<'_>) -> Result<()> {
        if b.cluster().distribution != Distribution::K8s {
            return Ok(());
        }
        plan_registry_track(b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cluster(toml: &str) -> Cluster {
        kurin_config::parse_str(std::path::Path::new("reg.toml"), toml).expect("cluster parses")
    }

    /// Two members + one registry node, `[registry]` appended per case.
    fn base(registry: &str) -> String {
        format!(
            r#"version = "v1alpha1"
name = "regcomp"
[kubernetes]
version = "1.33.1"
[network]
pod-cidr = "10.233.64.0/18"
service-cidr = "10.233.0.0/18"
[network.cni]
kind = "calico"
[ssh]
user = "root"
[ssh.auth]
method = "password"
password = "fake"
[registry]
{registry}
[nodes.cp1]
address = "10.99.0.11"
role = "control-plane"
[nodes.w1]
address = "10.99.0.21"
role = "worker"
[nodes.reg1]
address = "10.99.0.31"
role = "registry"
"#
        )
    }

    fn first_finding(toml: &str) -> Option<String> {
        let c = cluster(toml);
        let mut d = Diagnostics::new("");
        for component in super::super::registry().iter() {
            if component.enabled(&c) {
                component.validate(&c, &mut d);
            }
        }
        d.first_error().map(|e| e.to_string())
    }

    #[test]
    fn components_enable_exactly_for_their_kind() {
        let reg = super::super::registry();
        for (kind, id) in [
            ("zot", "registry/zot"),
            ("docker-registry", "registry/docker-registry"),
            ("harbor", "registry/harbor"),
        ] {
            let c = cluster(&base(&format!("kind = \"{kind}\"")));
            for component in reg.iter() {
                if component.kind() != ComponentKind::Registry {
                    continue;
                }
                assert_eq!(
                    component.enabled(&c),
                    component.id() == id,
                    "{kind} x {}",
                    component.id()
                );
            }
        }
    }

    #[test]
    fn a_configured_registry_without_its_node_is_rejected() {
        let toml = base("kind = \"zot\"").replace(
            "\n[nodes.reg1]\naddress = \"10.99.0.31\"\nrole = \"registry\"\n",
            "\n",
        );
        assert_eq!(
            first_finding(&toml),
            Some(
                "invalid configuration: [registry] is configured but no node has role = \"registry\""
                    .to_string()
            )
        );
    }

    #[test]
    fn credentials_must_come_in_pairs() {
        assert_eq!(
            first_finding(&base("kind = \"zot\"\nusername = \"kurin\"")),
            Some(
                "invalid configuration: [registry] username and password must be set together"
                    .to_string()
            )
        );
    }

    #[test]
    fn harbor_needs_a_single_node_and_the_admin_account() {
        let two_nodes = base("kind = \"harbor\"")
            + "\n[nodes.reg2]\naddress = \"10.99.0.32\"\nrole = \"registry\"\n";
        assert_eq!(
            first_finding(&two_nodes),
            Some(
                "invalid configuration: [registry] kind = \"harbor\" supports exactly one registry node (no shared storage)"
                    .to_string()
            )
        );
        assert_eq!(
            first_finding(&base(
                "kind = \"harbor\"\nusername = \"ops\"\npassword = \"fake\""
            )),
            Some(
                "invalid configuration: [registry] kind = \"harbor\" can only authenticate as \"admin\" (its single admin account)"
                    .to_string()
            )
        );
        assert_eq!(
            first_finding(&base(
                "kind = \"harbor\"\nusername = \"admin\"\npassword = \"fake\""
            )),
            None
        );
    }
    #[test]
    fn the_registry_sweep_lands_in_the_registry_phase() {
        // Node consultation order is the config's (alphabetical) node
        // order: cp1, reg1, w1 — install interleaves with the trusts,
        // matching the frozen registry-zot snapshot's layout.
        let plan = crate::test_util::plan_for(&cluster(&base("kind = \"zot\"")));
        assert_eq!(
            crate::test_util::phase_ids(&plan, "registry"),
            [
                "registry/trust@cp1",
                "registry/install@reg1",
                "registry/trust@w1",
                "registry/seed@cp1",
            ]
        );
    }

    #[test]
    fn k3s_track_plans_no_registry_tasks() {
        let toml = base("kind = \"zot\"")
            .replace(
                "version = \"1.33.1\"",
                "version = \"1.33.1\"\ndistribution = \"k3s\"",
            )
            .replace("kind = \"calico\"", "kind = \"flannel\"");
        let c = cluster(&toml);
        let plan = crate::test_util::plan_for(&c);
        assert!(crate::test_util::phase_ids(&plan, "registry").is_empty());
    }
}
