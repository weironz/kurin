//! Operation execution for the Web server (WP-52).
//!
//! [`OpRunner`] schedules persisted operations and owns their buses; the
//! backend seam lets tests run deterministic operations without SSH.

pub mod backend;
pub mod logs;
pub mod runner;
pub mod sink;
pub mod tracing;

pub use backend::{KurinBackend, OpContext, OpExecution, OperationBackend};
pub use logs::{LogFilter, LogPage, LogStore, DEFAULT_LIMIT, MAX_LIMIT};
pub use runner::{new_operation_id, OpRunner, SubmitRequest, DEFAULT_SSE_CAPACITY};
pub use sink::EventStoreSink;
pub use tracing::{BusRegistry, RouterLayer};
