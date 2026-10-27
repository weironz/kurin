//! Planner: Cluster + facts -> task DAG. Zero I/O by contract (template
//! rendering is in-memory; artifact hashes come from the version table).
//!
//! The plan shape lives in [`kurin_core::plan`]; this module assembles it
//! from the built-in components: the build iterates every enabled
//! component of the explicit [`crate::component`] registry and calls
//! `plan()`. Per-node tracks carry explicit positions
//! ([`crate::component::tracks`]) so the flat loop reproduces the
//! node-major layout the frozen snapshots pin, and each component gates
//! itself on the distributions it supports (the k3s track historically
//! stopped after the distribution's task set — the ha, cni, registry,
//! storage and post components contribute nothing there).
//!
//! Facts come from discovery (or [`FactsTable::synthetic`] for
//! `--dry-run`): every artifact-resolving task picks its node's arch
//! from them, which is what makes mixed-architecture clusters work.

use std::sync::Arc;

use kurin_config::Cluster;
use kurin_core::plan::{OpKind, Plan, PlanBuilder};
use kurin_core::run::RunContext;
use miette::Result;

use crate::component::registry;
use crate::discover::Discover;

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
    build_with_pool_for(OpKind::Create, cluster, pool, run)
}

/// [`build_with_pool`] with the operation kind made explicit. `add nodes`
/// shares the create plan shape but records itself as `add-nodes` in
/// `kurin-state` (ADR-09).
pub fn build_with_pool_for(
    op: OpKind,
    cluster: &Cluster,
    pool: Option<Arc<dyn kurin_core::executor::SessionPool>>,
    run: &Arc<RunContext>,
) -> Result<Plan> {
    // Facts are discovered before planning and live in the run context;
    // snapshot them so the plan build never holds the lock.
    let facts = run.facts_snapshot();
    let mut b = PlanBuilder::new(op, cluster, facts, run, pool);

    plan_discover(&mut b)?;

    // Every enabled component contributes its tasks. Inactive
    // components add nothing; k3s clusters disable the kubeadm track and
    // the standalone runtimes, and vice versa.
    for component in registry().iter() {
        if component.enabled(cluster) {
            component.plan(&mut b)?;
        }
    }

    Ok(b.finish())
}

/// Build the teardown plan: `target = Some(node)` plans `delete node`,
/// `None` plans `delete cluster`. Teardown needs no facts (nothing is
/// resolved per arch) and no session pool: the API-side tasks run on the
/// anchor control plane, which is their [`Task::node`](kurin_core::engine::Task::node).
pub fn build_delete(
    cluster: &Cluster,
    run: &Arc<RunContext>,
    target: Option<&kurin_config::Node>,
) -> Result<Plan> {
    crate::teardown::plan(cluster, run, target)
}

/// Discover: one display-only task per node recording what the pre-plan
/// discovery probed (arch/OS/CPU/mem).
pub(crate) fn plan_discover(b: &mut PlanBuilder<'_>) -> Result<()> {
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
