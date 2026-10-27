//! `kurin serve` options and the UI §6.10 binding rules.

use std::net::SocketAddr;
use std::path::PathBuf;

/// Default listen address (UI §3.1).
pub const DEFAULT_LISTEN: &str = "127.0.0.1:7080";

/// Parse [`DEFAULT_LISTEN`]; only used when the caller does not override it.
pub fn default_listen() -> SocketAddr {
    DEFAULT_LISTEN
        .parse()
        .expect("the default listen address is a valid SocketAddr")
}

/// Default data directory: `$XDG_DATA_HOME/kurin`, else `~/.local/share/kurin`
/// (UI §3.1), else `/tmp/kurin` (last resort, still writable in containers).
/// Mirrors `kurin_core::events::state_home` one level shallower (`share`).
pub fn default_data_dir() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|v| !v.is_empty())
                .map(|home| PathBuf::from(home).join(".local/share"))
        })
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("kurin")
}

/// PEM file pair enabling TLS (UI §5.2: rustls, only when requested).
#[derive(Debug, Clone)]
pub struct TlsPaths {
    pub cert: PathBuf,
    pub key: PathBuf,
}

/// Everything `kurin serve` needs (UI §3.1 / §6.10).
#[derive(Debug, Clone)]
pub struct ServeOptions {
    /// Bind address; loopback unless explicitly widened.
    pub listen: SocketAddr,
    /// Token (and, from WP-51, database) directory.
    pub data_dir: PathBuf,
    /// Additional allowed `Host` values (`host` or `host:port`).
    pub public_hosts: Vec<String>,
    /// TLS certificate chain + private key, both PEM.
    pub tls: Option<TlsPaths>,
    /// Plain HTTP on a non-loopback address (prints a warning).
    pub allow_insecure_http: bool,
    /// Generate a fresh token at startup, invalidating every session.
    pub rotate_token: bool,
}

impl Default for ServeOptions {
    fn default() -> Self {
        Self {
            listen: default_listen(),
            data_dir: default_data_dir(),
            public_hosts: Vec::new(),
            tls: None,
            allow_insecure_http: false,
            rotate_token: false,
        }
    }
}

/// UI §6.10: binding a non-loopback address requires TLS, or an explicit
/// `--allow-insecure-http` (the server then warns at startup).
pub fn validate(opts: &ServeOptions) -> Result<(), String> {
    if !opts.listen.ip().is_loopback() && opts.tls.is_none() && !opts.allow_insecure_http {
        return Err(format!(
            "监听 {} 不是回环地址：必须提供 --tls-cert/--tls-key，或显式 --allow-insecure-http \
             （明文 HTTP 会暴露访问令牌与会话 cookie）",
            opts.listen
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(listen: &str) -> ServeOptions {
        ServeOptions {
            listen: listen.parse().unwrap(),
            ..Default::default()
        }
    }

    #[test]
    fn loopback_without_tls_is_fine() {
        assert!(validate(&opts("127.0.0.1:7080")).is_ok());
        assert!(validate(&opts("[::1]:7080")).is_ok());
    }

    #[test]
    fn non_loopback_needs_tls_or_explicit_opt_out() {
        let mut wide = opts("0.0.0.0:7080");
        assert!(validate(&wide).is_err());
        wide.allow_insecure_http = true;
        assert!(validate(&wide).is_ok());
        let mut tls = opts("0.0.0.0:7080");
        tls.tls = Some(TlsPaths {
            cert: PathBuf::from("c.pem"),
            key: PathBuf::from("k.pem"),
        });
        assert!(validate(&tls).is_ok());
    }
}
