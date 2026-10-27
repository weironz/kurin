//! `ha/*`: the API-endpoint HA modes, one component per `[ha] mode`.
//! The kubeadm track's tasks live here; the k3s flavors of kube-vip and
//! the external LB stay inside `distro/k3s`'s task set (the historical
//! k3s branch stopped after the distribution's own tasks), so every
//! `plan` below contributes nothing on the k3s track while `enabled`
//! and `validate` stay mode-based — the rules apply to both tracks,
//! exactly where they lived in `kurin-config`'s validate.

use std::sync::Arc;

use kurin_config::{Cluster, Distribution, HaMode, NodeRole};
use kurin_core::arch::Arch;
use kurin_core::plan::{OpKind, PlanBuilder};
use miette::Result;

use super::{
    tracks, ArtifactRef, Capabilities, Component, ComponentKind, Diagnostics, Offline, ALL_ZONES,
};
use crate::tasks;

/// `ha/kube-vip`: the control-plane VIP as a kubelet static pod on every
/// control-plane node. On the kubeadm track the tasks carry the
/// per-node track position so they precede the distribution's init task
/// inside the `vip` phase.
pub struct KubeVip;

impl Component for KubeVip {
    fn id(&self) -> &'static str {
        "ha/kube-vip"
    }

    fn kind(&self) -> ComponentKind {
        ComponentKind::Ha
    }

    fn enabled(&self, spec: &Cluster) -> bool {
        spec.ha_mode == HaMode::KubeVip
    }

    fn validate(&self, spec: &Cluster, d: &mut Diagnostics) {
        // Moved from kurin-config's validate (WP-16); wording and rule
        // order unchanged — diagnostic snapshots guard them.
        if spec.nodes.iter().any(|n| n.role == NodeRole::Lb) {
            d.error("role = \"lb\" nodes are only valid with ha.mode = \"kurin-lb\"");
        }
        let cp = spec.control_plane().count();
        match (cp, spec.vip) {
            (1, Some(_)) => {
                d.error("ha.vip is set but only one control-plane node exists");
            }
            (n, None) if n > 1 => {
                // Allowed: stacked control plane; the first master is
                // the API SPOF. The CLI warns on join.
            }
            (_, Some(vip))
                if spec
                    .nodes
                    .iter()
                    .any(|n| n.address == vip || n.internal_address == vip) =>
            {
                d.error("ha.vip must not collide with any node address");
            }
            _ => {}
        }
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            distributions: &[Distribution::K8s, Distribution::K3s],
            archs: &[Arch::Amd64, Arch::Arm64],
            // The static pod's image is pinned in the version table but
            // pulled by kubelet at runtime; it is not manifest-managed,
            // so offline bundles do not carry it.
            offline: Offline::OnlineOnly,
            zones: ALL_ZONES,
            ops: &[OpKind::Create],
        }
    }

    fn artifacts(&self, _spec: &Cluster, _arch: Arch) -> Vec<ArtifactRef> {
        // See `capabilities`: the kube-vip image ref is pinned in
        // `kurin-artifact`'s version table and referenced directly by
        // the rendered static-pod manifest — it has no manifest entry.
        Vec::new()
    }

    fn plan(&self, b: &mut PlanBuilder<'_>) -> Result<()> {
        // The k3s track ships its own kube-vip flavor inside distro/k3s's
        // task set; this is the kubeadm track's static-pod task.
        if b.cluster().distribution != Distribution::K8s {
            return Ok(());
        }
        for node in b.nodes().control_plane().collect::<Vec<_>>() {
            if let Some(kubevip) = tasks::bootstrap::KubeVip::new(b.cluster(), node, b.run()) {
                let phase = b.phase("vip");
                b.add_track(phase, Arc::new(kubevip), tracks::KUBE_VIP);
            }
        }
        Ok(())
    }
}

/// `ha/kurin-lb`: kurin-managed HA — haproxy + keepalived on the `lb`
/// nodes fronting every control plane's 6443.
pub struct KurinLb;

impl Component for KurinLb {
    fn id(&self) -> &'static str {
        "ha/kurin-lb"
    }

    fn kind(&self) -> ComponentKind {
        ComponentKind::Ha
    }

    fn enabled(&self, spec: &Cluster) -> bool {
        spec.ha_mode == HaMode::KurinLb
    }

    fn validate(&self, spec: &Cluster, d: &mut Diagnostics) {
        // Moved from kurin-config's validate (WP-16); wording and rule
        // order unchanged.
        let cp = spec.control_plane().count();
        if cp < 2 {
            d.error("ha.mode = \"kurin-lb\" needs >= 2 control-plane nodes");
        }
        let lb_nodes = spec.nodes.iter().filter(|n| n.role == NodeRole::Lb).count();
        if lb_nodes == 0 {
            d.error(
                "ha.mode = \"kurin-lb\" requires at least one [nodes.*] entry with role = \"lb\"",
            );
        }
        if spec.vip.is_none() {
            d.error("ha.mode = \"kurin-lb\" requires ha.vip for the keepalived VIP");
        }
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            distributions: &[Distribution::K8s, Distribution::K3s],
            archs: &[Arch::Amd64, Arch::Arm64],
            // haproxy/keepalived are distro packages (apt/dnf), the
            // documented escape hatch for OS-provided daemons.
            offline: Offline::OnlineOnly,
            zones: ALL_ZONES,
            ops: &[OpKind::Create],
        }
    }

    fn artifacts(&self, _spec: &Cluster, _arch: Arch) -> Vec<ArtifactRef> {
        // See `capabilities`: the daemons come from the distro's own
        // package repositories, not the version table.
        Vec::new()
    }

    fn plan(&self, b: &mut PlanBuilder<'_>) -> Result<()> {
        // The k3s track plans the same distribution-agnostic ExternalLb
        // task inside its own task set (vip stage); this is the kubeadm
        // track's placement (images stage).
        if b.cluster().distribution != Distribution::K8s {
            return Ok(());
        }
        for (ordinal, lb) in b.nodes().role(NodeRole::Lb).enumerate() {
            let task = tasks::external_lb::ExternalLb::new(b.cluster(), lb, ordinal)?;
            let phase = b.phase("images");
            b.add(phase, Arc::new(task));
        }
        Ok(())
    }
}

/// `ha/external-endpoint`: the operator's own load balancer. kurin
/// installs nothing; the plan only points the API endpoint at `ha.vip`.
pub struct External;

impl Component for External {
    fn id(&self) -> &'static str {
        "ha/external"
    }

    fn kind(&self) -> ComponentKind {
        ComponentKind::Ha
    }

    fn enabled(&self, spec: &Cluster) -> bool {
        spec.ha_mode == HaMode::ExternalEndpoint
    }

    fn validate(&self, spec: &Cluster, d: &mut Diagnostics) {
        // Moved from kurin-config's validate (WP-16); wording and rule
        // order unchanged. The lb-role rejection runs first, matching
        // its position ahead of the mode-specific rules there.
        if spec.nodes.iter().any(|n| n.role == NodeRole::Lb) {
            d.error("role = \"lb\" nodes are only valid with ha.mode = \"kurin-lb\"");
        }
        if spec.control_plane().count() < 2 {
            d.error("ha.mode = \"external-endpoint\" needs >= 2 control-plane nodes");
        }
        if spec.vip.is_none() {
            d.error("ha.mode = \"external-endpoint\" requires ha.vip = <your LB address>");
        }
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            distributions: &[Distribution::K8s, Distribution::K3s],
            archs: &[Arch::Amd64, Arch::Arm64],
            // The endpoint is an address, not an install: nothing beyond
            // the base track is needed offline.
            offline: Offline::Bundle,
            zones: ALL_ZONES,
            ops: &[OpKind::Create],
        }
    }

    fn artifacts(&self, _spec: &Cluster, _arch: Arch) -> Vec<ArtifactRef> {
        Vec::new()
    }

    fn plan(&self, _b: &mut PlanBuilder<'_>) -> Result<()> {
        // Nothing to converge: both distributions only read `ha.vip`
        // when they render their API endpoint (kubeadm's init config and
        // the k3s kubeconfig rewrite respectively).
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cluster(toml: &str) -> Cluster {
        kurin_config::parse_str(std::path::Path::new("ha.toml"), toml).expect("cluster parses")
    }

    /// The base matrix: two control planes + one worker, `[ha]` appended
    /// per case.
    fn base(ha: &str) -> String {
        format!(
            r#"version = "v1alpha1"
name = "hacomp"
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
[ha]
{ha}
[nodes.cp1]
address = "10.99.0.11"
role = "control-plane"
[nodes.cp2]
address = "10.99.0.12"
role = "control-plane"
[nodes.w1]
address = "10.99.0.21"
role = "worker"
"#
        )
    }

    /// The same shape with a single control plane (cp2 demoted).
    fn single_cp(ha: &str) -> String {
        base(ha).replace(
            "[nodes.cp2]\naddress = \"10.99.0.12\"\nrole = \"control-plane\"",
            "[nodes.cp2]\naddress = \"10.99.0.12\"\nrole = \"worker\"",
        )
    }

    /// First validation finding of the enabled ha component, rendered
    /// like kurin-config would (the error text is frozen).
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
    fn kube_vip_rejects_a_vip_on_a_single_control_plane() {
        assert_eq!(
            first_finding(&single_cp("vip = \"10.99.0.100\"")),
            Some(
                "invalid configuration: ha.vip is set but only one control-plane node exists"
                    .to_string()
            )
        );
    }

    #[test]
    fn kube_vip_rejects_a_vip_colliding_with_a_node_address() {
        assert_eq!(
            first_finding(&base("vip = \"10.99.0.11\"")),
            Some(
                "invalid configuration: ha.vip must not collide with any node address".to_string()
            )
        );
    }

    #[test]
    fn stacked_control_plane_without_vip_is_allowed() {
        assert_eq!(first_finding(&base("")), None);
    }

    #[test]
    fn external_endpoint_needs_two_control_planes_and_a_vip() {
        assert_eq!(
            first_finding(&single_cp("mode = \"external-endpoint\"")),
            Some(
                "invalid configuration: ha.mode = \"external-endpoint\" needs >= 2 control-plane nodes"
                    .to_string()
            )
        );
        assert_eq!(
            first_finding(&base("mode = \"external-endpoint\"")),
            Some(
                "invalid configuration: ha.mode = \"external-endpoint\" requires ha.vip = <your LB address>"
                    .to_string()
            )
        );
    }

    #[test]
    fn kurin_lb_needs_two_control_planes_an_lb_node_and_a_vip() {
        assert_eq!(
            first_finding(&base("mode = \"kurin-lb\"\nvip = \"10.99.0.100\"")),
            Some(
                "invalid configuration: ha.mode = \"kurin-lb\" requires at least one [nodes.*] entry with role = \"lb\""
                    .to_string()
            )
        );
        let with_lb = base("mode = \"kurin-lb\"\nvip = \"10.99.0.100\"")
            + "\n[nodes.lb1]\naddress = \"10.99.0.31\"\nrole = \"lb\"\n";
        assert_eq!(first_finding(&with_lb), None);
        let no_vip = base("mode = \"kurin-lb\"")
            + "\n[nodes.lb1]\naddress = \"10.99.0.31\"\nrole = \"lb\"\n";
        assert_eq!(
            first_finding(&no_vip),
            Some(
                "invalid configuration: ha.mode = \"kurin-lb\" requires ha.vip for the keepalived VIP"
                    .to_string()
            )
        );
    }

    #[test]
    fn lb_nodes_are_rejected_outside_kurin_lb_mode() {
        let toml = base("vip = \"10.99.0.100\"")
            + "\n[nodes.lb1]\naddress = \"10.99.0.31\"\nrole = \"lb\"\n";
        assert_eq!(
            first_finding(&toml),
            Some(
                "invalid configuration: role = \"lb\" nodes are only valid with ha.mode = \"kurin-lb\""
                    .to_string()
            )
        );
    }

    #[test]
    fn kube_vip_tasks_precede_the_init_task_in_the_vip_phase() {
        let plan = crate::test_util::plan_for(&cluster(&base("vip = \"10.99.0.100\"")));
        assert_eq!(
            crate::test_util::phase_ids(&plan, "vip"),
            [
                "bootstrap/kube-vip@cp1",
                "bootstrap/kube-vip@cp2",
                "bootstrap/kubeadm-init@cp1",
            ]
        );
    }

    #[test]
    fn kurin_lb_tasks_land_after_the_image_imports() {
        let toml = base("mode = \"kurin-lb\"\nvip = \"10.99.0.100\"")
            + "\n[nodes.lb1]\naddress = \"10.99.0.31\"\nrole = \"lb\"\n";
        let plan = crate::test_util::plan_for(&cluster(&toml));
        assert_eq!(
            crate::test_util::phase_ids(&plan, "images"),
            [
                "images/import@cp1",
                "images/import@cp2",
                "images/import@w1",
                "vip/external-lb@lb1",
            ]
        );
    }

    #[test]
    fn k3s_track_gets_no_kubeadm_ha_tasks() {
        // kube-vip mode on k3s: its own flavor comes from the k3s task
        // set, never from these components.
        let toml = base("mode = \"kube-vip\"\nvip = \"10.99.0.100\"").replace(
            "version = \"1.33.1\"",
            "version = \"1.33.1\"\ndistribution = \"k3s\"",
        ) + "\n[network.cni]\nkind = \"flannel\"\n";
        let c = cluster(&toml);
        let plan = crate::test_util::plan_for(&c);
        assert!(!crate::test_util::phase_ids(&plan, "vip")
            .iter()
            .any(|id| id.starts_with("bootstrap/kube-vip")));
        assert!(crate::test_util::phase_ids(&plan, "images")
            .iter()
            .all(|id| !id.starts_with("vip/external-lb")));
    }
}
