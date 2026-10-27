//! Bulk node paste parsing and automatic naming.
//!
//! One node per line: `IP[,IP…][ whitespace role][ whitespace name]`.
//! Blank lines and `#` comments are ignored, so a table copied out of a
//! spreadsheet or a wiki page can be pasted as-is. A line listing several
//! addresses expands to one node per address.

use std::collections::{BTreeMap, BTreeSet};
use std::net::Ipv4Addr;

use crate::NodeRole;

use super::iprange::expand_ip_list;
use super::{parse_role_str, role_prefix, QuickError, Result};

/// One node produced by [`parse_bulk`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BulkNode {
    /// Primary (SSH) address.
    pub address: Ipv4Addr,
    /// Node role; worker unless the line named one.
    pub role: NodeRole,
    /// Explicit name from the line; `None` means "derive one".
    pub name: Option<String>,
    /// 1-based line number in the pasted text, for diagnostics.
    pub line: usize,
}

/// Parse a bulk paste into nodes.
///
/// Grammar per line:
///
/// ```text
/// IP[,IP…] [role] [name]
/// ```
///
/// `role` accepts `control-plane`, `cp`, `worker`, `lb` or `registry` and
/// defaults to worker. A name may only be given when the line expands to a
/// single address; otherwise the ambiguity is rejected.
///
/// # Errors
///
/// Returns [`QuickError::Bulk`] quoting the offending line for an invalid IP
/// list, unknown role, extra fields, or a name on a multi-address line.
pub fn parse_bulk(input: &str) -> Result<Vec<BulkNode>> {
    let mut nodes = Vec::new();

    for (index, raw_line) in input.lines().enumerate() {
        let line_no = index + 1;
        let line = raw_line.split('#').next().unwrap_or_default().trim();
        if line.is_empty() {
            continue;
        }

        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() > 3 {
            return Err(bulk_error(
                line_no,
                line,
                "expected `IP[,IP…] [role] [name]`",
            ));
        }

        let addresses = expand_ip_list(fields[0])
            .map_err(|message| bulk_error(line_no, line, format!("invalid IP list: {message}")))?;

        let role = match fields.get(1) {
            Some(spelling) => {
                parse_role_str(spelling).map_err(|message| bulk_error(line_no, line, message))?
            }
            None => NodeRole::Worker,
        };

        let name = fields.get(2).map(|name| name.to_string());
        if name.is_some() && addresses.len() > 1 {
            return Err(bulk_error(
                line_no,
                line,
                "a name cannot be given for a line that expands to multiple addresses",
            ));
        }

        for address in addresses {
            nodes.push(BulkNode {
                address,
                role,
                name: name.clone(),
                line: line_no,
            });
        }
    }

    Ok(nodes)
}

/// Build a [`QuickError::Bulk`] for one line.
fn bulk_error(line: usize, input: &str, message: impl Into<String>) -> QuickError {
    QuickError::Bulk {
        line,
        input: input.to_string(),
        message: message.into(),
    }
}

/// Assign names to nodes using the `role-index` strategy.
///
/// Explicit names are kept as given. Unnamed nodes get `<prefix>-<n>` where
/// the prefix is `cp`, `worker`, `lb` or `registry`, and `n` starts at 1 per
/// role and advances past every name in `existing` and every name already
/// handed out in this batch. Nodes are processed in order, so the result is
/// stable across runs.
///
/// ```
/// use kurin_config::quick::{assign_names, parse_bulk};
/// let nodes = parse_bulk("10.0.0.1-2 worker").unwrap();
/// let names = assign_names(&nodes, &["worker-1".to_string()]);
/// assert_eq!(names, vec!["worker-2", "worker-3"]);
/// ```
pub fn assign_names(nodes: &[BulkNode], existing: &[String]) -> Vec<String> {
    let mut used: BTreeSet<String> = existing.iter().cloned().collect();
    let mut counters: BTreeMap<&'static str, u32> = BTreeMap::new();
    let mut names = Vec::with_capacity(nodes.len());

    for node in nodes {
        if let Some(name) = &node.name {
            used.insert(name.clone());
            names.push(name.clone());
            continue;
        }

        let prefix = role_prefix(node.role);
        let counter = counters.entry(prefix).or_insert(1);
        loop {
            let candidate = format!("{prefix}-{counter}");
            *counter += 1;
            if used.insert(candidate.clone()) {
                names.push(candidate);
                break;
            }
        }
    }

    names
}
