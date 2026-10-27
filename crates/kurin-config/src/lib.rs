//! kurin-config: declarative, versioned cluster configuration.

// See kurin's lib.rs for the project-wide `clippy::result_large_err` policy:
// miette diagnostics need their spans; boxing them away buys nothing.
#![allow(clippy::result_large_err)]

//!
//! Pipeline (`load`): parse (toml/yaml by extension) -> version dispatch ->
//! defaults -> lower to internal `ClusterSpec` -> business validation.
//! The executor only ever sees `ClusterSpec`; raw config fields where
//! defaults live never leak past `load`. Secret fields keep their
//! [`SecretRef`] until `kurin-ops::prepare` resolves them once.
//!
//! Two schema versions are accepted: `v1alpha1` (unchanged) and
//! `v1alpha2`; both lower to the same model. `kurin config migrate`
//! rewrites a v1alpha1 document as v1alpha2 while preserving comments.

pub mod config;
pub mod error;
pub mod migrate;
pub mod model;
pub mod quick;
pub mod schema;
pub mod secret;
pub mod v1alpha2;

use std::path::Path;

pub use config::{
    ArtifactsConfig, BecomeMode, ClusterConfig, CniConfig, Distribution, HaConfig, HaMode,
    HostKeyMode, KubernetesConfig, NetworkConfig, NodeConfig, NodeRole, RegistryConfig,
    RegistryKind, RuntimeKind, SshAuth, SshDefaults, StorageConfig, StorageKind, CONFIG_VERSION,
};
pub use error::{ConfigError, Result};
pub use model::{
    AddonSpec, Cluster, ClusterSpec, Node, NodeSpec, PackageSources, RegistrySpec, ResolvedAuth,
    ResolvedSecrets, ResolvedSsh, ResolvedSshSecrets, SshDefaultsSpec, StorageSpec,
};
pub use secret::SecretRef;
pub use v1alpha2::BUILTIN_ADDONS;

/// A parsed document of either supported schema version.
///
/// The version is decided by the `version` field, never by the file name
/// or extension; both variants lower to [`ClusterSpec`].
#[derive(Debug, Clone)]
pub enum AnyConfig {
    V1Alpha1(ClusterConfig),
    V1Alpha2(v1alpha2::ClusterConfig),
}

impl AnyConfig {
    /// Schema version string of the parsed document.
    pub fn version(&self) -> &str {
        match self {
            AnyConfig::V1Alpha1(cfg) => &cfg.version,
            AnyConfig::V1Alpha2(cfg) => &cfg.version,
        }
    }

    /// Lower to the internal model, applying documented defaults.
    pub fn into_spec(self) -> Result<ClusterSpec> {
        match self {
            AnyConfig::V1Alpha1(cfg) => {
                model::check_version(&cfg)?;
                ClusterSpec::from_config(config::with_defaults(cfg))
            }
            AnyConfig::V1Alpha2(cfg) => ClusterSpec::try_from(cfg),
        }
    }
}

/// Load, validate, and lower a config file into the internal model.
pub fn load(path: &Path) -> Result<ClusterSpec> {
    let src = std::fs::read_to_string(path).map_err(|e| ConfigError::Io {
        path: path.display().to_string(),
        message: e.to_string(),
    })?;
    parse_str(path, &src)
}

/// Same as [`load`] but from in-memory text (used by tests and planner).
pub fn parse_str(path: &Path, src: &str) -> Result<ClusterSpec> {
    let raw = parse_any(path, src)?;
    let mut cluster = raw.into_spec()?;
    cluster.source = src.to_string();
    cluster.validate()?;
    Ok(cluster)
}

/// Parse a document of either schema version, keeping span info for
/// diagnostics.
pub fn parse_any(path: &Path, src: &str) -> Result<AnyConfig> {
    let version = match peek_version(path, src) {
        Ok(version) => version,
        // The document does not even parse untyped (a syntax error, not a
        // schema error). Report the schema-typed parse error instead: it
        // carries the precise field/span the old pipeline produced.
        Err(ConfigError::Parse { .. }) => return parse(path, src).map(AnyConfig::V1Alpha1),
        Err(e) => return Err(e),
    };
    match version.as_str() {
        config::CONFIG_VERSION => Ok(AnyConfig::V1Alpha1(parse(path, src)?)),
        v1alpha2::CONFIG_VERSION => Ok(AnyConfig::V1Alpha2(parse_v1alpha2(path, src)?)),
        other => Err(ConfigError::invalid(
            src.to_string(),
            format!(
                "unsupported config version {other:?}; supported versions are {} and {}",
                config::CONFIG_VERSION,
                v1alpha2::CONFIG_VERSION
            ),
        )),
    }
}

/// Parse only a v1alpha1 document, keeping span info for diagnostics.
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

/// Parse only a v1alpha2 document, keeping span info for diagnostics.
pub fn parse_v1alpha2(path: &Path, src: &str) -> Result<v1alpha2::ClusterConfig> {
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

/// Read just the `version` field, before committing to a typed schema.
///
/// Parsing the whole document as an untyped value first is what makes the
/// dispatch honest: an unknown version never produces a misleading
/// "unknown field" error from the wrong schema.
fn peek_version(path: &Path, src: &str) -> Result<String> {
    let version = match path.extension().and_then(|e| e.to_str()) {
        Some("toml") => {
            let value: toml::Value =
                toml::from_str(src).map_err(|e| ConfigError::toml(path, src, e))?;
            value
                .get("version")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        }
        Some("yml" | "yaml") => {
            let value: serde_yml::Value =
                serde_yml::from_str(src).map_err(|e| ConfigError::yaml(path, src, e))?;
            value
                .get("version")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        }
        other => {
            return Err(ConfigError::UnsupportedFormat {
                path: path.display().to_string(),
                extension: other.unwrap_or_default().to_string(),
            })
        }
    };
    version.ok_or_else(|| {
        ConfigError::invalid(
            src.to_string(),
            format!(
                "missing required field `version` (expected \"{}\" or \"{}\")",
                config::CONFIG_VERSION,
                v1alpha2::CONFIG_VERSION
            ),
        )
    })
}
