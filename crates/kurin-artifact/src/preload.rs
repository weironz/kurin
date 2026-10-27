//! Preload delivery (WP-35, 04 §5.1): where one node's payload bytes come
//! from, and how an image becomes a single-image OCI layout tar.
//!
//! A run's payloads come from exactly one of three sources, all selected
//! through [`kurin_core::run::ArtifactSources`]:
//!
//! - **online** — the pinned manifest's URLs, fetched into the local
//!   content-addressed cache;
//! - **legacy bundle** — a pre-package offline directory
//!   (`kurin-manifest.json` + per-artifact directories);
//! - **package** — a kurin OCI package (`oci-layout://`, `file://X.tar[.zst]`),
//!   opened and digest-verified.
//!
//! [`PayloadSet`] resolves one artifact (or image) to the local blob that
//! carries it, so the install tasks stream it to a node without ever
//! buffering the payload: [`BlobRef`] is a path plus the digest and size
//! its metadata pins, which is exactly what
//! [`kurin_core::engine::UploadSource::Blob`] carries.
//!
//! Images are re-hosted as a **single-image OCI layout tar**:
//! `oci-layout` + an `index.json` naming the selected platform's manifest
//! (tagged with the upstream reference) + the manifest, config and layer
//! blobs. `ctr images import` and k3s's agent image directory both accept
//! that form, and only the requested architecture is carried.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};

use oci_spec::image::{
    DescriptorBuilder, Digest, ImageIndex, ImageIndexBuilder, ImageManifest, MediaType, Platform,
    PlatformBuilder,
};
use sha2::{Digest as _, Sha256};
use thiserror::Error;

use kurin_core::engine::{BlobSource, UploadSource};
use kurin_core::run::ArtifactSources;
use kurin_core::zone::Zone;

use crate::artifact::{
    image_chain, install_manifest, Arch, ArtifactError, ArtifactManifest, ArtifactSpec,
    ArtifactStore, DirManifest,
};
use crate::oci::layout::REF_NAME_ANNOTATION;
use crate::package::{PackageError, PackageImageMeta, PackageSource};
use crate::store::{BlobStore, StoreError};
use crate::transfer::{materialize, PackageRef, TransferOptions};

/// The `oci-layout` marker's content (OCI image layout 1.0.0).
const LAYOUT_MARKER: &str = r#"{"imageLayoutVersion":"1.0.0"}"#;

/// Packages opened by this process, by source URI. A `file://` tar is
/// unpacked once and an `oci://` package pulled once, no matter how many
/// node tasks stream its blobs; a local layout is a cheap path wrapper.
static PACKAGES: LazyLock<Mutex<HashMap<String, Arc<PackageSource>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Open a package source and share it process-wide (see [`PACKAGES`]).
///
/// All four transport forms of 04 §3.3 are accepted: `oci://` is pulled
/// into a verified scratch layout, `file://X.tar[.zst]` is unpacked, and
/// `oci-layout://DIR` is used in place.
///
/// # Errors
///
/// [`PreloadError`] when the reference is malformed, the registry pull
/// fails, or the archive is not a package.
pub async fn open_package(uri: &str) -> Result<Arc<PackageSource>, PreloadError> {
    if let Some(found) = PACKAGES.lock().unwrap_or_else(|e| e.into_inner()).get(uri) {
        return Ok(Arc::clone(found));
    }
    let reference = PackageRef::parse(uri)?;
    let materialized = materialize(&reference, &TransferOptions::default()).await?;
    let package = Arc::new(PackageSource::from_layout(
        materialized.layout,
        materialized.temp,
    ));
    PACKAGES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(uri.to_string(), Arc::clone(&package));
    Ok(package)
}

/// One blob a registry push needs: a local file plus its digest/size and
/// whether it is a manifest (uploaded through the manifests API) or a
/// content blob.
#[derive(Debug, Clone)]
pub struct PushBlob {
    /// `sha256:<hex>` digest.
    pub digest: String,
    /// Local file holding the bytes.
    pub path: PathBuf,
    /// Payload size in bytes.
    pub size: u64,
    /// Media type sent with the upload.
    pub media_type: String,
    /// True for an image index/manifest (PUT manifests), false for
    /// config/layer blobs (PUT blobs).
    pub manifest: bool,
}

/// A complete image ready to be pushed to a registry under one tag: the
/// upstream multi-architecture index plus every blob it reaches.
#[derive(Debug, Clone)]
pub struct ImagePushPayload {
    /// Upstream reference (`registry.k8s.io/kube-apiserver:v1.33.1`).
    pub reference: String,
    /// Digest of the index itself.
    pub index_digest: String,
    /// Index bytes.
    pub index_bytes: Vec<u8>,
    /// Index media type.
    pub media_type: String,
    /// Sub-manifests and blobs (the index is pushed separately).
    pub blobs: Vec<PushBlob>,
}

/// One payload blob a node needs: a local file plus the digest and size
/// its metadata pins.
#[derive(Debug, Clone)]
pub struct BlobRef {
    /// Local file holding the payload.
    pub path: PathBuf,
    /// `sha256:<hex>` of the payload.
    pub digest: String,
    /// Payload size in bytes.
    pub size: u64,
}

impl BlobRef {
    /// The streaming upload source for this blob.
    pub fn upload_source(&self) -> UploadSource {
        UploadSource::Blob(BlobSource::new(
            self.path.clone(),
            self.digest.clone(),
            self.size,
        ))
    }
}

/// Where one run's payloads come from (04 §5.1 preload delivery).
pub enum PayloadSet {
    /// Fetch from the network into the local cache.
    Online { store: ArtifactStore, zone: Zone },
    /// A legacy offline bundle directory, verified once on open.
    Legacy { manifest: DirManifest },
    /// A kurin package, opened and digest-verified.
    Package { package: Arc<PackageSource> },
}

impl std::fmt::Debug for PayloadSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PayloadSet::Online { .. } => f.write_str("PayloadSet::Online"),
            PayloadSet::Legacy { manifest } => f
                .debug_struct("PayloadSet::Legacy")
                .field("path", &manifest.path)
                .finish(),
            PayloadSet::Package { .. } => f.write_str("PayloadSet::Package"),
        }
    }
}

impl PayloadSet {
    /// Open the run's payload source. A legacy bundle is verified end to
    /// end here (once per task, not once per artifact); a package is
    /// opened once per process and shared by every node task.
    ///
    /// # Errors
    ///
    /// [`PreloadError`] when a package cannot be opened or the legacy
    /// bundle is incomplete.
    pub async fn open(sources: &ArtifactSources) -> Result<Self, PreloadError> {
        if let Some(uri) = &sources.base {
            let package = open_package(uri).await?;
            return Ok(PayloadSet::Package { package });
        }
        if let Some(dir) = &sources.offline_dir {
            let manifest = install_manifest(sources)?;
            let dir_manifest = ArtifactManifest::from_dir_with(dir, &manifest)?;
            return Ok(PayloadSet::Legacy {
                manifest: dir_manifest,
            });
        }
        Ok(PayloadSet::Online {
            store: ArtifactStore::open_default()?,
            zone: sources.zone,
        })
    }

    /// The local blob for one file artifact (binary or plain manifest).
    ///
    /// # Errors
    ///
    /// [`PreloadError::MissingFile`] when the selected source does not
    /// carry the artifact for this architecture.
    pub async fn file(&self, spec: &ArtifactSpec) -> Result<BlobRef, PreloadError> {
        match self {
            PayloadSet::Online { store, zone } => {
                let path = store.fetch(spec, *zone).await?;
                let digest = spec
                    .expected_sha256()
                    .ok_or_else(|| PreloadError::Unpinned {
                        name: spec.name.clone(),
                        arch: spec.arch.as_str().to_string(),
                    })?;
                let size = std::fs::metadata(&path)?.len();
                Ok(BlobRef {
                    path,
                    digest: format!("sha256:{digest}"),
                    size,
                })
            }
            PayloadSet::Legacy { manifest } => {
                let found = manifest.find(&spec.name, spec.arch).ok_or_else(|| {
                    PreloadError::MissingFile {
                        name: spec.name.clone(),
                        arch: spec.arch.as_str().to_string(),
                    }
                })?;
                let size = std::fs::metadata(&found.file)?.len();
                Ok(BlobRef {
                    path: found.file.clone(),
                    digest: format!("sha256:{}", found.sha256),
                    size,
                })
            }
            PayloadSet::Package { package } => {
                let meta = package.manifest()?;
                let artifact = meta
                    .artifacts
                    .iter()
                    .find(|a| a.name == spec.name && a.arch == spec.arch.as_str())
                    .ok_or_else(|| PreloadError::MissingFile {
                        name: spec.name.clone(),
                        arch: spec.arch.as_str().to_string(),
                    })?;
                let digest = Digest::try_from(artifact.digest.as_str()).map_err(|e| {
                    PreloadError::InvalidDigest {
                        digest: artifact.digest.clone(),
                        detail: e.to_string(),
                    }
                })?;
                Ok(BlobRef {
                    path: package.layout().blob_path(&digest),
                    digest: artifact.digest.clone(),
                    size: artifact.size,
                })
            }
        }
    }

    /// The selected image as a registry push payload: the upstream
    /// multi-architecture index and every blob it reaches.
    ///
    /// The push keeps the upstream index intact (digests unchanged), so a
    /// mixed-architecture cluster resolves the same image correctly from
    /// the mirror.
    ///
    /// # Errors
    ///
    /// [`PreloadError::MissingImage`] when the source does not carry the
    /// image.
    pub async fn image_for_push(
        &self,
        spec: &ArtifactSpec,
    ) -> Result<ImagePushPayload, PreloadError> {
        match self {
            PayloadSet::Online { store, zone } => {
                store.fetch(spec, *zone).await?;
                let dir = store.artifact_dir(spec);
                image_push_from_dir(&dir, &spec.url())
            }
            PayloadSet::Legacy { manifest } => {
                let found = manifest.find(&spec.name, spec.arch).ok_or_else(|| {
                    PreloadError::MissingImage {
                        reference: spec.url(),
                    }
                })?;
                let dir = found
                    .file
                    .parent()
                    .ok_or_else(|| PreloadError::MissingImage {
                        reference: spec.url(),
                    })?
                    .to_path_buf();
                image_push_from_dir(&dir, &spec.url())
            }
            PayloadSet::Package { package } => image_push_from_package(package, spec),
        }
    }

    /// The selected platform's image, ready to be re-hosted as a
    /// single-image OCI layout tar.
    ///
    /// # Errors
    ///
    /// [`PreloadError::MissingImage`] when the source does not carry the
    /// image (or this architecture's sub-manifest).
    pub async fn image(&self, spec: &ArtifactSpec) -> Result<ImagePayload, PreloadError> {
        match self {
            PayloadSet::Online { store, zone } => {
                store.fetch(spec, *zone).await?;
                let dir = store.artifact_dir(spec);
                let expected = spec
                    .expected_sha256()
                    .ok_or_else(|| PreloadError::Unpinned {
                        name: spec.name.clone(),
                        arch: spec.arch.as_str().to_string(),
                    })?;
                image_from_cache(&dir, spec, &expected)
            }
            PayloadSet::Legacy { manifest } => {
                let found = manifest.find(&spec.name, spec.arch).ok_or_else(|| {
                    PreloadError::MissingImage {
                        reference: spec.url(),
                    }
                })?;
                let dir = found
                    .file
                    .parent()
                    .ok_or_else(|| PreloadError::MissingImage {
                        reference: spec.url(),
                    })?
                    .to_path_buf();
                image_from_cache(&dir, spec, &found.sha256)
            }
            PayloadSet::Package { package } => image_from_package(package, spec),
        }
    }
}

/// One image resolved for one architecture: everything needed to write a
/// single-image OCI layout tar.
pub struct ImagePayload {
    /// Upstream reference; becomes the layout's `ref.name` annotation.
    reference: String,
    /// The selected platform's manifest digest.
    manifest: Digest,
    /// The selected platform's manifest media type.
    manifest_media_type: MediaType,
    /// The selected platform's manifest size.
    manifest_size: u64,
    /// Architecture the manifest was selected for.
    arch: Arch,
    /// Config and layer digests, in manifest order.
    blobs: Vec<Digest>,
    /// Where the blobs are read from.
    source: BlobDir,
}

impl ImagePayload {
    /// The upstream reference this image is imported as.
    pub fn reference(&self) -> &str {
        &self.reference
    }

    /// Stream the image as a single-image OCI layout tar into `writer`.
    ///
    /// Entry order is fixed (`oci-layout`, `index.json`, then blobs by
    /// digest) and every header is normalized (mode 0644, mtime 0), so the
    /// same image always produces the same bytes — which is what lets a
    /// re-run recognize an already imported k3s image by digest.
    pub fn write_layout_tar(&self, writer: impl Write) -> Result<(), PreloadError> {
        let mut tar = tar::Builder::new(writer);

        let descriptor = DescriptorBuilder::default()
            .media_type(self.manifest_media_type.clone())
            .digest(self.manifest.clone())
            .size(self.manifest_size)
            .platform(platform(self.arch))
            .annotations(HashMap::from([(
                REF_NAME_ANNOTATION.to_string(),
                self.reference.clone(),
            )]))
            .build()
            .map_err(|e| PreloadError::Layout {
                detail: format!("image descriptor: {e}"),
            })?;
        let index = ImageIndexBuilder::default()
            .schema_version(2u32)
            .media_type(MediaType::ImageIndex)
            .manifests(vec![descriptor])
            .build()
            .map_err(|e| PreloadError::Layout {
                detail: format!("index.json: {e}"),
            })?;
        let index_json = index.to_string().map_err(|e| PreloadError::Layout {
            detail: format!("serialize index.json: {e}"),
        })?;

        append_bytes(&mut tar, "oci-layout", LAYOUT_MARKER.as_bytes())?;
        append_bytes(&mut tar, "index.json", index_json.as_bytes())?;

        let mut digests: Vec<Digest> = self.blobs.clone();
        digests.push(self.manifest.clone());
        digests.sort_by_key(|d| d.digest().to_string());
        digests.dedup();
        for digest in &digests {
            let mut file = self.source.open(digest, &self.manifest)?;
            let size = file.metadata()?.len();
            let path = format!("blobs/sha256/{}", digest.digest());
            let mut header = tar_header(&path, size)?;
            tar.append_data(&mut header, &path, &mut file)
                .map_err(|e| PreloadError::Layout {
                    detail: format!("write {path}: {e}"),
                })?;
        }

        tar.finish().map_err(|e| PreloadError::Layout {
            detail: format!("finish layout tar: {e}"),
        })?;
        Ok(())
    }

    /// Stream the layout tar into a fresh temporary file and return the
    /// blob reference (the digest is computed while writing).
    ///
    /// The caller owns the file and removes it after the upload; the
    /// payload never sits in memory, so a multi-hundred-MB layer costs one
    /// 64 KiB buffer plus the tar on disk.
    pub fn to_temp_tar(&self) -> Result<BlobRef, PreloadError> {
        let file = tempfile::Builder::new()
            .prefix("kurin-image-")
            .suffix(".tar")
            .tempfile()?;
        let mut hashing = HashingWriter::new(file);
        self.write_layout_tar(&mut hashing)?;
        let (file, digest, size) = hashing.finish()?;
        let (_file, path) = file.keep().map_err(|e| PreloadError::Io {
            detail: format!("persist image tar: {e}"),
        })?;
        Ok(BlobRef { path, digest, size })
    }
}

/// Where an [`ImagePayload`]'s blobs live.
enum BlobDir {
    /// A cache/offline artifact directory: `index.json`, `manifest.json`
    /// and hex-named blobs.
    Cache { dir: PathBuf },
    /// A package layout's content-addressed store.
    Store(BlobStore),
}

impl BlobDir {
    /// Open one blob, verifying it when the source is a [`BlobStore`]
    /// (a cache dir was verified as a whole by [`image_chain`]).
    fn open(&self, digest: &Digest, manifest: &Digest) -> Result<File, PreloadError> {
        match self {
            BlobDir::Cache { dir } => {
                let path = if digest == manifest {
                    dir.join("manifest.json")
                } else {
                    dir.join(digest.digest())
                };
                File::open(&path).map_err(|e| PreloadError::Io {
                    detail: format!("open {}: {e}", path.display()),
                })
            }
            BlobDir::Store(store) => Ok(store.open_blob(digest)?),
        }
    }
}

/// Resolve an image from a cache/offline artifact directory, verifying the
/// full digest chain against `expected_index`.
fn image_from_cache(
    dir: &Path,
    spec: &ArtifactSpec,
    expected_index: &str,
) -> Result<ImagePayload, PreloadError> {
    let chain = image_chain(dir, spec.arch, expected_index)?;
    let index_bytes = std::fs::read(dir.join("index.json"))?;
    let (manifest, media_type, size) = platform_entry(&index_bytes, spec.arch)?;
    let mut blobs = Vec::with_capacity(chain.blobs.len());
    for digest in &chain.blobs {
        blobs.push(digest_from_hex(digest)?);
    }
    Ok(ImagePayload {
        reference: spec.url(),
        manifest,
        manifest_media_type: media_type,
        manifest_size: size,
        arch: spec.arch,
        blobs,
        source: BlobDir::Cache {
            dir: dir.to_path_buf(),
        },
    })
}

/// Resolve an image from an opened package: the upstream index at its
/// pinned digest, then the selected platform's manifest and blobs.
fn image_from_package(
    package: &PackageSource,
    spec: &ArtifactSpec,
) -> Result<ImagePayload, PreloadError> {
    let meta = package.manifest()?;
    let reference = spec.url();
    let image = meta
        .images
        .iter()
        .find(|i| i.reference == reference)
        .ok_or_else(|| PreloadError::MissingImage {
            reference: reference.clone(),
        })?;
    if !image.arch.iter().any(|a| a == spec.arch.as_str()) {
        return Err(PreloadError::MissingImagePlatform {
            reference,
            arch: spec.arch.as_str().to_string(),
        });
    }
    let index_digest = package_digest(image)?;
    let layout = package.layout();
    let mut index_bytes = Vec::new();
    layout
        .blobs()
        .open_blob(&index_digest)?
        .read_to_end(&mut index_bytes)?;
    let (manifest_entry, media_type, size) = platform_entry(&index_bytes, spec.arch)?;

    // The selected manifest names the config and layers to carry.
    let mut manifest_bytes = Vec::new();
    layout
        .blobs()
        .open_blob(&manifest_entry)?
        .read_to_end(&mut manifest_bytes)?;
    let manifest =
        ImageManifest::from_reader(&manifest_bytes[..]).map_err(|e| PreloadError::Layout {
            detail: format!("parse image manifest: {e}"),
        })?;
    let mut blobs = Vec::with_capacity(manifest.layers().len() + 1);
    blobs.push(manifest.config().digest().clone());
    for layer in manifest.layers() {
        blobs.push(layer.digest().clone());
    }
    Ok(ImagePayload {
        reference,
        manifest: manifest_entry,
        manifest_media_type: media_type,
        manifest_size: size,
        arch: spec.arch,
        blobs,
        source: BlobDir::Store(layout.blobs().clone()),
    })
}

/// Where a push payload's blobs live.
enum PushSource {
    /// A package layout's content-addressed store.
    Store(BlobStore),
    /// A cache/offline artifact directory (`index.json`, `manifest.json`,
    /// hex-named blobs).
    Dir(PathBuf),
}

impl PushSource {
    fn read(&self, digest: &Digest) -> Result<Vec<u8>, PreloadError> {
        let path = self.path(digest)?.0;
        std::fs::read(&path).map_err(|e| PreloadError::Io {
            detail: format!("read {}: {e}", path.display()),
        })
    }

    fn path(&self, digest: &Digest) -> Result<(PathBuf, u64), PreloadError> {
        let path = match self {
            PushSource::Store(store) => store.path(digest),
            PushSource::Dir(dir) => {
                let hex = dir.join(digest.digest());
                if hex.is_file() {
                    hex
                } else {
                    dir.join("manifest.json")
                }
            }
        };
        let size = std::fs::metadata(&path)
            .map_err(|e| PreloadError::Io {
                detail: format!("stat {}: {e}", path.display()),
            })?
            .len();
        Ok((path, size))
    }
}

fn media_type_string(media_type: &MediaType) -> String {
    serde_json::to_value(media_type)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| format!("{media_type:?}"))
}

fn image_push_from_package(
    package: &PackageSource,
    spec: &ArtifactSpec,
) -> Result<ImagePushPayload, PreloadError> {
    let meta = package.manifest()?;
    let reference = spec.url();
    let image = meta
        .images
        .iter()
        .find(|i| i.reference == reference)
        .ok_or_else(|| PreloadError::MissingImage {
            reference: reference.clone(),
        })?;
    let index_digest = package_digest(image)?;
    let source = PushSource::Store(package.layout().blobs().clone());
    let index_bytes = source.read(&index_digest)?;
    let mut blobs = Vec::new();
    let mut seen = std::collections::HashSet::new();
    collect_index(&index_bytes, &source, &mut blobs, &mut seen)?;
    Ok(ImagePushPayload {
        reference,
        index_digest: index_digest.to_string(),
        index_bytes,
        media_type: "application/vnd.oci.image.index.v1+json".to_string(),
        blobs,
    })
}

fn image_push_from_dir(dir: &Path, reference: &str) -> Result<ImagePushPayload, PreloadError> {
    let index_path = dir.join("index.json");
    let index_bytes = std::fs::read(&index_path).map_err(|e| PreloadError::Io {
        detail: format!("read {}: {e}", index_path.display()),
    })?;
    let index_digest = {
        let mut hasher = Sha256::new();
        hasher.update(&index_bytes);
        format!("sha256:{:x}", hasher.finalize())
    };
    let source = PushSource::Dir(dir.to_path_buf());
    let mut blobs = Vec::new();
    let mut seen = std::collections::HashSet::new();
    collect_index(&index_bytes, &source, &mut blobs, &mut seen)?;
    Ok(ImagePushPayload {
        reference: reference.to_string(),
        index_digest,
        index_bytes,
        media_type: "application/vnd.oci.image.index.v1+json".to_string(),
        blobs,
    })
}

fn collect_index(
    bytes: &[u8],
    source: &PushSource,
    blobs: &mut Vec<PushBlob>,
    seen: &mut std::collections::HashSet<String>,
) -> Result<(), PreloadError> {
    let index = ImageIndex::from_reader(bytes).map_err(|e| PreloadError::Layout {
        detail: format!("parse image index: {e}"),
    })?;
    for descriptor in index.manifests() {
        let digest = descriptor.digest().clone();
        let media_type = media_type_string(descriptor.media_type());
        if seen.insert(digest.to_string()) {
            let (path, size) = source.path(&digest)?;
            blobs.push(PushBlob {
                digest: digest.to_string(),
                path,
                size,
                media_type: media_type.clone(),
                manifest: true,
            });
        }
        let child = source.read(&digest)?;
        if media_type.contains("index") || media_type.contains("manifest.list") {
            collect_index(&child, source, blobs, seen)?;
        } else {
            collect_manifest(&child, source, blobs, seen)?;
        }
    }
    Ok(())
}

fn collect_manifest(
    bytes: &[u8],
    source: &PushSource,
    blobs: &mut Vec<PushBlob>,
    seen: &mut std::collections::HashSet<String>,
) -> Result<(), PreloadError> {
    let manifest = ImageManifest::from_reader(bytes).map_err(|e| PreloadError::Layout {
        detail: format!("parse image manifest: {e}"),
    })?;
    let mut add = |descriptor: &oci_spec::image::Descriptor| -> Result<(), PreloadError> {
        let digest = descriptor.digest().clone();
        if !seen.insert(digest.to_string()) {
            return Ok(());
        }
        let (path, size) = source.path(&digest)?;
        blobs.push(PushBlob {
            digest: digest.to_string(),
            path,
            size,
            media_type: media_type_string(descriptor.media_type()),
            manifest: false,
        });
        Ok(())
    };
    add(manifest.config())?;
    for layer in manifest.layers() {
        add(layer)?;
    }
    Ok(())
}

/// The package's index digest for one image row.
fn package_digest(image: &PackageImageMeta) -> Result<Digest, PreloadError> {
    Digest::try_from(image.digest.as_str()).map_err(|e| PreloadError::InvalidDigest {
        digest: image.digest.clone(),
        detail: e.to_string(),
    })
}

/// The platform-matched manifest descriptor of an image index: its
/// digest, media type and size.
fn platform_entry(
    index_bytes: &[u8],
    arch: Arch,
) -> Result<(Digest, MediaType, u64), PreloadError> {
    let index = ImageIndex::from_reader(index_bytes).map_err(|e| PreloadError::Layout {
        detail: format!("parse image index: {e}"),
    })?;
    let entry = index
        .manifests()
        .iter()
        .find(|d| {
            d.platform().as_ref().is_some_and(|p| {
                p.architecture().to_string() == arch.as_str() && p.os().to_string() == "linux"
            })
        })
        .ok_or_else(|| PreloadError::MissingImagePlatform {
            reference: "image index".to_string(),
            arch: arch.as_str().to_string(),
        })?;
    Ok((
        entry.digest().clone(),
        entry.media_type().clone(),
        entry.size(),
    ))
}

/// The OCI platform for one kurin architecture.
fn platform(arch: Arch) -> Platform {
    PlatformBuilder::default()
        .architecture(match arch {
            Arch::Amd64 => oci_spec::image::Arch::Amd64,
            Arch::Arm64 => oci_spec::image::Arch::ARM64,
        })
        .os(oci_spec::image::Os::Linux)
        .build()
        .expect("constant platform is valid")
}

/// `sha256:<hex>` (or bare hex) → [`Digest`].
fn digest_from_hex(value: &str) -> Result<Digest, PreloadError> {
    let value = if value.contains(':') {
        value.to_string()
    } else {
        format!("sha256:{value}")
    };
    Digest::try_from(value.as_str()).map_err(|e| PreloadError::InvalidDigest {
        digest: value,
        detail: e.to_string(),
    })
}

/// Append one in-memory file to the layout tar with a normalized header.
fn append_bytes(
    tar: &mut tar::Builder<impl Write>,
    path: &str,
    bytes: &[u8],
) -> Result<(), PreloadError> {
    let mut header = tar_header(path, bytes.len() as u64)?;
    tar.append_data(&mut header, path, bytes)
        .map_err(|e| PreloadError::Layout {
            detail: format!("write {path}: {e}"),
        })
}

/// A deterministic tar header: regular file, 0644, mtime 0.
fn tar_header(path: &str, size: u64) -> Result<tar::Header, PreloadError> {
    let mut header = tar::Header::new_gnu();
    header.set_path(path).map_err(|e| PreloadError::Layout {
        detail: format!("tar path {path}: {e}"),
    })?;
    header.set_size(size);
    header.set_mode(0o644);
    header.set_mtime(0);
    header.set_uid(0);
    header.set_gid(0);
    header.set_cksum();
    Ok(header)
}

/// Writes through to the inner writer while hashing, so the generated tar
/// gets its digest without a second pass over the file.
struct HashingWriter<W: Write> {
    inner: W,
    hasher: Sha256,
    size: u64,
}

impl<W: Write> HashingWriter<W> {
    fn new(inner: W) -> Self {
        Self {
            inner,
            hasher: Sha256::new(),
            size: 0,
        }
    }

    fn finish(mut self) -> Result<(W, String, u64), PreloadError> {
        self.inner.flush()?;
        let digest = format!("sha256:{:x}", self.hasher.finalize());
        Ok((self.inner, digest, self.size))
    }
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let written = self.inner.write(buf)?;
        self.hasher.update(&buf[..written]);
        self.size += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Errors raised while resolving or re-hosting preload payloads.
#[derive(Debug, Error, miette::Diagnostic)]
pub enum PreloadError {
    /// The artifact manifest or offline bundle is unusable.
    #[error(transparent)]
    Artifact(#[from] ArtifactError),
    /// The package cannot be opened or read.
    #[error(transparent)]
    Package(#[from] PackageError),
    /// The local blob store failed.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// A local file could not be read or written.
    #[error("{detail}")]
    Io {
        /// What failed.
        detail: String,
    },
    /// A layout document is malformed.
    #[error("{detail}")]
    Layout {
        /// What failed.
        detail: String,
    },
    /// The payload source does not carry this artifact for this arch.
    #[error("payload source does not carry `{name}` for {arch}; rebuild the package/bundle")]
    MissingFile {
        /// Artifact name.
        name: String,
        /// Requested architecture.
        arch: String,
    },
    /// The payload source does not carry this image.
    #[error("payload source does not carry image {reference}")]
    MissingImage {
        /// Upstream reference.
        reference: String,
    },
    /// The image is present but not for this architecture.
    #[error("image {reference} carries no {arch} sub-manifest")]
    MissingImagePlatform {
        /// Upstream reference.
        reference: String,
        /// Requested architecture.
        arch: String,
    },
    /// The artifact has no pinned digest for this architecture.
    #[error("artifact `{name}` has no pinned sha256 for {arch}")]
    Unpinned {
        /// Artifact name.
        name: String,
        /// Requested architecture.
        arch: String,
    },
    /// A digest in the metadata is not a valid `sha256:<hex>`.
    #[error("invalid digest {digest}: {detail}")]
    InvalidDigest {
        /// The offending value.
        digest: String,
        /// Why it is invalid.
        detail: String,
    },
}

impl From<io::Error> for PreloadError {
    fn from(err: io::Error) -> Self {
        PreloadError::Io {
            detail: err.to_string(),
        }
    }
}
