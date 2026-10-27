//! Per-cluster mutual exclusion for mutating operations.
//!
//! Every operation that changes a cluster (`create cluster`, `add nodes`,
//! `delete node`, `delete cluster`, `upgrade`, `certs renew`) takes an
//! exclusive lock on `$XDG_STATE_HOME/kurin/locks/<cluster>.lock`
//! (fallback `~/.local/state/kurin/locks/`). The lock is an OS advisory
//! file lock (`File::try_lock`, i.e. `flock` on Unix), so it is released
//! automatically when the process exits — including a crash — and a stale
//! file left behind never blocks the next run.
//!
//! The file also records who holds the lock (pid, operation id, command
//! line, start time). A conflicting `acquire` reads that record and puts it
//! in the error, so the second operator immediately sees which process is
//! already working on the cluster instead of a bare "busy".
//!
//! Read-only commands (`status`, `precheck`, `certs check`, `logs`,
//! `artifact`, `config`) never take the lock: they are safe to run while a
//! mutation is in progress.

use std::fs::{File, OpenOptions, TryLockError};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use miette::{IntoDiagnostic, Result, WrapErr};
use serde::{Deserialize, Serialize};

/// What the lock file records about its holder.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockInfo {
    /// Process id of the holder.
    pub pid: u32,
    /// Operation id (also the JSONL log name) of the holder.
    pub op_id: String,
    /// Command line that acquired the lock.
    pub command: String,
    /// UTC start time, RFC 3339.
    pub started_at: String,
}

/// Held lock. Dropping the guard releases it (and the OS releases it even
/// if the process dies without dropping it).
#[derive(Debug)]
pub struct ClusterLockGuard {
    file: File,
    path: PathBuf,
    info: LockInfo,
}

impl ClusterLockGuard {
    /// Lock file path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Metadata written by the holder.
    pub fn info(&self) -> &LockInfo {
        &self.info
    }
}

impl Drop for ClusterLockGuard {
    fn drop(&mut self) {
        // Best-effort: closing the file releases the flock as well.
        let _ = self.file.unlock();
    }
}

/// Cluster lock namespace.
pub struct ClusterLock;

impl ClusterLock {
    /// `$XDG_STATE_HOME/kurin/locks/<cluster>.lock`, falling back to
    /// `~/.local/state/kurin/locks/<cluster>.lock`.
    pub fn path(cluster: &str) -> PathBuf {
        let base = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state"))
            })
            .unwrap_or_else(|| PathBuf::from(".local/state"));
        base.join("kurin")
            .join("locks")
            .join(format!("{}.lock", sanitize(cluster)))
    }

    /// Acquire the lock for `cluster`, generating an operation id.
    ///
    /// Fails immediately (never blocks) when another process holds it; the
    /// error names the holder's pid, operation, command and start time.
    pub fn acquire(cluster: &str) -> Result<ClusterLockGuard> {
        Self::acquire_with(cluster, &crate::events::new_op_id())
    }

    /// Acquire the lock for `cluster` under a caller-chosen operation id.
    pub fn acquire_with(cluster: &str, op_id: &str) -> Result<ClusterLockGuard> {
        Self::acquire_at(Self::path(cluster), cluster, op_id)
    }

    /// Acquire the lock at an explicit path. Used by tests and by callers
    /// that already resolved the state directory.
    pub fn acquire_at(
        path: impl Into<PathBuf>,
        cluster: &str,
        op_id: &str,
    ) -> Result<ClusterLockGuard> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .into_diagnostic()
                .wrap_err_with(|| format!("creating lock dir {}", parent.display()))?;
        }
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&path)
            .into_diagnostic()
            .wrap_err_with(|| format!("opening cluster lock {}", path.display()))?;

        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                let holder = read_holder(&mut file).unwrap_or_else(|| LockInfo {
                    pid: 0,
                    op_id: "<unknown>".into(),
                    command: "<unknown>".into(),
                    started_at: "<unknown>".into(),
                });
                return Err(miette::miette!(
                    "cluster {cluster:?} is busy: held by pid {} (op {}, started {}): {}",
                    holder.pid,
                    holder.op_id,
                    holder.started_at,
                    holder.command
                ));
            }
            Err(TryLockError::Error(e)) => {
                return Err(miette::miette!(
                    "locking cluster {cluster:?} at {}: {e}",
                    path.display()
                ));
            }
        }

        let info = LockInfo {
            pid: std::process::id(),
            op_id: op_id.to_string(),
            command: command_line(),
            started_at: now_rfc3339(),
        };
        let body = serde_json::to_vec_pretty(&info).into_diagnostic()?;
        file.set_len(0)
            .into_diagnostic()
            .wrap_err_with(|| format!("truncating lock {}", path.display()))?;
        file.seek(SeekFrom::Start(0))
            .into_diagnostic()
            .wrap_err_with(|| format!("seeking lock {}", path.display()))?;
        file.write_all(&body)
            .into_diagnostic()
            .wrap_err_with(|| format!("writing lock {}", path.display()))?;
        file.flush()
            .into_diagnostic()
            .wrap_err_with(|| format!("flushing lock {}", path.display()))?;

        Ok(ClusterLockGuard { file, path, info })
    }
}

fn read_holder(file: &mut File) -> Option<LockInfo> {
    file.seek(SeekFrom::Start(0)).ok()?;
    let mut text = String::new();
    file.read_to_string(&mut text).ok()?;
    serde_json::from_str(&text).ok()
}

/// The current process's command line, for the lock record.
fn command_line() -> String {
    std::env::args().collect::<Vec<_>>().join(" ")
}

/// UTC now as RFC 3339 without pulling in `time`'s formatting feature.
fn now_rfc3339() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let t = time::OffsetDateTime::from_unix_timestamp(secs as i64)
        .unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        t.year(),
        t.month() as u8,
        t.day(),
        t.hour(),
        t.minute(),
        t.second()
    )
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
    fn path_is_under_xdg_state_home() {
        // Environment mutation is process-global; keep this test to the
        // shape only and avoid racing other tests by not setting the var.
        let path = ClusterLock::path("../evil/cluster");
        assert_eq!(path.file_name().unwrap(), ".._evil_cluster.lock");
        assert!(path.to_string_lossy().contains("kurin/locks"));
    }

    #[test]
    fn second_acquire_in_same_process_conflicts_and_names_holder() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("demo.lock");
        let first = ClusterLock::acquire_at(&path, "demo", "op-first").unwrap();
        let err = ClusterLock::acquire_at(&path, "demo", "op-second").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("is busy"), "{msg}");
        assert!(
            msg.contains(&format!("pid {}", std::process::id())),
            "{msg}"
        );
        assert!(msg.contains("op-first"), "{msg}");
        drop(first);
        // Released: the next acquire succeeds and overwrites the record.
        let third = ClusterLock::acquire_at(&path, "demo", "op-third").unwrap();
        assert_eq!(third.info().op_id, "op-third");
    }

    #[test]
    fn sanitize_keeps_safe_names() {
        assert_eq!(sanitize("tier1-prod_01"), "tier1-prod_01");
        assert_eq!(sanitize(""), "cluster");
    }
}
