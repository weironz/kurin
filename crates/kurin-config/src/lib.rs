//! kurin-config: declarative, versioned cluster configuration.

// See kurin's lib.rs for the project-wide `clippy::result_large_err` policy:
// miette diagnostics need their spans; boxing them away buys nothing.
#![allow(clippy::result_large_err)]

//!
//! Pipeline (`load`): parse (toml/yaml by extension) -> version gate ->
//! defaults -> lower to internal `Cluster` -> business validation.
//! The executor only ever sees `Cluster`; raw config fields where defaults
//! live never leak past `load`.

pub mod config;
pub mod error;
pub mod model;
pub mod quick;

use std::path::Path;

pub use config::{
    ArtifactsConfig, BecomeMode, ClusterConfig, CniConfig, Distribution, HaConfig, HaMode,
    KubernetesConfig, NetworkConfig, NodeConfig, NodeRole, RegistryConfig, RegistryKind,
    RuntimeKind, SshAuth, SshDefaults, StorageConfig, StorageKind, CONFIG_VERSION,
};
pub use error::{ConfigError, Result};
pub use model::{Cluster, Node, ResolvedSsh};

/// Load, validate, and lower a config file into the internal model.
pub fn load(path: &Path) -> Result<Cluster> {
    let src = std::fs::read_to_string(path).map_err(|e| ConfigError::Io {
        path: path.display().to_string(),
        message: e.to_string(),
    })?;
    parse_str(path, &src)
}

/// Same as [`load`] but from in-memory text (used by tests and planner).
pub fn parse_str(path: &Path, src: &str) -> Result<Cluster> {
    let raw = parse(path, src)?;
    model::check_version(&raw)?;
    let cfg = config::with_defaults(raw);
    let mut cluster = model::Cluster::from_config(cfg)?;
    cluster.source = src.to_string();
    cluster.validate()?;
    Ok(cluster)
}

/// Parse only, keeping span info for diagnostics.
pub fn parse(path: &Path, src: &str) -> Result<ClusterConfig> {
    match path.extension().and_then(|e| e.to_str()) {
        Some("toml") => toml::from_str(src).map_err(|e| ConfigError::toml(path, src, e)),
        Some("yml" | "yaml") => {
            serde_yml::from_str(src).map_err(|e| ConfigError::yaml(path, src, e))
        }
        other => Err(ConfigError::UnsupportedFormat {
            path: path.display().to_string(),
            extension: other.unwrap_or_default().to_string(),
        }),
    }
}
