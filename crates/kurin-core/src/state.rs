//! Resume state: task-level "done" records persisted per cluster.
//!
//! Path: `$XDG_CACHE_HOME/kurin/runs/<cluster>.json` (or
//! `~/.cache/kurin/runs/<cluster>.json`). The file is advisory, never
//! authoritative: remote probes (`Task::check`) decide whether work is
//! skipped, the records only let `--resume` distinguish "this cluster was
//! already installed by us" from "first attempt".
//!
//! Writes are eager and atomic (write-ahead temp file + rename) *after every
//! completed task*, so a SIGINT / SIGKILL between tasks always leaves a
//! consistent snapshot on disk. `flush` is also called from `Drop` as a
//! best-effort safety net. A cooperative `SIGINT` handler should call
//! [`RunState::flush`] before exiting; the executor keeps the state only for
//! the duration of [`crate::executor::run_with`], so wiring the handler is the
//! integration owner's job (main.rs is not owned by this track).

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use miette::{Context, IntoDiagnostic, Result};
use serde::{Deserialize, Serialize};

/// On-disk schema version. Bumped when records change incompatibly; newer
/// files are rejected rather than silently misread.
pub const STATE_VERSION: u32 = 1;

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateFile {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub cluster: String,
    #[serde(default)]
    pub tasks: BTreeMap<String, TaskRecord>,
}

fn default_version() -> u32 {
    STATE_VERSION
}

/// Task-level progress for one run, optionally backed by a file.
#[derive(Debug)]
pub struct RunState {
    path: Option<PathBuf>,
    cluster: String,
    tasks: BTreeMap<String, TaskRecord>,
    dirty: bool,
}

impl RunState {
    /// State that is never persisted (default for plain `run`).
    pub fn disabled() -> Self {
        Self {
            path: None,
            cluster: String::new(),
            tasks: BTreeMap::new(),
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
        if resume && path.exists() {
            let text = fs::read_to_string(&path)
                .into_diagnostic()
                .wrap_err_with(|| format!("reading run state {}", path.display()))?;
            let file: StateFile = serde_json::from_str(&text)
                .into_diagnostic()
                .wrap_err_with(|| format!("parsing run state {}", path.display()))?;
            if file.version > STATE_VERSION {
                return Err(miette::miette!(
                    "run state {} was written by a newer kurin (schema {} > {}); \
                     re-run without --resume to start fresh",
                    path.display(),
                    file.version,
                    STATE_VERSION
                ));
            }
            tasks = file.tasks;
        }
        let mut state = Self {
            path: Some(path),
            cluster,
            tasks,
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

    /// Persist atomically. No-op when nothing changed or when persistence is
    /// disabled. Errors are returned; callers decide whether they are fatal.
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
            tasks: self.tasks.clone(),
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
        assert!(file.tasks.is_empty());
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
        fs::write(&path, r#"{"version":99,"cluster":"c","tasks":{}}"#).unwrap();
        let err = RunState::open(&path, "c", true).unwrap_err();
        assert!(err.to_string().contains("newer kurin"));
    }
}
