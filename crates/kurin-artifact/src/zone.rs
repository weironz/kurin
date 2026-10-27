//! Domestic (China) mirror zone: URL rewriting for artifact downloads.
//!
//! The zone now lives in [`kurin_core::zone`] (it is per-run state, not a
//! process global); this module re-exports it so the historical
//! `kurin_artifact::zone::…` paths keep compiling.

pub use kurin_core::zone::*;
