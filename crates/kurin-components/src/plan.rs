//! Planner: Cluster + facts -> task DAG. Zero I/O by contract (template
//! rendering is in-memory; artifact hashes come from the version table).
//!
//! The plan shape lives in [`kurin_core::plan`]; this module assembles it
//! from the built-in components. WP-15: the os, runtime and distro areas
//! contribute through the [`crate::component`] registry — the build
//! iterates every enabled component and calls `plan()`; their per-node
//! tracks carry explicit positions ([`crate::component::tracks`]) so the
//! flat loop reproduces the node-major layout the frozen snapshots pin.
//! The not-yet-migrated areas (ha, cni, registry, storage, post) stay
//! as the `plan_<area>` functions below until WP-16.
//!
//! Facts come from discovery (or [`FactsTable::synthetic`] for
//! `--dry-run`): every artifact-resolving task picks its node's arch
//! from them, which is what makes mixed-architecture clusters work.

use std::sync::Arc;

use kurin_config::{Cluster, Distribution, HaMode, NodeRole};
use kurin_core::plan::{OpKind, Plan, PlanBuilder};
use kurin_core::run::RunContext;
use miette::Result;

use crate::component::{registry, tracks};
use crate::discover::Discover;
use crate::tasks;

/// Build the create/add-nodes plan (no live session pool).
pub fn build(cluster: &Cluster, run: &Arc<RunContext>) -> Result<Plan> {
    build_with_pool(cluster, None, run)
}

/// Same as [`build`], but registers tasks that need live sessions
/// (the post-install summary probes certificates).
pub fn build_with_pool(
    cluster: &Cluster,
    pool: Option<Arc<dyn kurin_core::executor::SessionPool>>,
    run: &Arc<RunContext>,
) -> Result<Plan> {
    // Facts are discovered before planning and live in the run context;
    // snapshot them so the plan build never holds the lock.
    let facts = run.facts_snapshot();
    let mut b = PlanBuilder::new(OpKind::Create, cluster, facts, run, pool);

    plan_discover(&mut b)?;

    // Every enabled component contributes its tasks (WP-15: os, runtime,
    // distro). Inactive components add nothing; k3s clusters disable the
    // kubeadm track and the standalone runtimes, and vice versa.
    for component in registry().iter() {
        if component.enabled(cluster) {
            component.plan(&mut b)?;
        }
    }

    // The k3s track historically stops after the distribution's task
    // set: the areas below are kubeadm-track contributions (k3s embeds
    // or omits them — its own kube-vip and external-LB tasks come from
    // the k3s component above).
    if cluster.distribution == Distribution::K3s {
        return Ok(b.finish());
    }

    // Not yet migrated to components (WP-16): registry, ha, cni, storage, post.
    plan_registry(&mut b)?;
    plan_ha(&mut b)?;
    plan_cni(&mut b)?;
    plan_storage(&mut b)?;
    plan_post(&mut b);

    Ok(b.finish())
}

/// Discover: one display-only task per node recording what the pre-plan
/// discovery probed (arch/OS/CPU/mem).
fn plan_discover(b: &mut PlanBuilder<'_>) -> Result<()> {
    let phase = b.phase("discover");
    for node in &b.cluster().nodes {
        let task = {
            let facts_for_node = b
                .facts()
                .get(&node.name)
                .ok_or_else(|| miette::miette!("no discovered facts for node {}", node.name))?;
            Arc::new(Discover::new(node, facts_for_node)) as Arc<dyn kurin_core::engine::Task>
        };
        b.add(phase, task);
    }
    Ok(())
}

/// Registry install/trust/seed when a registry node exists; trust+seed run
/// on Kubernetes members only.
fn plan_registry(b: &mut PlanBuilder<'_>) -> Result<()> {
    if b.cluster().registry_kind.is_none() {
        return Ok(());
    }
    // ONE CA per run, shared by install/trust/seed (each task minting its
    // own CA wrote a different trust anchor than the registry's, and every
    // TLS client failed verification).
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

/// HA track: kube-vip static pod on every control-plane node (VIP only),
/// or the external LB pair for `ha.mode = kurin-lb`. The kube-vip tasks
/// carry the per-node track position so they precede the distribution's
/// init task inside the `vip` phase.
fn plan_ha(b: &mut PlanBuilder<'_>) -> Result<()> {
    match b.cluster().ha_mode {
        HaMode::KubeVip => {
            for node in b.nodes().control_plane().collect::<Vec<_>>() {
                if let Some(kubevip) = tasks::bootstrap::KubeVip::new(b.cluster(), node, b.run()) {
                    let phase = b.phase("vip");
                    b.add_track(phase, Arc::new(kubevip), tracks::KUBE_VIP);
                }
            }
        }
        HaMode::KurinLb => {
            for (ordinal, lb) in b.nodes().role(NodeRole::Lb).enumerate() {
                let task = tasks::external_lb::ExternalLb::new(b.cluster(), lb, ordinal)?;
                let phase = b.phase("images");
                b.add(phase, Arc::new(task));
            }
        }
        HaMode::ExternalEndpoint => {}
    }
    Ok(())
}

/// CNI plugin (and the storage add-on, which shares the cni phase).
fn plan_cni(b: &mut PlanBuilder<'_>) -> Result<()> {
    let task = tasks::cni::plugin_task(b.cluster(), b.facts(), b.run())?;
    let phase = b.phase("cni");
    b.add(phase, task);
    Ok(())
}

/// Dynamic storage provisioning, when configured.
fn plan_storage(b: &mut PlanBuilder<'_>) -> Result<()> {
    if b.cluster().storage_kind.is_some() {
        let task = tasks::storage::StorageInstall::new(b.cluster(), b.run())?;
        let phase = b.phase("cni");
        b.add(phase, Arc::new(task));
    }
    Ok(())
}

/// Post install: kubeconfig pull, optional summary probes and config-declared
/// node labels (kubeadm ignores nodeRegistration.labels since v1beta4).
fn plan_post(b: &mut PlanBuilder<'_>) {
    let phase = b.phase("post");
    b.add(
        phase,
        Arc::new(tasks::post::KubeconfigPull::new(b.cluster())),
    );
    if let Some(pool) = b.pool() {
        b.add(
            phase,
            Arc::new(tasks::post::Summary::new(b.cluster(), pool)),
        );
    }
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
}
