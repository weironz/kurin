//! Host header allowlist (UI §6.10) — the DNS-rebinding defense.
//!
//! A page served from `evil.example` cannot make the browser talk to
//! `http://127.0.0.1:7080` under an attacker-controlled name: every request
//! must carry an allowed `Host` (`127.0.0.1` / `localhost` / `[::1]` on the
//! listen port, or an explicit `--public-host` entry).

/// The allowed `Host` entries.
#[derive(Debug, Clone)]
pub struct HostAllowlist {
    entries: Vec<Entry>,
    /// Port a portless `Host` is assumed to mean (a browser omitting the
    /// default port 80 while we listen on 80).
    listen_port: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    /// Lowercased host; IPv6 keeps its brackets.
    host: String,
    /// Port when the entry pins one; `None` accepts any port of that host.
    port: Option<u16>,
}

impl Entry {
    fn matches(&self, host: &str, port: u16) -> bool {
        self.host == host && self.port.is_none_or(|pinned| pinned == port)
    }
}

impl HostAllowlist {
    /// The loopback names on the listen port plus every `--public-host`
    /// entry (`host` or `host:port`).
    pub fn new(listen_port: u16, public_hosts: &[String]) -> Self {
        let entries = vec![
            Entry {
                host: "127.0.0.1".into(),
                port: Some(listen_port),
            },
            Entry {
                host: "localhost".into(),
                port: Some(listen_port),
            },
            Entry {
                host: "[::1]".into(),
                port: Some(listen_port),
            },
        ];
        let mut entries = entries;
        for public in public_hosts {
            if let Some((host, port)) = split_host_port(public) {
                entries.push(Entry { host, port });
            }
        }
        Self {
            entries,
            listen_port,
        }
    }

    /// Is this `Host` header value allowed?
    pub fn allows(&self, host_header: &str) -> bool {
        let Some((host, port)) = split_host_port(host_header) else {
            return false;
        };
        let port = port.unwrap_or(self.listen_port);
        self.entries.iter().any(|entry| entry.matches(&host, port))
    }
}

/// Split a `Host` value into (host, port). IPv6 keeps its brackets
/// (`[::1]:7080` → `([::1], 7080)`). Returns `None` for empty hosts or a
/// non-numeric port (an unbracketed IPv6 literal is invalid in HTTP).
pub(super) fn split_host_port(value: &str) -> Option<(String, Option<u16>)> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    if let Some(rest) = value.strip_prefix('[') {
        let (host, rest) = rest.split_once(']')?;
        let port = match rest.strip_prefix(':') {
            Some(digits) => Some(digits.parse().ok()?),
            None => {
                if !rest.is_empty() {
                    return None;
                }
                None
            }
        };
        return Some((format!("[{host}]").to_ascii_lowercase(), port));
    }
    match value.rsplit_once(':') {
        Some((host, port)) => {
            let port = port.parse().ok()?;
            if host.is_empty() {
                return None;
            }
            Some((host.to_ascii_lowercase(), Some(port)))
        }
        None => Some((value.to_ascii_lowercase(), None)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list() -> HostAllowlist {
        HostAllowlist::new(7080, &["kurin.example.com".into(), "box.lan:9090".into()])
    }

    #[test]
    fn loopback_hosts_on_the_listen_port() {
        let list = list();
        for host in ["127.0.0.1:7080", "localhost:7080", "[::1]:7080"] {
            assert!(list.allows(host), "{host} should be allowed");
        }
        // A portless Host is normalized to the listen port (a browser only
        // omits port 80, and then we listen on 80); the host identity is
        // the rebinding defense.
        assert!(list.allows("localhost"));
        let port80 = HostAllowlist::new(80, &[]);
        assert!(port80.allows("localhost"));
        assert!(!port80.allows("localhost:7080"));
        // Case-insensitive host names.
        assert!(list.allows("LOCALHOST:7080"));
    }

    #[test]
    fn wrong_hosts_are_rejected() {
        let list = list();
        for host in [
            "evil.example",
            "evil.example:7080",
            "127.0.0.1:7081",
            "localhost.evil",
            "",
            "sub.localhost:7080",
            "box.lan:9091",
            "box.lan",
            "[::2]:7080",
            "host:notaport",
        ] {
            assert!(!list.allows(host), "{host} should be rejected");
        }
    }

    #[test]
    fn public_hosts_widen_the_list() {
        let list = list();
        assert!(list.allows("kurin.example.com:7080"));
        assert!(list.allows("kurin.example.com"));
        assert!(list.allows("KURIN.example.COM:1234"));
        assert!(list.allows("box.lan:9090"));
        assert!(!list.allows("box.lan:7080"));
    }

    #[test]
    fn split_host_port_shapes() {
        assert_eq!(
            split_host_port("[::1]:7080"),
            Some(("[::1]".into(), Some(7080)))
        );
        assert_eq!(split_host_port("[::1]"), Some(("[::1]".into(), None)));
        assert_eq!(split_host_port("a.b:80"), Some(("a.b".into(), Some(80))));
        assert_eq!(split_host_port("a.b"), Some(("a.b".into(), None)));
        assert_eq!(split_host_port(":::"), None);
        assert_eq!(split_host_port(""), None);
    }
}
