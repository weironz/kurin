//! CLI modules shared by the binary and its integration tests.
//!
//! `main.rs` stays a thin argv parser; the argument structures that tests
//! also exercise live here so both see the same surface.

pub mod quick;

pub mod addon;

pub mod ssh_key;
