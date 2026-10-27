//! `--node key=value,…` parsing.
//!
//! Field names are kebab-case, matching the configuration file. Only
//! `address` is mandatory; `role` defaults to worker and `name` is optional
//! (the quick-create path can derive one with [`super::assign_names`]).

use std::collections::{BTreeMap, BTreeSet};
use std::net::Ipv4Addr;

use crate::NodeRole;

use super::{parse_role_str, QuickError, Result};

/// One node described precisely with `--node`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeKv {
    /// Explicit node name; `None` means "derive one".
    pub name: Option<String>,
    /// Primary (SSH) address.
    pub address: Ipv4Addr,
    /// Node role; worker unless given.
    pub role: NodeRole,
    /// Kubernetes labels, parsed from `labels=k=v;k2=v2`.
    pub labels: BTreeMap<String, String>,
    /// Optional internal address used for cluster traffic.
    pub internal_address: Option<Ipv4Addr>,
    /// Per-node SSH user override.
    pub user: Option<String>,
    /// Per-node SSH port override.
    pub port: Option<u16>,
    /// Environment variable holding the SSH password.
    pub password_env: Option<String>,
    /// File holding the SSH password.
    pub password_file: Option<String>,
    /// SSH private key path.
    pub key: Option<String>,
}

/// Parse a `--node` spec such as
/// `name=m1,address=10.1.0.11,role=control-plane,labels=zone=a;tier=b`.
///
/// # Errors
///
/// Returns [`QuickError::NodeSpec`] for a missing `address`, an unknown or
/// duplicate field, an invalid role/IP/port, or malformed labels.
pub fn parse_node_kv(input: &str) -> Result<NodeKv> {
    let mut seen = BTreeSet::new();
    let mut name = None;
    let mut address = None;
    let mut role = NodeRole::Worker;
    let mut labels = BTreeMap::new();
    let mut internal_address = None;
    let mut user = None;
    let mut port = None;
    let mut password_env = None;
    let mut password_file = None;
    let mut key = None;

    for raw in input.split(',') {
        let field = raw.trim();
        if field.is_empty() {
            return Err(spec_error(input, "empty field in node spec"));
        }

        let (raw_key, raw_value) = field
            .split_once('=')
            .ok_or_else(|| spec_error(input, format!("expected key=value, got `{field}`")))?;
        let key_name = raw_key.trim();
        let value = raw_value.trim();
        if key_name.is_empty() {
            return Err(spec_error(input, format!("empty key in `{field}`")));
        }
        if !seen.insert(key_name.to_string()) {
            return Err(spec_error(input, format!("duplicate key `{key_name}`")));
        }

        match key_name {
            "name" => name = Some(require_value(input, key_name, value)?.to_string()),
            "address" => address = Some(parse_ipv4_field(input, key_name, value)?),
            "internal-address" => {
                internal_address = Some(parse_ipv4_field(input, key_name, value)?);
            }
            "role" => role = parse_role_field(input, value)?,
            "labels" => labels = parse_labels(input, value)?,
            "user" => user = Some(require_value(input, key_name, value)?.to_string()),
            "port" => port = Some(parse_port(input, value)?),
            "password-env" => {
                password_env = Some(require_value(input, key_name, value)?.to_string());
            }
            "password-file" => {
                password_file = Some(require_value(input, key_name, value)?.to_string());
            }
            "key" => key = Some(require_value(input, key_name, value)?.to_string()),
            other => {
                return Err(spec_error(
                    input,
                    format!(
                        "unknown node field `{other}` (expected name, address, role, \
                         internal-address, user, port, password-env, password-file, key, labels)"
                    ),
                ));
            }
        }
    }

    let address = address.ok_or_else(|| spec_error(input, "missing required field `address`"))?;

    Ok(NodeKv {
        name,
        address,
        role,
        labels,
        internal_address,
        user,
        port,
        password_env,
        password_file,
        key,
    })
}

/// Build a [`QuickError::NodeSpec`] for `input`.
fn spec_error(input: &str, message: impl Into<String>) -> QuickError {
    QuickError::NodeSpec {
        input: input.to_string(),
        message: message.into(),
    }
}

/// Reject an empty value for a field that must carry data.
fn require_value<'a>(
    input: &str,
    key: &str,
    value: &'a str,
) -> std::result::Result<&'a str, QuickError> {
    if value.is_empty() {
        return Err(spec_error(input, format!("`{key}` must not be empty")));
    }
    Ok(value)
}

/// Parse an address field, naming the field in the diagnostic.
fn parse_ipv4_field(
    input: &str,
    key: &str,
    value: &str,
) -> std::result::Result<Ipv4Addr, QuickError> {
    if value.contains(':') {
        return Err(spec_error(
            input,
            format!("IPv6 addresses are not supported (IPv4 only): `{key}={value}`"),
        ));
    }
    value
        .parse()
        .map_err(|_| spec_error(input, format!("invalid IPv4 address in `{key}={value}`")))
}

/// Parse the role field.
fn parse_role_field(input: &str, value: &str) -> std::result::Result<NodeRole, QuickError> {
    if value.is_empty() {
        return Err(spec_error(input, "`role` must not be empty"));
    }
    parse_role_str(value).map_err(|message| spec_error(input, message))
}

/// Parse the port field; port 0 is not a usable SSH port.
fn parse_port(input: &str, value: &str) -> std::result::Result<u16, QuickError> {
    let port: u16 = value
        .parse()
        .map_err(|_| spec_error(input, format!("invalid port `{value}` (expected 1-65535)")))?;
    if port == 0 {
        return Err(spec_error(input, "port must be 1-65535, got 0"));
    }
    Ok(port)
}

/// Parse `k=v;k2=v2`, splitting entries on `;` and each entry on its first `=`.
fn parse_labels(
    input: &str,
    value: &str,
) -> std::result::Result<BTreeMap<String, String>, QuickError> {
    if value.is_empty() {
        return Err(spec_error(input, "`labels` must not be empty"));
    }

    let mut labels = BTreeMap::new();
    for raw in value.split(';') {
        let entry = raw.trim();
        if entry.is_empty() {
            return Err(spec_error(
                input,
                format!("empty label in `labels={value}`"),
            ));
        }
        let (label_key, label_value) = entry.split_once('=').ok_or_else(|| {
            spec_error(
                input,
                format!("label `{entry}` must be key=value (in `labels={value}`)"),
            )
        })?;
        let label_key = label_key.trim();
        let label_value = label_value.trim();
        if label_key.is_empty() {
            return Err(spec_error(
                input,
                format!("empty label key in `labels={value}`"),
            ));
        }
        if labels.contains_key(label_key) {
            return Err(spec_error(
                input,
                format!("duplicate label key `{label_key}`"),
            ));
        }
        labels.insert(label_key.to_string(), label_value.to_string());
    }
    Ok(labels)
}
