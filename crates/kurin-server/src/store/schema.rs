//! Schema migrations driven by `PRAGMA user_version` (UI §6.7).
//!
//! No migration framework: the version is an integer in the database header,
//! each entry below runs exactly once, in order, inside its own transaction.
//! Version 1 is the v1 table set; version 2 is the v1.1 revision that moves
//! raw log lines out of `op_events` into JSONL files and adds
//! `operations.log_path` / `operations.log_lines`.

use rusqlite::Connection;

use super::StoreError;

/// Schema version this build creates and expects.
pub const LATEST_VERSION: i32 = 4;

/// The v1 table set, exactly as UI §6.7 lists it.
const V1: &str = r#"
CREATE TABLE clusters (
  name        TEXT PRIMARY KEY,
  lifecycle   TEXT NOT NULL,
  current_rev INTEGER NOT NULL,
  created_at  TEXT NOT NULL,
  updated_at  TEXT NOT NULL
);
CREATE TABLE config_revisions (
  cluster TEXT NOT NULL REFERENCES clusters(name),
  rev     INTEGER NOT NULL,
  toml    TEXT NOT NULL,
  sha256  TEXT NOT NULL,
  created_at TEXT NOT NULL,
  note    TEXT,
  PRIMARY KEY (cluster, rev)
);
CREATE TABLE operations (
  id          TEXT PRIMARY KEY,
  cluster     TEXT REFERENCES clusters(name),
  kind        TEXT NOT NULL,
  state       TEXT NOT NULL,
  config_rev  INTEGER,
  params      TEXT NOT NULL,
  retry_of    TEXT REFERENCES operations(id),
  actor       TEXT NOT NULL,
  error       TEXT,
  created_at TEXT NOT NULL, started_at TEXT, finished_at TEXT
);
CREATE UNIQUE INDEX one_active_op ON operations(cluster)
  WHERE state IN ('queued','running');
CREATE TABLE op_events (
  op_id   TEXT NOT NULL REFERENCES operations(id),
  seq     INTEGER NOT NULL,
  ts      TEXT NOT NULL,
  kind    TEXT NOT NULL,
  payload TEXT NOT NULL,
  PRIMARY KEY (op_id, seq)
);
CREATE TABLE snapshots (
  cluster       TEXT PRIMARY KEY REFERENCES clusters(name),
  attempted_at  TEXT NOT NULL,
  observed_at   TEXT,
  report        TEXT,
  error         TEXT,
  certs_at      TEXT, cert_min_days INTEGER
);
"#;

/// v1.1: raw log lines live in `<data>/logs/<cluster>/<op-id>.jsonl`; the
/// operations row keeps only the path and the line count (UI §6.7).
const V2: &str = r#"
ALTER TABLE operations ADD COLUMN log_path TEXT;
ALTER TABLE operations ADD COLUMN log_lines INTEGER;
"#;

/// v1.2: read-only operations (`precheck`, `status`) do not take the
/// mutation lock (UI §6.5), so they must not collide with a running
/// mutation through the partial unique index.
const V3: &str = r#"
DROP INDEX one_active_op;
CREATE UNIQUE INDEX one_active_op ON operations(cluster)
  WHERE state IN ('queued','running') AND kind NOT IN ('precheck','status');
"#;

/// v1.3 (WP-54): operations carry a JSON `result` payload (the plan view
/// of a `plan` operation, the precheck report, package reports);
/// snapshots gain the addon inventory read from `kurin-state`; `packages`
/// registers the package sources the packages page manages; and the
/// active-operation index learns the new lock-free kinds (`plan` and the
/// clusterless package operations), so they neither take nor collide on
/// the cluster guard.
const V4: &str = r#"
ALTER TABLE operations ADD COLUMN result TEXT;
ALTER TABLE snapshots ADD COLUMN addons TEXT;
DROP INDEX one_active_op;
CREATE UNIQUE INDEX one_active_op ON operations(cluster)
  WHERE state IN ('queued','running') AND kind NOT IN
  ('precheck','status','plan','package-verify','package-push','package-create-base');
CREATE TABLE packages (
  id          TEXT PRIMARY KEY,
  source      TEXT NOT NULL,
  origin      TEXT NOT NULL,
  kind        TEXT NOT NULL,
  name        TEXT NOT NULL,
  version     TEXT NOT NULL,
  distribution TEXT NOT NULL,
  arch        TEXT NOT NULL,
  size        INTEGER NOT NULL,
  "index"     TEXT NOT NULL,
  signatures  INTEGER NOT NULL DEFAULT 0,
  sbom        TEXT,
  created_at  TEXT NOT NULL,
  verified_at TEXT NOT NULL
);
"#;

/// Ordered migrations. Keep append-only: a shipped version is never edited.
const MIGRATIONS: &[(i32, &str)] = &[(1, V1), (2, V2), (3, V3), (4, V4)];

/// Bring `conn` from any older schema up to [`LATEST_VERSION`].
pub fn migrate(conn: &mut Connection) -> Result<(), StoreError> {
    let current: i32 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if current > LATEST_VERSION {
        return Err(StoreError::SchemaTooNew {
            found: current,
            supported: LATEST_VERSION,
        });
    }
    for (version, sql) in MIGRATIONS {
        if *version <= current {
            continue;
        }
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", *version)?;
        tx.commit()?;
    }
    Ok(())
}
