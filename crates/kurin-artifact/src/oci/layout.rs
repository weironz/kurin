//! OCI image layout: the on-disk directory format shared by the local
//! package store, `oras`/`skopeo`, and the single-file `.tar` /
//! `.tar.zst` transport.
//!
//! A layout is a directory with
//!
//! ```text
//! oci-layout          {"imageLayoutVersion":"1.0.0"}
//! index.json          OCI image index (tags live in descriptor annotations)
//! blobs/sha256/<hex>  every referenced blob, content-addressed
//! ```
//!
//! [`Layout::pack_tar`] and [`Layout::pack_tar_zstd`] serialize a layout;
//! [`Layout::unpack_tar`] and [`Layout::unpack_tar_zstd`] stream one back
//! out. Extraction refuses absolute paths, `..` traversal and anything
//! that is not a regular file or directory, so an archive from an
//! untrusted medium cannot write outside the destination.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use oci_spec::image::{Descriptor, Digest, ImageIndex, MediaType, OciLayout};
use thiserror::Error;

use crate::store::{BlobStore, StoreError};

/// Annotation carrying the tag name on an index descriptor
/// (`org.opencontainers.image.ref.name`).
pub const REF_NAME_ANNOTATION: &str = "org.opencontainers.image.ref.name";

/// The only layout version this implementation reads and writes.
pub const LAYOUT_VERSION: &str = "1.0.0";

static INDEX_COUNTER: AtomicU64 = AtomicU64::new(0);

/// An opened OCI image layout directory.
#[derive(Debug, Clone)]
pub struct Layout {
    root: PathBuf,
    blobs: BlobStore,
}

impl Layout {
    /// Create (or re-create the marker of) a layout at `root`. Writes
    /// `oci-layout`, creates `blobs/sha256/`, and writes an empty
    /// `index.json` when none exists yet.
    pub fn create(root: impl Into<PathBuf>) -> Result<Self, LayoutError> {
        let root = root.into();
        let blobs_dir = root.join("blobs").join("sha256");
        fs::create_dir_all(&blobs_dir).map_err(|e| io_err(&blobs_dir, e))?;

        let marker = layout_marker(LAYOUT_VERSION);
        marker
            .to_file(root.join("oci-layout"))
            .map_err(LayoutError::Oci)?;

        let index_path = root.join("index.json");
        if !index_path.is_file() {
            let mut index = ImageIndex::default();
            index.set_media_type(Some(MediaType::ImageIndex));
            write_atomic(&index_path, &index.to_string()?)?;
        }

        Ok(Self {
            blobs: BlobStore::open(root.join("blobs")),
            root,
        })
    }

    /// Open an existing layout and validate its `oci-layout` marker.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, LayoutError> {
        let root = root.into();
        let marker_path = root.join("oci-layout");
        if !marker_path.is_file() {
            return Err(LayoutError::InvalidLayout {
                root: root.clone(),
                detail: "oci-layout is missing".to_string(),
            });
        }
        let marker =
            OciLayout::from_file(&marker_path).map_err(|e| LayoutError::InvalidLayout {
                root: root.clone(),
                detail: format!("cannot parse oci-layout: {e}"),
            })?;
        if marker.image_layout_version() != LAYOUT_VERSION {
            return Err(LayoutError::InvalidLayout {
                root: root.clone(),
                detail: format!(
                    "unsupported imageLayoutVersion `{}` (expected `{LAYOUT_VERSION}`)",
                    marker.image_layout_version()
                ),
            });
        }
        let index_path = root.join("index.json");
        if !index_path.is_file() {
            return Err(LayoutError::InvalidLayout {
                root: root.clone(),
                detail: "index.json is missing".to_string(),
            });
        }
        Ok(Self {
            blobs: BlobStore::open(root.join("blobs")),
            root,
        })
    }

    /// Layout root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `blobs/` directory.
    pub fn blobs_dir(&self) -> &Path {
        self.blobs.root()
    }

    /// The content-addressed store backing this layout.
    pub fn blobs(&self) -> &BlobStore {
        &self.blobs
    }

    /// Path of a blob inside this layout.
    pub fn blob_path(&self, digest: &Digest) -> PathBuf {
        self.blobs.path(digest)
    }

    /// Write the layout's `index.json` atomically.
    pub fn write_index(&self, index: &ImageIndex) -> Result<(), LayoutError> {
        let json = index.to_string()?;
        write_atomic(&self.root.join("index.json"), &json)
    }

    /// Read and parse `index.json`.
    pub fn read_index(&self) -> Result<ImageIndex, LayoutError> {
        let path = self.root.join("index.json");
        ImageIndex::from_file(&path).map_err(|e| LayoutError::InvalidLayout {
            root: self.root.clone(),
            detail: format!("cannot parse {}: {e}", path.display()),
        })
    }

    /// Store an in-memory blob and return its digest.
    pub fn put_blob(&self, bytes: &[u8]) -> Result<Digest, LayoutError> {
        Ok(self.blobs.put_bytes(bytes)?)
    }

    /// Stream a blob into the layout and return its digest.
    pub fn put_blob_stream(&self, reader: impl Read) -> Result<Digest, LayoutError> {
        Ok(self.blobs.put_stream(reader)?)
    }

    /// Resolve a tag (a descriptor's `org.opencontainers.image.ref.name`
    /// annotation) or a `sha256:...` digest to its descriptor.
    pub fn resolve(&self, reference: &str) -> Result<Descriptor, LayoutError> {
        let index = self.read_index()?;
        if let Ok(digest) = Digest::try_from(reference) {
            if let Some(found) = index.manifests().iter().find(|d| d.digest() == &digest) {
                return Ok(found.clone());
            }
        }
        if let Some(found) = index.manifests().iter().find(|d| {
            d.annotations()
                .as_ref()
                .and_then(|a| a.get(REF_NAME_ANNOTATION))
                .is_some_and(|name| name == reference)
        }) {
            return Ok(found.clone());
        }
        Err(LayoutError::ReferenceNotFound {
            reference: reference.to_string(),
        })
    }

    /// Write the whole layout as an uncompressed tar stream.
    pub fn pack_tar(&self, writer: impl Write) -> Result<(), LayoutError> {
        let files = collect_files(&self.root)?;
        let mut builder = tar::Builder::new(writer);
        for rel in files {
            let path = self.root.join(&rel);
            let mut file = File::open(&path).map_err(|e| io_err(&path, e))?;
            let meta = file.metadata().map_err(|e| io_err(&path, e))?;
            let mut header = tar::Header::new_gnu();
            header.set_path(&rel).map_err(|e| io_err(&path, e))?;
            header.set_size(meta.len());
            header.set_mode(0o644);
            header.set_mtime(0);
            header.set_uid(0);
            header.set_gid(0);
            header.set_cksum();
            builder
                .append(&header, &mut file)
                .map_err(|e| io_err(&path, e))?;
        }
        builder.finish().map_err(|e| io_err(&self.root, e))?;
        Ok(())
    }

    /// Write the whole layout as a zstd-compressed tar stream.
    pub fn pack_tar_zstd(&self, writer: impl Write) -> Result<(), LayoutError> {
        let mut encoder = zstd::Encoder::new(writer, 3).map_err(|e| io_err(&self.root, e))?;
        self.pack_tar(&mut encoder)?;
        encoder.finish().map_err(|e| io_err(&self.root, e))?;
        Ok(())
    }

    /// Stream an uncompressed tar into `dest` and return the validated
    /// layout. Entries are copied one at a time; nothing is buffered
    /// whole.
    pub fn unpack_tar(reader: impl Read, dest: impl AsRef<Path>) -> Result<Self, LayoutError> {
        let dest = dest.as_ref();
        fs::create_dir_all(dest).map_err(|e| io_err(dest, e))?;
        let mut archive = tar::Archive::new(reader);
        let entries = archive.entries().map_err(|e| io_err(dest, e))?;
        for entry in entries {
            let mut entry = entry.map_err(|e| io_err(dest, e))?;
            let path = entry.path().map_err(|e| io_err(dest, e))?.into_owned();
            validate_entry_path(&path)?;
            let target = dest.join(&path);
            let kind = entry.header().entry_type();
            if kind.is_dir() {
                fs::create_dir_all(&target).map_err(|e| io_err(&target, e))?;
            } else if kind.is_file() {
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent).map_err(|e| io_err(parent, e))?;
                }
                let mut out = OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .open(&target)
                    .map_err(|e| io_err(&target, e))?;
                io::copy(&mut entry, &mut out).map_err(|e| io_err(&target, e))?;
            } else {
                return Err(LayoutError::UnsupportedEntry { path });
            }
        }
        Self::open(dest)
    }

    /// Stream a zstd-compressed tar into `dest`.
    pub fn unpack_tar_zstd(reader: impl Read, dest: impl AsRef<Path>) -> Result<Self, LayoutError> {
        let decoder = zstd::Decoder::new(reader).map_err(|e| LayoutError::Io {
            path: dest.as_ref().to_path_buf(),
            source: e,
        })?;
        Self::unpack_tar(decoder, dest)
    }
}

/// Build the `oci-layout` marker for `version`.
fn layout_marker(version: &str) -> OciLayout {
    oci_spec::image::OciLayoutBuilder::default()
        .image_layout_version(version)
        .build()
        .expect("constant layout version is valid")
}

/// Reject anything that could escape `dest`: absolute paths, `..`
/// components, and empty names.
fn validate_entry_path(path: &Path) -> Result<(), LayoutError> {
    if path.as_os_str().is_empty() {
        return Err(LayoutError::UnsafeEntry {
            path: path.to_path_buf(),
            reason: "empty path",
        });
    }
    if path.is_absolute() {
        return Err(LayoutError::UnsafeEntry {
            path: path.to_path_buf(),
            reason: "absolute path",
        });
    }
    for component in path.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir => {
                return Err(LayoutError::UnsafeEntry {
                    path: path.to_path_buf(),
                    reason: "`..` traversal",
                })
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(LayoutError::UnsafeEntry {
                    path: path.to_path_buf(),
                    reason: "absolute path",
                })
            }
        }
    }
    Ok(())
}

/// Every regular file in the layout, as a sorted relative path.
fn collect_files(root: &Path) -> Result<Vec<PathBuf>, LayoutError> {
    let mut files = Vec::new();
    collect_into(root, Path::new(""), &mut files)?;
    files.sort();
    Ok(files)
}

fn collect_into(root: &Path, rel: &Path, files: &mut Vec<PathBuf>) -> Result<(), LayoutError> {
    let dir = root.join(rel);
    let mut entries: Vec<_> = fs::read_dir(&dir)
        .map_err(|e| io_err(&dir, e))?
        .collect::<Result<_, _>>()
        .map_err(|e| io_err(&dir, e))?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let name = entry.file_name();
        if name.to_string_lossy().starts_with(".part-") {
            continue;
        }
        let child_rel = rel.join(&name);
        let path = entry.path();
        let file_type = entry.file_type().map_err(|e| io_err(&path, e))?;
        if file_type.is_dir() {
            collect_into(root, &child_rel, files)?;
        } else if file_type.is_file() {
            files.push(child_rel);
        }
    }
    Ok(())
}

/// Write `contents` to `path` through a temporary file and rename.
fn write_atomic(path: &Path, contents: &str) -> Result<(), LayoutError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|e| io_err(parent, e))?;
    let tmp = parent.join(format!(
        ".part-{}-{}-index",
        std::process::id(),
        INDEX_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| -> io::Result<()> {
        let mut file = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        file.write_all(contents.as_bytes())?;
        file.flush()?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, path)
    })();
    if let Err(e) = result {
        let _ = fs::remove_file(&tmp);
        return Err(io_err(path, e));
    }
    Ok(())
}

/// Errors raised by OCI layout handling.
#[derive(Debug, Error)]
pub enum LayoutError {
    /// A blob-store operation failed.
    #[error(transparent)]
    Store(#[from] StoreError),

    /// An `oci-spec` type could not be parsed or serialized.
    #[error(transparent)]
    Oci(#[from] oci_spec::OciSpecError),

    /// The directory is not a usable OCI layout.
    #[error("invalid OCI layout at {}: {detail}", root.display())]
    InvalidLayout { root: PathBuf, detail: String },

    /// No index descriptor matches the requested tag or digest.
    #[error("reference `{reference}` not found in index.json")]
    ReferenceNotFound { reference: String },

    /// An archive entry would escape the destination directory.
    #[error("unsafe tar entry `{}`: {reason}", path.display())]
    UnsafeEntry { path: PathBuf, reason: &'static str },

    /// An archive entry is not a regular file or directory.
    #[error("unsupported tar entry `{}`: only files and directories are allowed", path.display())]
    UnsupportedEntry { path: PathBuf },

    /// Filesystem failure.
    #[error("OCI layout I/O error at {}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

fn io_err(path: &Path, source: io::Error) -> LayoutError {
    LayoutError::Io {
        path: path.to_path_buf(),
        source,
    }
}
