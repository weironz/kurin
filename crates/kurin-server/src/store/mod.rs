//! SQLite store (WP-51): WAL, one dedicated writer thread and a read pool.
//!
//! Design: UI §6.6/§6.7. SQLite allows a single writer but many concurrent
//! readers in WAL mode, so every mutation is funnelled through one thread
//! owning the write connection, while [`Store::read`] borrows one of
//! `read_pool_size` pooled connections. This makes write ordering explicit
//! (no `SQLITE_BUSY` between our own writers) and keeps a slow write from
//! blocking a read.
//!
//! The schema is created from an empty database by [`schema::migrate`],
//! keyed on `PRAGMA user_version`; there is no migration framework.
//!
//! Low-level [`Store::read`] / [`Store::write`] exist for the operation
//! runner (WP-52) and for tests; cluster configuration goes through the
//! higher-level methods in [`cluster`].

mod cluster;
mod error;
mod operation;
mod package;
mod schema;
mod secret;
mod snapshot;

pub use cluster::{ClusterRecord, ConfigRevision, Lifecycle, RevisionSummary};
pub use error::StoreError;
pub use operation::{
    is_read_only_kind, NewOperation, OperationFilter, OperationRecord, OperationState, StoredEvent,
};
pub use package::{package_id, PackageRecord, PackageRegistration};
pub use secret::SECRET_SENTINEL;
pub use snapshot::SnapshotRecord;

use std::fs;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use rusqlite::Connection;

/// Database file name inside the data directory.
pub const DB_FILE: &str = "kurin.db";

/// Default number of read connections (UI §6.5 uses the same figure for the
/// global read semaphore).
pub const DEFAULT_READ_POOL_SIZE: usize = 4;

/// How to open a [`Store`].
#[derive(Debug, Clone)]
pub struct StoreOptions {
    /// Directory holding the database, the mirror files and (later) logs.
    pub data_dir: PathBuf,
    /// Override for the database file; defaults to `<data_dir>/kurin.db`.
    pub db_path: Option<PathBuf>,
    /// Number of pooled read connections (must be at least 1).
    pub read_pool_size: usize,
}

impl StoreOptions {
    /// Options for `data_dir` with the default database path and pool size.
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
            db_path: None,
            read_pool_size: DEFAULT_READ_POOL_SIZE,
        }
    }

    /// The database file this options set resolves to.
    pub fn resolved_db_path(&self) -> PathBuf {
        self.db_path
            .clone()
            .unwrap_or_else(|| self.data_dir.join(DB_FILE))
    }
}

type WriteJob = Box<dyn FnOnce(&mut Connection) + Send>;

/// Cloneable handle to the store.
#[derive(Clone)]
pub struct Store(Arc<StoreInner>);

struct StoreInner {
    /// `None` once the store is shutting down.
    writer: Mutex<Option<Sender<WriteJob>>>,
    writer_thread: Option<JoinHandle<()>>,
    readers: Vec<Mutex<Connection>>,
    next_reader: AtomicUsize,
    data_dir: PathBuf,
    db_path: PathBuf,
}

impl Store {
    /// Open (creating if needed) the store in `data_dir`.
    pub fn open(data_dir: impl AsRef<Path>) -> Result<Self, StoreError> {
        Self::open_with(StoreOptions::new(data_dir.as_ref()))
    }

    /// Open with explicit options; runs migrations before returning.
    pub fn open_with(options: StoreOptions) -> Result<Self, StoreError> {
        if options.read_pool_size == 0 {
            return Err(StoreError::InvalidOptions(
                "read_pool_size 必须至少为 1".into(),
            ));
        }
        create_private_dir(&options.data_dir)?;
        let db_path = options.resolved_db_path();
        if let Some(parent) = db_path.parent().filter(|p| !p.as_os_str().is_empty()) {
            create_private_dir(parent)?;
        }

        let mut writer_conn = Connection::open(&db_path)?;
        configure_writer(&writer_conn)?;
        schema::migrate(&mut writer_conn)?;

        let (writer, jobs) = mpsc::channel();
        let writer_thread = thread::Builder::new()
            .name("kurin-store-writer".into())
            .spawn(move || writer_loop(writer_conn, jobs))?;

        let mut readers = Vec::with_capacity(options.read_pool_size);
        for _ in 0..options.read_pool_size {
            let conn = Connection::open(&db_path)?;
            configure_reader(&conn)?;
            readers.push(Mutex::new(conn));
        }

        Ok(Self(Arc::new(StoreInner {
            writer: Mutex::new(Some(writer)),
            writer_thread: Some(writer_thread),
            readers,
            next_reader: AtomicUsize::new(0),
            data_dir: options.data_dir,
            db_path,
        })))
    }

    /// Run `f` on a pooled read connection. Concurrent calls use different
    /// connections; WAL keeps them consistent while a write is in flight.
    pub fn read<T, F>(&self, f: F) -> Result<T, StoreError>
    where
        F: FnOnce(&Connection) -> Result<T, StoreError>,
    {
        let index = self.0.next_reader.fetch_add(1, Ordering::Relaxed) % self.0.readers.len();
        let conn = self.0.readers[index]
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        f(&conn)
    }

    /// Run `f` on the single write connection, in submission order.
    ///
    /// `f` runs inside the writer thread; use [`Connection::transaction`]
    /// for atomic multi-statement changes.
    pub fn write<T, F>(&self, f: F) -> Result<T, StoreError>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T, StoreError> + Send + 'static,
    {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        {
            let guard = self
                .0
                .writer
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let sender = guard.as_ref().ok_or(StoreError::WriterClosed)?;
            sender
                .send(Box::new(move |conn| {
                    // A dropped receiver means the caller went away; the
                    // write still happened, which is the documented contract.
                    let _ = reply_tx.send(f(conn));
                }))
                .map_err(|_| StoreError::WriterClosed)?;
        }
        reply_rx.recv().map_err(|_| StoreError::WriterClosed)?
    }

    /// Directory holding the database and mirror files.
    pub fn data_dir(&self) -> &Path {
        &self.0.data_dir
    }

    /// The SQLite file backing this store.
    pub fn db_path(&self) -> &Path {
        &self.0.db_path
    }
}

impl Drop for StoreInner {
    fn drop(&mut self) {
        // Dropping the sender ends `writer_loop`; joining it guarantees no
        // write is lost when the last `Store` clone goes away.
        {
            let mut guard = self
                .writer
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            guard.take();
        }
        if let Some(handle) = self.writer_thread.take() {
            let _ = handle.join();
        }
    }
}

fn writer_loop(mut conn: Connection, jobs: Receiver<WriteJob>) {
    while let Ok(job) = jobs.recv() {
        job(&mut conn);
    }
}

fn configure_writer(conn: &Connection) -> Result<(), StoreError> {
    conn.busy_timeout(Duration::from_secs(5))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    Ok(())
}

fn configure_reader(conn: &Connection) -> Result<(), StoreError> {
    conn.busy_timeout(Duration::from_secs(5))?;
    // Belt and braces: a read connection must never mutate the database.
    conn.pragma_update(None, "query_only", "ON")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    Ok(())
}

fn create_private_dir(path: &Path) -> std::io::Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
}
