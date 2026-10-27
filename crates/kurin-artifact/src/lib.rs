//! Kurin artifact handling: pinned manifest, local cache, offline bundles,
//! dynamic manifest resolution and zone mirror rewriting.

#![allow(clippy::result_large_err)]

pub mod artifact;
pub mod artifact_bundle;
pub mod deploy;
pub mod dynamic_manifest;
pub mod oci;
pub mod os;
pub mod package;
pub mod preload;
pub mod referrer;
pub mod signature;
pub mod store;
pub mod transfer;
pub mod version;
pub mod zone;
