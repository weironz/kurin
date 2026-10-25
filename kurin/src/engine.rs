//! Engine contract (W0). The executor sees only these traits plus the
//! plan graph; SSH/artifact/CLI live behind them.

use std::sync::Arc;

use miette::Result;
use serde::Serialize;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub enum Stage {
    Preflight,
    Prepare,
    Bootstrap,
    Join,
    Cni,
    Post,
}

impl Stage {
    pub fn label(self) -> &'static str {
        match self {
            Stage::Preflight => "preflight",
            Stage::Prepare => "prepare",
            Stage::Bootstrap => "bootstrap",
            Stage::Join => "join",
            Stage::Cni => "cni",
            Stage::Post => "post",
        }
    }
}

/// A unit of work on one node. check_state -> apply -> verify, always.
#[async_trait::async_trait]
pub trait Task: Send + Sync {
    /// Stable identity: `<stage>/<kind>@<node>`, used for events and resume.
    fn id(&self) -> String;
    fn stage(&self) -> Stage;
    fn node(&self) -> &str;
    /// One-line human summary for plan rendering.
    fn describe(&self) -> String;
    async fn check(&self, session: &dyn Session) -> Result<Drift>;
    /// Execute the change. Returning `Unknown` on transport errors is
    /// mandatory; the engine re-probes before any retry.
    async fn apply(&self, session: &dyn Session) -> Result<Outcome>;
}

/// Remote session abstraction: the only way tasks touch a node.
/// Implemented by russh (real) and MockSession (tests / dry-run --check).
#[async_trait::async_trait]
pub trait Session: Send + Sync {
    /// Run a command to completion.
    async fn exec(&self, command: &str) -> Result<ExecOutput>;
    /// Write a file (mkdir -p the parents, overwrite).
    async fn write_file(&self, path: &str, contents: &[u8]) -> Result<()>;
    /// sha256 of a remote file, or None if it does not exist.
    async fn file_sha256(&self, path: &str) -> Result<Option<String>>;
    /// `systemctl is-active <name>` == active.
    async fn service_active(&self, name: &str) -> Result<bool>;
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
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum Event {
    TaskStart { id: String, node: String, stage: &'static str },
    TaskSkipped { id: String, node: String, stage: &'static str },
    TaskDone { id: String, node: String, stage: &'static str, ms: u128 },
    TaskFailed { id: String, node: String, stage: &'static str, reason: String },
    /// Unknown outcome re-probed as in-sync.
    TaskReconciled { id: String, node: String, stage: &'static str },
    StageDone { stage: &'static str },
    ClusterReady,
}
