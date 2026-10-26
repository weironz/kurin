//! kurin library: everything the CLI and future API layer calls.

// `result_large_err` is a performance hint, not a correctness rule. Our
// miette-based error carries file/span context required for rustc-style
// diagnostics; boxing every miette diagnostic for this lint would churn
// every error constructor without user value. Allowed project-wide in one
// conspicuous place instead of per-fn.
#![allow(clippy::result_large_err)]

//! The thin binary in `main.rs` only parses argv and picks an output sink.

pub mod artifact;
pub mod engine;
pub mod executor;
pub mod plan;
pub mod render;
pub mod ssh;
pub mod state;
pub mod tasks;
pub mod teardown;
pub mod templates;
pub mod upgrade;
pub mod version;

pub use engine::{Drift, Event, ExecOutput, Outcome, Session, Stage, Task};
pub use executor::{run, SessionPool};
pub use plan::Plan;
pub use state::RunState;
