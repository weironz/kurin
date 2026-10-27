//! IPv4 address-list expansion.
//!
//! Accepted forms, comma separated and whitespace tolerant:
//!
//! - a single address: `192.168.10.11`;
//! - a last-octet range: `192.168.10.11-13`;
//! - a full range: `192.168.10.11-192.168.10.20`.
//!
//! IPv6 is deliberately rejected: the quick-create path only configures IPv4
//! clusters today, and silently accepting an IPv6 literal would fail much
//! later, during SSH.

use std::net::Ipv4Addr;

use super::{QuickError, Result};

/// Hard cap on the number of addresses produced by one [`parse_ips`] call.
pub const MAX_IPS: usize = 512;

/// Expand a comma-separated IPv4 list, including ranges.
///
/// # Errors
///
/// Returns [`QuickError::Ip`] when the list is empty, contains an empty item,
/// contains an IPv6 literal, a malformed octet, a reversed range, or would
/// produce more than [`MAX_IPS`] addresses.
///
/// ```
/// use std::net::Ipv4Addr;
/// let ips = kurin_config::quick::parse_ips("192.168.10.11-13").unwrap();
/// assert_eq!(ips, vec![
///     Ipv4Addr::new(192, 168, 10, 11),
///     Ipv4Addr::new(192, 168, 10, 12),
///     Ipv4Addr::new(192, 168, 10, 13),
/// ]);
/// ```
pub fn parse_ips(input: &str) -> Result<Vec<Ipv4Addr>> {
    expand_ip_list(input).map_err(|message| QuickError::Ip {
        input: input.to_string(),
        message,
    })
}

/// Expand an IP list, returning a bare diagnostic message.
///
/// Used by the bulk parser so a line error can quote the line rather than the
/// whole paste.
pub(crate) fn expand_ip_list(input: &str) -> std::result::Result<Vec<Ipv4Addr>, String> {
    if input.trim().is_empty() {
        return Err("address list is empty".to_string());
    }

    let mut out = Vec::new();
    for raw in input.split(',') {
        let token = raw.trim();
        if token.is_empty() {
            return Err("empty address in comma-separated list".to_string());
        }
        expand_token(token, &mut out)?;
        if out.len() > MAX_IPS {
            return Err(format!("more than {MAX_IPS} addresses in one range list"));
        }
    }
    Ok(out)
}

/// Expand one comma-list token into `out`.
fn expand_token(token: &str, out: &mut Vec<Ipv4Addr>) -> std::result::Result<(), String> {
    if token.contains(':') {
        return Err(format!(
            "IPv6 addresses are not supported (IPv4 only): `{token}`"
        ));
    }

    let Some((start, end)) = token.split_once('-') else {
        out.push(parse_ipv4(token)?);
        return Ok(());
    };

    let start = parse_ipv4(start.trim())?;
    let end = end.trim();
    if end.is_empty() {
        return Err(format!("range `{token}` is missing its end address"));
    }

    let end = if end.contains('.') {
        parse_ipv4(end)?
    } else {
        let last: u16 = end
            .parse()
            .map_err(|_| format!("invalid last octet `{end}` in range `{token}`"))?;
        if last > 255 {
            return Err(format!(
                "last octet `{end}` in range `{token}` is out of range 0-255"
            ));
        }
        let [a, b, c, _] = start.octets();
        Ipv4Addr::new(a, b, c, last as u8)
    };

    if end < start {
        return Err(format!(
            "reversed range `{token}`: end address is before start address"
        ));
    }

    let count = u32::from(end) - u32::from(start) + 1;
    if out.len() + count as usize > MAX_IPS {
        return Err(format!("more than {MAX_IPS} addresses in one range list"));
    }

    let end = u32::from(end);
    let mut current = u32::from(start);
    while current <= end {
        out.push(Ipv4Addr::from(current));
        current += 1;
    }
    Ok(())
}

/// Parse one dotted-quad, with a diagnostic that names the bad token.
fn parse_ipv4(input: &str) -> std::result::Result<Ipv4Addr, String> {
    input
        .parse()
        .map_err(|_| format!("invalid IPv4 address `{input}`"))
}
