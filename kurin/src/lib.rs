//! kurin library facade: preserves the historical `kurin::...` paths for the
//! CLI and integration tests while the implementation lives in the split
//! workspace crates.
//!
//! The thin binary in `main.rs` only parses argv and picks an output sink.

// `result_large_err` is a performance hint, not a correctness rule. Our
// miette-based error carries file/span context required for rustc-style
// diagnostics; boxing every miette diagnostic for this lint would churn
// every error constructor without user value. Allowed project-wide in one
// conspicuous place instead of per-fn.
#![allow(clippy::result_large_err)]

pub mod arch {
    pub use kurin_core::arch::*;
}
pub mod artifact {
    pub use kurin_artifact::artifact::*;
}
pub mod artifact_bundle {
    pub use kurin_artifact::artifact_bundle::*;
}
pub mod blackboard {
    pub use kurin_core::blackboard::*;
}
pub mod certs {
    pub use kurin_components::certs::*;
}
pub mod components {
    pub use kurin_components::component::*;
}
pub mod discover {
    pub use kurin_components::discover::*;
}
pub mod dynamic_manifest {
    pub use kurin_artifact::dynamic_manifest::*;
}
pub mod engine {
    pub use kurin_core::engine::*;
}
pub mod events {
    pub use kurin_core::events::*;
}
pub mod executor {
    pub use kurin_core::executor::*;
}
pub mod facts {
    pub use kurin_core::facts::*;
}
pub mod lock {
    pub use kurin_core::lock::*;
}
pub mod ops {
    pub use kurin_ops::*;
}
pub mod plan {
    pub use kurin_components::plan::{build, build_delete, build_with_pool};
    pub use kurin_core::plan::{
        NodeSel, OpKind, Phase, PhaseHandle, Plan, PlanBuilder, StageNode, TaskIdx,
    };
}
pub mod precheck {
    pub use kurin_components::precheck::*;
}
pub mod render {
    pub use kurin_core::render::*;
}
pub mod run {
    pub use kurin_core::run::*;
}
pub mod ssh {
    pub use kurin_ssh::*;
}
pub mod state {
    pub use kurin_core::state::*;
}
pub mod status {
    pub use kurin_components::status::*;
}
pub mod tasks {
    pub use kurin_components::tasks::*;
}
pub mod teardown {
    pub use kurin_components::teardown::*;
}
pub mod templates {
    pub use kurin_components::templates::*;
}
pub mod upgrade {
    pub use kurin_components::upgrade::*;
}
pub mod version {
    pub use kurin_artifact::version::*;
}
pub mod zone {
    pub use kurin_artifact::zone::*;
}

pub use kurin_components::plan::{build, build_with_pool};
pub use kurin_core::engine::{Drift, Event, ExecOutput, Outcome, Session, Stage, Task};
pub use kurin_core::executor::{run, SessionPool};
pub use kurin_core::plan::Plan;
pub use kurin_core::state::RunState;
