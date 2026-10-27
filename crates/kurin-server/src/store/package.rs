//! Registered package sources (WP-54, UI §8.1 包 page).
//!
//! `packages` rows are the sources an operator told the server about
//! (`POST /packages/import`) plus the ones created server-side
//! (`POST /packages/create-base`). Registration is idempotent on the
//! source string: re-importing the same URI refreshes the metadata and
//! the verification stamp instead of duplicating a row.

use rusqlite::{params, Connection, OptionalExtension, Row};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use super::{Store, StoreError};

/// A row of `packages`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageRecord {
    /// Stable id (a digest of the source string).
    pub id: String,
    /// The source URI as registered (`oci://…`, `oci-layout://…`,
    /// `file://…`).
    pub source: String,
    /// Where the row came from: `imported` or `created`.
    pub origin: String,
    /// Package kind (`base`, `addon`, `os`...).
    pub kind: String,
    /// Package name (`base` for the built-in base package).
    pub name: String,
    /// The version a `base` package pins.
    pub version: String,
    /// `kubeadm` or `k3s`.
    pub distribution: String,
    /// Architectures covered, comma-separated.
    pub arch: String,
    /// Total payload size in bytes.
    pub size: i64,
    /// Digest of the package index — the package's identity.
    pub index: String,
    /// Signatures verified (0 in digest mode).
    pub signatures: i64,
    /// SPDX SBOM document (pretty JSON), when one is attached.
    pub sbom: Option<String>,
    /// RFC 3339 creation time of the row.
    pub created_at: String,
    /// RFC 3339 time of the last successful verification.
    pub verified_at: String,
}

/// New or updated registration data for [`Store::upsert_package`].
#[derive(Debug, Clone)]
pub struct PackageRegistration {
    pub source: String,
    pub origin: String,
    pub kind: String,
    pub name: String,
    pub version: String,
    pub distribution: String,
    pub arch: String,
    pub size: i64,
    pub index: String,
    pub signatures: i64,
    pub sbom: Option<String>,
}

/// Row id: a short digest of the source string, so re-importing the same
/// source refreshes the same row.
pub fn package_id(source: &str) -> String {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(source.as_bytes());
    digest.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

fn row_to_package(row: &Row<'_>) -> rusqlite::Result<PackageRecord> {
    Ok(PackageRecord {
        id: row.get(0)?,
        source: row.get(1)?,
        origin: row.get(2)?,
        kind: row.get(3)?,
        name: row.get(4)?,
        version: row.get(5)?,
        distribution: row.get(6)?,
        arch: row.get(7)?,
        size: row.get(8)?,
        index: row.get(9)?,
        signatures: row.get(10)?,
        sbom: row.get(11)?,
        created_at: row.get(12)?,
        verified_at: row.get(13)?,
    })
}

impl Store {
    /// Insert or refresh a package registration. The row id derives from
    /// the source, so importing the same URI twice updates it.
    pub fn upsert_package(&self, reg: PackageRegistration) -> Result<PackageRecord, StoreError> {
        let id = package_id(&reg.source);
        let source_for_error = reg.source.clone();
        self.write(move |conn| {
            let timestamp = now();
            conn.execute(
                r#"INSERT INTO packages (id, source, origin, kind, name, version, distribution,
                 arch, size, "index", signatures, sbom, created_at, verified_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?13)
                 ON CONFLICT(id) DO UPDATE SET
                 origin = excluded.origin, kind = excluded.kind, name = excluded.name,
                 version = excluded.version, distribution = excluded.distribution,
                 arch = excluded.arch, size = excluded.size,
                 "index" = excluded."index",
                 signatures = excluded.signatures, sbom = excluded.sbom,
                 verified_at = excluded.verified_at"#,
                params![
                    &id,
                    &reg.source,
                    &reg.origin,
                    &reg.kind,
                    &reg.name,
                    &reg.version,
                    &reg.distribution,
                    &reg.arch,
                    reg.size,
                    &reg.index,
                    reg.signatures,
                    &reg.sbom,
                    &timestamp,
                ],
            )?;
            load_package(conn, &id)
        })?
        .ok_or_else(|| StoreError::PackageNotFound(source_for_error))
    }

    /// One registered package, or `None`.
    pub fn get_package(&self, id: &str) -> Result<Option<PackageRecord>, StoreError> {
        let id = id.to_string();
        self.read(move |conn| load_package(conn, &id))
    }

    /// Every registered package, oldest first.
    pub fn list_packages(&self) -> Result<Vec<PackageRecord>, StoreError> {
        self.read(|conn| {
            let mut statement = conn.prepare(
                r#"SELECT id, source, origin, kind, name, version, distribution, arch, size,
                 "index", signatures, sbom, created_at, verified_at FROM packages
                 ORDER BY created_at, id"#,
            )?;
            let rows = statement.query_map([], row_to_package)?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
                .map_err(StoreError::from)
        })
    }

    /// Remove one registration (the package itself is untouched).
    pub fn delete_package(&self, id: &str) -> Result<bool, StoreError> {
        let id = id.to_string();
        self.write(move |conn| {
            Ok(conn.execute("DELETE FROM packages WHERE id = ?1", params![&id])? > 0)
        })
    }

    /// Refresh the verification stamp after a successful verify run.
    pub fn touch_package_verified(&self, id: &str) -> Result<(), StoreError> {
        let id = id.to_string();
        self.write(move |conn| {
            conn.execute(
                "UPDATE packages SET verified_at = ?2, signatures = ?3 WHERE id = ?1",
                params![&id, now(), 0],
            )?;
            Ok(())
        })
    }
}

fn load_package(conn: &Connection, id: &str) -> Result<Option<PackageRecord>, StoreError> {
    conn.query_row(
        r#"SELECT id, source, origin, kind, name, version, distribution, arch, size,
         "index", signatures, sbom, created_at, verified_at FROM packages WHERE id = ?1"#,
        [id],
        row_to_package,
    )
    .optional()
    .map_err(StoreError::from)
}

fn now() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .expect("RFC 3339 formatting cannot fail")
}
