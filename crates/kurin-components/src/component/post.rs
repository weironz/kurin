//! `post/*`: the kubeadm track's post-install reporting — the kubeconfig
//! fetch, the optional certificate summary and the config-declared node
//! labels. The k3s track has its own kubeconfig task inside `distro/k3s`
//! and never planned the rest (the historical k3s branch shape), so
//! these components are kubeadm-only by `enabled`.

use std::sync::Arc;

use kurin_config::{Cluster, Distribution};
use kurin_core::arch::Arch;
use kurin_core::plan::{OpKind, PlanBuilder};
use miette::Result;

use super::{ArtifactRef, Capabilities, Component, ComponentKind, Diagnostics, Offline, ALL_ZONES};
use crate::tasks;

/// `post/kubeconfig`: fetch admin.conf and merge it into the operator's
/// local kubeconfig under context `<cluster-name>`.
pub struct Kubeconfig;

impl Component for Kubeconfig {
    fn id(&self) -> &'static str {
        "post/kubeconfig"
    }

    fn kind(&self) -> ComponentKind {
        ComponentKind::Post
    }

    fn enabled(&self, spec: &Cluster) -> bool {
        spec.distribution == Distribution::K8s
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

    fn artifacts(&self, _spec: &Cluster, _arch: Arch) -> Vec<ArtifactRef> {
        Vec::new()
    }

    fn plan(&self, b: &mut PlanBuilder<'_>) -> Result<()> {
        let phase = b.phase("post");
        b.add(
            phase,
            Arc::new(tasks::post::KubeconfigPull::new(b.cluster())),
        );
        Ok(())
    }
}

/// `post/summary`: the certificate-expiry summary probed through the live
/// session pool (only planned when the caller provided one).
pub struct Summary;

impl Component for Summary {
    fn id(&self) -> &'static str {
        "post/summary"
    }

    fn kind(&self) -> ComponentKind {
        ComponentKind::Post
    }

    fn enabled(&self, spec: &Cluster) -> bool {
        spec.distribution == Distribution::K8s
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

    fn artifacts(&self, _spec: &Cluster, _arch: Arch) -> Vec<ArtifactRef> {
        Vec::new()
    }

    fn plan(&self, b: &mut PlanBuilder<'_>) -> Result<()> {
        if let Some(pool) = b.pool() {
            let phase = b.phase("post");
            b.add(
                phase,
                Arc::new(tasks::post::Summary::new(b.cluster(), pool)),
            );
        }
        Ok(())
    }
}

/// `post/labels`: config-declared node labels, converged through the API
/// from the first control plane (kubeadm ignores
/// nodeRegistration.labels since v1beta4).
pub struct Labels;

impl Component for Labels {
    fn id(&self) -> &'static str {
        "post/labels"
    }

    fn kind(&self) -> ComponentKind {
        ComponentKind::Post
    }

    fn enabled(&self, spec: &Cluster) -> bool {
        spec.distribution == Distribution::K8s
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

    fn artifacts(&self, _spec: &Cluster, _arch: Arch) -> Vec<ArtifactRef> {
        Vec::new()
    }

    fn plan(&self, b: &mut PlanBuilder<'_>) -> Result<()> {
        let phase = b.phase("post");
        for node in b
            .nodes()
            .k8s()
            .filter(|n| !n.labels.is_empty())
            .collect::<Vec<_>>()
        {
            b.add(
                phase,
                Arc::new(tasks::post::NodeLabels::new(b.cluster(), node)),
            );
        }
        Ok(())
    }
}

/// `post/state-record`: write `kube-system/ConfigMap kurin-state` (ADR-09)
/// at the end of every mutation so the cluster remembers what kurin
/// installed. Runs on both distributions.
pub struct StateRecord;

impl Component for StateRecord {
    fn id(&self) -> &'static str {
        "post/state-record"
    }

    fn kind(&self) -> ComponentKind {
        ComponentKind::Post
    }

    fn enabled(&self, _spec: &Cluster) -> bool {
        true
    }

    fn validate(&self, _spec: &Cluster, _d: &mut Diagnostics) {}

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            distributions: &[Distribution::K8s, Distribution::K3s],
            archs: &[Arch::Amd64, Arch::Arm64],
            offline: Offline::Bundle,
            zones: ALL_ZONES,
            ops: &[OpKind::Create, OpKind::AddNodes],
        }
    }

    fn artifacts(&self, _spec: &Cluster, _arch: Arch) -> Vec<ArtifactRef> {
        Vec::new()
    }

    fn plan(&self, b: &mut PlanBuilder<'_>) -> Result<()> {
        let phase = b.phase("post");
        b.add(
            phase,
            Arc::new(tasks::post::StateRecord::new(
                b.cluster(),
                b.run().op_id.clone(),
                b.op().as_str(),
            )),
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cluster(toml: &str) -> Cluster {
        kurin_config::parse_str(std::path::Path::new("post.toml"), toml).expect("cluster parses")
    }

    fn base(extra: &str) -> String {
        format!(
            r#"version = "v1alpha1"
name = "postcomp"
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
{extra}
[nodes.cp1]
address = "10.99.0.11"
role = "control-plane"
[nodes.w1]
address = "10.99.0.21"
role = "worker"
labels = {{"node-role.kurin.io/pool" = "general"}}
"#
        )
    }

    #[test]
    fn post_components_are_kubeadm_only() {
        let reg = super::super::registry();
        let k8s = cluster(&base(""));
        let k3s = cluster(
            &base("")
                .replace(
                    "version = \"1.33.1\"",
                    "version = \"1.33.1\"\ndistribution = \"k3s\"",
                )
                .replace("kind = \"calico\"", "kind = \"flannel\""),
        );
        for id in ["post/kubeconfig", "post/summary", "post/labels"] {
            let component = reg.get(id).expect(id);
            assert!(component.enabled(&k8s), "{id} on k8s");
            assert!(!component.enabled(&k3s), "{id} on k3s");
        }
        // The state record is distribution-independent: k3s clusters
        // remember what kurin installed too.
        let record = reg.get("post/state-record").expect("post/state-record");
        assert!(record.enabled(&k8s));
        assert!(record.enabled(&k3s));
    }

    #[test]
    fn the_post_phase_keeps_its_order_without_a_pool() {
        let plan = crate::test_util::plan_for(&cluster(&base("")));
        assert_eq!(
            crate::test_util::phase_ids(&plan, "post"),
            [
                "post/kubeconfig@cp1",
                "post/labels@w1",
                "post/state-record@cp1"
            ]
        );
    }
}
