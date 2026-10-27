//! Typed cross-task data exchange for one run (ADR-18).
//!
//! Tasks only receive their own node's session, so anything one task
//! produces for another (kubeadm join material, the k3s node token) travels
//! through the [`Blackboard`] instead of a control-machine file. Values are
//! addressed by [`Key<T>`]: the key carries the name, the persistence class
//! and the value type, so a reader cannot silently decode the wrong shape.
//!
//! Keys with [`Persist::RunState`] or [`Persist::RunStateSecret`] are
//! written to `$XDG_STATE_HOME/kurin/runs/<cluster>.bb.json` (mode 0600)
//! after every mutation, which is what lets `--resume` hand a downstream
//! task the upstream task's output. `RunStateSecret` values are additionally
//! registered with the operation's [`EventBus`](crate::events::EventBus)
//! redactor, so they never reach a log line, event or error text.

use std::collections::BTreeMap;
use std::fmt;
use std::io::Write;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::events::EventBus;

/// On-disk schema version of the Blackboard snapshot.
pub const BLACKBOARD_VERSION: u32 = 1;

/// How a key survives a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Persist {
    /// Run-scoped only: never written to disk.
    No,
    /// Written to the run-state file so `--resume` can restore it.
    RunState,
    /// Like [`Persist::RunState`], and the value is registered with the
    /// event redactor.
    RunStateSecret,
}

/// A typed address on the [`Blackboard`].
///
/// `T` is the value type; the phantom is `fn() -> T` so a key is `Send`,
/// `Sync` and `Copy` regardless of `T`.
pub struct Key<T> {
    /// Stable key name (`kubeadm.join`, `k3s.token`, …).
    pub name: &'static str,
    /// Persistence class.
    pub persist: Persist,
    _t: PhantomData<fn() -> T>,
}

impl<T> Key<T> {
    /// Define a key. `const` so keys can be module-level constants.
    pub const fn new(name: &'static str, persist: Persist) -> Self {
        Self {
            name,
            persist,
            _t: PhantomData,
        }
    }
}

impl<T> Clone for Key<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for Key<T> {}

impl<T> fmt::Debug for Key<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Key")
            .field("name", &self.name)
            .field("persist", &self.persist)
            .finish()
    }
}

/// kubeadm join material produced by `bootstrap/kubeadm-init` and consumed
/// by `join/kubeadm`. Secret: the join command embeds a bootstrap token.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinMaterial {
    /// Full `kubeadm join <endpoint> --token ... --discovery-token-ca-cert-hash ...`.
    pub join_command: String,
    /// `--certificate-key` for control-plane joins (upload-certs, 2h TTL).
    pub certificate_key: String,
}

/// Join material for the kubeadm track.
pub const JOIN_MATERIAL: Key<JoinMaterial> = Key::new("kubeadm.join", Persist::RunStateSecret);

/// k3s node token harvested by the first server, consumed by joins.
pub const K3S_TOKEN: Key<String> = Key::new("k3s.token", Persist::RunStateSecret);

/// Blackboard failure: serialization or run-state file I/O.
#[derive(Debug, thiserror::Error)]
pub enum BlackboardError {
    /// A value could not be encoded/decoded.
    #[error("blackboard value {key}: {source}")]
    Codec {
        /// Key name.
        key: &'static str,
        /// Underlying serde error.
        source: serde_json::Error,
    },
    /// The run-state file could not be read or written.
    #[error("blackboard state {path}: {source}")]
    Io {
        /// File that failed.
        path: PathBuf,
        /// Underlying I/O error.
        source: std::io::Error,
    },
}

#[derive(Clone)]
struct Entry {
    value: Value,
    persist: Persist,
}

/// On-disk shape of the persistent part.
#[derive(Debug, Serialize, Deserialize)]
struct Snapshot {
    version: u32,
    #[serde(default)]
    values: BTreeMap<String, Value>,
}

/// Typed, optionally persistent key/value store for one run.
#[derive(Debug)]
pub struct Blackboard {
    entries: RwLock<BTreeMap<String, Entry>>,
    path: Option<PathBuf>,
    events: Option<Arc<EventBus>>,
}

impl Default for Blackboard {
    fn default() -> Self {
        Self::in_memory()
    }
}

impl fmt::Debug for Entry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never render a value: it may be a secret.
        f.debug_struct("Entry")
            .field("persist", &self.persist)
            .finish_non_exhaustive()
    }
}

impl Blackboard {
    /// Run-scoped store with no persistence.
    pub fn in_memory() -> Self {
        Self {
            entries: RwLock::new(BTreeMap::new()),
            path: None,
            events: None,
        }
    }

    /// Store backed by `path`. `resume == true` loads an existing snapshot
    /// (a missing file is an empty board); `resume == false` starts empty and
    /// immediately writes an empty snapshot, so stale secrets can never leak
    /// into a fresh run.
    pub fn open(path: impl Into<PathBuf>, resume: bool) -> Result<Self, BlackboardError> {
        let path = path.into();
        let mut entries = BTreeMap::new();
        if resume && path.exists() {
            let text = std::fs::read_to_string(&path).map_err(|source| BlackboardError::Io {
                path: path.clone(),
                source,
            })?;
            let snapshot: Snapshot =
                serde_json::from_str(&text).map_err(|source| BlackboardError::Codec {
                    key: "<snapshot>",
                    source,
                })?;
            if snapshot.version > BLACKBOARD_VERSION {
                return Err(BlackboardError::Codec {
                    key: "<snapshot>",
                    source: serde_json::Error::io(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!(
                            "blackboard {} was written by a newer kurin (schema {} > {})",
                            path.display(),
                            snapshot.version,
                            BLACKBOARD_VERSION
                        ),
                    )),
                });
            }
            for (name, value) in snapshot.values {
                // The snapshot only holds persistent keys; the exact
                // secret/non-secret split is restored when a key is set again.
                entries.insert(
                    name,
                    Entry {
                        value,
                        persist: Persist::RunState,
                    },
                );
            }
        }
        let board = Self {
            entries: RwLock::new(entries),
            path: Some(path),
            events: None,
        };
        if !resume {
            board.save()?;
        }
        Ok(board)
    }

    /// `$XDG_STATE_HOME/kurin/runs/<cluster>.bb.json` (fallback
    /// `~/.local/state/...`), or `None` when neither variable is set.
    pub fn default_path(cluster: &str) -> Option<PathBuf> {
        let base = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state"))
            })?;
        Some(
            base.join("kurin")
                .join("runs")
                .join(format!("{}.bb.json", sanitize(cluster))),
        )
    }

    /// Attach the operation bus so secret values are redacted from events.
    pub fn with_events(mut self, events: Arc<EventBus>) -> Self {
        // Values restored by `open` were written to disk before the bus
        // existed; register them now.
        {
            let entries = self.entries.read().unwrap_or_else(|e| e.into_inner());
            for entry in entries.values() {
                // Only persistent keys reach the file, and every persistent
                // key today is a secret; register unconditionally so a
                // resumed value is redacted even though the snapshot does not
                // record which keys were secret.
                if entry.persist != Persist::No {
                    register_secret(&events, &entry.value);
                }
            }
        }
        self.events = Some(events);
        self
    }

    /// Path of the backing file, if any.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Read a value. `None` when the key was never set or the stored value
    /// does not decode as `T`.
    pub fn get<T: DeserializeOwned>(&self, key: &Key<T>) -> Option<T> {
        let entries = self.entries.read().unwrap_or_else(|e| e.into_inner());
        let entry = entries.get(key.name)?;
        serde_json::from_value(entry.value.clone()).ok()
    }

    /// Write a value, registering it as a secret when the key asks for it,
    /// and persist when the key is a `RunState*` key.
    pub fn set<T: Serialize>(&self, key: Key<T>, value: T) -> Result<(), BlackboardError> {
        let encoded = serde_json::to_value(value).map_err(|source| BlackboardError::Codec {
            key: key.name,
            source,
        })?;
        if key.persist == Persist::RunStateSecret {
            if let Some(events) = &self.events {
                register_secret(events, &encoded);
            }
        }
        {
            let mut entries = self.entries.write().unwrap_or_else(|e| e.into_inner());
            entries.insert(
                key.name.to_string(),
                Entry {
                    value: encoded,
                    persist: key.persist,
                },
            );
        }
        if key.persist != Persist::No {
            self.save()?;
        }
        Ok(())
    }

    /// Drop a key (and its persisted copy).
    pub fn remove<T>(&self, key: Key<T>) -> Result<(), BlackboardError> {
        let removed = {
            let mut entries = self.entries.write().unwrap_or_else(|e| e.into_inner());
            entries.remove(key.name)
        };
        if removed.is_some_and(|e| e.persist != Persist::No) {
            self.save()?;
        }
        Ok(())
    }

    /// True when nothing is stored.
    pub fn is_empty(&self) -> bool {
        self.entries.read().map(|e| e.is_empty()).unwrap_or(true)
    }

    /// Snapshot of the values that would be written to the backing file.
    ///
    /// Used by [`crate::state::RunState`] to mirror the persistent part of
    /// the Blackboard into the run-state record; `<cluster>.bb.json` stays
    /// the authoritative store.
    pub fn persistent_snapshot(&self) -> BTreeMap<String, Value> {
        let entries = self.entries.read().unwrap_or_else(|e| e.into_inner());
        entries
            .iter()
            .filter(|(_, e)| e.persist != Persist::No)
            .map(|(name, e)| (name.clone(), e.value.clone()))
            .collect()
    }

    /// Write every persistent entry to the backing file atomically, with
    /// mode 0600 (secrets live here).
    fn save(&self) -> Result<(), BlackboardError> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let values: BTreeMap<String, Value> = {
            let entries = self.entries.read().unwrap_or_else(|e| e.into_inner());
            entries
                .iter()
                .filter(|(_, e)| e.persist != Persist::No)
                .map(|(name, e)| (name.clone(), e.value.clone()))
                .collect()
        };
        let snapshot = Snapshot {
            version: BLACKBOARD_VERSION,
            values,
        };
        let body =
            serde_json::to_vec_pretty(&snapshot).map_err(|source| BlackboardError::Codec {
                key: "<snapshot>",
                source,
            })?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|source| BlackboardError::Io {
                path: dir.to_path_buf(),
                source,
            })?;
        }
        let tmp = path.with_extension("json.tmp");
        write_private(&tmp, &body)?;
        std::fs::rename(&tmp, path).map_err(|source| BlackboardError::Io {
            path: path.clone(),
            source,
        })
    }
}

/// Create/replace `path` with `body`, mode 0600.
fn write_private(path: &Path, body: &[u8]) -> Result<(), BlackboardError> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let io = |source| BlackboardError::Io {
        path: path.to_path_buf(),
        source,
    };
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(io)?;
    // `mode` only applies on create: a leftover temp file may be wider.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(io)?;
    file.write_all(body).map_err(io)?;
    file.flush().map_err(io)
}

/// Register every string inside a secret value with the redactor.
fn register_secret(events: &EventBus, value: &Value) {
    match value {
        Value::String(s) => events.redactor().register(s.clone()),
        Value::Array(items) => items.iter().for_each(|v| register_secret(events, v)),
        Value::Object(map) => map.values().for_each(|v| register_secret(events, v)),
        _ => {}
    }
}

fn sanitize(cluster: &str) -> String {
    cluster
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '\0' => '_',
            _ => c,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_typed_values() {
        let bb = Blackboard::in_memory();
        bb.set(K3S_TOKEN, "K10SECRET::node:abc".to_string())
            .unwrap();
        assert_eq!(bb.get(&K3S_TOKEN).as_deref(), Some("K10SECRET::node:abc"));
        bb.remove(K3S_TOKEN).unwrap();
        assert!(bb.get(&K3S_TOKEN).is_none());
    }

    #[test]
    fn persist_round_trip_and_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runs/c1.bb.json");
        let bb = Blackboard::open(&path, false).unwrap();
        bb.set(
            JOIN_MATERIAL,
            JoinMaterial {
                join_command: "kubeadm join 10.0.0.1:6443 --token fake".into(),
                certificate_key: "deadbeef".into(),
            },
        )
        .unwrap();
        drop(bb);

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "run-state secrets must be 0600");

        let resumed = Blackboard::open(&path, true).unwrap();
        let material = resumed.get(&JOIN_MATERIAL).expect("join material restored");
        assert_eq!(material.certificate_key, "deadbeef");
    }

    #[test]
    fn missing_file_is_empty_on_resume() {
        let dir = tempfile::tempdir().unwrap();
        let bb = Blackboard::open(dir.path().join("nope.bb.json"), true).unwrap();
        assert!(bb.get(&K3S_TOKEN).is_none());
        assert!(bb.is_empty());
    }

    #[test]
    fn resumed_secrets_are_redacted() {
        use crate::events::{Body, EventBus, Level, Stream, REDACTED};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c1.bb.json");
        {
            let bb = Blackboard::open(&path, false).unwrap();
            bb.set(K3S_TOKEN, "K10SECRET::node:abc".to_string())
                .unwrap();
        }

        let bus = Arc::new(EventBus::new("op"));
        let bb = Blackboard::open(&path, true)
            .unwrap()
            .with_events(Arc::clone(&bus));
        assert_eq!(bb.get(&K3S_TOKEN).as_deref(), Some("K10SECRET::node:abc"));

        let envelope = bus.publish(Body::Log {
            level: Level::Info,
            node: None,
            task: None,
            stream: Stream::Stdout,
            line: "node token K10SECRET::node:abc".into(),
        });
        match envelope.body {
            Body::Log { line, .. } => {
                assert!(!line.contains("K10SECRET"), "secret leaked: {line}");
                assert!(line.contains(REDACTED));
            }
            other => panic!("unexpected body {other:?}"),
        }
    }

    #[test]
    fn non_persistent_keys_stay_off_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c1.bb.json");
        let key: Key<String> = Key::new("scratch", Persist::No);
        let bb = Blackboard::open(&path, false).unwrap();
        bb.set(key, "ephemeral".into()).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("ephemeral"));
        assert_eq!(bb.get(&key).as_deref(), Some("ephemeral"));
    }
}
