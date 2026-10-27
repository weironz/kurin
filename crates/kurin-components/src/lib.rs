//! Kurin built-in components and the planner that assembles them into a plan.

#![allow(clippy::result_large_err)]

pub mod addon;
pub mod certs;
pub mod component;
pub mod discover;
pub mod plan;
pub mod precheck;
pub mod status;
pub mod tasks;
pub mod teardown;
pub mod templates;
pub mod upgrade;

/// Helpers shared by the crate's unit tests.
#[cfg(test)]
pub(crate) mod test_util {
    use std::sync::Arc;

    use kurin_config::Cluster;
    use kurin_core::facts::FactsTable;
    use kurin_core::plan::Plan;
    use kurin_core::run::{ArtifactSources, RunContext};

    /// A run context for tests: embedded manifest, global zone, in-memory
    /// blackboard, no persistence.
    pub(crate) fn run() -> Arc<RunContext> {
        Arc::new(RunContext::new(
            "test-op",
            "test-cluster",
            ArtifactSources::embedded(),
        ))
    }

    /// A run context for tests of `cluster`, with synthetic facts so the
    /// planner can resolve per-node arches.
    pub(crate) fn run_for(cluster: &Cluster) -> Arc<RunContext> {
        let facts = FactsTable::synthetic(cluster).expect("synthetic facts");
        Arc::new(
            RunContext::new("test-op", &cluster.name, ArtifactSources::embedded())
                .with_facts(facts),
        )
    }

    /// Build the create plan for `cluster` through the real planner.
    pub(crate) fn plan_for(cluster: &Cluster) -> Plan {
        let run = run_for(cluster);
        crate::plan::build(cluster, &run)
            .unwrap_or_else(|e| panic!("plan builds for {}: {e:?}", cluster.name))
    }

    /// The task ids of one phase, in plan order.
    pub(crate) fn phase_ids(plan: &Plan, phase: &str) -> Vec<String> {
        plan.phase_groups()
            .into_iter()
            .find(|g| g.phase.id == phase)
            .map(|g| g.tasks.iter().map(|t| t.task.id()).collect())
            .unwrap_or_default()
    }
}
