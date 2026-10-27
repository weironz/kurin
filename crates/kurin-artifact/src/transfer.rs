//! Moving packages between the three transport forms (04 §3.3, WP-33)
//! and maintaining the machine's content-addressed cache.
//!
//! A [`PackageRef`] names a package in any of the forms —
//!
//! | form | URI |
//! |---|---|
//! | OCI registry | `oci://<host>[:<port>]/<repo>[:<tag>][@<digest>]` |
//! | OCI layout directory | `oci-layout://<dir>` |
//! | single file | `file://<x>.tar` / `file://<x>.tar.zst` (bare paths tolerated) |
//!
//! — and [`transfer`] copies between any two of them, sending only what
//! the destination is missing: a registry is probed with HEAD requests,
//! a layout with digest lookups. [`verify_package`] re-hashes the whole
//! digest closure of a package in any form (signature checks arrive in
//! WP-39). [`list_cache`] and [`prune_cache`] maintain the local
//! content-addressed cache behind `kurin package list` / `prune`.

use std::collections::HashSet;
use std::fmt;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

use oci_spec::image::{Descriptor, Digest, ImageIndex, ImageManifest, MediaType};
use tempfile::TempDir;

use crate::artifact::ArtifactStore;
use crate::oci::layout::{Layout, REF_NAME_ANNOTATION};
use crate::oci::remote::{
    is_manifest_media_type, pull_package, push_package, resolve_registry_auth, RegistryRef,
};
use crate::package::PackageError;

// ---------------------------------------------------------------------------
// references
// ---------------------------------------------------------------------------

/// A package location in any of the three transport forms (04 §3.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackageRef {
    /// `oci://host[:port]/repo[:tag][@digest]` — an OCI registry.
    Registry(RegistryRef),
    /// `oci-layout://DIR` — an OCI layout directory.
    Layout(PathBuf),
    /// `file://X.tar` — a layout packed as an uncompressed tar.
    Tar(PathBuf),
    /// `file://X.tar.zst` — a layout packed as a zstd-compressed tar.
    TarZstd(PathBuf),
}

impl PackageRef {
    /// Parse one of the four reference forms. A bare path is tolerated
    /// for tars (matching `PackageOutput::parse`).
    pub fn parse(input: &str) -> Result<Self, PackageError> {
        let input = input.trim();
        if input.starts_with("oci://") {
            let reg = RegistryRef::parse(input).map_err(remote_error)?;
            return Ok(PackageRef::Registry(reg));
        }
        if let Some(rest) = input.strip_prefix("oci-layout://") {
            if rest.is_empty() {
                return Err(PackageError::InvalidRef {
                    input: input.to_string(),
                });
            }
            return Ok(PackageRef::Layout(PathBuf::from(rest)));
        }
        let path = input.strip_prefix("file://").unwrap_or(input);
        if path.ends_with(".tar.zst") {
            return Ok(PackageRef::TarZstd(PathBuf::from(path)));
        }
        if path.ends_with(".tar") {
            return Ok(PackageRef::Tar(PathBuf::from(path)));
        }
        Err(PackageError::InvalidRef {
            input: input.to_string(),
        })
    }

    /// True for the registry form.
    pub fn is_registry(&self) -> bool {
        matches!(self, PackageRef::Registry(_))
    }
}

impl fmt::Display for PackageRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PackageRef::Registry(reg) => write!(f, "oci://{}", reg.reference().whole()),
            PackageRef::Layout(dir) => write!(f, "oci-layout://{}", dir.display()),
            PackageRef::Tar(path) => write!(f, "file://{}", path.display()),
            PackageRef::TarZstd(path) => write!(f, "file://{}", path.display()),
        }
    }
}

/// Wrap a remote error with its source for the package error chain.
fn remote_error(e: crate::oci::remote::RemoteError) -> PackageError {
    PackageError::Remote {
        detail: e.to_string(),
        source: e,
    }
}

// ---------------------------------------------------------------------------
// options and reports
// ---------------------------------------------------------------------------

/// Credentials for a transfer: `--user` + `--password-env` when set,
/// else the Docker config entry for the registry, else anonymous.
#[derive(Debug, Clone, Default)]
pub struct TransferOptions {
    /// `--user`; requires `password_env`.
    pub user: Option<String>,
    /// `--password-env`; the environment variable holding the password.
    pub password_env: Option<String>,
}

impl TransferOptions {
    fn auth_for(&self, registry: &str) -> Result<oci_client::secrets::RegistryAuth, PackageError> {
        resolve_registry_auth(self.user.as_deref(), self.password_env.as_deref(), registry)
            .map_err(remote_error)
    }
}

/// What a transfer moved, form agnostic: "moved" counts everything that
/// had to cross to the destination, "present" what was already there.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TransferCounts {
    /// Blobs sent to / written at the destination.
    pub blobs_moved: usize,
    /// Blobs already at the destination (HEAD hit / digest hit).
    pub blobs_present: usize,
    /// Bytes behind the moved blobs.
    pub bytes_moved: u64,
    /// Manifest documents sent / written.
    pub manifests_moved: usize,
    /// Manifest documents already at the destination.
    pub manifests_present: usize,
}

impl TransferCounts {
    /// Total blobs in the closure.
    pub fn blobs_total(&self) -> usize {
        self.blobs_moved + self.blobs_present
    }

    /// Total manifests in the closure (the package index included).
    pub fn manifests_total(&self) -> usize {
        self.manifests_moved + self.manifests_present
    }
}

/// What [`transfer`] did.
#[derive(Debug, Clone)]
pub struct TransferReport {
    /// Digest of the package index — the package's identity.
    pub index: String,
    /// The tag the package travelled under, when known.
    pub tag: Option<String>,
    /// What moved.
    pub counts: TransferCounts,
    /// For local destinations: the layout directory or archive written.
    pub output: Option<PathBuf>,
}

/// What [`verify_package`] checked.
#[derive(Debug, Clone)]
pub struct VerifyReport {
    /// Digest of the package index — the package's identity.
    pub index: String,
    /// The tag the package is filed under, when known.
    pub tag: Option<String>,
    /// Manifest documents verified (the package index included).
    pub manifests: usize,
    /// Blobs verified.
    pub blobs: usize,
    /// Total verified payload bytes.
    pub bytes: u64,
}

// ---------------------------------------------------------------------------
// closure walking
// ---------------------------------------------------------------------------

/// The full content of a package: every blob and manifest document its
/// index references, transitively. Manifests come back children-first
/// (the package index itself is last), so a consumer that writes them in
/// order never references a manifest that does not exist yet.
#[derive(Debug, Clone, Default)]
pub struct PackageClosure {
    /// Leaf blobs (file layers, image layers, configs), in walk order.
    pub blobs: Vec<Descriptor>,
    /// Manifest documents, children before parents.
    pub manifests: Vec<ClosedManifest>,
}

/// One manifest document of a [`PackageClosure`].
#[derive(Debug, Clone)]
pub struct ClosedManifest {
    /// The descriptor the parent used to reference this manifest.
    pub descriptor: Descriptor,
    /// The exact stored bytes (digest-preserving copies re-send these).
    pub bytes: Vec<u8>,
}

/// Walk the whole closure below `root` in `layout`. Every manifest read
/// on the way is sha256-verified by the blob store, so a corrupted
/// source fails here rather than at the destination.
pub fn walk_closure(layout: &Layout, root: &Descriptor) -> Result<PackageClosure, PackageError> {
    let mut closure = PackageClosure::default();
    let mut seen: HashSet<Digest> = HashSet::new();
    walk(layout, root, &mut closure, &mut seen)?;
    Ok(closure)
}

fn walk(
    layout: &Layout,
    descriptor: &Descriptor,
    closure: &mut PackageClosure,
    seen: &mut HashSet<Digest>,
) -> Result<(), PackageError> {
    if !seen.insert(descriptor.digest().clone()) {
        return Ok(());
    }
    let mut bytes = Vec::new();
    layout
        .blobs()
        .open_blob(descriptor.digest())?
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 != descriptor.size() {
        return Err(PackageError::ManifestSizeMismatch {
            digest: descriptor.digest().to_string(),
            expected: descriptor.size(),
            actual: bytes.len() as u64,
        });
    }

    let media_type = descriptor.media_type().as_ref().to_string();
    let children: Vec<Descriptor> = if is_manifest_media_type(&media_type) {
        if media_type.ends_with("index.v1+json") || media_type.ends_with("manifest.list.v2+json") {
            let index: ImageIndex = serde_json::from_slice(&bytes)?;
            index.manifests().clone()
        } else {
            let manifest: ImageManifest = serde_json::from_slice(&bytes)?;
            std::iter::once(manifest.config().clone())
                .chain(manifest.layers().iter().cloned())
                .collect()
        }
    } else {
        return Err(PackageError::NotAPackage {
            path: layout.root().display().to_string(),
            detail: format!(
                "manifest-typed descriptor {digest} has unsupported media type {media_type}",
                digest = descriptor.digest()
            ),
        });
    };

    for child in children {
        if is_manifest_media_type(child.media_type().as_ref()) {
            walk(layout, &child, closure, seen)?;
        } else if seen.insert(child.digest().clone()) {
            closure.blobs.push(child);
        }
    }
    closure.manifests.push(ClosedManifest {
        descriptor: descriptor.clone(),
        bytes,
    });
    Ok(())
}

// ---------------------------------------------------------------------------
// materializing a source
// ---------------------------------------------------------------------------

/// A package opened for reading in any form: a layout plus the root
/// descriptor of the package index and its tag, when known.
pub struct MaterializedPackage {
    /// The backing layout (a temp dir when the source was a registry or
    /// an archive).
    pub layout: Layout,
    /// Keeps the temp dir (if any) alive as long as the package.
    pub temp: Option<TempDir>,
    /// The package index descriptor from the layout's `index.json`.
    pub root: Descriptor,
    /// The `org.opencontainers.image.ref.name` of the root, when set.
    pub tag: Option<String>,
}

/// Open `src` as a layout-backed package. Registry sources are pulled
/// into a fresh temp layout first — the pull verifies every blob as it
/// arrives.
pub async fn materialize(
    src: &PackageRef,
    opts: &TransferOptions,
) -> Result<MaterializedPackage, PackageError> {
    match src {
        PackageRef::Registry(reg) => {
            let auth = opts.auth_for(reg.registry())?;
            let temp = TempDir::new()?;
            let layout = Layout::create(temp.path())?;
            let outcome = pull_package(reg, &layout, &auth)
                .await
                .map_err(remote_error)?;
            Ok(MaterializedPackage {
                layout,
                root: outcome.root,
                tag: outcome.tag,
                temp: Some(temp),
            })
        }
        PackageRef::Layout(dir) => open_local(Layout::open(dir.clone())?),
        PackageRef::Tar(path) => {
            let temp = TempDir::new()?;
            let file = File::open(path)?;
            Ok(open_local(Layout::unpack_tar(file, temp.path())?)?.with_temp(temp))
        }
        PackageRef::TarZstd(path) => {
            let temp = TempDir::new()?;
            let file = File::open(path)?;
            Ok(open_local(Layout::unpack_tar_zstd(file, temp.path())?)?.with_temp(temp))
        }
    }
}

/// Read the root descriptor (and its tag) out of a local layout's
/// `index.json`.
fn open_local(layout: Layout) -> Result<MaterializedPackage, PackageError> {
    let index = layout.read_index()?;
    let root = index
        .manifests()
        .first()
        .cloned()
        .ok_or_else(|| PackageError::NotAPackage {
            path: layout.root().display().to_string(),
            detail: "index.json has no manifests".to_string(),
        })?;
    let tag = root
        .annotations()
        .as_ref()
        .and_then(|a| a.get(REF_NAME_ANNOTATION))
        .cloned();
    Ok(MaterializedPackage {
        layout,
        root,
        tag,
        temp: None,
    })
}

impl MaterializedPackage {
    fn with_temp(mut self, temp: TempDir) -> Self {
        self.temp = Some(temp);
        self
    }
}

// ---------------------------------------------------------------------------
// transfer
// ---------------------------------------------------------------------------

/// Copy the package at `src` to `dst` (04 §3.3): any two forms, sending
/// only what the destination is missing. This is the engine behind both
/// `kurin package push` and `kurin package pull` — the commands differ
/// in spelling, not in behavior.
pub async fn transfer(
    src: &PackageRef,
    dst: &PackageRef,
    opts: &TransferOptions,
) -> Result<TransferReport, PackageError> {
    let source = materialize(src, opts).await?;
    let closure = walk_closure(&source.layout, &source.root)?;
    let index = source.root.digest().to_string();

    match dst {
        PackageRef::Registry(reg) => {
            let auth = opts.auth_for(reg.registry())?;
            let counts = push_package(&source.layout, &closure, reg, &auth)
                .await
                .map_err(remote_error)?;
            Ok(TransferReport {
                index,
                tag: reg.tag().map(str::to_string).or(source.tag.clone()),
                counts,
                output: None,
            })
        }
        PackageRef::Layout(dir) => {
            let layout = Layout::create(dir.clone())?;
            let counts = copy_closure(&source.layout, &closure, &layout)?;
            write_merged_index(&layout, &tagged_root(&source.root, source.tag.as_deref()))?;
            Ok(TransferReport {
                index,
                tag: source.tag.clone(),
                counts,
                output: Some(dir.clone()),
            })
        }
        PackageRef::Tar(path) => {
            let staging = TempDir::new()?;
            let layout = Layout::create(staging.path())?;
            let counts = copy_closure(&source.layout, &closure, &layout)?;
            write_merged_index(&layout, &tagged_root(&source.root, source.tag.as_deref()))?;
            let file = File::create(path)?;
            layout.pack_tar(file)?;
            Ok(TransferReport {
                index,
                tag: source.tag.clone(),
                counts,
                output: Some(path.clone()),
            })
        }
        PackageRef::TarZstd(path) => {
            let staging = TempDir::new()?;
            let layout = Layout::create(staging.path())?;
            let counts = copy_closure(&source.layout, &closure, &layout)?;
            write_merged_index(&layout, &tagged_root(&source.root, source.tag.as_deref()))?;
            let file = File::create(path)?;
            layout.pack_tar_zstd(file)?;
            Ok(TransferReport {
                index,
                tag: source.tag.clone(),
                counts,
                output: Some(path.clone()),
            })
        }
    }
}

/// Copy a closure between two layouts, skipping digests the destination
/// already stores. Source blobs are read through `open_blob`, which
/// re-hashes them: a corrupted source is rejected, not propagated.
fn copy_closure(
    source: &Layout,
    closure: &PackageClosure,
    destination: &Layout,
) -> Result<TransferCounts, PackageError> {
    let mut counts = TransferCounts::default();
    for blob in &closure.blobs {
        if destination.blobs().has(blob.digest()) {
            counts.blobs_present += 1;
            continue;
        }
        let mut reader = source.blobs().open_blob(blob.digest())?;
        let stored = destination.blobs().put_stream(&mut reader)?;
        if stored != *blob.digest() {
            return Err(PackageError::Remote {
                detail: format!("copied blob {stored} but expected {}", blob.digest()),
                source: crate::oci::remote::RemoteError::DigestMismatch {
                    expected_ref: blob.digest().to_string(),
                    expected: blob.digest().to_string(),
                    actual: stored.to_string(),
                },
            });
        }
        counts.blobs_moved += 1;
        counts.bytes_moved += blob.size();
    }
    for manifest in &closure.manifests {
        if destination.blobs().has(manifest.descriptor.digest()) {
            counts.manifests_present += 1;
            continue;
        }
        let stored = destination.blobs().put_bytes(&manifest.bytes)?;
        if stored != *manifest.descriptor.digest() {
            return Err(PackageError::Remote {
                detail: format!(
                    "stored manifest {stored} but expected {}",
                    manifest.descriptor.digest()
                ),
                source: crate::oci::remote::RemoteError::DigestMismatch {
                    expected_ref: manifest.descriptor.digest().to_string(),
                    expected: manifest.descriptor.digest().to_string(),
                    actual: stored.to_string(),
                },
            });
        }
        counts.manifests_moved += 1;
    }
    Ok(counts)
}

/// The root descriptor with its tag annotation set / kept, so a local
/// destination files the package under the name it travelled with.
fn tagged_root(root: &Descriptor, tag: Option<&str>) -> Descriptor {
    let mut tagged = root.clone();
    if let Some(tag) = tag {
        let mut annotations = tagged.annotations().clone().unwrap_or_default();
        annotations.insert(REF_NAME_ANNOTATION.to_string(), tag.to_string());
        tagged.set_annotations(Some(annotations));
    }
    tagged
}

/// Merge `root` into a layout's `index.json`: an entry with the same tag
/// is replaced, otherwise the entry is appended. A layout can therefore
/// hold several packages, one per tag.
fn write_merged_index(layout: &Layout, root: &Descriptor) -> Result<(), PackageError> {
    let mut index = layout.read_index().unwrap_or_else(|_| {
        let mut index = ImageIndex::default();
        index.set_media_type(Some(MediaType::ImageIndex));
        index
    });
    let tag = root
        .annotations()
        .as_ref()
        .and_then(|a| a.get(REF_NAME_ANNOTATION))
        .cloned();
    let mut manifests = index.manifests().clone();
    if let Some(tag) = &tag {
        manifests.retain(|d| {
            d.annotations()
                .as_ref()
                .and_then(|a| a.get(REF_NAME_ANNOTATION))
                != Some(tag)
        });
    }
    manifests.push(root.clone());
    index.set_manifests(manifests);
    layout.write_index(&index)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// verify
// ---------------------------------------------------------------------------

/// Verify a package end to end (04 §6 integrity layer): every blob in
/// the digest closure below its index is re-hashed and every manifest
/// document must parse. Registry sources are pulled (and thereby
/// verified on arrival) first. Signature verification arrives in WP-39.
pub async fn verify_package(
    src: &PackageRef,
    opts: &TransferOptions,
) -> Result<VerifyReport, PackageError> {
    let source = materialize(src, opts).await?;
    let closure = walk_closure(&source.layout, &source.root)?;
    let mut failures: Vec<String> = Vec::new();
    let mut bytes = 0u64;
    for blob in &closure.blobs {
        match source.layout.blobs().verify_descriptor(blob) {
            Ok(()) => bytes += blob.size(),
            Err(e) => failures.push(format!("{}: {e}", blob.digest())),
        }
    }
    if !failures.is_empty() {
        return Err(PackageError::Verification { failures });
    }
    Ok(VerifyReport {
        index: source.root.digest().to_string(),
        tag: source.tag.clone(),
        manifests: closure.manifests.len(),
        blobs: closure.blobs.len(),
        bytes,
    })
}

// ---------------------------------------------------------------------------
// the local content-addressed cache (kurin package list | prune)
// ---------------------------------------------------------------------------

/// One entry of the local content-addressed cache.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheEntry {
    /// The blob's sha256 digest.
    pub digest: String,
    /// Size in bytes.
    pub size: u64,
}

/// What [`prune_cache`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CachePruneReport {
    /// Blobs kept: referenced by the artifact cache's name→digest index.
    pub kept: usize,
    /// Blobs removed.
    pub removed: usize,
    /// Bytes removed.
    pub bytes_removed: u64,
}

/// List every blob in the machine's content-addressed cache (the blob
/// store behind the artifact cache), newest-unsorted, digest order.
pub fn list_cache(store: &ArtifactStore) -> Result<Vec<CacheEntry>, PackageError> {
    let mut entries: Vec<CacheEntry> = store
        .blob_store()
        .list()?
        .into_iter()
        .map(|(digest, size)| CacheEntry {
            digest: digest.to_string(),
            size,
        })
        .collect();
    entries.sort_by(|a, b| a.digest.cmp(&b.digest));
    Ok(entries)
}

/// Delete cache blobs no artifact cache entry references. The keep-set
/// is the digest of every payload file under the artifact cache's
/// `<name>-<version>-<arch>/` directories (the name→digest index of
/// WP-31); anything else in the blob store is leftover staging.
pub fn prune_cache(store: &ArtifactStore) -> Result<CachePruneReport, PackageError> {
    let keep = referenced_digests(store.root())?;
    let before = store.blob_store().list()?;
    let pruned = store.blob_store().prune(&keep)?;
    let kept = before.len().saturating_sub(pruned.files_removed as usize);
    Ok(CachePruneReport {
        kept,
        removed: pruned.files_removed as usize,
        bytes_removed: pruned.bytes_removed,
    })
}

/// Digests of every payload file under the artifact cache root
/// (`.sha256` sidecars and interrupted `.part` files excluded).
fn referenced_digests(root: &Path) -> Result<HashSet<Digest>, PackageError> {
    use sha2::Digest as _;
    let mut keep = HashSet::new();
    let Ok(dirs) = fs::read_dir(root) else {
        return Ok(keep);
    };
    for dir in dirs {
        let dir = dir?.path();
        if !dir.is_dir() {
            continue;
        }
        let Ok(files) = fs::read_dir(&dir) else {
            continue;
        };
        for file in files {
            let file = file?.path();
            if !file.is_file() || is_sidecar(&file) || is_part(&file) {
                continue;
            }
            let mut hasher = sha2::Sha256::new();
            let mut handle = File::open(&file)?;
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                let read = handle.read(&mut buf)?;
                if read == 0 {
                    break;
                }
                hasher.update(&buf[..read]);
            }
            let digest = format!("sha256:{:x}", hasher.finalize());
            if let Ok(digest) = Digest::try_from(digest.as_str()) {
                keep.insert(digest);
            }
        }
    }
    Ok(keep)
}

fn is_sidecar(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()) == Some("sha256")
}

fn is_part(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()) == Some("part")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_refs_parse_all_four_forms() {
        assert!(matches!(
            PackageRef::parse("oci://reg.io/kurin/base:v1").unwrap(),
            PackageRef::Registry(_)
        ));
        assert_eq!(
            PackageRef::parse("oci-layout:///srv/kurin/base").unwrap(),
            PackageRef::Layout(PathBuf::from("/srv/kurin/base"))
        );
        assert_eq!(
            PackageRef::parse("file:///tmp/base.tar").unwrap(),
            PackageRef::Tar(PathBuf::from("/tmp/base.tar"))
        );
        assert_eq!(
            PackageRef::parse("file:///media/usb/base.tar.zst").unwrap(),
            PackageRef::TarZstd(PathBuf::from("/media/usb/base.tar.zst"))
        );
        // bare tar paths are tolerated, like PackageOutput::parse
        assert!(matches!(
            PackageRef::parse("/tmp/base.tar.zst").unwrap(),
            PackageRef::TarZstd(_)
        ));
        for bad in ["", "/srv/dir", "oci-layout://", "file:///srv/dir"] {
            assert!(PackageRef::parse(bad).is_err(), "{bad:?} should not parse");
        }
    }

    #[test]
    fn package_refs_display_round_trip() {
        for uri in [
            "oci://reg.io/kurin/base:v1",
            "oci-layout:///srv/base",
            "file:///tmp/base.tar",
            "file:///tmp/base.tar.zst",
        ] {
            let parsed = PackageRef::parse(uri).unwrap();
            assert_eq!(parsed.to_string(), uri, "round trip of {uri}");
        }
    }
}
