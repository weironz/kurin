//! Operation rows, structured events and the active-operation guard
//! (UI §6.7/§6.8).
//!
//! `operations` is the durable record of every submitted run; `op_events`
//! keeps the structured bodies (task/phase/resource/state), while raw log
//! lines go to the per-operation JSONL file. Both share the bus `seq`, so
//! the SSE backfill can merge them into one timeline.
//!
//! The partial unique index `one_active_op` is the database half of the
//! cluster mutex (the `flock` is the other). Read-only kinds (`precheck`,
//! `status`) are excluded: UI §6.5 lets them run while a mutation holds the
//! lock.

use rusqlite::{params, Connection, OptionalExtension, Row};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use super::{Store, StoreError};

/// Operation lifecycle values (UI §6.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationState {
    Queued,
    Running,
    Succeeded,
    Failed,
    Cancelled,
    Interrupted,
}

impl OperationState {
    /// Every accepted value.
    pub const ALL: [OperationState; 6] = [
        OperationState::Queued,
        OperationState::Running,
        OperationState::Succeeded,
        OperationState::Failed,
        OperationState::Cancelled,
        OperationState::Interrupted,
    ];

    /// The exact string stored in `operations.state`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Interrupted => "interrupted",
        }
    }

    /// Parse a stored value.
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|state| state.as_str() == value)
    }

    /// True once the operation can no longer change state.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Cancelled | Self::Interrupted
        )
    }
}

/// A row of `operations`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationRecord {
    pub id: String,
    pub cluster: Option<String>,
    pub kind: String,
    pub state: OperationState,
    pub config_rev: Option<i64>,
    /// Operation parameters as submitted (JSON text).
    pub params: String,
    pub retry_of: Option<String>,
    pub actor: String,
    pub error: Option<String>,
    pub created_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub log_path: Option<String>,
    /// JSON payload the backend attached on success (the plan view of a
    /// `plan` operation, a precheck or package report); `None` until then.
    pub result: Option<String>,
    pub log_lines: i64,
}

/// A new operation to insert.
#[derive(Debug, Clone)]
pub struct NewOperation {
    pub id: String,
    pub cluster: Option<String>,
    pub kind: String,
    pub config_rev: Option<i64>,
    pub params: String,
    pub retry_of: Option<String>,
    pub actor: String,
}

/// Filters for `GET /operations` (UI §7).
#[derive(Debug, Clone, Default)]
pub struct OperationFilter {
    pub cluster: Option<String>,
    pub state: Option<OperationState>,
    pub kind: Option<String>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
}

/// One persisted structured event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredEvent {
    pub seq: u64,
    pub ts: String,
    pub kind: String,
    pub payload: String,
}

/// Kinds that never take the mutation lock and are exempt from the partial
/// unique index (UI §6.5).
///
/// Beyond the read-only probes (`precheck`, `status`, `plan`) this covers
/// the clusterless package operations (WP-54): their synthetic `packages`
/// cluster is a persistence anchor, not a mutation target, so a package
/// build must not collide with a concurrent verify the way a cluster
/// mutation would.
pub fn is_read_only_kind(kind: &str) -> bool {
    matches!(
        kind,
        "precheck" | "status" | "plan" | "package-verify" | "package-push" | "package-create-base"
    )
}

impl Store {
    /// Insert a queued operation.
    ///
    /// A mutating operation on a cluster that already has a `queued` or
    /// `running` one is rejected with [`StoreError::ActiveOperation`]; the
    /// check and the insert share one writer transaction, so the partial
    /// unique index is a backstop rather than the primary guard.
    pub fn insert_operation(&self, new: NewOperation) -> Result<OperationRecord, StoreError> {
        self.write(move |conn| {
            let tx = conn.transaction()?;
            if let Some(cluster) = new.cluster.as_deref() {
                if !is_read_only_kind(&new.kind) {
                    if let Some(active) = load_active_operation(&tx, cluster)? {
                        return Err(StoreError::ActiveOperation {
                            cluster: cluster.to_string(),
                            op_id: active.id,
                        });
                    }
                }
            }
            let timestamp = now();
            tx.execute(
                "INSERT INTO operations \
                 (id, cluster, kind, state, config_rev, params, retry_of, actor, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    &new.id,
                    &new.cluster,
                    &new.kind,
                    OperationState::Queued.as_str(),
                    new.config_rev,
                    &new.params,
                    &new.retry_of,
                    &new.actor,
                    &timestamp,
                ],
            )?;
            tx.commit()?;
            Ok(OperationRecord {
                id: new.id,
                cluster: new.cluster,
                kind: new.kind,
                state: OperationState::Queued,
                config_rev: new.config_rev,
                params: new.params,
                retry_of: new.retry_of,
                actor: new.actor,
                error: None,
                created_at: timestamp,
                started_at: None,
                finished_at: None,
                log_path: None,
                result: None,
                log_lines: 0,
            })
        })
    }

    /// One operation, or `None` when the id is unknown.
    pub fn get_operation(&self, id: &str) -> Result<Option<OperationRecord>, StoreError> {
        self.read(|conn| load_operation(conn, id))
    }

    /// Operations matching `filter`, newest first.
    pub fn list_operations(
        &self,
        filter: &OperationFilter,
    ) -> Result<Vec<OperationRecord>, StoreError> {
        let filter = filter.clone();
        self.read(move |conn| {
            let mut sql = String::from(
                "SELECT id, cluster, kind, state, config_rev, params, retry_of, actor, error, \
                 created_at, started_at, finished_at, log_path, result, log_lines FROM operations",
            );
            let mut clauses = Vec::new();
            if filter.cluster.is_some() {
                clauses.push("cluster = ?");
            }
            if filter.state.is_some() {
                clauses.push("state = ?");
            }
            if filter.kind.is_some() {
                clauses.push("kind = ?");
            }
            if !clauses.is_empty() {
                sql.push_str(" WHERE ");
                sql.push_str(&clauses.join(" AND "));
            }
            sql.push_str(" ORDER BY created_at DESC, id DESC LIMIT ? OFFSET ?");

            let mut values: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
            if let Some(cluster) = &filter.cluster {
                values.push(Box::new(cluster.clone()));
            }
            if let Some(state) = filter.state {
                values.push(Box::new(state.as_str().to_string()));
            }
            if let Some(kind) = &filter.kind {
                values.push(Box::new(kind.clone()));
            }
            values.push(Box::new(filter.limit.unwrap_or(100).min(1000) as i64));
            values.push(Box::new(filter.offset.unwrap_or(0) as i64));
            let refs: Vec<&dyn rusqlite::ToSql> = values.iter().map(|v| v.as_ref()).collect();

            let mut statement = conn.prepare(&sql)?;
            let rows = statement.query_map(refs.as_slice(), row_to_operation)?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
                .map_err(StoreError::from)
        })
    }

    /// The active (`queued`/`running`) operation of `cluster`, if any.
    pub fn active_operation(&self, cluster: &str) -> Result<Option<OperationRecord>, StoreError> {
        self.read(|conn| load_active_operation(conn, cluster))
    }

    /// Mark a queued operation as running and stamp `started_at`.
    pub fn mark_operation_running(&self, id: &str) -> Result<(), StoreError> {
        let id = id.to_string();
        self.write(move |conn| {
            let timestamp = now();
            conn.execute(
                "UPDATE operations SET state = ?2, started_at = COALESCE(started_at, ?3) \
                 WHERE id = ?1 AND state = ?4",
                params![
                    &id,
                    OperationState::Running.as_str(),
                    &timestamp,
                    OperationState::Queued.as_str()
                ],
            )?;
            Ok(())
        })
    }

    /// Move an active operation to a terminal state. Returns `false` when
    /// the row was already terminal (so callers can publish `OpFinished`
    /// exactly once).
    pub fn finish_operation(
        &self,
        id: &str,
        state: OperationState,
        error: Option<String>,
    ) -> Result<bool, StoreError> {
        let id = id.to_string();
        self.write(move |conn| {
            let timestamp = now();
            let changed = conn.execute(
                "UPDATE operations SET state = ?2, error = ?3, finished_at = ?4 \
                 WHERE id = ?1 AND state IN ('queued','running')",
                params![&id, state.as_str(), error, &timestamp],
            )?;
            Ok(changed > 0)
        })
    }

    /// Startup recovery (UI §6.4): every operation left `queued`/`running`
    /// by a previous process is marked `interrupted`. Returns their ids.
    pub fn interrupt_active_operations(&self) -> Result<Vec<String>, StoreError> {
        self.write(|conn| {
            let tx = conn.transaction()?;
            let ids = {
                let mut statement = tx.prepare(
                    "SELECT id FROM operations WHERE state IN ('queued','running') ORDER BY created_at",
                )?;
                let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
                rows.collect::<rusqlite::Result<Vec<_>>>()?
            };
            if !ids.is_empty() {
                let timestamp = now();
                tx.execute(
                    "UPDATE operations SET state = 'interrupted', finished_at = ?1, \
                     error = COALESCE(error, '服务重启，操作被中断') \
                     WHERE state IN ('queued','running')",
                    params![&timestamp],
                )?;
            }
            tx.commit()?;
            Ok(ids)
        })
    }

    /// Store the JSONL path and current line count.
    pub fn set_log_meta(&self, id: &str, path: &str, lines: i64) -> Result<(), StoreError> {
        let id = id.to_string();
        let path = path.to_string();
        self.write(move |conn| {
            conn.execute(
                "UPDATE operations SET log_path = ?2, log_lines = ?3 WHERE id = ?1",
                params![&id, &path, lines],
            )?;
            Ok(())
        })
    }

    /// Attach the backend's JSON result payload to a finished operation
    /// (the plan view, a package report, ...). Only the backend writes
    /// this, after the operation succeeded.
    pub fn set_operation_result(&self, id: &str, result: &str) -> Result<(), StoreError> {
        let id = id.to_string();
        let result = result.to_string();
        self.write(move |conn| {
            conn.execute(
                "UPDATE operations SET result = ?2 WHERE id = ?1",
                params![&id, &result],
            )?;
            Ok(())
        })
    }

    /// Append one structured event (`op_events`). Duplicate `(op_id, seq)`
    /// is ignored so a retried write cannot fail the operation.
    pub fn append_event(
        &self,
        op_id: &str,
        seq: u64,
        ts: &str,
        kind: &str,
        payload: &str,
    ) -> Result<(), StoreError> {
        let op_id = op_id.to_string();
        let ts = ts.to_string();
        let kind = kind.to_string();
        let payload = payload.to_string();
        self.write(move |conn| {
            conn.execute(
                "INSERT OR IGNORE INTO op_events (op_id, seq, ts, kind, payload) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![&op_id, seq as i64, &ts, &kind, &payload],
            )?;
            Ok(())
        })
    }

    /// Structured events with `seq > after`, oldest first.
    pub fn events_after(
        &self,
        op_id: &str,
        after: u64,
        limit: usize,
    ) -> Result<Vec<StoredEvent>, StoreError> {
        let op_id = op_id.to_string();
        self.read(move |conn| {
            let mut statement = conn.prepare(
                "SELECT seq, ts, kind, payload FROM op_events \
                 WHERE op_id = ?1 AND seq > ?2 ORDER BY seq LIMIT ?3",
            )?;
            let rows = statement.query_map(
                params![&op_id, after as i64, limit.min(i64::MAX as usize) as i64],
                |row| {
                    Ok(StoredEvent {
                        seq: row.get::<_, i64>(0)? as u64,
                        ts: row.get(1)?,
                        kind: row.get(2)?,
                        payload: row.get(3)?,
                    })
                },
            )?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
                .map_err(StoreError::from)
        })
    }

    /// `(event_count, last_seq)` for the operation detail view.
    pub fn operation_stats(&self, op_id: &str) -> Result<(i64, u64), StoreError> {
        let op_id = op_id.to_string();
        self.read(move |conn| {
            conn.query_row(
                "SELECT COUNT(*), COALESCE(MAX(seq), 0) FROM op_events WHERE op_id = ?1",
                [&op_id],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)? as u64)),
            )
            .map_err(StoreError::from)
        })
    }
}

fn load_operation(conn: &Connection, id: &str) -> Result<Option<OperationRecord>, StoreError> {
    conn.query_row(
        "SELECT id, cluster, kind, state, config_rev, params, retry_of, actor, error, \
         created_at, started_at, finished_at, log_path, result, log_lines FROM operations WHERE id = ?1",
        [id],
        row_to_operation,
    )
    .optional()
    .map_err(StoreError::from)
}

fn load_active_operation(
    conn: &Connection,
    cluster: &str,
) -> Result<Option<OperationRecord>, StoreError> {
    conn.query_row(
        "SELECT id, cluster, kind, state, config_rev, params, retry_of, actor, error, \
         created_at, started_at, finished_at, log_path, result, log_lines FROM operations \
         WHERE cluster = ?1 AND state IN ('queued','running') ORDER BY created_at LIMIT 1",
        [cluster],
        row_to_operation,
    )
    .optional()
    .map_err(StoreError::from)
}

fn row_to_operation(row: &Row<'_>) -> rusqlite::Result<OperationRecord> {
    let state: String = row.get(3)?;
    Ok(OperationRecord {
        id: row.get(0)?,
        cluster: row.get(1)?,
        kind: row.get(2)?,
        state: OperationState::parse(&state).unwrap_or(OperationState::Interrupted),
        config_rev: row.get(4)?,
        params: row.get(5)?,
        retry_of: row.get(6)?,
        actor: row.get(7)?,
        error: row.get(8)?,
        created_at: row.get(9)?,
        started_at: row.get(10)?,
        finished_at: row.get(11)?,
        log_path: row.get(12)?,
        result: row.get(13)?,
        log_lines: row.get::<_, Option<i64>>(14)?.unwrap_or(0),
    })
}

fn now() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .expect("RFC 3339 formatting cannot fail")
}
