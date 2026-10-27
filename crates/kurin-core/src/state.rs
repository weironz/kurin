//! Resume state: task-level "done" records persisted per cluster.
//!
//! Path: `$XDG_CACHE_HOME/kurin/runs/<cluster>.json` (or
//! `~/.cache/kurin/runs/<cluster>.json`). The file is advisory, never
//! authoritative: remote probes (`Task::check`) decide whether work is
//! skipped, the records only let `--resume` distinguish "this cluster was
//! already installed by us" from "first attempt".
//!
//! Schema v2 carries the operation id, the plan fingerprint and a mirror of
//! the Blackboard's persistent values:
//!
//! ```json
//! { "version": 2, "cluster": "tier1", "op_id": "…",
//!   "plan_fingerprint": "…", "done": [{"key": "…", "finished_at": 0}],
//!   "blackboard": {"kubeadm.join": {…}} }
//! ```
//!
//! The Blackboard's own `<cluster>.bb.json` (see
//! [`crate::blackboard`]) remains the authoritative store for
//! `Persist::RunState*` values; the `blackboard` field here is a
//! write-through mirror so the run record is self-describing and a future
//! reader does not need two files. Version 1 files (`tasks` map, no
//! fingerprint) are still read: they resume with an empty fingerprint, so
//! the fingerprint check is skipped for them.
//!
//! Writes are eager and atomic (write-ahead temp file + rename) *after every
//! completed task*, so a SIGINT / SIGKILL between tasks always leaves a
//! consistent snapshot on disk. The file is mode 0600 because the mirrored
//! Blackboard values may be secrets. `flush` is also called from `Drop` as a
//! best-effort safety net.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use miette::{Context, IntoDiagnostic, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// On-disk schema version. Bumped when records change incompatibly; newer
/// files are rejected rather than silently misread.
pub const STATE_VERSION: u32 = 2;

/// Schema version of files written before the fingerprint/blackboard fields
/// existed. Read-only compatibility: such files resume with no fingerprint.
pub const LEGACY_STATE_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TaskState {
    Done,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRecord {
    pub state: TaskState,
    /// Unix seconds at which the task reached its terminal state.
    pub finished_at: u64,
}

/// One completed task in the v2 `done` list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoneTask {
    /// Stable task id (`<stage>/<kind>@<node>`).
    pub key: String,
    /// Unix seconds at which the task reached its terminal state.
    pub finished_at: u64,
}

/// On-disk v2 shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateFile {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub cluster: String,
    #[serde(default)]
    pub op_id: String,
    #[serde(default)]
    pub plan_fingerprint: String,
    #[serde(default)]
    pub done: Vec<DoneTask>,
    #[serde(default)]
    pub blackboard: BTreeMap<String, Value>,
}

fn default_version() -> u32 {
    STATE_VERSION
}

/// Task-level progress for one run, optionally backed by a file.
#[derive(Debug)]
pub struct RunState {
    path: Option<PathBuf>,
    cluster: String,
    op_id: String,
    plan_fingerprint: String,
    tasks: BTreeMap<String, TaskRecord>,
    blackboard: BTreeMap<String, Value>,
    dirty: bool,
}

impl RunState {
    /// State that is never persisted (default for plain `run`).
    pub fn disabled() -> Self {
        Self {
            path: None,
            cluster: String::new(),
            op_id: String::new(),
            plan_fingerprint: String::new(),
            tasks: BTreeMap::new(),
            blackboard: BTreeMap::new(),
            dirty: false,
        }
    }

    /// Open the state for `cluster` at `path`.
    ///
    /// `resume == true` loads existing records (missing file = empty);
    /// `resume == false` starts empty and immediately overwrites the file, so
    /// a stale resume snapshot can never leak into a fresh install.
    pub fn open(
        path: impl Into<PathBuf>,
        cluster: impl Into<String>,
        resume: bool,
    ) -> Result<Self> {
        let path = path.into();
        let cluster = cluster.into();
        let mut tasks = BTreeMap::new();
        let mut op_id = String::new();
        let mut plan_fingerprint = String::new();
        let mut blackboard = BTreeMap::new();
        if resume && path.exists() {
            let text = fs::read_to_string(&path)
                .into_diagnostic()
                .wrap_err_with(|| format!("reading run state {}", path.display()))?;
            let file = parse_state(&text, &path)?;
            if file.version > STATE_VERSION {
                return Err(newer_schema(&path, file.version));
            }
            tasks = file
                .done
                .into_iter()
                .map(|done| {
                    (
                        done.key,
                        TaskRecord {
                            state: TaskState::Done,
                            finished_at: done.finished_at,
                        },
                    )
                })
                .collect();
            op_id = file.op_id;
            plan_fingerprint = file.plan_fingerprint;
            blackboard = file.blackboard;
        }
        let mut state = Self {
            path: Some(path),
            cluster,
            op_id,
            plan_fingerprint,
            tasks,
            blackboard,
            dirty: true,
        };
        if !resume {
            state.flush()?;
        }
        Ok(state)
    }

    /// `~/.cache/kurin/runs/<cluster>.json`, or `None` if no cache dir is known.
    pub fn default_path(cluster: &str) -> Option<PathBuf> {
        let base = std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))?;
        Some(
            base.join("kurin")
                .join("runs")
                .join(format!("{}.json", sanitize(cluster))),
        )
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Record the operation id and plan fingerprint this run belongs to.
    /// Empty values are ignored, so a caller that has no plan keeps the
    /// loaded identity.
    pub fn with_identity(
        mut self,
        op_id: impl Into<String>,
        plan_fingerprint: impl Into<String>,
    ) -> Self {
        let op_id = op_id.into();
        let fingerprint = plan_fingerprint.into();
        if !op_id.is_empty() && op_id != self.op_id {
            self.op_id = op_id;
            self.dirty = true;
        }
        if !fingerprint.is_empty() && fingerprint != self.plan_fingerprint {
            self.plan_fingerprint = fingerprint;
            self.dirty = true;
        }
        self
    }

    /// Operation id stored in the file (empty for v1 files).
    pub fn op_id(&self) -> &str {
        &self.op_id
    }

    /// Plan fingerprint stored in the file (empty for v1 files or runs
    /// without a plan).
    pub fn plan_fingerprint(&self) -> &str {
        &self.plan_fingerprint
    }

    /// Reject a resumed run whose plan no longer matches the stored one.
    ///
    /// A v1 file (or any file written without a plan) has no fingerprint and
    /// is accepted: there is nothing to compare against.
    pub fn check_fingerprint(&self, current: &str) -> Result<()> {
        if self.plan_fingerprint.is_empty() || current.is_empty() {
            return Ok(());
        }
        if self.plan_fingerprint != current {
            return Err(miette::miette!(
                "计划已变化，请去掉 --resume 重新运行（已收敛的任务会被检查跳过）"
            ));
        }
        Ok(())
    }

    /// Replace the mirrored Blackboard snapshot. Marks the state dirty only
    /// when the values actually changed.
    pub fn set_blackboard_snapshot(&mut self, values: BTreeMap<String, Value>) {
        if self.blackboard != values {
            self.blackboard = values;
            self.dirty = true;
        }
    }

    /// Mirrored persistent Blackboard values.
    pub fn blackboard(&self) -> &BTreeMap<String, Value> {
        &self.blackboard
    }

    /// Was this task recorded as done by a previous run?
    pub fn is_done(&self, id: &str) -> bool {
        self.tasks.contains_key(id)
    }

    pub fn done_count(&self) -> usize {
        self.tasks.len()
    }

    /// Record a task as done. Skipped (`check == InSync`) tasks count too:
    /// their desired state is already present on the node.
    pub fn mark_done(&mut self, id: &str) {
        self.tasks.insert(
            id.to_string(),
            TaskRecord {
                state: TaskState::Done,
                finished_at: now_secs(),
            },
        );
        self.dirty = true;
    }

    /// Persist atomically with mode 0600. No-op when nothing changed or when
    /// persistence is disabled. Errors are returned; callers decide whether
    /// they are fatal.
    pub fn flush(&mut self) -> Result<()> {
        let Some(path) = self.path.clone() else {
            self.dirty = false;
            return Ok(());
        };
        if !self.dirty {
            return Ok(());
        }
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(parent)
                .into_diagnostic()
                .wrap_err_with(|| format!("creating run state dir {}", parent.display()))?;
        }
        let file = StateFile {
            version: STATE_VERSION,
            cluster: self.cluster.clone(),
            op_id: self.op_id.clone(),
            plan_fingerprint: self.plan_fingerprint.clone(),
            done: self
                .tasks
                .iter()
                .map(|(key, record)| DoneTask {
                    key: key.clone(),
                    finished_at: record.finished_at,
                })
                .collect(),
            blackboard: self.blackboard.clone(),
        };
        let json = serde_json::to_vec_pretty(&file).into_diagnostic()?;
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(Path::to_path_buf);
        let mut tmp = match &parent {
            Some(dir) => tempfile::NamedTempFile::new_in(dir),
            None => tempfile::NamedTempFile::new(),
        }
        .into_diagnostic()
        .wrap_err("creating temporary run state file")?;
        tmp.write_all(&json)
            .into_diagnostic()
            .wrap_err("writing run state")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tmp.as_file()
                .set_permissions(fs::Permissions::from_mode(0o600))
                .into_diagnostic()
                .wrap_err("setting run state permissions")?;
        }
        tmp.persist(&path)
            .map_err(|e| miette::miette!("persisting run state {}: {e}", path.display()))?;
        self.dirty = false;
        Ok(())
    }
}

impl Drop for RunState {
    fn drop(&mut self) {
        // Best-effort: normal completion and error paths already flush, this
        // covers early returns / panic unwinds.
        if self.dirty {
            let _ = self.flush();
        }
    }
}

fn newer_schema(path: &Path, version: u32) -> miette::Report {
    miette::miette!(
        "run state {} was written by a newer kurin (schema {} > {}); \
         re-run without --resume to start fresh",
        path.display(),
        version,
        STATE_VERSION
    )
}

/// Parse either the v2 shape or the legacy v1 `tasks` map.
fn parse_state(text: &str, path: &Path) -> Result<StateFile> {
    let raw: Value = serde_json::from_str(text)
        .into_diagnostic()
        .wrap_err_with(|| format!("parsing run state {}", path.display()))?;
    let version = raw
        .get("version")
        .and_then(Value::as_u64)
        .unwrap_or(u64::from(LEGACY_STATE_VERSION)) as u32;
    if version > STATE_VERSION {
        return Err(newer_schema(path, version));
    }
    if version <= LEGACY_STATE_VERSION {
        #[derive(Deserialize)]
        struct Legacy {
            #[serde(default)]
            cluster: String,
            #[serde(default)]
            tasks: BTreeMap<String, TaskRecord>,
        }
        let legacy: Legacy = serde_json::from_value(raw)
            .into_diagnostic()
            .wrap_err_with(|| format!("parsing v1 run state {}", path.display()))?;
        return Ok(StateFile {
            version: LEGACY_STATE_VERSION,
            cluster: legacy.cluster,
            op_id: String::new(),
            plan_fingerprint: String::new(),
            done: legacy
                .tasks
                .into_iter()
                .map(|(key, record)| DoneTask {
                    key,
                    finished_at: record.finished_at,
                })
                .collect(),
            blackboard: BTreeMap::new(),
        });
    }
    serde_json::from_value(raw)
        .into_diagnostic()
        .wrap_err_with(|| format!("parsing run state {}", path.display()))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Keep cluster names path-safe: anything outside `[A-Za-z0-9._-]` becomes `_`.
fn sanitize(cluster: &str) -> String {
    let mut out: String = cluster
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if out.is_empty() {
        out.push_str("cluster");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_records_survive_resume_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runs").join("demo.json");

        let mut fresh = RunState::open(&path, "demo", false).unwrap();
        assert!(!fresh.is_done("prepare/sysctl@n1"));
        fresh.mark_done("prepare/sysctl@n1");
        fresh.flush().unwrap();
        assert!(path.exists());

        let resumed = RunState::open(&path, "demo", true).unwrap();
        assert!(resumed.is_done("prepare/sysctl@n1"));

        let restarted = RunState::open(&path, "demo", false).unwrap();
        assert_eq!(restarted.done_count(), 0);
        let file: StateFile = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert!(file.done.is_empty());
        assert_eq!(file.version, STATE_VERSION);
    }

    #[test]
    fn default_path_sanitizes_cluster_names() {
        // Never touch the real cache dir; only assert the shape.
        if let Some(path) = RunState::default_path("../evil/cluster") {
            let name = path.file_name().unwrap().to_string_lossy();
            assert_eq!(name, ".._evil_cluster.json");
        }
    }

    #[test]
    fn newer_schema_is_rejected_on_resume() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.json");
        fs::write(&path, r#"{"version":99,"cluster":"c","done":[]}"#).unwrap();
        let err = RunState::open(&path, "c", true).unwrap_err();
        assert!(err.to_string().contains("newer kurin"));
    }

    #[test]
    fn v1_file_resumes_without_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.json");
        fs::write(
            &path,
            r#"{"version":1,"cluster":"c","tasks":{"stage/a@n1":{"state":"done","finished_at":42}}}"#,
        )
        .unwrap();
        let state = RunState::open(&path, "c", true).unwrap();
        assert!(state.is_done("stage/a@n1"));
        assert_eq!(state.plan_fingerprint(), "");
        // No stored fingerprint: any current plan is accepted.
        state.check_fingerprint("whatever").unwrap();
    }

    #[test]
    fn fingerprint_mismatch_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.json");
        let mut state = RunState::open(&path, "c", false)
            .unwrap()
            .with_identity("op-1", "fingerprint-a");
        state.mark_done("stage/a@n1");
        state.flush().unwrap();

        let resumed = RunState::open(&path, "c", true).unwrap();
        assert_eq!(resumed.plan_fingerprint(), "fingerprint-a");
        resumed.check_fingerprint("fingerprint-a").unwrap();
        let err = resumed.check_fingerprint("fingerprint-b").unwrap_err();
        assert_eq!(
            err.to_string(),
            "计划已变化，请去掉 --resume 重新运行（已收敛的任务会被检查跳过）"
        );
    }

    #[test]
    fn v2_file_is_mode_0600_and_carries_identity_and_blackboard() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.json");
        let mut state = RunState::open(&path, "c", false)
            .unwrap()
            .with_identity("op-9", "fp");
        state.mark_done("stage/a@n1");
        state.set_blackboard_snapshot(BTreeMap::from([(
            "k3s.token".to_string(),
            Value::String("fake-token".into()),
        )]));
        state.flush().unwrap();

        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let file: StateFile = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(file.version, 2);
        assert_eq!(file.op_id, "op-9");
        assert_eq!(file.plan_fingerprint, "fp");
        assert_eq!(file.done.len(), 1);
        assert_eq!(file.blackboard["k3s.token"], "fake-token");
    }
}
