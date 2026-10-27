//! Kurin SSH transport: russh-backed [`Session`](kurin_core::engine::Session)
//! implementation and its session pool.

#![allow(clippy::result_large_err)]

mod ssh;

pub use ssh::*;
