//! Collector snapshots (UI §6.7 `snapshots` table, filled by WP-53).
//!
//! One row per cluster: the last successful core report (`report`,
//! observed at `observed_at`) plus the last *attempt* (`attempted_at`,
//! `error`). A failed collection only touches the attempt columns — the
//! last good snapshot survives so the UI can show "unreachable / data
//! stale" instead of silently presenting old data as current (UI §6.9).
//!
//! The certificate probe is throttled to every 6 hours, so its result
//! (`certs_at`, `cert_min_days`) is carried in dedicated columns and only
//! overwritten by a round that actually probed certificates.

use rusqlite::{params, Connection, OptionalExtension, Row};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use super::{Store, StoreError};

/// A row of `snapshots`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotRecord {
    pub cluster: String,
    /// When the last attempt (successful or not) started.
    pub attempted_at: String,
    /// When `report` was observed; `None` until the first success.
    pub observed_at: Option<String>,
    /// The core `StatusReport` JSON of the last successful collection.
    pub report: Option<String>,
    /// The last attempt's error; `None` after a success.
    pub error: Option<String>,
    /// When certificates were last probed (every 6 h, UI §6.9).
    pub certs_at: Option<String>,
    /// Days until the earliest control-plane certificate expiry.
    pub cert_min_days: Option<i64>,
}

fn row_to_snapshot(row: &Row<'_>) -> rusqlite::Result<SnapshotRecord> {
    Ok(SnapshotRecord {
        cluster: row.get(0)?,
        attempted_at: row.get(1)?,
        observed_at: row.get(2)?,
        report: row.get(3)?,
        error: row.get(4)?,
        certs_at: row.get(5)?,
        cert_min_days: row.get(6)?,
    })
}

fn load_snapshot(conn: &Connection, cluster: &str) -> Result<Option<SnapshotRecord>, StoreError> {
    conn.query_row(
        "SELECT cluster, attempted_at, observed_at, report, error, certs_at, cert_min_days \
         FROM snapshots WHERE cluster = ?1",
        [cluster],
        row_to_snapshot,
    )
    .optional()
    .map_err(StoreError::from)
}

fn now() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .expect("UTC timestamps always format as RFC 3339")
}

impl Store {
    /// The cluster's snapshot row, or `None` when it was never collected.
    pub fn get_snapshot(&self, cluster: &str) -> Result<Option<SnapshotRecord>, StoreError> {
        self.read(|conn| load_snapshot(conn, cluster))
    }

    /// Record a successful core collection (UI §6.9).
    ///
    /// `report` is the serialized `StatusReport`. A round that also probed
    /// certificates (`certs_due`) overwrites both certificate columns —
    /// with `cert_min_days`, or with NULL when the probe found nothing; a
    /// core-only round leaves them alone so the last probe survives.
    pub fn save_snapshot_success(
        &self,
        cluster: &str,
        report: &str,
        certs_due: bool,
        cert_min_days: Option<i64>,
    ) -> Result<(), StoreError> {
        let cluster = cluster.to_string();
        let report = report.to_string();
        self.write(move |conn| {
            let timestamp = now();
            conn.execute(
                "INSERT INTO snapshots (cluster, attempted_at, observed_at, report, error, \
                 certs_at, cert_min_days) \
                 VALUES (?1, ?2, ?2, ?3, NULL, CASE WHEN ?4 THEN ?2 ELSE NULL END, ?5) \
                 ON CONFLICT(cluster) DO UPDATE SET \
                 attempted_at = excluded.attempted_at, \
                 observed_at = excluded.observed_at, \
                 report = excluded.report, \
                 error = NULL, \
                 certs_at = CASE WHEN ?4 THEN excluded.certs_at ELSE snapshots.certs_at END, \
                 cert_min_days = CASE WHEN ?4 THEN excluded.cert_min_days \
                 ELSE snapshots.cert_min_days END",
                params![&cluster, &timestamp, &report, certs_due, cert_min_days],
            )?;
            Ok(())
        })
    }

    /// Record a failed collection attempt: only `attempted_at` and `error`
    /// change; the last good `report` / `observed_at` / certificate columns
    /// are preserved verbatim (UI §6.9).
    pub fn save_snapshot_failure(&self, cluster: &str, error: &str) -> Result<(), StoreError> {
        let cluster = cluster.to_string();
        let error = error.to_string();
        self.write(move |conn| {
            let timestamp = now();
            conn.execute(
                "INSERT INTO snapshots (cluster, attempted_at, observed_at, report, error, \
                 certs_at, cert_min_days) VALUES (?1, ?2, NULL, NULL, ?3, NULL, NULL) \
                 ON CONFLICT(cluster) DO UPDATE SET \
                 attempted_at = excluded.attempted_at, error = excluded.error",
                params![&cluster, &timestamp, &error],
            )?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Lifecycle, StoreOptions};

    fn store(dir: &std::path::Path) -> Store {
        Store::open_with(StoreOptions::new(dir)).expect("open store")
    }

    #[test]
    fn success_then_failure_keeps_the_last_good_report() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        store.register_cluster("alpha", Lifecycle::Active).unwrap();

        assert!(store.get_snapshot("alpha").unwrap().is_none());

        store
            .save_snapshot_success("alpha", r#"{"healthy":true}"#, true, Some(42))
            .unwrap();
        let first = store.get_snapshot("alpha").unwrap().unwrap();
        assert_eq!(first.report.as_deref(), Some(r#"{"healthy":true}"#));
        assert_eq!(first.error, None);
        assert_eq!(first.cert_min_days, Some(42));
        let (observed, certs_at) = (first.observed_at.clone(), first.certs_at.clone());
        assert!(observed.is_some() && certs_at.is_some());

        // A core-only round refreshes the report but keeps the cert data.
        store
            .save_snapshot_success("alpha", r#"{"healthy":false}"#, false, None)
            .unwrap();
        let second = store.get_snapshot("alpha").unwrap().unwrap();
        assert_eq!(second.report.as_deref(), Some(r#"{"healthy":false}"#));
        assert_eq!(second.cert_min_days, Some(42));
        assert_eq!(second.certs_at, certs_at);

        // A failure only moves `attempted_at` / `error`.
        std::thread::sleep(std::time::Duration::from_millis(10));
        store.save_snapshot_failure("alpha", "ssh down").unwrap();
        let third = store.get_snapshot("alpha").unwrap().unwrap();
        assert_eq!(third.report.as_deref(), Some(r#"{"healthy":false}"#));
        assert_eq!(third.observed_at, second.observed_at);
        assert_eq!(third.error.as_deref(), Some("ssh down"));
        assert_eq!(third.cert_min_days, Some(42));
        assert!(third.attempted_at > second.attempted_at);

        // Recovery clears the error again.
        store
            .save_snapshot_success("alpha", r#"{"healthy":true}"#, true, Some(40))
            .unwrap();
        let fourth = store.get_snapshot("alpha").unwrap().unwrap();
        assert_eq!(fourth.error, None);
        assert_eq!(fourth.cert_min_days, Some(40));
    }

    #[test]
    fn failure_without_prior_success_writes_attempt_only() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        store.register_cluster("beta", Lifecycle::Active).unwrap();
        store
            .save_snapshot_failure("beta", "no route to host")
            .unwrap();
        let record = store.get_snapshot("beta").unwrap().unwrap();
        assert_eq!(record.report, None);
        assert_eq!(record.observed_at, None);
        assert_eq!(record.error.as_deref(), Some("no route to host"));
    }
}
