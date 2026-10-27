//! Content-addressed blob cache: `~/.cache/kurin/blobs/sha256/<hex>`.
//!
//! Every blob is named after its own sha256, so identical payloads —
//! binary artifacts, image layers, package files — are stored once no
//! matter how many names or versions point at them.
//!
//! Writes are crash-safe and concurrent-safe: the payload is streamed
//! into a unique `.part-*` file in the destination directory while its
//! sha256 is computed, fsynced, then renamed into place. A rename over
//! an existing blob is atomic on every supported platform, so two
//! writers racing on the same content both succeed and leave exactly one
//! blob (and no `.part` files).
//!
//! Reads are verified: [`BlobStore::open_blob`] and
//! [`BlobStore::verify`] re-hash the file and reject content that does
//! not match the digest it is stored under.

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use oci_spec::image::Digest;
use sha2::{Digest as _, Sha256};
use thiserror::Error;

/// Environment variable overriding the cache base directory
/// (`$XDG_CACHE_HOME`, else `$HOME/.cache`).
const XDG_CACHE_HOME: &str = "XDG_CACHE_HOME";

/// Counter making temporary file names unique within one process.
static PART_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A content-addressed blob store rooted at a directory containing
/// `sha256/<hex>`.
#[derive(Debug, Clone)]
pub struct BlobStore {
    root: PathBuf,
}

impl BlobStore {
    /// Open the default store: `$XDG_CACHE_HOME/kurin/blobs` (falling
    /// back to `$HOME/.cache/kurin/blobs`). The directory is created on
    /// the first write.
    pub fn open_default() -> Result<Self, StoreError> {
        let base = match std::env::var_os(XDG_CACHE_HOME).filter(|d| !d.is_empty()) {
            Some(dir) => PathBuf::from(dir),
            None => std::env::var_os("HOME")
                .filter(|h| !h.is_empty())
                .map(PathBuf::from)
                .ok_or(StoreError::NoCacheRoot)?
                .join(".cache"),
        };
        Ok(Self::open(base.join("kurin").join("blobs")))
    }

    /// Open a store rooted at `root` (the directory that contains
    /// `sha256/`). No filesystem access happens until a blob is written.
    pub fn open(root: impl Into<PathBuf>) -> Self {
        BlobStore { root: root.into() }
    }

    /// Store root (`.../blobs`).
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Path of `digest` inside the store (whether or not it exists).
    pub fn path(&self, digest: &Digest) -> PathBuf {
        self.root.join("sha256").join(digest_hex(digest))
    }

    /// True when a blob for `digest` exists. Does not verify its content;
    /// use [`Self::verify`] for that.
    pub fn has(&self, digest: &Digest) -> bool {
        self.path(digest).is_file()
    }

    /// Size in bytes of the stored blob.
    pub fn size(&self, digest: &Digest) -> Result<u64, StoreError> {
        let path = self.path(digest);
        let meta = fs::metadata(&path).map_err(|e| io_err(&path, e))?;
        if !meta.is_file() {
            return Err(StoreError::NotFound {
                digest: digest.to_string(),
                root: self.root.clone(),
            });
        }
        Ok(meta.len())
    }

    /// Open a blob for reading after verifying its sha256. A missing,
    /// truncated or tampered file is rejected.
    pub fn open_blob(&self, digest: &Digest) -> Result<File, StoreError> {
        self.verify(digest)?;
        let path = self.path(digest);
        File::open(&path).map_err(|e| io_err(&path, e))
    }

    /// Store an in-memory payload and return its digest.
    pub fn put_bytes(&self, bytes: &[u8]) -> Result<Digest, StoreError> {
        self.put_stream(bytes)
    }

    /// Stream `reader` into the store, hashing while writing, and return
    /// the resulting digest. The blob is fsynced and atomically renamed
    /// into place; concurrent writers of the same content all succeed.
    pub fn put_stream(&self, mut reader: impl Read) -> Result<Digest, StoreError> {
        let dir = self.root.join("sha256");
        fs::create_dir_all(&dir).map_err(|e| io_err(&dir, e))?;

        let part = dir.join(format!(
            ".part-{}-{}-{}",
            std::process::id(),
            PART_COUNTER.fetch_add(1, Ordering::Relaxed),
            unique_suffix()
        ));
        let result = self.write_part(&part, &mut reader);
        match result {
            Ok(digest) => {
                let dest = self.path(&digest);
                if let Err(e) = fs::rename(&part, &dest) {
                    let _ = fs::remove_file(&part);
                    return Err(io_err(&dest, e));
                }
                Ok(digest)
            }
            Err(e) => {
                let _ = fs::remove_file(&part);
                Err(e)
            }
        }
    }

    /// Re-hash the stored blob and compare it with `digest`.
    pub fn verify(&self, digest: &Digest) -> Result<(), StoreError> {
        let path = self.path(digest);
        let mut file = match File::open(&path) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(StoreError::NotFound {
                    digest: digest.to_string(),
                    root: self.root.clone(),
                })
            }
            Err(e) => return Err(io_err(&path, e)),
        };
        let actual = hash_reader(&mut file).map_err(|e| io_err(&path, e))?;
        if actual != digest_hex(digest) {
            return Err(StoreError::DigestMismatch {
                digest: digest.to_string(),
                path,
                actual,
            });
        }
        Ok(())
    }

    /// Verify a blob against a full OCI descriptor: both the declared
    /// size and the sha256 must match.
    pub fn verify_descriptor(
        &self,
        descriptor: &oci_spec::image::Descriptor,
    ) -> Result<(), StoreError> {
        let path = self.path(descriptor.digest());
        let actual_size = self.size(descriptor.digest())?;
        if actual_size != descriptor.size() {
            return Err(StoreError::SizeMismatch {
                digest: descriptor.digest().to_string(),
                path,
                expected: descriptor.size(),
                actual: actual_size,
            });
        }
        self.verify(descriptor.digest())
    }

    /// Delete every blob whose digest is not in `keep`, returning what
    /// was removed. Temporary `.part-*` files are left alone: another
    /// process may still be writing them.
    pub fn prune(&self, keep: &HashSet<Digest>) -> Result<PruneReport, StoreError> {
        let dir = self.root.join("sha256");
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(PruneReport::default()),
            Err(e) => return Err(io_err(&dir, e)),
        };
        let keep: HashSet<&str> = keep.iter().map(digest_hex).collect();
        let mut report = PruneReport::default();
        for entry in entries {
            let entry = entry.map_err(|e| io_err(&dir, e))?;
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if !is_sha256_hex(name) || keep.contains(name) {
                continue;
            }
            let meta = entry.metadata().map_err(|e| io_err(&path, e))?;
            if !meta.is_file() {
                continue;
            }
            fs::remove_file(&path).map_err(|e| io_err(&path, e))?;
            report.files_removed += 1;
            report.bytes_removed += meta.len();
        }
        Ok(report)
    }

    /// Every stored blob with its size. `.part-*` scratch files are
    /// not included. A missing store directory reads as empty.
    pub fn list(&self) -> Result<Vec<(Digest, u64)>, StoreError> {
        let dir = self.root.join("sha256");
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(io_err(&dir, e)),
        };
        let mut out = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| io_err(&dir, e))?;
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if !is_sha256_hex(name) {
                continue;
            }
            let meta = entry.metadata().map_err(|e| io_err(&path, e))?;
            if !meta.is_file() {
                continue;
            }
            out.push((digest_from_hex(name)?, meta.len()));
        }
        Ok(out)
    }

    /// Write `reader` to `part` while hashing; fsync before returning.
    fn write_part(&self, part: &Path, reader: &mut impl Read) -> Result<Digest, StoreError> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(part)
            .map_err(|e| io_err(part, e))?;
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let read = reader.read(&mut buf).map_err(|e| io_err(part, e))?;
            if read == 0 {
                break;
            }
            hasher.update(&buf[..read]);
            file.write_all(&buf[..read]).map_err(|e| io_err(part, e))?;
        }
        file.flush().map_err(|e| io_err(part, e))?;
        file.sync_all().map_err(|e| io_err(part, e))?;
        drop(file);
        digest_from_hex(&lower_hex(&hasher.finalize()))
    }
}

/// Result of [`BlobStore::prune`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PruneReport {
    /// Number of blob files deleted.
    pub files_removed: u64,
    /// Total bytes deleted.
    pub bytes_removed: u64,
}

/// Errors raised by the blob store.
#[derive(Debug, Error)]
pub enum StoreError {
    /// The default cache root could not be determined.
    #[error("cannot locate the blob cache root: set XDG_CACHE_HOME or HOME")]
    NoCacheRoot,

    /// No blob exists for the digest.
    #[error("blob {digest} is not in the store at {}", root.display())]
    NotFound { digest: String, root: PathBuf },

    /// The file's content does not hash to the digest it is stored under.
    #[error("blob {digest} is corrupt at {}: sha256 is {actual}", path.display())]
    DigestMismatch {
        digest: String,
        path: PathBuf,
        actual: String,
    },

    /// The file's size does not match the descriptor.
    #[error(
        "blob {digest} has size {actual} at {}, expected {expected}",
        path.display()
    )]
    SizeMismatch {
        digest: String,
        path: PathBuf,
        expected: u64,
        actual: u64,
    },

    /// A digest used a non-sha256 algorithm or an invalid hex value.
    #[error("unsupported or invalid digest `{0}` (only sha256 is supported)")]
    InvalidDigest(String),

    /// Filesystem failure.
    #[error("blob store I/O error at {}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// The hex value of a digest. Only sha256 is supported by this store;
/// other algorithms simply never match a stored sha256 file.
fn digest_hex(digest: &Digest) -> &str {
    digest.digest()
}

/// Build a sha256 [`Digest`] from a lowercase hex value.
fn digest_from_hex(hex: &str) -> Result<Digest, StoreError> {
    Digest::try_from(format!("sha256:{hex}"))
        .map_err(|_| StoreError::InvalidDigest(hex.to_string()))
}

fn lower_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn hash_reader(reader: &mut impl Read) -> io::Result<String> {
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let read = reader.read(&mut buf)?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
    }
    Ok(lower_hex(&hasher.finalize()))
}

fn is_sha256_hex(name: &str) -> bool {
    name.len() == 64 && name.bytes().all(|b| b.is_ascii_hexdigit())
}

/// A process-unique suffix so concurrent writers never collide on the
/// temporary file name.
fn unique_suffix() -> u128 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default()
}

fn io_err(path: &Path, source: io::Error) -> StoreError {
    StoreError::Io {
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_helpers_round_trip() {
        let digest = digest_from_hex(&"ab".repeat(32)).unwrap();
        assert_eq!(digest_hex(&digest), "ab".repeat(32));
    }
}
