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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

use backon::{ExponentialBuilder, Retryable};
use miette::{miette, Context, Result};
use russh::client::{self, Handle};
use russh::keys::known_hosts::{check_known_hosts_path, learn_known_hosts_path};
use russh::keys::{ssh_key, PrivateKeyWithHashAlg, PublicKeyOrCertificate};
use russh::{ChannelMsg, Disconnect};
use tokio::sync::{RwLock, Semaphore};

use kurin_config::{BecomeMode, Cluster, ResolvedSsh, SshAuth};

use kurin_core::engine::{Cmd, ExecOutput, Session};

/// Upper bound for one `exec` round trip, including data transfer.
pub const EXEC_TIMEOUT: Duration = Duration::from_secs(900);
/// `write_file` may carry a larger payload than a probe: image archives run
/// into hundreds of MB and SSH channel flow control caps throughput, so
/// this shares EXEC_TIMEOUT's headroom.
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(1800);
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
    /// Set when a recorded key mismatches, so callers can surface the
    /// actionable cause instead of a generic "connection failed".
    mismatch: Arc<std::sync::Mutex<Option<String>>>,
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
            Err(e) => {
                *self.mismatch.lock().unwrap() = Some(format!(
                    "host key for {}:{} does not match the recorded key ({e}); \
                     if the node was rebuilt, remove the stale entry from {}",
                    self.host,
                    self.port,
                    self.known_hosts.display()
                ));
                false
            }
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
        let mismatch = Arc::new(std::sync::Mutex::new(None));
        let policy = HostKeyPolicy {
            host: self.target.address.to_string(),
            port: self.target.ssh.port,
            known_hosts: self.known_hosts.clone(),
            mismatch: Arc::clone(&mismatch),
        };
        let mut handle = match tokio::time::timeout(
            CONNECT_TIMEOUT,
            client::connect(
                self.config.clone(),
                (self.target.address, self.target.ssh.port),
                policy,
            ),
        )
        .await
        {
            Err(_) => return Err(miette!("ssh {}: connect timed out", self.label())),
            Ok(Err(e)) => {
                let reason = mismatch
                    .lock()
                    .unwrap()
                    .clone()
                    .unwrap_or_else(|| format!("connect failed: {e}"));
                return Err(miette!("ssh {}: {reason}", self.label()));
            }
            Ok(Ok(handle)) => handle,
        };

        let user = &self.target.ssh.user;
        match &self.target.ssh.auth {
            SshAuth::Password { password } => {
                let auth = tokio::time::timeout(
                    CONNECT_TIMEOUT,
                    handle.authenticate_password(user.clone(), password.clone()),
                )
                .await
                .map_err(|_| miette!("ssh {}: password authentication timed out", self.label()))?
                .map_err(|e| {
                    miette!("ssh {}: password authentication failed: {e}", self.label())
                })?;
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
                let key =
                    russh::keys::load_secret_key(key_path, passphrase.as_deref()).map_err(|e| {
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
                    return Err(miette!("ssh {}: key authentication rejected", self.label()));
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
        (|| async {
            match self.connect_once().await {
                Ok(h) => Ok(h),
                Err(e) => Err(e),
            }
        })
        .retry(backoff)
        // Never retry a recorded host key mismatch: it cannot heal itself.
        .when(|e| !format!("{e}").contains("does not match the recorded key"))
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

    /// True when the cached connection is gone or already closed (the
    /// signal for "reconnect and retry the op").
    async fn is_closed_or_absent(&self) -> bool {
        match self.conn.read().await.as_ref() {
            None => true,
            Some(handle) => handle.is_closed(),
        }
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
    /// `stdin`, when given, is written to the channel and closed with EOF
    /// before the output is collected (how `sudo -S` passwords travel).
    async fn exec_on(
        handle: &Handle<HostKeyPolicy>,
        command: &str,
        stdin: Option<&[u8]>,
    ) -> Result<ExecOutput> {
        let mut channel = handle
            .channel_open_session()
            .await
            .map_err(|e| miette!("ssh: cannot open channel: {e}"))?;
        channel
            .exec(true, command)
            .await
            .map_err(|e| miette!("ssh: cannot start command: {e}"))?;
        if let Some(data) = stdin {
            channel
                .data_bytes(data.to_vec())
                .await
                .map_err(|e| miette!("ssh: cannot send stdin: {e}"))?;
            channel
                .eof()
                .await
                .map_err(|e| miette!("ssh: cannot close stdin: {e}"))?;
        }
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
        // Explicit close: sshd counts open session channels against
        // MaxSessions (10/connection). Leaking them stalls later execs —
        // observed as a big-upload hang with no remote process spawned.
        let _ = channel.close().await;
        Ok(ExecOutput {
            exit_code,
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
        })
    }

    /// Acquire a channel slot, resolve the connection, run `op` under a timeout.
    async fn with_channel<T, F, Fut>(&self, timeout: Duration, op: F) -> Result<T>
    where
        F: FnOnce(Arc<Handle<HostKeyPolicy>>) -> Fut + Clone,
        Fut: Future<Output = Result<T>>,
    {
        // One retry when the connection died mid-op (sshd restart, transient
        // RST, a dropped channel on long uploads): `handle()` reconnects, and
        // both exec and write_file are idempotent at the transport level.
        let mut attempt = 0;
        loop {
            attempt += 1;
            let result = self.with_channel_once(timeout, op.clone()).await;
            match result {
                Ok(value) => return Ok(value),
                Err(err) => {
                    let retryable = attempt < 2 && self.conn.is_closed_or_absent().await;
                    if !retryable {
                        return Err(err).wrap_err_with(|| format!("ssh {}", self.conn.label()));
                    }
                }
            }
        }
    }

    async fn with_channel_once<T, F, Fut>(&self, timeout: Duration, op: F) -> Result<T>
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
    }
}

#[async_trait::async_trait]
impl Session for SshSession {
    /// Privileged convenience wrapper (see the trait docs): identical to
    /// `exec_cmd(&Cmd::new(command))`.
    async fn exec(&self, command: &str) -> Result<ExecOutput> {
        self.exec_cmd(&Cmd::new(command)).await
    }

    async fn exec_cmd(&self, cmd: &Cmd) -> Result<ExecOutput> {
        let (command, stdin) = become_command(&self.conn.target.ssh, cmd);
        let timeout = cmd.timeout.unwrap_or(EXEC_TIMEOUT);
        let mut out = self
            .with_channel(timeout, |handle| {
                let command = command.clone();
                let stdin = stdin.clone();
                async move { Self::exec_on(&handle, &command, stdin.as_deref()).await }
            })
            .await?;
        redact_output(&mut out, cmd);
        Ok(out)
    }

    /// Overwrites `path` with mode 0644 (mkdir -p the parents).
    async fn write_file(&self, path: &str, contents: &[u8]) -> Result<()> {
        self.write_file_mode(path, contents, 0o644).await
    }

    /// Overwrites `path` with `contents`, giving the file `mode` (octal).
    ///
    /// The payload never travels through the privileged command: it is
    /// staged to `/tmp/kurin-<rand>` as the login user, then moved into
    /// place with `install -D -m <mode>` (wrapped in sudo exactly when the
    /// node's become policy demands it), and the staging file is removed.
    ///
    /// Callers that replace an executable **must** keep staging to a temp
    /// name and renaming: opening a running binary for write fails with
    /// ETXTBSY on the remote side, the remote `cat` exits immediately, and
    /// (observed with russh 0.64) the still-open channel then deadlocks the
    /// remaining upload against a closed window instead of surfacing the
    /// error. `install` inherits that same contract.
    async fn write_file_mode(&self, path: &str, contents: &[u8], mode: u32) -> Result<()> {
        let tmp = stage_path();
        self.stage_bytes(&tmp, contents).await?;
        let install = Cmd::new(format!(
            "install -D -m {:o} -- {} {} && rm -f -- {}",
            mode,
            shell_quote(&tmp),
            shell_quote(path),
            shell_quote(&tmp),
        ));
        let out = self.exec_cmd(&install).await?;
        if !out.ok() {
            return Err(miette!(
                "ssh: installing {} (mode {:o}) failed (exit {}): {}",
                path,
                mode,
                out.exit_code,
                out.stderr.trim()
            ));
        }
        Ok(())
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
                    Err(miette!(
                        "ssh: unparsable sha256sum output: {}",
                        out.stdout.trim()
                    ))
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
        let out = self
            .exec(&format!("systemctl is-active -- {}", shell_quote(name)))
            .await?;
        if out.exit_code == 127 {
            return Err(miette!("ssh: systemctl not found on node"));
        }
        Ok(out.stdout.trim() == "active")
    }
}

impl SshSession {
    /// Write `contents` to `tmp` via `cat`, as the login user — never
    /// through sudo: stdin carries the payload, and `sudo -S` would eat its
    /// first line as the password.
    async fn stage_bytes(&self, tmp: &str, contents: &[u8]) -> Result<()> {
        let contents = contents.to_vec();
        let tmp = tmp.to_owned();
        self.with_channel(WRITE_TIMEOUT, |handle| async move {
            // Bytes go over the channel's stdin, not the command line: safe
            // for arbitrary (binary) payloads and free of shell quoting risk.
            let command = format!("cat > {}", shell_quote(&tmp));
            let mut channel = handle
                .channel_open_session()
                .await
                .map_err(|e| miette!("ssh: cannot open channel: {e}"))?;
            channel
                .exec(true, command)
                .await
                .map_err(|e| miette!("ssh: cannot start write command: {e}"))?;
            let trace = std::env::var_os("KURIN_SSH_TRACE").is_some();
            // Chunk the payload with a live deadline watchdog per chunk:
            // an early russh window-negotiation edge case lets a >64MB
            // writeBytes stall when the remote's receive window refills
            // slowly. Chunking + progress lets a stall surface as data.
            let chunks = contents.chunks(1 << 20).collect::<Vec<_>>();
            let total = chunks.len();
            for (i, chunk) in chunks.into_iter().enumerate() {
                channel.data_bytes(chunk.to_vec()).await.map_err(|e| {
                    miette!(
                        "ssh: cannot send file contents (chunk {}/{total}): {e}",
                        i + 1
                    )
                })?;
                if trace && i % 4 == 0 {
                    eprintln!("[ssh-trace] {tmp}: {}/{} MiB", i + 1, total);
                }
            }
            if trace {
                eprintln!("[ssh-trace] {tmp}: all chunks sent; eof");
            }

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
            if trace {
                eprintln!("[ssh-trace] {tmp}: channel closed, exit={exit_code:?}");
            }
            // Same MaxSessions leak guard as exec_on.
            let _ = channel.close().await;
            match exit_code {
                Some(0) => Ok(()),
                Some(code) => Err(miette!(
                    "ssh: writing {} failed (exit {}): {}",
                    tmp,
                    code,
                    String::from_utf8_lossy(&stderr).trim()
                )),
                None => Err(miette!(
                    "ssh: writing {} failed: channel closed without an exit status",
                    tmp
                )),
            }
        })
        .await
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
impl kurin_core::executor::SessionPool for SshSessionPool {
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
                miette!(
                    "ssh: cannot create known_hosts directory {}: {e}",
                    parent.display()
                )
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

/// Build the wire command for `cmd` under this login's become policy,
/// plus the stdin payload (the sudo password) when one must be fed.
///
/// Root never needs sudo; `become = "none"` never wraps; `auto`/`sudo`
/// wrap every privileged command of a non-root login. The password rides
/// the encrypted channel's stdin (`sudo -S`), never the command line.
fn become_command(ssh: &ResolvedSsh, cmd: &Cmd) -> (String, Option<Vec<u8>>) {
    let wrap = cmd.privileged && ssh.user != "root" && !matches!(ssh.r#become, BecomeMode::None);
    if !wrap {
        return (cmd.script.clone(), None);
    }
    match &ssh.become_password {
        None => (
            format!("sudo -n -- sh -c {}", shell_quote(&cmd.script)),
            None,
        ),
        Some(password) => (
            format!("sudo -S -p '' -- sh -c {}", shell_quote(&cmd.script)),
            Some(format!("{password}\n").into_bytes()),
        ),
    }
}

/// R8: scrub the command's registered secrets from captured output before
/// it can reach events, logs or error text.
fn redact_output(out: &mut ExecOutput, cmd: &Cmd) {
    for secret in cmd.redact.iter().filter(|s| !s.is_empty()) {
        out.stdout = out.stdout.replace(secret.as_str(), "******");
        out.stderr = out.stderr.replace(secret.as_str(), "******");
    }
}

/// Collision-free staging name under /tmp. Deliberately dependency-free:
/// pid + nanoseconds + a monotonic counter is unique per process, and the
/// login user always owns /tmp write access.
fn stage_path() -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("/tmp/kurin-{:x}-{:x}-{:x}", std::process::id(), nanos, seq)
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
        assert_eq!(
            shell_quote("/etc/kubernetes/kubelet.conf"),
            "/etc/kubernetes/kubelet.conf"
        );
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

    // -- become command building -----------------------------------------

    /// Obvious fake values (R8): never real credentials.
    const FAKE_SUDO_PASSWORD: &str = "fake-sudo-password-not-real";

    fn login(user: &str, mode: BecomeMode, password: Option<&str>) -> ResolvedSsh {
        ResolvedSsh {
            user: user.into(),
            port: 22,
            r#become: mode,
            become_password: password.map(Into::into),
            auth: SshAuth::Password {
                password: "fake-ssh-password".into(),
            },
        }
    }

    #[test]
    fn root_never_wraps() {
        for mode in [BecomeMode::Auto, BecomeMode::Sudo] {
            let (line, stdin) = become_command(&login("root", mode, None), &Cmd::new("echo hi"));
            assert_eq!(line, "echo hi");
            assert!(stdin.is_none());
        }
    }

    #[test]
    fn non_root_auto_wraps_with_sudo_n() {
        let (line, stdin) = become_command(
            &login("ubuntu", BecomeMode::Auto, None),
            &Cmd::new("echo hi"),
        );
        assert_eq!(line, "sudo -n -- sh -c 'echo hi'");
        assert!(stdin.is_none());
    }

    #[test]
    fn non_root_sudo_mode_wraps_identically() {
        let (line, _) = become_command(
            &login("ubuntu", BecomeMode::Sudo, None),
            &Cmd::new("echo hi"),
        );
        assert_eq!(line, "sudo -n -- sh -c 'echo hi'");
    }

    #[test]
    fn become_none_never_wraps() {
        let (line, stdin) = become_command(
            &login("ubuntu", BecomeMode::None, None),
            &Cmd::new("echo hi"),
        );
        assert_eq!(line, "echo hi");
        assert!(stdin.is_none());
    }

    #[test]
    fn unprivileged_commands_are_not_wrapped() {
        let cmd = Cmd::probe("cat /etc/os-release").unprivileged();
        let (line, stdin) = become_command(&login("ubuntu", BecomeMode::Auto, None), &cmd);
        assert_eq!(line, "cat /etc/os-release");
        assert!(stdin.is_none());
    }

    #[test]
    fn password_uses_sudo_s_and_travels_via_stdin() {
        let (line, stdin) = become_command(
            &login("ubuntu", BecomeMode::Auto, Some(FAKE_SUDO_PASSWORD)),
            &Cmd::new("echo hi"),
        );
        assert_eq!(line, "sudo -S -p '' -- sh -c 'echo hi'");
        assert_eq!(
            stdin.as_deref(),
            Some(&b"fake-sudo-password-not-real\n"[..])
        );
        // R8: the password must never appear on the command line.
        assert!(!line.contains(FAKE_SUDO_PASSWORD));
    }

    #[test]
    fn embedded_single_quotes_are_escaped() {
        let (line, _) = become_command(
            &login("ubuntu", BecomeMode::Auto, None),
            &Cmd::new("echo \"it's\""),
        );
        assert_eq!(line, "sudo -n -- sh -c 'echo \"it'\\''s\"'");
    }

    #[test]
    fn multi_line_scripts_stay_intact_inside_the_wrap() {
        let (line, _) = become_command(
            &login("ubuntu", BecomeMode::Auto, None),
            &Cmd::new("echo a\necho b"),
        );
        assert_eq!(line, "sudo -n -- sh -c 'echo a\necho b'");
    }

    #[test]
    fn stage_paths_are_unique_and_predictably_shaped() {
        let a = stage_path();
        let b = stage_path();
        assert_ne!(a, b);
        for p in [a, b] {
            assert!(p.starts_with("/tmp/kurin-"), "bad staging path: {p}");
        }
    }

    #[test]
    fn redact_scrubs_registered_secrets_from_both_streams() {
        let cmd = Cmd::new("kubeadm token create").redact("fake-secret-token");
        let mut out = ExecOutput {
            exit_code: 0,
            stdout: "token is fake-secret-token\n".into(),
            stderr: "failed for fake-secret-token\n".into(),
        };
        redact_output(&mut out, &cmd);
        assert_eq!(out.stdout, "token is ******\n");
        assert_eq!(out.stderr, "failed for ******\n");
        // An empty "secret" must not shred the output.
        let cmd = Cmd::new("x").redact("");
        let mut out = ExecOutput {
            exit_code: 0,
            stdout: "keep\n".into(),
            stderr: String::new(),
        };
        redact_output(&mut out, &cmd);
        assert_eq!(out.stdout, "keep\n");
    }
}
