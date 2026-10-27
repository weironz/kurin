//! `storage/*`: the dynamic-provisioning storage plugins. First-layer
//! components for now — WP-47 moves them to the declarative addon layer.
//! The k3s track never planned them (the historical k3s branch stopped
//! after the distribution's task set), so `plan` contributes nothing
//! there.

use std::sync::Arc;

use kurin_config::{Cluster, Distribution, StorageKind};
use kurin_core::arch::Arch;
use kurin_core::plan::{OpKind, PlanBuilder};
use miette::Result;

use super::{ArtifactRef, Capabilities, Component, ComponentKind, Diagnostics, Offline, ALL_ZONES};
use crate::tasks;

/// `storage/local-path`: Rancher local-path-provisioner (hostPath under
/// `/opt/local-path-provisioner`); its helper pod uses busybox.
pub struct LocalPath;

impl Component for LocalPath {
    fn id(&self) -> &'static str {
        "storage/local-path"
    }

    fn kind(&self) -> ComponentKind {
        ComponentKind::Storage
    }

    fn enabled(&self, spec: &Cluster) -> bool {
        spec.storage_kind == Some(StorageKind::LocalPath)
    }

    fn validate(&self, _spec: &Cluster, _d: &mut Diagnostics) {
        // No local-path-specific rules: the storage section is validated
        // structurally in kurin-config.
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            distributions: &[Distribution::K8s],
            archs: &[Arch::Amd64, Arch::Arm64],
            // The provisioner images ship in the bundle, but the
            // bootstrap import set does not preload them yet (WP-47's
            // addon engine owns install-time preloading); on an
            // air-gapped cluster the provisioner pod cannot pull.
            offline: Offline::OnlineOnly,
            zones: ALL_ZONES,
            ops: &[OpKind::Create],
        }
    }

    fn artifacts(&self, _spec: &Cluster, arch: Arch) -> Vec<ArtifactRef> {
        vec![
            ArtifactRef::of("local-path-provisioner", arch),
            ArtifactRef::of("busybox", arch),
        ]
    }

    fn plan(&self, b: &mut PlanBuilder<'_>) -> Result<()> {
        if b.cluster().distribution != Distribution::K8s {
            return Ok(());
        }
        let task = tasks::storage::StorageInstall::new(b.cluster(), b.run())?;
        let phase = b.phase("cni");
        b.add(phase, Arc::new(task));
        Ok(())
    }
}

/// `storage/nfs`: kubernetes-sigs nfs-subdir-external-provisioner against
/// a user-provided server + export path.
pub struct Nfs;

impl Component for Nfs {
    fn id(&self) -> &'static str {
        "storage/nfs"
    }

    fn kind(&self) -> ComponentKind {
        ComponentKind::Storage
    }

    fn enabled(&self, spec: &Cluster) -> bool {
        spec.storage_kind == Some(StorageKind::Nfs)
    }

    fn validate(&self, spec: &Cluster, d: &mut Diagnostics) {
        // Moved from kurin-config's validate (WP-16); wording unchanged.
        if spec.storage_server.is_none() || spec.storage_path.is_none() {
            d.error("storage.kind = \"nfs\" requires storage.server and storage.path");
        }
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            distributions: &[Distribution::K8s],
            archs: &[Arch::Amd64, Arch::Arm64],
            // See `storage/local-path`: the image ships in the bundle
            // but is not preloaded by the bootstrap import set.
            offline: Offline::OnlineOnly,
            zones: ALL_ZONES,
            ops: &[OpKind::Create],
        }
    }

    fn artifacts(&self, _spec: &Cluster, arch: Arch) -> Vec<ArtifactRef> {
        vec![ArtifactRef::of("nfs-subdir-provisioner", arch)]
    }

    fn plan(&self, b: &mut PlanBuilder<'_>) -> Result<()> {
        if b.cluster().distribution != Distribution::K8s {
            return Ok(());
        }
        let task = tasks::storage::StorageInstall::new(b.cluster(), b.run())?;
        let phase = b.phase("cni");
        b.add(phase, Arc::new(task));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cluster(toml: &str) -> Cluster {
        kurin_config::parse_str(std::path::Path::new("sto.toml"), toml).expect("cluster parses")
    }

    fn base(storage: &str) -> String {
        format!(
            r#"version = "v1alpha1"
name = "stocomp"
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
[storage]
{storage}
[nodes.cp1]
address = "10.99.0.11"
role = "control-plane"
[nodes.w1]
address = "10.99.0.21"
role = "worker"
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
        for (kind, id) in [("local-path", "storage/local-path"), ("nfs", "storage/nfs")] {
            let c = cluster(&base(&format!("kind = \"{kind}\"")));
            for component in reg.iter() {
                if component.kind() != ComponentKind::Storage {
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
    fn nfs_needs_a_server_and_a_path_with_the_previous_error_text() {
        assert_eq!(
            first_finding(&base("kind = \"nfs\"")),
            Some(
                "invalid configuration: storage.kind = \"nfs\" requires storage.server and storage.path"
                    .to_string()
            )
        );
        assert_eq!(
            first_finding(&base(
                "kind = \"nfs\"\nserver = \"10.99.9.9\"\npath = \"/export/pv\""
            )),
            None
        );
    }

    #[test]
    fn the_storage_task_lands_in_the_cni_phase_after_the_plugin() {
        // Storage shares the cni phase with the plugin (historical
        // placement); the plugin task comes first.
        let plan = crate::test_util::plan_for(&cluster(&base("kind = \"local-path\"")));
        assert_eq!(
            crate::test_util::phase_ids(&plan, "cni"),
            ["cni/calico-operator@cp1", "storage/install@cp1"]
        );
    }

    #[test]
    fn k3s_track_plans_no_storage_task() {
        let toml = base("kind = \"local-path\"")
            .replace(
                "version = \"1.33.1\"",
                "version = \"1.33.1\"\ndistribution = \"k3s\"",
            )
            .replace("kind = \"calico\"", "kind = \"flannel\"");
        let c = cluster(&toml);
        let plan = crate::test_util::plan_for(&c);
        assert!(!crate::test_util::phase_ids(&plan, "cni")
            .iter()
            .any(|id| id.starts_with("storage/")));
    }
}
