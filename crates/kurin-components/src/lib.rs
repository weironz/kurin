//! Kurin built-in components and the planner that assembles them into a plan.

#![allow(clippy::result_large_err)]

pub mod certs;
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
}
