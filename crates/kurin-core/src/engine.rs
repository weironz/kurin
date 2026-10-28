//! Engine contract (W0). The executor sees only these traits plus the
//! plan graph; SSH/artifact/CLI live behind them.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use miette::Result;
use serde::Serialize;
use sha2::{Digest, Sha256};

/// The SSH connection died while a mutating operation was in flight.
///
/// The remote side may or may not have executed the command, so the engine
/// must re-probe instead of retrying. Read-only probes never produce this:
/// they are retried once transparently.
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
#[error("transport lost during `{operation}`: {reason}")]
#[diagnostic(
    code(kurin::transport_lost),
    help("the remote state is unknown; re-probe the node before retrying")
)]
pub struct TransportLost {
    /// What was being attempted (e.g. `exec` on the node label).
    pub operation: String,
    /// Underlying connection error.
    pub reason: String,
}

impl TransportLost {
    pub fn new(operation: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            operation: operation.into(),
            reason: reason.into(),
        }
    }
}

/// A local TCP listener tunnelled to a remote address (`ssh -L`).
///
/// Dropping it stops accepting new connections and aborts the listener task;
/// already-forwarded connections close when their copy loop ends.
pub struct LocalForward {
    /// The bound loopback address clients connect to.
    pub local_addr: SocketAddr,
    stop: Option<tokio::task::AbortHandle>,
}

impl LocalForward {
    pub fn new(local_addr: SocketAddr, stop: tokio::task::AbortHandle) -> Self {
        Self {
            local_addr,
            stop: Some(stop),
        }
    }

    /// Stop the listener now (also runs on drop).
    pub fn stop(&mut self) {
        if let Some(handle) = self.stop.take() {
            handle.abort();
        }
    }
}

impl Drop for LocalForward {
    fn drop(&mut self) {
        self.stop();
    }
}

/// A remote TCP listener tunnelled to a local address (`ssh -R`).
#[derive(Debug, Clone)]
pub struct RemoteForward {
    /// Port the server assigned on its loopback interface.
    pub remote_port: u16,
}

/// Result of a remote probe: is the node already in the desired state?
#[derive(Debug, Clone)]
pub enum Drift {
    /// check_state matched: apply would be a no-op.
    InSync,
    /// Needs change; carries the human reason shown in dry-run --check.
    Drifted(String),
}

/// Result of an apply. Distinguishes failure from *unknown*: transport
/// loss means the remote op may or may not have happened.
#[derive(Debug, Clone)]
pub enum Outcome {
    /// apply succeeded and post-verify passed.
    Verified,
    /// Transport dropped mid-apply; remote state is unknown. The engine
    /// must re-probe, never blind-retry.
    Unknown(String),
}

/// A remote command plus the metadata the real transport needs to run it
/// safely: whether it must run escalated (sudo for non-root logins), whether
/// it mutates the node, how long it may run, and which secret values must
/// never survive into its captured output (R8).
#[derive(Debug, Clone)]
pub struct Cmd {
    pub script: String,
    /// Run escalated: the transport wraps this in sudo when the login
    /// user is not root and `[ssh] become` allows it.
    pub privileged: bool,
    /// False for read-only probes (reserved for the transport-loss retry
    /// policy; mutating commands must never be retried blindly).
    pub mutating: bool,
    /// Per-command timeout; `None` uses the transport default.
    pub timeout: Option<Duration>,
    /// Secret values scrubbed from stdout/stderr after execution.
    pub redact: Vec<String>,
    /// Bytes delivered on the command's stdin before EOF (e.g. a manifest
    /// for `kubectl apply -f -`). `None` closes stdin immediately.
    pub stdin: Option<Vec<u8>>,
}

impl Cmd {
    /// A privileged, mutating command — the safe default: managing the
    /// node is what kurin does.
    pub fn new(script: impl Into<String>) -> Self {
        Self {
            script: script.into(),
            privileged: true,
            mutating: true,
            timeout: None,
            redact: Vec::new(),
            stdin: None,
        }
    }

    /// A privileged, read-only probe.
    pub fn probe(script: impl Into<String>) -> Self {
        Self {
            script: script.into(),
            privileged: true,
            mutating: false,
            timeout: None,
            redact: Vec::new(),
            stdin: None,
        }
    }

    /// Run as the login user, without escalation (host introspection).
    pub fn unprivileged(mut self) -> Self {
        self.privileged = false;
        self
    }

    /// Mark the command read-only.
    pub fn readonly(mut self) -> Self {
        self.mutating = false;
        self
    }

    /// Bound the command's runtime.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Register a secret that must be scrubbed from captured output.
    pub fn redact(mut self, secret: impl Into<String>) -> Self {
        self.redact.push(secret.into());
        self
    }

    /// Deliver the given bytes on the command's stdin, then close it.
    pub fn stdin(mut self, bytes: impl Into<Vec<u8>>) -> Self {
        self.stdin = Some(bytes.into());
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub enum Stage {
    /// Node facts collection; runs before plan construction — the task
    /// recorded here only displays what was probed.
    Discover,
    Preflight,
    Artifacts,
    Prepare,
    /// Private registry install/trust/seed (only when role = "registry").
    Registry,
    /// Seed container images (offline installs only; online pulls them).
    Images,
    /// kube-vip static pod manifests on every control-plane node
    /// (must precede init: kubeadm binds the VIP as controlPlaneEndpoint).
    Vip,
    Bootstrap,
    /// VIP-backed API is actually serving before any join attempt.
    ApiReady,
    Join,
    Cni,
    /// Declarative addons (WP-43): one `addons/<name>` task per enabled
    /// addon, converged by the addon engine.
    Addons,
    Post,
    /// Teardown: drain and delete the node object through the anchor
    /// control plane.
    Drain,
    /// Teardown: the distribution's reset (`kubeadm reset` / k3s uninstall).
    Reset,
    /// Teardown: stop services and remove artifacts, CNI and iptables state.
    Cleanup,
    /// Teardown: control-machine state (run state, blackboard, join cache).
    Local,
    /// Upgrade: per-node kubelet version gate.
    VersionCheck,
    /// Upgrade: mark the node unschedulable.
    Cordon,
    /// Upgrade: install the target kubeadm/kubelet/kubectl binaries.
    InstallBins,
    /// Upgrade: `kubeadm upgrade apply` (first control plane) or
    /// `kubeadm upgrade node` (every other node).
    KubeadmUpgrade,
    /// Upgrade: restart kubelet and confirm it is active.
    RestartKubelet,
    /// Upgrade: wait for the node's Ready condition.
    ReadyGate,
    /// Upgrade: mark the node schedulable again.
    Uncordon,
    /// Upgrade (k3s): stop the node's k3s unit before the binary swap.
    StopService,
    /// Upgrade (k3s): start the node's k3s unit after the binary swap.
    StartService,
    /// Certs renew: `kubeadm certs renew all` / `k3s certificate rotate`.
    CertsRenew,
    /// Certs renew: cycle the static control-plane pods (or restart k3s).
    RestartStaticPods,
    /// Certs renew: wait for the local apiserver `/readyz`.
    ApiReadyGate,
}

impl Stage {
    pub fn label(self) -> &'static str {
        match self {
            Stage::Discover => "discover",
            Stage::Preflight => "preflight",
            Stage::Artifacts => "artifacts",
            Stage::Prepare => "prepare",
            Stage::Registry => "registry",
            Stage::Images => "images",
            Stage::Vip => "vip",
            Stage::Bootstrap => "bootstrap",
            Stage::ApiReady => "api-ready",
            Stage::Join => "join",
            Stage::Cni => "cni",
            Stage::Addons => "addons",
            Stage::Post => "post",
            Stage::Drain => "drain",
            Stage::Reset => "reset",
            Stage::Cleanup => "cleanup",
            Stage::Local => "local",
            Stage::VersionCheck => "version-check",
            Stage::Cordon => "cordon",
            Stage::InstallBins => "install-bins",
            Stage::KubeadmUpgrade => "kubeadm-upgrade",
            Stage::RestartKubelet => "restart-kubelet",
            Stage::ReadyGate => "ready-gate",
            Stage::Uncordon => "uncordon",
            Stage::StopService => "stop-service",
            Stage::StartService => "start-service",
            Stage::CertsRenew => "renew",
            Stage::RestartStaticPods => "restart-static-pods",
            Stage::ApiReadyGate => "apiready-gate",
        }
    }

    /// Inverse of [`Stage::label`]; `None` for an unknown phase name.
    pub fn from_label(label: &str) -> Option<Self> {
        Some(match label {
            "discover" => Stage::Discover,
            "preflight" => Stage::Preflight,
            "artifacts" => Stage::Artifacts,
            "prepare" => Stage::Prepare,
            "registry" => Stage::Registry,
            "images" => Stage::Images,
            "vip" => Stage::Vip,
            "bootstrap" => Stage::Bootstrap,
            "api-ready" => Stage::ApiReady,
            "join" => Stage::Join,
            "cni" => Stage::Cni,
            "addons" => Stage::Addons,
            "post" => Stage::Post,
            "drain" => Stage::Drain,
            "reset" => Stage::Reset,
            "cleanup" => Stage::Cleanup,
            "local" => Stage::Local,
            "version-check" => Stage::VersionCheck,
            "cordon" => Stage::Cordon,
            "install-bins" => Stage::InstallBins,
            "kubeadm-upgrade" => Stage::KubeadmUpgrade,
            "restart-kubelet" => Stage::RestartKubelet,
            "ready-gate" => Stage::ReadyGate,
            "uncordon" => Stage::Uncordon,
            "stop-service" => Stage::StopService,
            "start-service" => Stage::StartService,
            "renew" => Stage::CertsRenew,
            "restart-static-pods" => Stage::RestartStaticPods,
            "apiready-gate" => Stage::ApiReadyGate,
            _ => return None,
        })
    }
}

/// What a task failure does to the rest of the plan (ADR-02).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FailurePolicy {
    /// The default: the operation fails, transitive successors are blocked
    /// and never scheduled.
    #[default]
    Strict,
    /// The failure is reported as `TaskWarn` and successors still run
    /// (teardown steps that used to end in `; true`).
    BestEffort,
}

/// Where a task executes.
///
/// Most tasks run on a cluster node through its SSH session. Registry
/// distribution pushes from the *control machine* (the host running
/// `kurin`), which has no SSH session of its own; the executor resolves
/// [`Target::Controller`] to the reserved [`CONTROLLER_NODE`] label and
/// the caller's pool must answer it with a local session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Target {
    /// A cluster node (`Task::node` names it).
    #[default]
    Node,
    /// The control machine running kurin.
    Controller,
}

/// Reserved node label for [`Target::Controller`] tasks.
pub const CONTROLLER_NODE: &str = "controller";

/// A unit of work on one node. check_state -> apply -> verify, always.
#[async_trait::async_trait]
pub trait Task: Send + Sync {
    /// Stable identity: `<stage>/<kind>@<node>`, used for events and resume.
    fn id(&self) -> String;
    fn stage(&self) -> Stage;
    fn node(&self) -> &str;
    /// Where the task runs. Defaults to a cluster node.
    fn target(&self) -> Target {
        Target::Node
    }
    /// One-line human summary for plan rendering.
    fn describe(&self) -> String;
    /// What a failure means for the rest of the plan. Defaults to
    /// [`FailurePolicy::Strict`].
    fn failure_policy(&self) -> FailurePolicy {
        FailurePolicy::Strict
    }
    async fn check(&self, session: &dyn Session) -> Result<Drift>;
    /// Execute the change. Returning `Unknown` on transport errors is
    /// mandatory; the engine re-probes before any retry.
    async fn apply(&self, session: &dyn Session) -> Result<Outcome>;
}

/// Which stream a remote output line came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Stream {
    Stdout,
    Stderr,
}

/// Where an upload's bytes come from.
#[derive(Debug, Clone)]
pub enum UploadSource {
    /// A local file, streamed in chunks by the real transport.
    File(PathBuf),
    /// An in-memory payload (small configs, rendered manifests).
    Bytes(Vec<u8>),
    /// A content-addressed payload from the local cache or a package
    /// (WP-35, 04 §5.1): [`BlobSource::path`] is streamed in bounded
    /// chunks by the real transport, never read whole into memory, and
    /// the transfer is checked against [`BlobSource::digest`].
    Blob(BlobSource),
}

/// One verified payload blob a node needs.
///
/// The path points at a file in the local content-addressed store (or a
/// file inside an opened package layout); `digest` and `size` come from
/// that store's verified metadata, so the transport can check the streamed
/// bytes against the pin and a corrupted cache is caught before the bytes
/// reach a node.
#[derive(Debug, Clone)]
pub struct BlobSource {
    /// Local file holding the payload.
    pub path: PathBuf,
    /// Expected payload digest, `sha256:<hex>` (a bare hex digest is
    /// accepted too).
    pub digest: String,
    /// Payload size in bytes.
    pub size: u64,
}

impl BlobSource {
    /// A blob reference from a local path and its `sha256:<hex>` digest.
    pub fn new(path: impl Into<PathBuf>, digest: impl Into<String>, size: u64) -> Self {
        Self {
            path: path.into(),
            digest: digest.into(),
            size,
        }
    }

    /// The digest as bare lowercase hex (the form `sha256sum` prints).
    pub fn hex(&self) -> &str {
        self.digest.rsplit(':').next().unwrap_or(&self.digest)
    }
}

/// What an upload actually transferred, for logging and verification.
#[derive(Debug, Clone, Serialize)]
pub struct UploadReport {
    /// Number of payload bytes written.
    pub bytes: u64,
    /// Lowercase hex sha256 of the payload, verified against the remote file.
    pub sha256: String,
    /// Wall-clock duration of the transfer.
    pub ms: u64,
}

/// Lowercase hex sha256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex_lower(&hasher.finalize())
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// Remote session abstraction: the only way tasks touch a node.
/// Implemented by russh (real) and MockSession (tests / dry-run --check).
///
/// `exec` is the privileged convenience wrapper: in the real transport it
/// is exactly `exec_cmd(&Cmd::new(command))`, i.e. every command runs
/// escalated (sudo for non-root logins). Probes that must observe the node
/// as the login user — precheck, preflight — use
/// `exec_cmd(&Cmd::probe(..).unprivileged())` instead.
#[async_trait::async_trait]
pub trait Session: Send + Sync {
    /// Run a command to completion, escalated.
    async fn exec(&self, command: &str) -> Result<ExecOutput>;
    /// Run a command with its full metadata (become, timeout, redaction).
    /// The default runs the raw script through `exec`, so mock transports
    /// keep compiling and behave exactly as before.
    async fn exec_cmd(&self, cmd: &Cmd) -> Result<ExecOutput> {
        self.exec(&cmd.script).await
    }
    /// Run a command and report its output line by line as it arrives.
    ///
    /// The default waits for `exec_cmd` to finish and then replays the
    /// captured lines, so mock transports keep compiling and behave exactly
    /// as before; the real SSH transport emits each line as it is read.
    /// `cmd.redact` secrets must not reach the sink.
    async fn exec_stream(
        &self,
        cmd: &Cmd,
        sink: &mut (dyn for<'a> FnMut(Stream, &'a str) + Send),
    ) -> Result<ExecOutput> {
        let out = self.exec_cmd(cmd).await?;
        for line in out.stdout.lines() {
            sink(Stream::Stdout, line);
        }
        for line in out.stderr.lines() {
            sink(Stream::Stderr, line);
        }
        Ok(out)
    }
    /// Write a file (mkdir -p the parents, overwrite) with mode 0644.
    async fn write_file(&self, path: &str, contents: &[u8]) -> Result<()>;
    /// Write a file with an explicit mode (octal, e.g. `0o600`). The
    /// default ignores the mode and defers to `write_file`.
    async fn write_file_mode(&self, path: &str, contents: &[u8], mode: u32) -> Result<()> {
        let _ = mode;
        self.write_file(path, contents).await
    }
    /// Upload `src` to `dst` with `mode`, verifying the remote digest.
    ///
    /// The default reads the whole source into memory and defers to
    /// `write_file_mode`, so mock transports keep working; the real SSH
    /// transport streams files in 64 KiB chunks and checks `sha256sum`
    /// on the remote side before installing. A [`UploadSource::Blob`]
    /// carries its digest and size, which are verified here (mock path)
    /// before anything is written.
    async fn upload(&self, src: UploadSource, dst: &str, mode: u32) -> Result<UploadReport> {
        let started = Instant::now();
        let bytes = match src {
            UploadSource::File(path) => tokio::fs::read(&path)
                .await
                .map_err(|e| miette::miette!("read upload source {}: {e}", path.display()))?,
            UploadSource::Bytes(bytes) => bytes,
            UploadSource::Blob(blob) => {
                let bytes = tokio::fs::read(&blob.path).await.map_err(|e| {
                    miette::miette!("read upload source {}: {e}", blob.path.display())
                })?;
                if bytes.len() as u64 != blob.size {
                    return Err(miette::miette!(
                        "blob {} is {} bytes, metadata says {}",
                        blob.path.display(),
                        bytes.len(),
                        blob.size
                    ));
                }
                let actual = sha256_hex(&bytes);
                if actual != blob.hex() {
                    return Err(miette::miette!(
                        "blob {} digest mismatch: expected sha256:{}, got sha256:{actual}",
                        blob.path.display(),
                        blob.hex()
                    ));
                }
                bytes
            }
        };
        let sha256 = sha256_hex(&bytes);
        self.write_file_mode(dst, &bytes, mode).await?;
        Ok(UploadReport {
            bytes: bytes.len() as u64,
            sha256,
            ms: started.elapsed().as_millis() as u64,
        })
    }
    /// sha256 of a remote file, or None if it does not exist.
    async fn file_sha256(&self, path: &str) -> Result<Option<String>>;
    /// `systemctl is-active <name>` == active.
    async fn service_active(&self, name: &str) -> Result<bool>;
    /// Listen on `127.0.0.1:0` and tunnel each accepted connection to
    /// `remote_addr` through the node (`ssh -L`). The returned handle owns
    /// the listener; dropping it stops accepting.
    async fn forward_local(&self, _remote_addr: SocketAddr) -> Result<LocalForward> {
        Err(miette::miette!(
            "ssh: this transport does not support local forwarding"
        ))
    }
    /// Ask the node to listen on `127.0.0.1:0` and tunnel connections back to
    /// `local_addr` (`ssh -R`). Returns the port the server assigned.
    async fn forward_remote(&self, _local_addr: SocketAddr) -> Result<RemoteForward> {
        Err(miette::miette!(
            "ssh: this transport does not support remote forwarding"
        ))
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ExecOutput {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl ExecOutput {
    pub fn ok(&self) -> bool {
        self.exit_code == 0
    }
}

/// A task plus its runtime verdicts, ready for rendering or execution.
#[derive(Clone)]
pub struct PlannedTask {
    pub task: Arc<dyn Task>,
    /// Set by `--dry-run --check` probes.
    pub drift: Option<Drift>,
}

/// Structured events; emitted as one JSON object per line on `--json`.
///
/// This is the frozen v1 wire shape (docs/v1.0-gates.md §2). `ts` is the
/// only addition: the envelope's UTC-millisecond timestamp, so v1 consumers
/// can order events without parsing the v2 envelope.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum Event {
    TaskStart {
        id: String,
        node: String,
        stage: &'static str,
        ts: u64,
    },
    TaskSkipped {
        id: String,
        node: String,
        stage: &'static str,
        /// Why the task did not run (`blocked by <key>`); absent for a task
        /// that was already in sync.
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        ts: u64,
    },
    TaskDone {
        id: String,
        node: String,
        stage: &'static str,
        ms: u128,
        ts: u64,
    },
    TaskFailed {
        id: String,
        node: String,
        stage: &'static str,
        reason: String,
        ts: u64,
    },
    /// Unknown outcome re-probed as in-sync.
    TaskReconciled {
        id: String,
        node: String,
        stage: &'static str,
        ts: u64,
    },
    StageDone {
        stage: &'static str,
        ts: u64,
    },
    ClusterReady {
        ts: u64,
    },
}

impl Event {
    /// Stable kebab-case discriminant, matching the JSON `type` field.
    pub fn kind(&self) -> &'static str {
        match self {
            Event::TaskStart { .. } => "task-start",
            Event::TaskSkipped { .. } => "task-skipped",
            Event::TaskDone { .. } => "task-done",
            Event::TaskFailed { .. } => "task-failed",
            Event::TaskReconciled { .. } => "task-reconciled",
            Event::StageDone { .. } => "stage-done",
            Event::ClusterReady { .. } => "cluster-ready",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// The shape every mock transport in the workspace has: only `exec`
    /// (plus the other required methods), no knowledge of `Cmd`.
    struct RecordingSession {
        scripts: Mutex<Vec<String>>,
        files: Mutex<Vec<String>>,
        written: Mutex<Vec<(String, Vec<u8>)>>,
        output: Mutex<ExecOutput>,
    }

    impl RecordingSession {
        fn new() -> Self {
            Self {
                scripts: Mutex::new(Vec::new()),
                files: Mutex::new(Vec::new()),
                written: Mutex::new(Vec::new()),
                output: Mutex::new(ExecOutput {
                    exit_code: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                }),
            }
        }

        fn with_output(stdout: &str, stderr: &str) -> Self {
            let session = Self::new();
            *session.output.lock().unwrap() = ExecOutput {
                exit_code: 0,
                stdout: stdout.to_string(),
                stderr: stderr.to_string(),
            };
            session
        }
    }

    #[async_trait::async_trait]
    impl Session for RecordingSession {
        async fn exec(&self, command: &str) -> Result<ExecOutput> {
            self.scripts.lock().unwrap().push(command.to_string());
            Ok(self.output.lock().unwrap().clone())
        }
        async fn write_file(&self, path: &str, contents: &[u8]) -> Result<()> {
            self.files.lock().unwrap().push(path.to_string());
            self.written
                .lock()
                .unwrap()
                .push((path.to_string(), contents.to_vec()));
            Ok(())
        }
        async fn file_sha256(&self, _path: &str) -> Result<Option<String>> {
            Ok(None)
        }
        async fn service_active(&self, _name: &str) -> Result<bool> {
            Ok(false)
        }
    }

    #[test]
    fn cmd_constructors_and_builders() {
        let c = Cmd::new("s");
        assert!(c.privileged && c.mutating && c.timeout.is_none() && c.redact.is_empty());
        let p = Cmd::probe("s");
        assert!(p.privileged && !p.mutating && p.timeout.is_none());
        let u = Cmd::probe("s").unprivileged();
        assert!(!u.privileged && !u.mutating);
        let r = Cmd::new("s").readonly();
        assert!(r.privileged && !r.mutating);
        let t = Cmd::new("s").with_timeout(Duration::from_secs(5));
        assert_eq!(t.timeout, Some(Duration::from_secs(5)));
        let d = Cmd::new("s").redact("fake-secret");
        assert_eq!(d.redact, vec!["fake-secret".to_string()]);
    }

    /// The provided `exec_cmd` must hand the raw script to `exec`, so the
    /// existing mock transports keep working unchanged.
    #[tokio::test]
    async fn exec_cmd_default_runs_raw_script_via_exec() {
        let s = RecordingSession::new();
        let out = s.exec_cmd(&Cmd::new("uname -a")).await.unwrap();
        assert!(out.ok());
        assert_eq!(
            *s.scripts.lock().unwrap(),
            vec!["uname -a".to_string()],
            "exec_cmd must delegate to exec with the script verbatim"
        );
    }

    /// The provided `write_file_mode` must defer to `write_file` (mode
    /// handling belongs to the real transport).
    #[tokio::test]
    async fn write_file_mode_default_defers_to_write_file() {
        let s = RecordingSession::new();
        s.write_file_mode("/etc/kurin/x.conf", b"body", 0o600)
            .await
            .unwrap();
        assert_eq!(
            *s.files.lock().unwrap(),
            vec!["/etc/kurin/x.conf".to_string()]
        );
    }

    /// The default `exec_stream` replays `exec`'s captured lines, stream by
    /// stream, and still returns the full output.
    #[tokio::test]
    async fn exec_stream_default_replays_exec_lines() {
        let s = RecordingSession::with_output("one\ntwo\n", "warn\n");
        let mut seen: Vec<(Stream, String)> = Vec::new();
        let out = s
            .exec_stream(&Cmd::new("x"), &mut |stream, line| {
                seen.push((stream, line.to_string()));
            })
            .await
            .unwrap();
        assert_eq!(
            seen,
            vec![
                (Stream::Stdout, "one".to_string()),
                (Stream::Stdout, "two".to_string()),
                (Stream::Stderr, "warn".to_string()),
            ]
        );
        assert_eq!(out.stdout, "one\ntwo\n");
        assert_eq!(out.stderr, "warn\n");
    }

    /// The default `upload` writes the bytes through `write_file_mode` and
    /// reports the locally computed digest.
    #[tokio::test]
    async fn upload_default_writes_bytes_and_reports_sha256() {
        let s = RecordingSession::new();
        let report = s
            .upload(
                UploadSource::Bytes(b"hello".to_vec()),
                "/etc/kurin/x",
                0o600,
            )
            .await
            .unwrap();
        assert_eq!(report.bytes, 5);
        assert_eq!(
            report.sha256,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
        assert_eq!(
            *s.written.lock().unwrap(),
            vec![("/etc/kurin/x".to_string(), b"hello".to_vec())]
        );
    }

    #[test]
    fn sha256_hex_matches_known_vector() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}
