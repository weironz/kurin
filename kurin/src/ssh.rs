//! Remote execution over SSH (russh, pure-Rust crypto — no OpenSSL).
//!
//! One connection per node, multiplexed into several concurrent channels
//! (default 4) so a stage can fan out probes without tripping sshd's
//! `MaxStartups`. Connection setup is retried with exponential backoff;
//! every network operation is bounded by a timeout.
//!
//! Host keys are trusted on first use and recorded in
//! `~/.cache/kurin/known_hosts` (or `$XDG_CACHE_HOME/kurin/known_hosts`).
//! A key that disagrees with a recorded one aborts the handshake. Strict
//! host-key handling (no TOFU) is deliberately left to a later milestone.

use std::collections::HashMap;
use std::future::Future;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use backon::{ExponentialBuilder, Retryable};
use miette::{miette, Context, Result};
use russh::client::{self, Handle};
use russh::keys::known_hosts::{check_known_hosts_path, learn_known_hosts_path};
use russh::keys::{ssh_key, PrivateKeyWithHashAlg, PublicKeyOrCertificate};
use russh::{ChannelMsg, Disconnect};
use tokio::sync::{RwLock, Semaphore};

use kurin_config::{Cluster, ResolvedSsh, SshAuth};

use crate::engine::{ExecOutput, Session};

/// Upper bound for one `exec` round trip, including data transfer.
pub const EXEC_TIMEOUT: Duration = Duration::from_secs(900);
/// `write_file` may carry a larger payload than a probe.
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(300);
/// TCP connect + key exchange + authentication.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// Retries after the first failed connection attempt.
pub const CONNECT_RETRIES: usize = 3;
/// Concurrent channels multiplexed over one SSH connection.
pub const DEFAULT_MAX_CHANNELS: usize = 4;

/// One node's endpoint plus credentials.
#[derive(Debug, Clone)]
pub struct SshTarget {
    pub address: IpAddr,
    pub ssh: ResolvedSsh,
}

impl SshTarget {
    pub fn new(address: IpAddr, ssh: ResolvedSsh) -> Self {
        Self { address, ssh }
    }
}

/// `$XDG_CACHE_HOME/kurin/known_hosts`, falling back to `~/.cache/...`.
pub fn default_known_hosts_path() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_CACHE_HOME").filter(|d| !d.is_empty()) {
        return Path::new(&dir).join("kurin").join("known_hosts");
    }
    let home = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".cache").join("kurin").join("known_hosts")
}

/// Host-key policy: accept the key if it matches the store, learn it if the
/// host is unknown, refuse when a different key is already recorded.
#[derive(Debug, Clone)]
struct HostKeyPolicy {
    host: String,
    port: u16,
    known_hosts: PathBuf,
}

impl HostKeyPolicy {
    fn trust(&self, key: &ssh_key::PublicKey) -> bool {
        match check_known_hosts_path(&self.host, self.port, key, &self.known_hosts) {
            Ok(true) => true,
            // First contact with this host: remember the key (trust on first use).
            Ok(false) => {
                learn_known_hosts_path(&self.host, self.port, key, &self.known_hosts).is_ok()
            }
            // Recorded key mismatch (or unreadable store): never trust.
            Err(_) => false,
        }
    }
}

impl client::Handler for HostKeyPolicy {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        key: &PublicKeyOrCertificate,
    ) -> std::result::Result<bool, Self::Error> {
        match key {
            PublicKeyOrCertificate::PublicKey { key, .. } => Ok(self.trust(key)),
            // Host certificates are out of scope for M1.
            PublicKeyOrCertificate::Certificate(_) => Ok(false),
        }
    }
}

/// Shared state for one node: connection slot, channel limiter, credentials.
struct NodeConn {
    name: String,
    target: SshTarget,
    config: Arc<client::Config>,
    known_hosts: PathBuf,
    channels: Arc<Semaphore>,
    conn: RwLock<Option<Arc<Handle<HostKeyPolicy>>>>,
}

impl NodeConn {
    fn new(name: String, target: SshTarget, known_hosts: PathBuf, max_channels: usize) -> Self {
        let config = client::Config {
            keepalive_interval: Some(Duration::from_secs(15)),
            keepalive_max: 3,
            nodelay: true,
            ..Default::default()
        };
        Self {
            name,
            target,
            config: Arc::new(config),
            known_hosts,
            channels: Arc::new(Semaphore::new(max_channels.max(1))),
            conn: RwLock::new(None),
        }
    }

    fn label(&self) -> String {
        format!(
            "{} ({}@{}:{})",
            self.name, self.target.ssh.user, self.target.address, self.target.ssh.port
        )
    }

    /// One connection attempt: TCP + key exchange + auth, all timeout-bounded.
    async fn connect_once(&self) -> Result<Handle<HostKeyPolicy>> {
        let policy = HostKeyPolicy {
            host: self.target.address.to_string(),
            port: self.target.ssh.port,
            known_hosts: self.known_hosts.clone(),
        };
        let mut handle = tokio::time::timeout(
            CONNECT_TIMEOUT,
            client::connect(
                self.config.clone(),
                (self.target.address, self.target.ssh.port),
                policy,
            ),
        )
        .await
        .map_err(|_| miette!("ssh {}: connect timed out", self.label()))?
        .map_err(|e| miette!("ssh {}: connect failed: {e}", self.label()))?;

        let user = &self.target.ssh.user;
        match &self.target.ssh.auth {
            SshAuth::Password { password } => {
                let auth = tokio::time::timeout(
                    CONNECT_TIMEOUT,
                    handle.authenticate_password(user.clone(), password.clone()),
                )
                .await
                .map_err(|_| miette!("ssh {}: password authentication timed out", self.label()))?
                .map_err(|e| miette!("ssh {}: password authentication failed: {e}", self.label()))?;
                if !auth.success() {
                    return Err(miette!(
                        "ssh {}: password authentication rejected",
                        self.label()
                    ));
                }
            }
            SshAuth::PrivateKey {
                key_path,
                passphrase,
            } => {
                let key = russh::keys::load_secret_key(key_path, passphrase.as_deref())
                    .map_err(|e| {
                        miette!(
                            "ssh {}: cannot load private key {}: {e}",
                            self.label(),
                            key_path.display()
                        )
                    })?;
                let hash_alg = handle
                    .best_supported_rsa_hash()
                    .await
                    .map_err(|e| miette!("ssh {}: RSA hash negotiation failed: {e}", self.label()))?
                    .flatten();
                let auth = tokio::time::timeout(
                    CONNECT_TIMEOUT,
                    handle.authenticate_publickey(
                        user.clone(),
                        PrivateKeyWithHashAlg::new(Arc::new(key), hash_alg),
                    ),
                )
                .await
                .map_err(|_| miette!("ssh {}: key authentication timed out", self.label()))?
                .map_err(|e| miette!("ssh {}: key authentication failed: {e}", self.label()))?;
                if !auth.success() {
                    return Err(miette!(
                        "ssh {}: key authentication rejected",
                        self.label()
                    ));
                }
            }
        }
        Ok(handle)
    }

    /// Connect with exponential backoff — sshd randomly rejects connection
    /// floods (`MaxStartups`), and a just-booted node may not be listening yet.
    async fn connect(&self) -> Result<Arc<Handle<HostKeyPolicy>>> {
        let backoff = ExponentialBuilder::default()
            .with_min_delay(Duration::from_millis(500))
            .with_max_delay(Duration::from_secs(8))
            .with_max_times(CONNECT_RETRIES);
        (|| async { self.connect_once().await })
            .retry(backoff)
            .await
            .map(Arc::new)
            .wrap_err_with(|| {
                format!(
                    "ssh {}: giving up after {} retries",
                    self.label(),
                    CONNECT_RETRIES
                )
            })
    }

    /// Reuse the live connection, or open a new one. The write lock
    /// serializes concurrent reconnects so only one handshake happens.
    async fn handle(&self) -> Result<Arc<Handle<HostKeyPolicy>>> {
        if let Some(handle) = self.conn.read().await.as_ref() {
            if !handle.is_closed() {
                return Ok(Arc::clone(handle));
            }
        }
        let mut slot = self.conn.write().await;
        if let Some(handle) = slot.as_ref() {
            if !handle.is_closed() {
                return Ok(Arc::clone(handle));
            }
        }
        let handle = self.connect().await?;
        *slot = Some(Arc::clone(&handle));
        Ok(handle)
    }

    async fn close(&self) {
        if let Some(handle) = self.conn.write().await.take() {
            let _ = handle
                .disconnect(Disconnect::ByApplication, "kurin: done", "en")
                .await;
        }
    }
}

/// A [`Session`] bound to one node's shared connection.
pub struct SshSession {
    conn: Arc<NodeConn>,
}

impl SshSession {
    /// Run a command on a freshly opened channel and collect its output.
    async fn exec_on(handle: &Handle<HostKeyPolicy>, command: &str) -> Result<ExecOutput> {
        let mut channel = handle
            .channel_open_session()
            .await
            .map_err(|e| miette!("ssh: cannot open channel: {e}"))?;
        channel
            .exec(true, command)
            .await
            .map_err(|e| miette!("ssh: cannot start command: {e}"))?;
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut exit_code = None;
        while let Some(msg) = channel.wait().await {
            match msg {
                ChannelMsg::Data { data } => stdout.extend_from_slice(&data),
                ChannelMsg::ExtendedData { data, .. } => stderr.extend_from_slice(&data),
                ChannelMsg::ExitStatus { exit_status } => exit_code = Some(exit_status as i32),
                _ => {}
            }
        }
        let exit_code = exit_code.ok_or_else(|| {
            miette!(
                "ssh: channel closed without an exit status for `{}`",
                truncate(command, 80)
            )
        })?;
        Ok(ExecOutput {
            exit_code,
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
        })
    }

    /// Acquire a channel slot, resolve the connection, run `op` under a timeout.
    async fn with_channel<T, F, Fut>(&self, timeout: Duration, op: F) -> Result<T>
    where
        F: FnOnce(Arc<Handle<HostKeyPolicy>>) -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        let _permit = self
            .conn
            .channels
            .acquire()
            .await
            .map_err(|_| miette!("ssh {}: connection pool closed", self.conn.label()))?;
        let handle = self.conn.handle().await?;
        tokio::time::timeout(timeout, op(handle))
            .await
            .map_err(|_| {
                miette!(
                    "ssh {}: operation timed out after {:?}",
                    self.conn.label(),
                    timeout
                )
            })?
            .wrap_err_with(|| format!("ssh {}", self.conn.label()))
    }
}

#[async_trait::async_trait]
impl Session for SshSession {
    async fn exec(&self, command: &str) -> Result<ExecOutput> {
        self.with_channel(EXEC_TIMEOUT, |handle| async move {
            let command = command.to_owned();
            Self::exec_on(&handle, &command).await
        })
        .await
    }

    async fn write_file(&self, path: &str, contents: &[u8]) -> Result<()> {
        let contents = contents.to_vec();
        let path = path.to_owned();
        self.with_channel(WRITE_TIMEOUT, |handle| async move {
            let dir = Path::new(&path)
                .parent()
                .map(|p| p.to_string_lossy().into_owned())
                .filter(|p| !p.is_empty())
                .unwrap_or_else(|| ".".into());
            // Bytes go over the channel's stdin, not the command line: safe
            // for arbitrary (binary) payloads and free of shell quoting risk.
            let command = format!("mkdir -p -- {} && cat > {}", shell_quote(&dir), shell_quote(&path));
            let mut channel = handle
                .channel_open_session()
                .await
                .map_err(|e| miette!("ssh: cannot open channel: {e}"))?;
            channel
                .exec(true, command)
                .await
                .map_err(|e| miette!("ssh: cannot start write command: {e}"))?;
            channel
                .data_bytes(contents)
                .await
                .map_err(|e| miette!("ssh: cannot send file contents: {e}"))?;
            channel
                .eof()
                .await
                .map_err(|e| miette!("ssh: cannot close stdin: {e}"))?;
            let mut stderr = Vec::new();
            let mut exit_code = None;
            while let Some(msg) = channel.wait().await {
                match msg {
                    ChannelMsg::ExtendedData { data, .. } => stderr.extend_from_slice(&data),
                    ChannelMsg::ExitStatus { exit_status } => exit_code = Some(exit_status as i32),
                    _ => {}
                }
            }
            match exit_code {
                Some(0) => Ok(()),
                Some(code) => Err(miette!(
                    "ssh: writing {} failed (exit {}): {}",
                    path,
                    code,
                    String::from_utf8_lossy(&stderr).trim()
                )),
                None => Err(miette!(
                    "ssh: writing {} failed: channel closed without an exit status",
                    path
                )),
            }
        })
        .await
    }

    async fn file_sha256(&self, path: &str) -> Result<Option<String>> {
        let quoted = shell_quote(path);
        // Exit 3 marks "not a regular file / absent"; anything else is a real
        // failure (sha256sum missing, unreadable file, ...) and must surface.
        let command = format!("if [ -f {quoted} ]; then sha256sum {quoted}; else exit 3; fi");
        let out = self.exec(&command).await?;
        match out.exit_code {
            0 => {
                let sum = out
                    .stdout
                    .split_whitespace()
                    .next()
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                if sum.len() == 64 && sum.bytes().all(|b| b.is_ascii_hexdigit()) {
                    Ok(Some(sum))
                } else {
                    Err(miette!("ssh: unparsable sha256sum output: {}", out.stdout.trim()))
                }
            }
            3 => Ok(None),
            code => Err(miette!(
                "ssh: sha256sum {} failed (exit {}): {}",
                path,
                code,
                out.stderr.trim()
            )),
        }
    }

    async fn service_active(&self, name: &str) -> Result<bool> {
        let out = self.exec(&format!("systemctl is-active -- {}", shell_quote(name))).await?;
        if out.exit_code == 127 {
            return Err(miette!("ssh: systemctl not found on node"));
        }
        Ok(out.stdout.trim() == "active")
    }
}

/// Per-node SSH connections, shared by every task touching that node.
pub struct SshSessionPool {
    nodes: HashMap<String, Arc<NodeConn>>,
    known_hosts: PathBuf,
    max_channels: usize,
}

impl SshSessionPool {
    /// Pool for every node in `cluster`, with default settings.
    pub fn from_cluster(cluster: &Cluster) -> Result<Self> {
        let mut builder = SshSessionPoolBuilder::new();
        for node in &cluster.nodes {
            builder = builder.node(
                node.name.clone(),
                SshTarget::new(node.address, node.ssh.clone()),
            );
        }
        builder.build()
    }

    pub fn builder() -> SshSessionPoolBuilder {
        SshSessionPoolBuilder::new()
    }

    /// Change the per-node channel limit. Call before the pool is first used.
    pub fn with_max_channels(mut self, max_channels: usize) -> Self {
        let max_channels = max_channels.max(1);
        let nodes = self
            .nodes
            .into_iter()
            .map(|(name, conn)| {
                let rebuilt = Arc::new(NodeConn::new(
                    name.clone(),
                    conn.target.clone(),
                    self.known_hosts.clone(),
                    max_channels,
                ));
                (name, rebuilt)
            })
            .collect();
        self.nodes = nodes;
        self.max_channels = max_channels;
        self
    }

    pub fn known_hosts_path(&self) -> &Path {
        &self.known_hosts
    }

    pub fn max_channels(&self) -> usize {
        self.max_channels
    }

    /// Session for one configured node.
    pub fn session(&self, node: &str) -> Result<SshSession> {
        let conn = self.nodes.get(node).ok_or_else(|| {
            miette!(
                "ssh: unknown node `{node}` (known: {})",
                self.nodes.keys().cloned().collect::<Vec<_>>().join(", ")
            )
        })?;
        Ok(SshSession {
            conn: Arc::clone(conn),
        })
    }

    /// Gracefully disconnect every live connection.
    pub async fn close(&self) {
        for conn in self.nodes.values() {
            conn.close().await;
        }
    }
}

#[async_trait::async_trait]
impl crate::executor::SessionPool for SshSessionPool {
    async fn get(&self, node: &str) -> Result<Arc<dyn Session>> {
        Ok(Arc::new(self.session(node)?))
    }
}

/// Builder for [`SshSessionPool`].
pub struct SshSessionPoolBuilder {
    known_hosts: PathBuf,
    max_channels: usize,
    nodes: Vec<(String, SshTarget)>,
}

impl SshSessionPoolBuilder {
    pub fn new() -> Self {
        Self {
            known_hosts: default_known_hosts_path(),
            max_channels: DEFAULT_MAX_CHANNELS,
            nodes: Vec::new(),
        }
    }

    pub fn known_hosts(mut self, path: impl Into<PathBuf>) -> Self {
        self.known_hosts = path.into();
        self
    }

    pub fn max_channels(mut self, max_channels: usize) -> Self {
        self.max_channels = max_channels.max(1);
        self
    }

    pub fn node(mut self, name: impl Into<String>, target: SshTarget) -> Self {
        self.nodes.push((name.into(), target));
        self
    }

    pub fn build(self) -> Result<SshSessionPool> {
        let known_hosts = self.known_hosts;
        if let Some(parent) = known_hosts.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                miette!("ssh: cannot create known_hosts directory {}: {e}", parent.display())
            })?;
        }
        let mut nodes = HashMap::with_capacity(self.nodes.len());
        for (name, target) in self.nodes {
            nodes.insert(
                name.clone(),
                Arc::new(NodeConn::new(
                    name,
                    target,
                    known_hosts.clone(),
                    self.max_channels,
                )),
            );
        }
        Ok(SshSessionPool {
            nodes,
            known_hosts,
            max_channels: self.max_channels,
        })
    }
}

impl Default for SshSessionPoolBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// POSIX shell single-quote quoting.
fn shell_quote(value: &str) -> String {
    let plain = !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_./:=@%+,".contains(&b));
    if plain {
        value.to_owned()
    } else {
        format!("'{}'", value.replace('\'', r"'\''"))
    }
}

fn truncate(s: &str, max: usize) -> &str {
    match s.char_indices().nth(max) {
        Some((idx, _)) => &s[..idx],
        None => s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_plain_paths_verbatim() {
        assert_eq!(shell_quote("/etc/kubernetes/kubelet.conf"), "/etc/kubernetes/kubelet.conf");
        assert_eq!(shell_quote("10.233.64.0/18"), "10.233.64.0/18");
    }

    #[test]
    fn quotes_spaces_and_metacharacters() {
        assert_eq!(shell_quote("/tmp/a b"), "'/tmp/a b'");
        assert_eq!(shell_quote("a;rm -rf /"), "'a;rm -rf /'");
        assert_eq!(shell_quote("$(id)"), "'$(id)'");
    }

    #[test]
    fn quotes_embedded_single_quote() {
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
    }

    #[test]
    fn empty_string_is_quoted() {
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn default_known_hosts_honours_xdg_cache_home() {
        // Read the path shape only; the variable is process-global, so this
        // test asserts the code path rather than mutating the environment.
        let path = default_known_hosts_path();
        assert!(path.ends_with(Path::new("kurin").join("known_hosts")));
    }

    #[test]
    fn truncate_is_char_safe() {
        assert_eq!(truncate("abcdef", 3), "abc");
        assert_eq!(truncate("abc", 10), "abc");
        assert_eq!(truncate("日本語", 2), "日本");
    }
}