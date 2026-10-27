//! Kurin engine core: task/session traits, plan data model, DAG executor,
//! run state and terminal rendering. No transport or component knowledge.

// `result_large_err` is a performance hint, not a correctness rule. Our
// miette-based error carries file/span context required for rustc-style
// diagnostics; boxing every miette diagnostic for this lint would churn
// every error constructor without user value. Allowed project-wide in one
// conspicuous place instead of per-fn.
#![allow(clippy::result_large_err)]

pub mod engine;
pub mod executor;
pub mod plan;
pub mod render;
pub mod state;
