//! kurin library: everything the CLI and future API layer calls.
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
pub mod version;

pub use engine::{Drift, Event, ExecOutput, Outcome, Session, Stage, Task};
pub use executor::{run, SessionPool};
pub use plan::Plan;
pub use state::RunState;
