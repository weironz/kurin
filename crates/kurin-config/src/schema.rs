//! JSON Schema export (WP-22).
//!
//! The schema is derived from the same Rust types the loader deserializes
//! into, so it cannot drift from the accepted document shape: doc comments
//! become `description`, `deny_unknown_fields` becomes
//! `additionalProperties: false`, and serde renames/aliases are reflected.
//! `docs/schema/cluster.v1alpha2.json` is the committed copy; a test fails
//! when it differs from [`schema_json`].

use serde_json::Value;

use crate::error::{ConfigError, Result};
use crate::{config, v1alpha2};

/// Schema versions that can be exported, newest first.
pub const SCHEMA_VERSIONS: &[&str] = &[v1alpha2::CONFIG_VERSION, config::CONFIG_VERSION];

/// JSON Schema for `version` as a [`serde_json::Value`].
///
/// # Errors
///
/// Returns [`ConfigError::Invalid`] for an unknown version string.
pub fn schema(version: &str) -> Result<Value> {
    match version {
        v1alpha2::CONFIG_VERSION => Ok(schemars::schema_for!(v1alpha2::ClusterConfig).to_value()),
        config::CONFIG_VERSION => Ok(schemars::schema_for!(config::ClusterConfig).to_value()),
        other => Err(ConfigError::invalid(
            String::new(),
            format!(
                "unknown schema version {other:?}; supported versions are {}",
                SCHEMA_VERSIONS.join(", ")
            ),
        )),
    }
}

/// Pretty-printed JSON Schema for `version`, newline-terminated.
///
/// This is the exact text written to `docs/schema/` and printed by
/// `kurin config schema`, so shell redirection reproduces the committed
/// file byte for byte.
///
/// # Errors
///
/// Returns [`ConfigError::Invalid`] for an unknown version string.
pub fn schema_json(version: &str) -> Result<String> {
    let value = schema(version)?;
    let mut text = serde_json::to_string_pretty(&value).map_err(serialize_error)?;
    text.push('\n');
    Ok(text)
}

fn serialize_error(error: serde_json::Error) -> ConfigError {
    ConfigError::invalid(
        String::new(),
        format!("failed to serialize config JSON Schema: {error}"),
    )
}
