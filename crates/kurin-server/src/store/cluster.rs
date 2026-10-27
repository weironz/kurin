//! Cluster registration, immutable configuration revisions and the exported
//! mirror file (UI §6.6).
//!
//! The cluster name is the unique key. Every save appends a revision and
//! updates `clusters.current_rev`; the current revision is also written to
//! `<data>/clusters/<name>/cluster.toml` with mode 0600 so the CLI can keep
//! operating on the same cluster. That file is an export mirror: editing it
//! by hand is overwritten by the next save, which is why the UI tells users
//! to import instead.
//!
//! Secrets are stored once per revision (the database is 0600, the same
//! trust level as the CLI's `cluster.toml`). [`Store::read_config`] returns
//! the document with every literal secret replaced by
//! [`super::SECRET_SENTINEL`]; [`Store::save_config`] restores those values
//! from the previous revision.

use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OptionalExtension, Row};
use sha2::{Digest, Sha256};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use toml_edit::DocumentMut;

use super::{secret, Store, StoreError};

/// Cluster lifecycle values (UI §6.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lifecycle {
    /// Registered, no successful create yet.
    Draft,
    /// A create/add operation is running.
    Provisioning,
    /// Healthy managed cluster.
    Active,
    /// A delete operation is running.
    Deleting,
    /// Deleted, retained for history.
    Deleted,
    /// No longer collected or operated on.
    Archived,
}

impl Lifecycle {
    /// Every accepted value, in UI order.
    pub const ALL: [Lifecycle; 6] = [
        Lifecycle::Draft,
        Lifecycle::Provisioning,
        Lifecycle::Active,
        Lifecycle::Deleting,
        Lifecycle::Deleted,
        Lifecycle::Archived,
    ];

    /// The exact string stored in `clusters.lifecycle`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::Provisioning => "provisioning",
            Self::Active => "active",
            Self::Deleting => "deleting",
            Self::Deleted => "deleted",
            Self::Archived => "archived",
        }
    }

    /// Parse a stored value.
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|item| item.as_str() == value)
    }
}

impl fmt::Display for Lifecycle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A row of `clusters`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClusterRecord {
    pub name: String,
    pub lifecycle: Lifecycle,
    /// Latest revision number; 0 means "registered but no configuration yet".
    pub current_rev: i64,
    pub created_at: String,
    pub updated_at: String,
}

/// A stored configuration revision.
///
/// `toml` is the plaintext document (it may contain secrets). The `Debug`
/// impl redacts it so a stray `{:?}` in a log cannot leak a password; API
/// code must use [`Store::read_config`], which returns the sentinel form.
#[derive(Clone, PartialEq, Eq)]
pub struct ConfigRevision {
    pub cluster: String,
    pub rev: i64,
    pub toml: String,
    pub sha256: String,
    pub created_at: String,
    pub note: Option<String>,
}

impl fmt::Debug for ConfigRevision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConfigRevision")
            .field("cluster", &self.cluster)
            .field("rev", &self.rev)
            .field("toml", &"<redacted>")
            .field("sha256", &self.sha256)
            .field("created_at", &self.created_at)
            .field("note", &self.note)
            .finish()
    }
}

/// A revision without its (potentially secret) body.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct RevisionSummary {
    pub rev: i64,
    pub sha256: String,
    pub created_at: String,
    pub note: Option<String>,
}

impl Store {
    /// Register a cluster name without configuration (`current_rev = 0`).
    pub fn register_cluster(
        &self,
        name: &str,
        lifecycle: Lifecycle,
    ) -> Result<ClusterRecord, StoreError> {
        let name = name.to_string();
        self.write(move |conn| {
            validate_name(&name)?;
            let tx = conn.transaction()?;
            if load_cluster(&tx, &name)?.is_some() {
                return Err(StoreError::AlreadyExists(name));
            }
            let timestamp = now();
            tx.execute(
                "INSERT INTO clusters (name, lifecycle, current_rev, created_at, updated_at) \
                 VALUES (?1, ?2, 0, ?3, ?3)",
                params![&name, lifecycle.as_str(), &timestamp],
            )?;
            tx.commit()?;
            Ok(ClusterRecord {
                name,
                lifecycle,
                current_rev: 0,
                created_at: timestamp.clone(),
                updated_at: timestamp,
            })
        })
    }

    /// Change the lifecycle of an existing cluster.
    pub fn set_lifecycle(
        &self,
        name: &str,
        lifecycle: Lifecycle,
    ) -> Result<ClusterRecord, StoreError> {
        let name = name.to_string();
        self.write(move |conn| {
            let timestamp = now();
            let changed = conn.execute(
                "UPDATE clusters SET lifecycle = ?2, updated_at = ?3 WHERE name = ?1",
                params![&name, lifecycle.as_str(), &timestamp],
            )?;
            if changed == 0 {
                return Err(StoreError::NotFound(name));
            }
            load_cluster(conn, &name)?.ok_or(StoreError::NotFound(name))
        })
    }

    /// One cluster, or `None` when the name is unknown.
    pub fn get_cluster(&self, name: &str) -> Result<Option<ClusterRecord>, StoreError> {
        self.read(|conn| load_cluster(conn, name))
    }

    /// All clusters, ordered by name.
    pub fn list_clusters(&self) -> Result<Vec<ClusterRecord>, StoreError> {
        self.read(|conn| {
            let mut statement = conn.prepare(
                "SELECT name, lifecycle, current_rev, created_at, updated_at \
                 FROM clusters ORDER BY name",
            )?;
            let rows = statement.query_map([], row_to_cluster)?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
                .map_err(StoreError::from)
        })
    }

    /// Import a configuration as revision 1 of a new (or just-registered)
    /// cluster. An existing cluster with configuration is rejected: imports
    /// never overwrite history — use [`Store::save_config`] with `base_rev`.
    pub fn import_cluster(
        &self,
        name: &str,
        toml: &str,
        note: Option<String>,
    ) -> Result<ConfigRevision, StoreError> {
        let name = name.to_string();
        let toml = toml.to_string();
        let revision = self.write(move |conn| {
            validate_name(&name)?;
            let tx = conn.transaction()?;
            let current = match current_rev(&tx, &name)? {
                Some(current) => current,
                None => {
                    let timestamp = now();
                    tx.execute(
                        "INSERT INTO clusters \
                         (name, lifecycle, current_rev, created_at, updated_at) \
                         VALUES (?1, ?2, 0, ?3, ?3)",
                        params![&name, Lifecycle::Draft.as_str(), &timestamp],
                    )?;
                    0
                }
            };
            if current != 0 {
                return Err(StoreError::AlreadyExists(name));
            }
            let revision = save_locked(&tx, &name, &toml, current, note)?;
            tx.commit()?;
            Ok(revision)
        })?;
        self.export_mirror(&revision.cluster)?;
        Ok(revision)
    }

    /// Append a revision. `base_rev` must equal the current revision, else
    /// [`StoreError::RevConflict`] (409) — this prevents two browser tabs
    /// from overwriting each other (UI §6.6).
    pub fn save_config(
        &self,
        name: &str,
        toml: &str,
        base_rev: i64,
        note: Option<String>,
    ) -> Result<ConfigRevision, StoreError> {
        let name = name.to_string();
        let toml = toml.to_string();
        let revision = self.write(move |conn| {
            validate_name(&name)?;
            let tx = conn.transaction()?;
            let revision = save_locked(&tx, &name, &toml, base_rev, note)?;
            tx.commit()?;
            Ok(revision)
        })?;
        self.export_mirror(&revision.cluster)?;
        Ok(revision)
    }

    /// The current revision with secrets replaced by the sentinel. `None`
    /// when the cluster is registered but has no configuration yet.
    pub fn read_config(&self, name: &str) -> Result<Option<String>, StoreError> {
        let name = name.to_string();
        self.read(move |conn| {
            let current =
                current_rev(conn, &name)?.ok_or_else(|| StoreError::NotFound(name.clone()))?;
            if current == 0 {
                return Ok(None);
            }
            let revision = load_revision(conn, &name, current)?;
            let mut document: DocumentMut = revision.toml.parse().map_err(StoreError::toml)?;
            secret::redact(&mut document);
            Ok(Some(document.to_string()))
        })
    }

    /// The current revision, plaintext. Internal callers only; `Debug` is
    /// redacted and API handlers must use [`Store::read_config`].
    pub fn current_revision(&self, name: &str) -> Result<Option<ConfigRevision>, StoreError> {
        let name = name.to_string();
        self.read(move |conn| {
            let current =
                current_rev(conn, &name)?.ok_or_else(|| StoreError::NotFound(name.clone()))?;
            if current == 0 {
                return Ok(None);
            }
            load_revision(conn, &name, current).map(Some)
        })
    }

    /// A specific revision, plaintext (see [`Store::current_revision`]).
    pub fn get_revision(&self, name: &str, rev: i64) -> Result<ConfigRevision, StoreError> {
        self.read(|conn| load_revision(conn, name, rev))
    }

    /// Revision history, newest first, without bodies.
    pub fn list_revisions(&self, name: &str) -> Result<Vec<RevisionSummary>, StoreError> {
        self.read(|conn| {
            if current_rev(conn, name)?.is_none() {
                return Err(StoreError::NotFound(name.to_string()));
            }
            let mut statement = conn.prepare(
                "SELECT rev, sha256, created_at, note FROM config_revisions \
                 WHERE cluster = ?1 ORDER BY rev DESC",
            )?;
            let rows = statement.query_map([name], |row| {
                Ok(RevisionSummary {
                    rev: row.get(0)?,
                    sha256: row.get(1)?,
                    created_at: row.get(2)?,
                    note: row.get(3)?,
                })
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
                .map_err(StoreError::from)
        })
    }

    /// Write the current revision to `<data>/clusters/<name>/cluster.toml`
    /// (0600, parent 0700) and return its path. The mirror is plaintext so
    /// the CLI can use it; it is an export, not an edit target.
    pub fn export_mirror(&self, name: &str) -> Result<PathBuf, StoreError> {
        let name = name.to_string();
        let data_dir = self.0.data_dir.clone();
        self.write(move |conn| {
            validate_name(&name)?;
            let current =
                current_rev(conn, &name)?.ok_or_else(|| StoreError::NotFound(name.clone()))?;
            if current == 0 {
                return Err(StoreError::NotFound(name));
            }
            let revision = load_revision(conn, &name, current)?;
            let path = mirror_path(&data_dir, &name);
            write_private(&path, &revision.toml)?;
            Ok(path)
        })
    }
}

/// Reject names that cannot be used as a single path component for the
/// mirror directory.
pub fn validate_name(name: &str) -> Result<(), StoreError> {
    let valid = !name.is_empty()
        && name.len() <= 128
        && name != "."
        && name != ".."
        && !name.contains(['/', '\\', '\0'])
        && !name.chars().any(char::is_control);
    if valid {
        Ok(())
    } else {
        Err(StoreError::InvalidName(name.to_string()))
    }
}

/// Append a revision inside an already open transaction.
fn save_locked(
    conn: &Connection,
    name: &str,
    submitted: &str,
    base_rev: i64,
    note: Option<String>,
) -> Result<ConfigRevision, StoreError> {
    let current = current_rev(conn, name)?.ok_or_else(|| StoreError::NotFound(name.to_string()))?;
    if current != base_rev {
        return Err(StoreError::RevConflict {
            cluster: name.to_string(),
            expected: base_rev,
            current,
        });
    }

    let mut document: DocumentMut = submitted.parse().map_err(StoreError::toml)?;
    let previous = if current > 0 {
        let stored = load_revision(conn, name, current)?;
        Some(
            stored
                .toml
                .parse::<DocumentMut>()
                .map_err(StoreError::toml)?,
        )
    } else {
        None
    };
    secret::restore(&mut document, previous.as_ref())?;

    let toml = document.to_string();
    let sha256 = sha256_hex(&toml);
    let rev = current + 1;
    let timestamp = now();
    conn.execute(
        "INSERT INTO config_revisions (cluster, rev, toml, sha256, created_at, note) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![name, rev, &toml, &sha256, &timestamp, &note],
    )?;
    conn.execute(
        "UPDATE clusters SET current_rev = ?2, updated_at = ?3 WHERE name = ?1",
        params![name, rev, &timestamp],
    )?;
    Ok(ConfigRevision {
        cluster: name.to_string(),
        rev,
        toml,
        sha256,
        created_at: timestamp,
        note,
    })
}

fn current_rev(conn: &Connection, name: &str) -> Result<Option<i64>, StoreError> {
    conn.query_row(
        "SELECT current_rev FROM clusters WHERE name = ?1",
        [name],
        |row| row.get(0),
    )
    .optional()
    .map_err(StoreError::from)
}

fn load_cluster(conn: &Connection, name: &str) -> Result<Option<ClusterRecord>, StoreError> {
    conn.query_row(
        "SELECT name, lifecycle, current_rev, created_at, updated_at \
         FROM clusters WHERE name = ?1",
        [name],
        row_to_cluster,
    )
    .optional()
    .map_err(StoreError::from)
}

fn load_revision(conn: &Connection, name: &str, rev: i64) -> Result<ConfigRevision, StoreError> {
    conn.query_row(
        "SELECT cluster, rev, toml, sha256, created_at, note \
         FROM config_revisions WHERE cluster = ?1 AND rev = ?2",
        params![name, rev],
        |row| {
            Ok(ConfigRevision {
                cluster: row.get(0)?,
                rev: row.get(1)?,
                toml: row.get(2)?,
                sha256: row.get(3)?,
                created_at: row.get(4)?,
                note: row.get(5)?,
            })
        },
    )
    .optional()?
    .ok_or_else(|| StoreError::RevisionNotFound {
        cluster: name.to_string(),
        rev,
    })
}

fn row_to_cluster(row: &Row<'_>) -> rusqlite::Result<ClusterRecord> {
    let lifecycle: String = row.get(1)?;
    let lifecycle = Lifecycle::parse(&lifecycle).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            1,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unknown lifecycle {lifecycle:?}"),
            )),
        )
    })?;
    Ok(ClusterRecord {
        name: row.get(0)?,
        lifecycle,
        current_rev: row.get(2)?,
        created_at: row.get(3)?,
        updated_at: row.get(4)?,
    })
}

fn now() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .expect("UTC timestamps always format as RFC 3339")
}

fn sha256_hex(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn mirror_path(data_dir: &Path, name: &str) -> PathBuf {
    data_dir.join("clusters").join(name).join("cluster.toml")
}

/// Atomically write `content` as a 0600 file, creating the parent directory
/// with mode 0700. A rename replaces any previous inode, so a file that was
/// once world-readable ends up 0600 too.
fn write_private(path: &Path, content: &str) -> std::io::Result<()> {
    let directory = path.parent().expect("mirror path always has a parent");
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(directory)?;
    // `create` does not tighten an existing directory; the mirror holds
    // plaintext secrets, so make sure the cluster directory is private.
    fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
    let temporary = path.with_extension("toml.tmp");
    {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(content.as_bytes())?;
        if !content.ends_with('\n') {
            file.write_all(b"\n")?;
        }
        file.sync_all()?;
    }
    fs::rename(&temporary, path)
}
