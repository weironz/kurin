//! Quick-create argument parsing shared by the CLI and the web backend.
//!
//! The command line accepts three compact node notations (see
//! `docs/architecture/05-cli-quick-create.md` §3.1):
//!
//! - an IP list with last-octet or full ranges ([`parse_ips`]);
//! - a single node as `key=value` pairs ([`parse_node_kv`]);
//! - a bulk paste, one node per line ([`parse_bulk`]).
//!
//! Automatic names follow the `role-index` strategy ([`assign_names`]).
//! Every parser is pure: no I/O, no environment access, and no dependency on
//! the rest of the configuration pipeline.

pub mod bulk;
pub mod iprange;
pub mod nodespec;

use thiserror::Error;

pub use bulk::{assign_names, parse_bulk, BulkNode};
pub use iprange::{parse_ips, MAX_IPS};
pub use nodespec::{parse_node_kv, NodeKv};

/// Parse error for the quick-create node notations.
///
/// Each variant carries the offending input (or the offending line for bulk
/// input) so diagnostics can quote what the user actually typed.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum QuickError {
    /// An IP list could not be expanded.
    #[error("invalid IP list `{input}`: {message}")]
    Ip {
        /// The full list as supplied on the command line.
        input: String,
        /// What is wrong with it.
        message: String,
    },

    /// A `--node key=value,…` spec could not be parsed.
    #[error("invalid node spec `{input}`: {message}")]
    NodeSpec {
        /// The full spec as supplied on the command line.
        input: String,
        /// What is wrong with it.
        message: String,
    },

    /// One line of a bulk paste could not be parsed.
    #[error("invalid bulk node list at line {line} (`{input}`): {message}")]
    Bulk {
        /// 1-based line number in the pasted text.
        line: usize,
        /// The offending line, trimmed and with comments removed.
        input: String,
        /// What is wrong with it.
        message: String,
    },
}

/// Result alias for quick-create parsers.
pub type Result<T, E = QuickError> = std::result::Result<T, E>;

/// Map a role spelling to its [`crate::NodeRole`].
///
/// Shared by the `--node` and bulk notations so both accept exactly the same
/// spellings. The returned error is a bare message; callers attach their own
/// input excerpt.
pub(crate) fn parse_role_str(input: &str) -> std::result::Result<crate::NodeRole, String> {
    use crate::NodeRole;

    match input.trim().to_ascii_lowercase().as_str() {
        "control-plane" | "cp" => Ok(NodeRole::ControlPlane),
        "worker" => Ok(NodeRole::Worker),
        "lb" => Ok(NodeRole::Lb),
        "registry" => Ok(NodeRole::Registry),
        other => Err(format!(
            "unknown role `{other}` (expected control-plane, cp, worker, lb or registry)"
        )),
    }
}

/// The `role-index` prefix for automatic node names.
pub(crate) fn role_prefix(role: crate::NodeRole) -> &'static str {
    use crate::NodeRole;

    match role {
        NodeRole::ControlPlane => "cp",
        NodeRole::Worker => "worker",
        NodeRole::Lb => "lb",
        NodeRole::Registry => "registry",
    }
}
