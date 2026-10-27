//! The kurin package format (04 §3, ADR-11): one OCI image index that
//! carries every payload an air-gapped install needs, readable by
//! `oras`, `skopeo` and plain `tar`.
//!
//! ```text
//! index (mediaType: application/vnd.oci.image.index.v1+json
//!        artifactType: application/vnd.kurin.package.v1)
//!  ├─ manifest: package metadata   artifactType = application/vnd.kurin.package.meta.v1+json
//!  │     config = application/vnd.oci.empty.v1+json
//!  │     layers = [ kurin-package.json ]
//!  ├─ manifest: file set, one per platform   artifactType = application/vnd.kurin.files.v1
//!  │     layers = [ kubeadm, containerd.tar.gz, … ]
//!  │              each layer annotated org.opencontainers.image.title=<file name>
//!  │              and dev.kurin.artifact.name=<manifest artifact name>
//!  └─ images: the upstream index of every pinned image, kept at its
//!       original digest, annotated dev.kurin.image.ref=<upstream reference>
//! ```
//!
//! Images keep their **upstream digests**: the runtime resolves them by
//! original reference through the registry mirror, so the index digest
//! sub-manifests and blobs; the upstream index document itself is stored
//! verbatim, so it may reference platforms the package does not carry —
//! registries that validate index children on upload (distribution,
//! harbor) reject such packages; see `oci::remote` and the WP-30 spike.
//!
//! `serde_json`'s map type is sorted, so the bytes — and therefore every
//! digest — are stable across runs and hosts. The SPDX SBOM is attached
//! to the package index as a referrer (04 §6, WP-39) and written next to
//! the output as a `<output>.spdx.json` sidecar for plain-file tooling.

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use oci_spec::image::{
    Arch as OciArch, Descriptor, DescriptorBuilder, Digest, ImageIndex, ImageIndexBuilder,
    ImageManifest, ImageManifestBuilder, MediaType, Os as OciOs, Platform, PlatformBuilder,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use kurin_core::zone::Zone;

use crate::artifact::{image_chain, ArtifactKind, ArtifactManifest, ArtifactSpec, ArtifactStore};
use crate::oci::layout::{Layout, REF_NAME_ANNOTATION};
use crate::store::BlobStore;

// ---------------------------------------------------------------------------
// media types and annotations (04 §3.1)
// ---------------------------------------------------------------------------

/// artifactType of the package index: a kurin package.
pub const MEDIA_TYPE_PACKAGE: &str = "application/vnd.kurin.package.v1";
/// artifactType of the metadata manifest (and media type of its layer,
/// the `kurin-package.json` document itself).
pub const MEDIA_TYPE_META: &str = "application/vnd.kurin.package.meta.v1+json";
/// artifactType of one platform's file-set manifest.
pub const MEDIA_TYPE_FILES: &str = "application/vnd.kurin.files.v1";
/// media type of one file layer inside a file-set manifest.
pub const MEDIA_TYPE_FILE: &str = "application/vnd.kurin.file.v1";
/// SPDX 2.3 JSON: the media type of the SBOM referrer attached to the
/// package index and of its payload layer (04 §6).
pub const MEDIA_TYPE_SPDX: &str = "application/spdx+json";

/// Annotation on an image descriptor: the upstream full reference the
/// image is pinned by (`registry.k8s.io/kube-apiserver:v1.33.1`).
pub const IMAGE_REF_ANNOTATION: &str = "dev.kurin.image.ref";
/// Annotation on a file layer: the artifact manifest entry name the file
/// was resolved from (`kubeadm`, `cni-plugins`, …).
pub const ARTIFACT_NAME_ANNOTATION: &str = "dev.kurin.artifact.name";

/// The `application/vnd.oci.empty.v1+json` config every non-image
/// manifest points at; the blob is `{}`.
const EMPTY_JSON: &[u8] = b"{}";
/// Digest of [`EMPTY_JSON`] per the OCI spec.
const EMPTY_JSON_DIGEST: &str =
    "sha256:44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a";

// ---------------------------------------------------------------------------
// kurin-package.json (schema 1, docs/schema/package.v1.json)
// ---------------------------------------------------------------------------

/// What a package contains (04 §3.4). `base` is the only kind built
/// in-tree so far; the others document the format for WP-40+.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PackageKind {
    /// One Kubernetes version + distribution's full binary and image set.
    Base,
    /// One addon (addon.yaml + templates + chart + images).
    Addon,
    /// One OS family/version/arch's dependency package repository.
    Os,
    /// Several packages merged into one index for single-file transfer.
    Bundle,
    /// A user-defined package (`kurin-package.yaml`).
    Custom,
}

impl PackageKind {
    pub fn as_str(self) -> &'static str {
        match self {
            PackageKind::Base => "base",
            PackageKind::Addon => "addon",
            PackageKind::Os => "os",
            PackageKind::Bundle => "bundle",
            PackageKind::Custom => "custom",
        }
    }
}

/// `kurin-package.json`: the package's machine-readable manifest, stored
/// as the metadata manifest's single layer. Replaces the legacy
/// `kurin-manifest.json` (its fields are a superset).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageManifest {
    /// Format schema version; this file documents `1`.
    pub schema: u32,
    /// Package name (`base` for the built-in base package).
    pub name: String,
    /// What the package contains (see [`PackageKind`]).
    pub kind: PackageKind,
    /// The Kubernetes version a `base` package pins.
    pub version: String,
    /// kurin versions able to consume this package.
    pub kurin: String,
    /// Creation time (RFC 3339, UTC).
    pub created: String,
    /// `kubeadm` or `k3s`.
    pub distribution: String,
    /// Architectures covered.
    pub arch: Vec<String>,
    /// The components whose artifacts were aggregated, with the version
    /// each resolved to.
    pub components: Vec<PackageComponent>,
    /// File artifacts (binaries and plain manifests), one entry per
    /// artifact per architecture.
    pub artifacts: Vec<PackageArtifact>,
    /// Container images, kept at upstream digests.
    pub images: Vec<PackageImageMeta>,
    /// Helm charts (empty until WP-40).
    pub charts: Vec<PackageChart>,
    /// OS dependency repositories (empty until WP-36).
    pub os: Vec<PackageOs>,
}

/// One component row of [`PackageManifest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageComponent {
    /// Component id (`distro/kubeadm`).
    pub id: String,
    /// The version the component's artifacts resolved to.
    pub version: String,
}

/// One file artifact row of [`PackageManifest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageArtifact {
    pub name: String,
    pub kind: String,
    pub version: String,
    pub arch: String,
    /// `sha256:<hex>` of the payload blob.
    pub digest: String,
    pub size: u64,
    /// Upstream URL the payload was downloaded from.
    pub upstream: String,
}

/// One image row of [`PackageManifest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageImageMeta {
    /// Upstream full reference (`registry.k8s.io/kube-apiserver:v1.33.1`).
    #[serde(rename = "ref")]
    pub reference: String,
    /// Digest of the upstream multi-arch index (`sha256:<hex>`).
    pub digest: String,
    /// Architectures whose sub-manifests the package carries.
    pub arch: Vec<String>,
    /// Component ids that reference the image.
    #[serde(rename = "used-by")]
    pub used_by: Vec<String>,
}

/// One Helm chart row (reserved for WP-40).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageChart {
    pub name: String,
    pub version: String,
    pub digest: String,
}

/// One OS dependency repository row (reserved for WP-36).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageOs {
    /// e.g. `ubuntu-24.04`.
    pub os: String,
    pub arch: String,
    pub digest: String,
}

// ---------------------------------------------------------------------------
// builder inputs
// ---------------------------------------------------------------------------

/// One file artifact carried by the package, already downloaded and
/// verified into the blob store named by `digest`.
#[derive(Debug, Clone)]
pub struct PackageFile {
    /// Artifact manifest entry name (`kubeadm`).
    pub name: String,
    /// `binary` or `manifest`.
    pub kind: ArtifactKind,
    pub version: String,
    pub arch: crate::artifact::Arch,
    /// Content digest of the payload.
    pub digest: Digest,
    /// Payload size in bytes.
    pub size: u64,
    /// File name on the node (`org.opencontainers.image.title`).
    pub title: String,
    /// Upstream download URL.
    pub upstream: String,
}

/// One container image carried by the package: the upstream index at its
/// original digest plus the selected platforms' sub-manifests and blobs.
#[derive(Debug, Clone)]
pub struct PackageImage {
    /// Upstream full reference (`registry.k8s.io/pause:3.10`).
    pub reference: String,
    /// Digest of the upstream index document.
    pub digest: Digest,
    /// Size of the upstream index document.
    pub size: u64,
    /// The index's own media type.
    pub media_type: String,
    /// Architectures whose sub-manifests are carried.
    pub arch: Vec<crate::artifact::Arch>,
    /// Component ids that reference the image.
    pub used_by: Vec<String>,
    /// Sub-manifest, config and layer digests to carry (all platforms).
    pub blobs: Vec<Digest>,
}

// ---------------------------------------------------------------------------
// package builder
// ---------------------------------------------------------------------------

/// Assembles a kurin package: content blobs plus the metadata / file-set
/// manifests and the final index, written to an OCI layout (04 §3.1).
///
/// All content must already live in the cache [`BlobStore`] the builder
/// is given at [`PackageBuilder::build`] time; the builder only links or
/// copies blobs into the target layout and writes the JSON it owns.
#[derive(Debug, Clone)]
pub struct PackageBuilder {
    name: String,
    kind: PackageKind,
    version: String,
    distribution: String,
    kurin: String,
    created: String,
    tag: Option<String>,
    components: Vec<PackageComponent>,
    files: Vec<PackageFile>,
    images: Vec<PackageImage>,
    os: Vec<PackageOs>,
    charts: Vec<PackageChart>,
    /// Explicit architecture list; `None` derives it from the files.
    arches: Option<Vec<crate::artifact::Arch>>,
}

impl PackageBuilder {
    /// Start a package of `kind` (see 04 §3.4).
    pub fn new(kind: PackageKind, name: &str, version: &str, distribution: &str) -> Self {
        PackageBuilder {
            name: name.to_string(),
            kind,
            version: version.to_string(),
            distribution: distribution.to_string(),
            kurin: kurin_compat_range(),
            created: now_rfc3339(),
            tag: None,
            components: Vec::new(),
            files: Vec::new(),
            images: Vec::new(),
            os: Vec::new(),
            charts: Vec::new(),
            arches: None,
        }
    }

    /// Pin the creation timestamp (tests fix it for snapshots).
    pub fn with_created(mut self, created: impl Into<String>) -> Self {
        self.created = created.into();
        self
    }

    /// Override the kurin compatibility range.
    pub fn with_kurin(mut self, kurin: impl Into<String>) -> Self {
        self.kurin = kurin.into();
        self
    }

    /// Override the layout tag (`org.opencontainers.image.ref.name`).
    pub fn with_tag(mut self, tag: impl Into<String>) -> Self {
        self.tag = Some(tag.into());
        self
    }

    /// Record a contributing component with the version it resolved to.
    pub fn add_component(&mut self, id: impl Into<String>, version: impl Into<String>) {
        self.components.push(PackageComponent {
            id: id.into(),
            version: version.into(),
        });
    }

    /// Add one file artifact (one arch of one artifact).
    pub fn add_file(&mut self, file: PackageFile) {
        self.files.push(file);
    }

    /// Add one image.
    pub fn add_image(&mut self, image: PackageImage) {
        self.images.push(image);
    }

    /// Add one OS dependency repository (04 §3.4 `os` kind): the repo
    /// tar joins the arch's file-set manifest as a file layer, and the
    /// package metadata gains the matching `os` row (WP-38).
    pub fn add_os_repo(
        &mut self,
        os: impl Into<String>,
        arch: crate::artifact::Arch,
        file: PackageFile,
    ) {
        self.os.push(PackageOs {
            os: os.into(),
            arch: arch.as_str().to_string(),
            digest: file.digest.to_string(),
        });
        self.files.push(file);
    }

    /// Add one Helm chart row (the chart's payload travels as a file
    /// layer; the row records its name, version and digest).
    pub fn add_chart(&mut self, chart: PackageChart) {
        self.charts.push(chart);
    }

    /// Pin the architecture list instead of deriving it from the file
    /// entries. Addon packages carry architecture-independent files but
    /// images for several architectures, so the derived list would be
    /// wrong.
    pub fn with_arches(mut self, arches: Vec<crate::artifact::Arch>) -> Self {
        self.arches = Some(arches);
        self
    }

    /// The `kurin-package.json` document this builder will write.
    pub fn manifest(&self) -> PackageManifest {
        PackageManifest {
            schema: 1,
            name: self.name.clone(),
            kind: self.kind,
            version: self.version.clone(),
            kurin: self.kurin.clone(),
            created: self.created.clone(),
            distribution: self.distribution.clone(),
            arch: self
                .arches()
                .iter()
                .map(|a| a.as_str().to_string())
                .collect(),
            components: self.components.clone(),
            artifacts: self
                .files
                .iter()
                .map(|f| PackageArtifact {
                    name: f.name.clone(),
                    kind: f.kind.to_string(),
                    version: f.version.clone(),
                    arch: f.arch.as_str().to_string(),
                    digest: f.digest.to_string(),
                    size: f.size,
                    upstream: f.upstream.clone(),
                })
                .collect(),
            images: self
                .images
                .iter()
                .map(|i| PackageImageMeta {
                    reference: i.reference.clone(),
                    digest: i.digest.to_string(),
                    arch: i.arch.iter().map(|a| a.as_str().to_string()).collect(),
                    used_by: i.used_by.clone(),
                })
                .collect(),
            charts: self.charts.clone(),
            os: self.os.clone(),
        }
    }

    /// Architectures the package covers: explicit when set, else those
    /// with at least one file, in first-seen order.
    fn arches(&self) -> Vec<crate::artifact::Arch> {
        if let Some(arches) = &self.arches {
            return arches.clone();
        }
        let mut out: Vec<crate::artifact::Arch> = Vec::new();
        for file in &self.files {
            if !out.contains(&file.arch) {
                out.push(file.arch);
            }
        }
        out
    }

    /// The layout tag: `v<version>-<distribution>` for base packages
    /// (04 §3.4 naming, `kurin/base:v1.33.1-kubeadm`).
    fn layout_tag(&self) -> String {
        if self.kind == PackageKind::Os {
            // `kurin/os/rocky-9:2026.10` (04 §3.4): no distribution
            // suffix on os packages.
            return self
                .tag
                .clone()
                .unwrap_or_else(|| format!("v{}", self.version));
        }
        self.tag
            .clone()
            .unwrap_or_else(|| format!("v{}-{}", self.version, self.distribution))
    }

    /// Assemble and write the package to `out`, pulling content blobs
    /// from `cache`. Returns the output path, the package index digest
    /// (the package's identity) and the SBOM sidecar path.
    pub fn build(
        &self,
        cache: &BlobStore,
        out: &PackageOutput,
    ) -> Result<PackageReport, PackageError> {
        let staging = Staging::for_output(out)?;
        let layout = Layout::create(staging.path())?;

        // 1. content blobs: files, images, and the shared empty config.
        for file in &self.files {
            transfer_blob(cache, layout.blobs(), &file.digest)?;
        }
        for image in &self.images {
            // the upstream index document itself (04 §3.1: images keep
            // their original digest); `blobs` carries only its children
            transfer_blob(cache, layout.blobs(), &image.digest)?;
            for blob in &image.blobs {
                transfer_blob(cache, layout.blobs(), blob)?;
            }
        }
        let empty = layout.put_blob(EMPTY_JSON)?;
        if empty.to_string() != EMPTY_JSON_DIGEST {
            return Err(PackageError::EmptyJsonChanged {
                actual: empty.to_string(),
            });
        }

        // 2. the metadata manifest: config = empty, layers = [meta].
        let manifest = self.manifest();
        let meta_bytes = canonical_json(&manifest)?;
        let meta_doc = layout.put_blob(meta_bytes.as_bytes())?;
        let meta_doc_size = layout.blobs().size(&meta_doc)?;
        let meta_manifest = ImageManifestBuilder::default()
            .schema_version(2u32)
            .media_type(MediaType::ImageManifest)
            .artifact_type(MediaType::Other(MEDIA_TYPE_META.to_string()))
            .config(empty_descriptor())
            .layers(vec![DescriptorBuilder::default()
                .media_type(MediaType::Other(MEDIA_TYPE_META.to_string()))
                .digest(meta_doc)
                .size(meta_doc_size)
                .build()
                .map_err(|e| PackageError::Build {
                    detail: format!("meta layer descriptor: {e}"),
                })?])
            .build()
            .map_err(|e| PackageError::Build {
                detail: format!("metadata manifest: {e}"),
            })?;
        let meta_digest = layout.put_blob(canonical_json(&meta_manifest)?.as_bytes())?;
        let meta_size = layout.blobs().size(&meta_digest)?;
        let mut index_entries = vec![DescriptorBuilder::default()
            .media_type(MediaType::ImageManifest)
            .artifact_type(MediaType::Other(MEDIA_TYPE_META.to_string()))
            .digest(meta_digest.clone())
            .size(meta_size)
            .build()
            .map_err(|e| PackageError::Build {
                detail: format!("meta descriptor: {e}"),
            })?];

        // 3. one file-set manifest per architecture, platform-tagged.
        for arch in self.arches() {
            let layers: Vec<Descriptor> = self
                .files
                .iter()
                .filter(|f| f.arch == arch)
                .map(|f| {
                    DescriptorBuilder::default()
                        .media_type(MediaType::Other(MEDIA_TYPE_FILE.to_string()))
                        .digest(f.digest.clone())
                        .size(f.size)
                        .annotations(file_annotations(&f.title, &f.name))
                        .build()
                })
                .collect::<Result<_, _>>()
                .map_err(|e| PackageError::Build {
                    detail: format!("file layer descriptor: {e}"),
                })?;
            let files_manifest = ImageManifestBuilder::default()
                .schema_version(2u32)
                .media_type(MediaType::ImageManifest)
                .artifact_type(MediaType::Other(MEDIA_TYPE_FILES.to_string()))
                .config(empty_descriptor())
                .layers(layers)
                .build()
                .map_err(|e| PackageError::Build {
                    detail: format!("files manifest: {e}"),
                })?;
            let bytes = canonical_json(&files_manifest)?;
            let digest = layout.put_blob(bytes.as_bytes())?;
            let size = layout.blobs().size(&digest)?;
            index_entries.push(
                DescriptorBuilder::default()
                    .media_type(MediaType::ImageManifest)
                    .artifact_type(MediaType::Other(MEDIA_TYPE_FILES.to_string()))
                    .digest(digest)
                    .size(size)
                    .platform(oci_platform(arch))
                    .build()
                    .map_err(|e| PackageError::Build {
                        detail: format!("files descriptor: {e}"),
                    })?,
            );
        }

        // 4. image descriptors: the upstream index, kept at its digest.
        for image in &self.images {
            index_entries.push(
                DescriptorBuilder::default()
                    .media_type(MediaType::Other(image.media_type.clone()))
                    .digest(image.digest.clone())
                    .size(image.size)
                    .annotations(HashMap::from([(
                        IMAGE_REF_ANNOTATION.to_string(),
                        image.reference.clone(),
                    )]))
                    .build()
                    .map_err(|e| PackageError::Build {
                        detail: format!("image descriptor: {e}"),
                    })?,
            );
        }

        // 5. the package index itself.
        let tag = self.layout_tag();
        let index = ImageIndexBuilder::default()
            .schema_version(2u32)
            .media_type(MediaType::ImageIndex)
            .artifact_type(MediaType::Other(MEDIA_TYPE_PACKAGE.to_string()))
            .manifests(index_entries)
            .annotations(HashMap::from([
                ("dev.kurin.package.name".to_string(), self.name.clone()),
                (
                    "dev.kurin.package.kind".to_string(),
                    self.kind.as_str().to_string(),
                ),
                (
                    "dev.kurin.package.version".to_string(),
                    self.version.clone(),
                ),
            ]))
            .build()
            .map_err(|e| PackageError::Build {
                detail: format!("package index: {e}"),
            })?;
        let index_bytes = canonical_json(&index)?;
        let index_digest = layout.put_blob(index_bytes.as_bytes())?;
        let index_size = layout.blobs().size(&index_digest)?;

        // 6. index.json: the tagged root descriptor.
        let root = DescriptorBuilder::default()
            .media_type(MediaType::ImageIndex)
            .digest(index_digest.clone())
            .size(index_size)
            .annotations(HashMap::from([(
                REF_NAME_ANNOTATION.to_string(),
                tag.clone(),
            )]))
            .build()
            .map_err(|e| PackageError::Build {
                detail: format!("root descriptor: {e}"),
            })?;
        let root_index = ImageIndexBuilder::default()
            .schema_version(2u32)
            .media_type(MediaType::ImageIndex)
            .manifests(vec![root.clone()])
            .build()
            .map_err(|e| PackageError::Build {
                detail: format!("index.json: {e}"),
            })?;
        write_file_atomic(
            &layout.root().join("index.json"),
            canonical_json(&root_index)?.as_bytes(),
        )?;

        // 7. the SPDX SBOM (04 §6), attached to the index as a
        //    referrer and written next to the output as a sidecar.
        let sbom = spdx::Document::from_package(
            &manifest,
            &index_digest,
            &format!("kurin-{}", env!("CARGO_PKG_VERSION")),
        );
        let sbom_bytes = canonical_json(&sbom)?.into_bytes();
        crate::referrer::attach(
            &layout,
            &root,
            MEDIA_TYPE_SPDX,
            MEDIA_TYPE_SPDX,
            &sbom_bytes,
            HashMap::new(),
        )?;
        let sbom_path = out.sbom_sidecar_path();
        write_file_atomic(&sbom_path, &sbom_bytes)?;

        // 8. serialize single-file outputs from the staging layout.
        let output_path = match out {
            PackageOutput::Layout(dir) => dir.clone(),
            PackageOutput::Tar(path) => {
                let file = File::create(path)?;
                layout.pack_tar(file)?;
                path.clone()
            }
            PackageOutput::TarZstd(path) => {
                let file = File::create(path)?;
                layout.pack_tar_zstd(file)?;
                path.clone()
            }
        };
        drop(staging);

        Ok(PackageReport {
            output: output_path,
            tag,
            index: index_digest,
            sbom: sbom_path,
            files: self.files.len(),
            images: self.images.len(),
        })
    }
}

/// What [`PackageBuilder::build`] produced.
#[derive(Debug, Clone)]
pub struct PackageReport {
    /// The written output (layout dir, or the `.tar` / `.tar.zst` file).
    pub output: PathBuf,
    /// The layout tag the package index is filed under.
    pub tag: String,
    /// Digest of the package index — the package's identity.
    pub index: Digest,
    /// Path of the SPDX SBOM sidecar written next to the output.
    pub sbom: PathBuf,
    /// Number of file entries (one per artifact per arch).
    pub files: usize,
    /// Number of images.
    pub images: usize,
}

// ---------------------------------------------------------------------------
// package create base
// ---------------------------------------------------------------------------

/// One component's contribution to a package, as plain data: kurin's
/// component registry lives in `kurin-components`, this keeps the
/// package format independent of it.
#[derive(Debug, Clone)]
pub struct ComponentSpec {
    /// Component id (`runtime/containerd`).
    pub id: String,
    /// Artifact manifest entry names, in consultation order.
    pub artifacts: Vec<String>,
}

/// Options for [`create_base`]: what a `base` package covers (04 §3.4).
#[derive(Debug, Clone)]
pub struct BasePackage {
    /// Kubernetes version override (`--kubernetes v1.33.2`); `None`
    /// uses the compile-time manifest.
    pub kubernetes: Option<String>,
    /// Distribution label: `kubeadm` or `k3s`.
    pub distribution: String,
    /// Architectures to cover (must be non-empty).
    pub arches: Vec<crate::artifact::Arch>,
    /// The components (and their artifacts) to include — the filtered
    /// output of `kurin_components::base_components`.
    pub components: Vec<ComponentSpec>,
    /// Addon payloads embedded in the base package (`--addons`, WP-45):
    /// their files land under `addons/<name>/`, their images join the
    /// package's image set, so one base package installs the selected
    /// addons offline.
    pub extras: Vec<PackageExtras>,
}

/// Payloads one addon contributes to a base package (WP-45). The blobs
/// must already live in the cache [`BlobStore`] the builder is given.
#[derive(Debug, Clone, Default)]
pub struct PackageExtras {
    /// The `addon/<name>` component row (version = the addon's own).
    pub component: Option<PackageComponent>,
    /// Addon files, titled `addons/<name>/<relative path>`.
    pub files: Vec<PackageFile>,
    /// The addon's images, at their upstream digests.
    pub images: Vec<PackageImage>,
    /// The addon's Helm chart rows.
    pub charts: Vec<PackageChart>,
}

/// Build a `base` package: resolve the manifest (embedded, or dynamic
/// via `--kubernetes`), fetch and verify every artifact of every
/// component for every architecture into the cache, then assemble the
/// OCI index (04 §3.1) and write it to `out`.
///
/// Reuses [`ArtifactStore::fetch`] for the download/sha256 path (curl +
/// pinned checksums, image pulls as index → manifest → blobs) and
/// [`image_chain`] to re-host images at their upstream digests.
pub async fn create_base(
    store: &ArtifactStore,
    opts: &BasePackage,
    zone: Zone,
    out: &PackageOutput,
) -> Result<PackageReport, PackageError> {
    if opts.arches.is_empty() {
        return Err(PackageError::NoArch);
    }
    let version = match &opts.kubernetes {
        Some(v) => v.trim_start_matches('v').to_string(),
        None => crate::version::KUBERNETES_VERSION.to_string(),
    };
    let mut builder = PackageBuilder::new(PackageKind::Base, "base", &version, &opts.distribution);

    // One manifest per arch: --kubernetes re-pins binaries per arch and
    // image indexes for the target version.
    let mut manifests: Vec<(crate::artifact::Arch, ArtifactManifest)> = Vec::new();
    for arch in &opts.arches {
        let manifest = match &opts.kubernetes {
            Some(v) => {
                let target: semver::Version =
                    v.trim_start_matches('v')
                        .parse()
                        .map_err(|e: semver::Error| PackageError::InvalidKubernetes {
                            value: v.clone(),
                            detail: e.to_string(),
                        })?;
                crate::dynamic_manifest::k8s_manifest_for(&target, *arch, zone)
                    .await
                    .map_err(|e| PackageError::Miette {
                        detail: e.to_string(),
                    })?
            }
            None => ArtifactManifest::embedded()?,
        };
        manifests.push((*arch, manifest));
    }

    // Component metadata: the version each component resolved to (its
    // first artifact in the manifest).
    for component in &opts.components {
        let version = component
            .artifacts
            .iter()
            .find_map(|name| manifests[0].1.get(name))
            .map(|entry| entry.version.clone());
        if let Some(version) = version {
            builder.add_component(component.id.clone(), version);
        }
    }

    // Artifact name -> component ids (the images' `used-by` field).
    let mut used_by: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for component in &opts.components {
        for name in &component.artifacts {
            used_by
                .entry(name.clone())
                .or_default()
                .push(component.id.clone());
        }
    }

    let mut images: Vec<PackageImage> = Vec::new();
    for (arch, manifest) in &manifests {
        // Names deduplicated across components, in first-seen order.
        let mut names: Vec<&str> = Vec::new();
        for component in &opts.components {
            for name in &component.artifacts {
                if !names.iter().any(|n| n == name) {
                    names.push(name);
                }
            }
        }
        for name in names {
            let spec = manifest.spec(name, *arch)?;
            let expected =
                spec.expected_sha256()
                    .ok_or_else(|| PackageError::UnpinnedArtifact {
                        name: name.to_string(),
                        target: spec.target_key(),
                    })?;
            store.fetch(&spec, zone).await?;
            match spec.kind {
                ArtifactKind::Binary | ArtifactKind::Manifest => {
                    let payload = store.artifact_path(&spec);
                    let digest = import_blob(store.blob_store(), &payload, &expected, &spec)?;
                    let size = store.blob_store().size(&digest)?;
                    builder.add_file(PackageFile {
                        name: spec.name.clone(),
                        kind: spec.kind,
                        version: spec.version.clone(),
                        arch: spec.arch,
                        digest,
                        size,
                        title: spec.file_name(),
                        upstream: spec.url(),
                    });
                }
                ArtifactKind::Image => {
                    let dir = store.artifact_dir(&spec);
                    let chain = image_chain(&dir, spec.arch, &expected)?;
                    let reference = spec.url();
                    let blobs = import_image_chain(store, &dir, &chain, &spec)?;
                    let digest = import_blob(
                        store.blob_store(),
                        &dir.join("index.json"),
                        &chain.index,
                        &spec,
                    )?;
                    match images.iter_mut().find(|i| i.reference == reference) {
                        Some(image) => {
                            if image.digest != digest {
                                return Err(PackageError::InconsistentImagePin {
                                    reference: reference.clone(),
                                    first: image.digest.to_string(),
                                    second: digest.to_string(),
                                });
                            }
                            if !image.arch.contains(&spec.arch) {
                                image.arch.push(spec.arch);
                            }
                            for blob in blobs {
                                if !image.blobs.contains(&blob) {
                                    image.blobs.push(blob);
                                }
                            }
                        }
                        None => images.push(PackageImage {
                            reference: reference.clone(),
                            digest,
                            size: chain.index_size,
                            media_type: chain.media_type.clone(),
                            arch: vec![spec.arch],
                            used_by: used_by.get(name).cloned().unwrap_or_default(),
                            blobs,
                        }),
                    }
                    eprintln!(
                        "[package] image {} {} ({})",
                        spec.name, spec.version, spec.arch
                    );
                }
            }
        }
    }
    for image in images {
        builder.add_image(image);
    }

    // Addon payloads (WP-45): files namespaced under `addons/<name>/`,
    // their images and chart rows, so the selected addons install from
    // this one package offline.
    for extra in &opts.extras {
        if let Some(component) = &extra.component {
            builder.add_component(component.id.clone(), component.version.clone());
        }
        for file in &extra.files {
            builder.add_file(file.clone());
        }
        for image in &extra.images {
            builder.add_image(image.clone());
        }
        for chart in &extra.charts {
            builder.add_chart(chart.clone());
        }
    }

    builder.build(store.blob_store(), out)
}

/// Import one cached payload into the content-addressed store, verifying
/// the computed digest against the manifest pin (which `fetch` already
/// checked at download time — this catches a cache corrupted since).
fn import_blob(
    store: &BlobStore,
    path: &Path,
    expected_hex: &str,
    spec: &ArtifactSpec,
) -> Result<Digest, PackageError> {
    let expected = Digest::try_from(format!("sha256:{expected_hex}"))
        .map_err(|_| PackageError::InvalidDigest(expected_hex.to_string()))?;
    if store.has(&expected) {
        return Ok(expected);
    }
    let mut file = File::open(path)?;
    let digest = store.put_stream(&mut file)?;
    if digest != expected {
        return Err(PackageError::ChecksumMismatch {
            name: spec.name.clone(),
            version: spec.version.clone(),
            target: spec.target_key(),
            expected: expected.to_string(),
            actual: digest.to_string(),
        });
    }
    Ok(digest)
}

/// Import an image's sub-manifest and blobs into the store; returns the
/// full blob list (sub-manifest first, then config and layers).
fn import_image_chain(
    store: &ArtifactStore,
    dir: &Path,
    chain: &crate::artifact::ImageChain,
    spec: &ArtifactSpec,
) -> Result<Vec<Digest>, PackageError> {
    let mut blobs = vec![import_blob(
        store.blob_store(),
        &dir.join("manifest.json"),
        &chain.manifest,
        spec,
    )?];
    for hex in &chain.blobs {
        blobs.push(import_blob(store.blob_store(), &dir.join(hex), hex, spec)?);
    }
    Ok(blobs)
}

// ---------------------------------------------------------------------------
// output targets
// ---------------------------------------------------------------------------

/// Where [`PackageBuilder::build`] writes the package (04 §3.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackageOutput {
    /// `oci-layout://DIR` — an OCI layout directory.
    Layout(PathBuf),
    /// `file://X.tar` — the layout packed as an uncompressed tar.
    Tar(PathBuf),
    /// `file://X.tar.zst` — the layout packed as a zstd-compressed tar.
    TarZstd(PathBuf),
}

impl PackageOutput {
    /// Parse a `-o` target. Accepted forms: `oci-layout://DIR`,
    /// `file://X.tar`, `file://X.tar.zst` (a bare path with a tar
    /// extension is tolerated for convenience). `oci://` targets are
    /// the job of `kurin package push`.
    pub fn parse(target: &str) -> Result<Self, PackageError> {
        let target = target.trim();
        if let Some(rest) = target.strip_prefix("oci-layout://") {
            return Ok(PackageOutput::Layout(PathBuf::from(rest)));
        }
        if target.starts_with("oci://") {
            return Err(PackageError::RegistryOutputUnsupported);
        }
        let path = target.strip_prefix("file://").unwrap_or(target);
        if path.ends_with(".tar.zst") {
            return Ok(PackageOutput::TarZstd(PathBuf::from(path)));
        }
        if path.ends_with(".tar") {
            return Ok(PackageOutput::Tar(PathBuf::from(path)));
        }
        Err(PackageError::InvalidOutput {
            output: target.to_string(),
        })
    }

    /// The SPDX SBOM sidecar path for this output (04 §6): the output
    /// path with a `.spdx.json` suffix instead of any tar extension, or
    /// `<dir>.spdx.json` for a layout.
    fn sbom_sidecar_path(&self) -> PathBuf {
        let base = match self {
            PackageOutput::Layout(dir) => dir.clone().into_os_string(),
            PackageOutput::Tar(path) | PackageOutput::TarZstd(path) => {
                strip_tar_extensions(path).into_os_string()
            }
        };
        let mut s = base.to_string_lossy().into_owned();
        s.push_str(".spdx.json");
        PathBuf::from(s)
    }
}

/// `base.tar.zst` → `base` (drops one `.tar` plus one compression
/// extension).
fn strip_tar_extensions(path: &Path) -> PathBuf {
    let mut out = path.to_path_buf();
    if matches!(
        out.extension().and_then(|e| e.to_str()),
        Some("zst") | Some("gz") | Some("xz")
    ) {
        out.set_extension("");
    }
    if out.extension().and_then(|e| e.to_str()) == Some("tar") {
        out.set_extension("");
    }
    out
}

/// Where [`PackageBuilder::build`] stages the layout: the output
/// directory itself, or a temp directory a tar is packed from.
enum Staging {
    Dir(PathBuf),
    Temp(tempfile::TempDir),
}

impl Staging {
    fn for_output(out: &PackageOutput) -> Result<Self, PackageError> {
        match out {
            PackageOutput::Layout(dir) => Ok(Staging::Dir(dir.clone())),
            PackageOutput::Tar(_) | PackageOutput::TarZstd(_) => {
                Ok(Staging::Temp(tempfile::TempDir::new()?))
            }
        }
    }

    fn path(&self) -> &Path {
        match self {
            Staging::Dir(dir) => dir,
            Staging::Temp(temp) => temp.path(),
        }
    }
}

// reading a package back (inspect; verify/push/pull live in transfer.rs)
// ---------------------------------------------------------------------------

/// A package opened for reading: an OCI layout, either a directory or a
/// tar unpacked into a temporary directory.
pub struct PackageSource {
    layout: Layout,
    _temp: Option<tempfile::TempDir>,
}

impl PackageSource {
    /// Open `oci-layout://DIR`, `file://X.tar` or `file://X.tar.zst`.
    /// `oci://` sources must be fetched first — `kurin package pull`,
    /// `inspect` and `verify` do that through
    /// [`crate::transfer::materialize`].
    pub fn open(source: &str) -> Result<Self, PackageError> {
        match crate::transfer::PackageRef::parse(source)? {
            crate::transfer::PackageRef::Layout(dir) => {
                Ok(PackageSource::from_layout(Layout::open(dir)?, None))
            }
            crate::transfer::PackageRef::Tar(path) => {
                let temp = tempfile::TempDir::new()?;
                let file = File::open(&path)?;
                let layout = Layout::unpack_tar(file, temp.path())?;
                Ok(PackageSource::from_layout(layout, Some(temp)))
            }
            crate::transfer::PackageRef::TarZstd(path) => {
                let temp = tempfile::TempDir::new()?;
                let file = File::open(&path)?;
                let layout = Layout::unpack_tar_zstd(file, temp.path())?;
                Ok(PackageSource::from_layout(layout, Some(temp)))
            }
            crate::transfer::PackageRef::Registry(_) => {
                Err(PackageError::RegistrySourceUnsupported)
            }
        }
    }

    /// Wrap an already-opened layout (for example one `transfer`
    /// pulled from a registry) as a package source. `temp`, when set,
    /// keeps the layout's scratch directory alive as long as the
    /// source.
    pub fn from_layout(layout: Layout, temp: Option<tempfile::TempDir>) -> Self {
        PackageSource {
            layout,
            _temp: temp,
        }
    }

    /// The backing layout.
    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    /// The package index digest: the single manifest `index.json` files
    /// (the package's identity).
    pub fn index_digest(&self) -> Result<Digest, PackageError> {
        let root = self.layout.read_index()?;
        let desc = root
            .manifests()
            .first()
            .ok_or_else(|| PackageError::NotAPackage {
                path: self.layout.root().display().to_string(),
                detail: "index.json has no manifests".to_string(),
            })?;
        Ok(desc.digest().clone())
    }

    /// The root descriptor out of `index.json` (the first manifest
    /// entry; referrers are filed after it).
    pub fn root_descriptor(&self) -> Result<Descriptor, PackageError> {
        let root = self.layout.read_index()?;
        root.manifests()
            .first()
            .cloned()
            .ok_or_else(|| PackageError::NotAPackage {
                path: self.layout.root().display().to_string(),
                detail: "index.json has no manifests".to_string(),
            })
    }

    /// The package's SPDX SBOM: the bytes of the SBOM referrer
    /// attached to the index (WP-39), or `None` for packages built
    /// before referrers existed — callers fall back to rendering the
    /// document from the verified metadata.
    ///
    /// # Errors
    ///
    /// A referrer or blob-store error.
    pub fn sbom(&self) -> Result<Option<Vec<u8>>, PackageError> {
        let index = self.index_digest()?;
        let Some(sbom) = crate::referrer::find(&self.layout, &index)?
            .into_iter()
            .find(|r| r.artifact_type() == MEDIA_TYPE_SPDX)
        else {
            return Ok(None);
        };
        let mut bytes = Vec::new();
        self.layout
            .blobs()
            .open_blob(sbom.payload()?.digest())?
            .read_to_end(&mut bytes)?;
        Ok(Some(bytes))
    }

    /// Read `kurin-package.json`: follow index.json → package index →
    /// metadata manifest → its single layer. Every blob read on the way
    /// is digest-verified by the store.
    pub fn manifest(&self) -> Result<PackageManifest, PackageError> {
        let root = self.layout.read_index()?;
        let desc = root
            .manifests()
            .first()
            .ok_or_else(|| PackageError::NotAPackage {
                path: self.layout.root().display().to_string(),
                detail: "index.json has no manifests".to_string(),
            })?;
        let mut bytes = Vec::new();
        self.layout
            .blobs()
            .open_blob(desc.digest())?
            .read_to_end(&mut bytes)?;
        let index: ImageIndex = serde_json::from_slice(&bytes)?;
        let meta_desc = index.manifests().iter().find(|d| {
            d.artifact_type()
                .as_ref()
                .is_some_and(|t| t.as_ref() == MEDIA_TYPE_META)
        });
        let Some(meta_desc) = meta_desc else {
            return Err(PackageError::NotAPackage {
                path: self.layout.root().display().to_string(),
                detail: format!("no manifest with artifactType {MEDIA_TYPE_META}"),
            });
        };
        let mut bytes = Vec::new();
        self.layout
            .blobs()
            .open_blob(meta_desc.digest())?
            .read_to_end(&mut bytes)?;
        let meta_manifest: ImageManifest = serde_json::from_slice(&bytes)?;
        let layer = meta_manifest
            .layers()
            .first()
            .ok_or_else(|| PackageError::NotAPackage {
                path: self.layout.root().display().to_string(),
                detail: "metadata manifest has no layers".to_string(),
            })?;
        let mut bytes = Vec::new();
        self.layout
            .blobs()
            .open_blob(layer.digest())?
            .read_to_end(&mut bytes)?;
        Ok(serde_json::from_slice(&bytes)?)
    }
}

/// Render a one-screen package summary for `kurin package inspect`.
pub fn inspect_summary(source: &PackageSource) -> Result<String, PackageError> {
    let index = source.index_digest()?;
    let meta = source.manifest()?;
    let mut out = String::new();
    use std::fmt::Write as _;
    writeln!(
        out,
        "package:   {} {} ({})",
        meta.name, meta.version, meta.distribution
    )
    .unwrap();
    writeln!(out, "kind:      {}", meta.kind.as_str()).unwrap();
    writeln!(out, "index:     {index}").unwrap();
    writeln!(out, "created:   {}", meta.created).unwrap();
    writeln!(out, "kurin:     {}", meta.kurin).unwrap();
    writeln!(out, "arch:      {}", meta.arch.join(", ")).unwrap();
    writeln!(out, "components: {}", meta.components.len()).unwrap();
    for component in &meta.components {
        writeln!(out, "  {} {}", component.id, component.version).unwrap();
    }
    let mut by_arch: BTreeMap<&str, Vec<&PackageArtifact>> = BTreeMap::new();
    for artifact in &meta.artifacts {
        by_arch
            .entry(artifact.arch.as_str())
            .or_default()
            .push(artifact);
    }
    for (arch, artifacts) in &by_arch {
        writeln!(out, "files ({arch}, {}):", artifacts.len()).unwrap();
        for artifact in artifacts {
            let digest = artifact.digest.trim_start_matches("sha256:");
            writeln!(
                out,
                "  {} {} {} {} sha256:{}… ({} bytes, {})",
                artifact.name,
                artifact.version,
                artifact.kind,
                artifact.upstream,
                &digest[..digest.len().min(12)],
                artifact.size,
                artifact.upstream
            )
            .unwrap();
        }
    }
    writeln!(out, "images: {}", meta.images.len()).unwrap();
    for image in &meta.images {
        let digest = image.digest.trim_start_matches("sha256:");
        writeln!(
            out,
            "  {} sha256:{}… [{}] ({})",
            image.reference,
            &digest[..digest.len().min(12)],
            image.arch.join(", "),
            image.used_by.join(", ")
        )
        .unwrap();
    }
    if !meta.os.is_empty() {
        writeln!(out, "os:        {}", meta.os.len()).unwrap();
        for row in &meta.os {
            let digest = row.digest.trim_start_matches("sha256:");
            writeln!(
                out,
                "  {} ({}) sha256:{}…",
                row.os,
                row.arch,
                &digest[..digest.len().min(12)]
            )
            .unwrap();
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// SPDX SBOM (04 §6)
// ---------------------------------------------------------------------------

/// SPDX 2.3 JSON document generation from package metadata. Every file
/// and image becomes one package with name, version, purl, digest and
/// download location, DESCRIBES-related to the document.
pub mod spdx {
    use serde::Serialize;

    /// The SPDX spec version this module emits.
    pub const SPEC_VERSION: &str = "SPDX-2.3";

    /// The SPDX document (only the fields kurin fills in).
    #[derive(Debug, Clone, Serialize)]
    pub struct Document {
        #[serde(rename = "spdxVersion")]
        pub spdx_version: String,
        #[serde(rename = "dataLicense")]
        pub data_license: String,
        #[serde(rename = "SPDXID")]
        pub spdx_id: String,
        pub name: String,
        #[serde(rename = "documentNamespace")]
        pub document_namespace: String,
        #[serde(rename = "creationInfo")]
        pub creation_info: CreationInfo,
        pub packages: Vec<Package>,
        pub relationships: Vec<Relationship>,
    }

    /// When and with what the document was created.
    #[derive(Debug, Clone, Serialize)]
    pub struct CreationInfo {
        pub created: String,
        pub creators: Vec<String>,
    }

    /// One file artifact or image.
    #[derive(Debug, Clone, Serialize)]
    pub struct Package {
        pub name: String,
        #[serde(rename = "SPDXID")]
        pub spdx_id: String,
        #[serde(rename = "versionInfo")]
        pub version_info: String,
        #[serde(rename = "downloadLocation")]
        pub download_location: String,
        #[serde(rename = "filesAnalyzed")]
        pub files_analyzed: bool,
        #[serde(rename = "licenseConcluded")]
        pub license_concluded: String,
        #[serde(rename = "licenseDeclared")]
        pub license_declared: String,
        #[serde(rename = "copyrightText")]
        pub copyright_text: String,
        pub checksums: Vec<Checksum>,
        #[serde(rename = "externalRefs")]
        pub external_refs: Vec<ExternalRef>,
    }

    /// One sha256 checksum.
    #[derive(Debug, Clone, Serialize)]
    pub struct Checksum {
        pub algorithm: String,
        #[serde(rename = "checksumValue")]
        pub checksum_value: String,
    }

    /// The purl external reference.
    #[derive(Debug, Clone, Serialize)]
    pub struct ExternalRef {
        #[serde(rename = "referenceCategory")]
        pub reference_category: String,
        #[serde(rename = "referenceType")]
        pub reference_type: String,
        #[serde(rename = "referenceLocator")]
        pub reference_locator: String,
    }

    /// DOCUMENT DESCRIBES <package>.
    #[derive(Debug, Clone, Serialize)]
    pub struct Relationship {
        #[serde(rename = "spdxElementId")]
        pub spdx_element_id: String,
        #[serde(rename = "relationshipType")]
        pub relationship_type: String,
        #[serde(rename = "relatedSpdxElement")]
        pub related_spdx_element: String,
    }

    impl Document {
        /// Build the SBOM of one package: one SPDX package per file
        /// artifact (per arch) and per image, all DESCRIBES-related to
        /// the document. The namespace embeds the package index digest
        /// so every distinct package gets a distinct namespace.
        pub fn from_package(
            meta: &super::PackageManifest,
            index: &super::Digest,
            tool: &str,
        ) -> Document {
            let mut packages = Vec::new();
            let mut relationships = Vec::new();
            for artifact in &meta.artifacts {
                let id = format!("SPDXRef-File-{}-{}", spdx_id(&artifact.name), artifact.arch);
                packages.push(file_package(
                    &id,
                    &artifact.name,
                    &artifact.version,
                    &artifact.upstream,
                    artifact.digest.trim_start_matches("sha256:"),
                ));
                relationships.push(describes(&id));
            }
            for image in &meta.images {
                let id = format!("SPDXRef-Image-{}", spdx_id(&image.reference));
                packages.push(image_package(
                    &id,
                    &image.reference,
                    image.digest.trim_start_matches("sha256:"),
                ));
                relationships.push(describes(&id));
            }
            Document {
                spdx_version: SPEC_VERSION.to_string(),
                data_license: "CC0-1.0".to_string(),
                spdx_id: "SPDXRef-DOCUMENT".to_string(),
                name: format!("kurin-{}-{}", meta.name, meta.version),
                document_namespace: format!(
                    "https://kurin.dev/spdx/{}/v{}-{}",
                    meta.name,
                    meta.version,
                    index.digest()
                ),
                creation_info: CreationInfo {
                    created: meta.created.clone(),
                    creators: vec![format!("Tool: {tool}")],
                },
                packages,
                relationships,
            }
        }
    }

    fn file_package(id: &str, name: &str, version: &str, upstream: &str, hex: &str) -> Package {
        Package {
            name: name.to_string(),
            spdx_id: id.to_string(),
            version_info: version.to_string(),
            download_location: upstream.to_string(),
            files_analyzed: false,
            license_concluded: "NOASSERTION".to_string(),
            license_declared: "NOASSERTION".to_string(),
            copyright_text: "NOASSERTION".to_string(),
            checksums: vec![Checksum {
                algorithm: "SHA256".to_string(),
                checksum_value: hex.to_string(),
            }],
            external_refs: vec![ExternalRef {
                reference_category: "PACKAGE-MANAGER".to_string(),
                reference_type: "purl".to_string(),
                reference_locator: format!("pkg:generic/{name}@{version}"),
            }],
        }
    }

    fn image_package(id: &str, reference: &str, hex: &str) -> Package {
        // `registry.k8s.io/kube-apiserver:v1.33.1` → name + purl.
        let (name, purl) = match reference.rsplit_once(':') {
            Some((path, tag)) => (path.to_string(), format!("pkg:docker/{path}@{tag}")),
            None => (reference.to_string(), format!("pkg:docker/{reference}")),
        };
        Package {
            name,
            spdx_id: id.to_string(),
            version_info: tag_of(reference),
            download_location: format!("docker://{reference}"),
            files_analyzed: false,
            license_concluded: "NOASSERTION".to_string(),
            license_declared: "NOASSERTION".to_string(),
            copyright_text: "NOASSERTION".to_string(),
            checksums: vec![Checksum {
                algorithm: "SHA256".to_string(),
                checksum_value: hex.to_string(),
            }],
            external_refs: vec![ExternalRef {
                reference_category: "PACKAGE-MANAGER".to_string(),
                reference_type: "purl".to_string(),
                reference_locator: purl,
            }],
        }
    }

    fn describes(id: &str) -> Relationship {
        Relationship {
            spdx_element_id: "SPDXRef-DOCUMENT".to_string(),
            relationship_type: "DESCRIBES".to_string(),
            related_spdx_element: id.to_string(),
        }
    }

    /// `registry.k8s.io/etcd:3.5.21-0` → `3.5.21-0`; `NOASSERTION` when
    /// the reference has no tag.
    fn tag_of(reference: &str) -> String {
        reference
            .rsplit_once(':')
            .map(|(_, tag)| tag.to_string())
            .unwrap_or_else(|| "NOASSERTION".to_string())
    }

    /// SPDXIDs allow `[A-Za-z0-9.-]` only; everything else becomes `-`.
    fn spdx_id(s: &str) -> String {
        s.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                    c
                } else {
                    '-'
                }
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Serialize to canonical JSON. `serde_json`'s map type is a `BTreeMap`
/// (the `preserve_order` feature is off), so every object key is sorted
/// and the bytes — hence every digest — are stable across runs.
fn canonical_json<T: Serialize>(value: &T) -> Result<String, PackageError> {
    let value = serde_json::to_value(value)?;
    Ok(serde_json::to_string(&value)?)
}

/// The `application/vnd.oci.empty.v1+json` config descriptor.
pub(crate) fn empty_descriptor() -> Descriptor {
    DescriptorBuilder::default()
        .media_type(MediaType::EmptyJSON)
        .digest(
            Digest::try_from(EMPTY_JSON_DIGEST).expect("the constant empty-JSON digest is valid"),
        )
        .size(EMPTY_JSON.len() as u64)
        .build()
        .expect("the constant empty-JSON descriptor is valid")
}

/// Annotations of one file layer: the on-node file name and the artifact
/// manifest entry it was resolved from.
fn file_annotations(title: &str, name: &str) -> HashMap<String, String> {
    HashMap::from([
        (
            "org.opencontainers.image.title".to_string(),
            title.to_string(),
        ),
        (ARTIFACT_NAME_ANNOTATION.to_string(), name.to_string()),
    ])
}

fn oci_platform(arch: crate::artifact::Arch) -> Platform {
    PlatformBuilder::default()
        .architecture(match arch {
            crate::artifact::Arch::Amd64 => OciArch::Amd64,
            crate::artifact::Arch::Arm64 => OciArch::ARM64,
        })
        .os(OciOs::Linux)
        .build()
        .expect("constant platform is valid")
}

/// Copy a blob from the cache into the layout, hard-linking when the
/// two share a filesystem (packages are mostly renames of cached
/// content) and falling back to a copy.
fn transfer_blob(cache: &BlobStore, into: &BlobStore, digest: &Digest) -> Result<(), PackageError> {
    if into.has(digest) {
        return Ok(());
    }
    let src = cache.path(digest);
    let dst = into.path(digest);
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match std::fs::hard_link(&src, &dst) {
        Ok(()) => Ok(()),
        Err(_) => Ok(std::fs::copy(&src, &dst).map(|_| ())?),
    }
}

/// Write `bytes` to `path` through a temporary file and rename.
fn write_file_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut tmp = path.as_os_str().to_os_string();
    tmp.push(".part");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, bytes)?;
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// The kurin compatibility range of the running build: this minor
/// series plus the next one (`0.4.0` → `>=0.4.0, <0.6.0`).
fn kurin_compat_range() -> String {
    let version = semver::Version::parse(env!("CARGO_PKG_VERSION"))
        .expect("CARGO_PKG_VERSION parses as semver");
    let upper_minor = version.minor + 2;
    format!(">={version}, <0.{upper_minor}.0")
}

/// The current UTC time, RFC 3339.
fn now_rfc3339() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .expect("formatting a timestamp cannot fail")
}

// ---------------------------------------------------------------------------
// errors
// ---------------------------------------------------------------------------

/// Errors raised by package creation and inspection.
#[derive(Debug, Error, miette::Diagnostic)]
pub enum PackageError {
    #[error(transparent)]
    Artifact(#[from] crate::artifact::ArtifactError),

    #[error(transparent)]
    Store(#[from] crate::store::StoreError),

    #[error(transparent)]
    Layout(#[from] crate::oci::layout::LayoutError),

    #[error(transparent)]
    Json(#[from] serde_json::Error),

    #[error("{detail}")]
    Miette { detail: String },

    #[error("package I/O error: {message}")]
    Io {
        message: String,
        #[source]
        source: std::io::Error,
    },

    #[error("no architecture given: pass --arch (e.g. --arch amd64,arm64)")]
    NoArch,

    #[error("invalid --kubernetes {value:?}: {detail}")]
    InvalidKubernetes { value: String, detail: String },

    #[error("unsupported package output `{output}` (expected oci-layout://DIR, file://X.tar or file://X.tar.zst)")]
    InvalidOutput { output: String },

    #[error("unsupported package source `{input}` (expected oci-layout://DIR, file://X.tar or file://X.tar.zst)")]
    InvalidSource { input: String },

    #[error("oci:// targets need `kurin package push`; build to oci-layout:// or file:// first")]
    RegistryOutputUnsupported,

    #[error(
        "unsupported package reference `{input}` (expected oci://host/repo[:tag], oci-layout://DIR, file://X.tar or file://X.tar.zst)"
    )]
    InvalidRef { input: String },

    #[error("registry transport: {detail}")]
    Remote {
        detail: String,
        #[source]
        source: crate::oci::remote::RemoteError,
    },

    #[error("manifest {digest} is {expected} bytes but stores {actual}")]
    ManifestSizeMismatch {
        digest: String,
        expected: u64,
        actual: u64,
    },

    #[error("package verification failed:\n{}", failures.join("\n"))]
    Verification { failures: Vec<String> },

    #[error(
        "oci:// sources must be fetched through the registry client: use `kurin package pull/inspect/verify`"
    )]
    RegistrySourceUnsupported,

    #[error("artifact `{name}` has no pinned sha256 for {target}")]
    UnpinnedArtifact { name: String, target: String },

    #[error("`{name}` {version} ({target}): blob sha256 mismatch\n  expected: {expected}\n  actual:   {actual}")]
    ChecksumMismatch {
        name: String,
        version: String,
        target: String,
        expected: String,
        actual: String,
    },

    #[error("invalid digest `{0}`")]
    InvalidDigest(String),

    #[error("image `{reference}` is pinned differently per arch: {first} vs {second}")]
    InconsistentImagePin {
        reference: String,
        first: String,
        second: String,
    },

    #[error("the empty-JSON blob digest changed: {actual}")]
    EmptyJsonChanged { actual: String },

    #[error("{path} is not a kurin package: {detail}")]
    NotAPackage { path: String, detail: String },

    #[error("cannot build the package: {detail}")]
    Build { detail: String },

    /// A referrer could not be attached or read.
    #[error(transparent)]
    Referrer {
        #[from]
        source: crate::referrer::ReferrerError,
    },

    /// Signature verification failed.
    #[error(transparent)]
    Signature {
        #[from]
        source: crate::signature::SignatureError,
    },
}

impl From<std::io::Error> for PackageError {
    fn from(source: std::io::Error) -> Self {
        PackageError::Io {
            message: source.to_string(),
            source,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest as _, Sha256};

    fn digest_of(bytes: &[u8]) -> Digest {
        Digest::try_from(format!("sha256:{:x}", Sha256::digest(bytes)))
            .expect("sha256 hex is a valid digest")
    }

    fn builder() -> PackageBuilder {
        let mut b = PackageBuilder::new(PackageKind::Base, "base", "1.33.1", "kubeadm")
            .with_created("2026-10-28T00:00:00Z")
            .with_kurin(">=0.4.0, <0.6.0");
        b.add_component("distro/kubeadm", "1.33.1");
        b
    }

    #[test]
    fn output_targets_parse() {
        assert_eq!(
            PackageOutput::parse("oci-layout:///srv/kurin/base").unwrap(),
            PackageOutput::Layout(PathBuf::from("/srv/kurin/base"))
        );
        assert_eq!(
            PackageOutput::parse("oci-layout://rel/base").unwrap(),
            PackageOutput::Layout(PathBuf::from("rel/base"))
        );
        assert_eq!(
            PackageOutput::parse("file:///tmp/base.tar.zst").unwrap(),
            PackageOutput::TarZstd(PathBuf::from("/tmp/base.tar.zst"))
        );
        assert_eq!(
            PackageOutput::parse("file:///tmp/base.tar").unwrap(),
            PackageOutput::Tar(PathBuf::from("/tmp/base.tar"))
        );
        assert_eq!(
            PackageOutput::parse("/tmp/base.tar.zst").unwrap(),
            PackageOutput::TarZstd(PathBuf::from("/tmp/base.tar.zst"))
        );
        assert!(PackageOutput::parse("oci://reg/base:v1").is_err());
        assert!(PackageOutput::parse("/srv/dir").is_err());
    }

    #[test]
    fn sbom_sidecar_paths() {
        assert_eq!(
            PackageOutput::Layout(PathBuf::from("/srv/base")).sbom_sidecar_path(),
            PathBuf::from("/srv/base.spdx.json")
        );
        assert_eq!(
            PackageOutput::TarZstd(PathBuf::from("/tmp/base.tar.zst")).sbom_sidecar_path(),
            PathBuf::from("/tmp/base.spdx.json")
        );
        assert_eq!(
            PackageOutput::Tar(PathBuf::from("/tmp/base.tar")).sbom_sidecar_path(),
            PathBuf::from("/tmp/base.spdx.json")
        );
    }

    #[test]
    fn canonical_json_sorts_keys() {
        let mut annotations = HashMap::new();
        annotations.insert("b".to_string(), "2".to_string());
        annotations.insert("a".to_string(), "1".to_string());
        let first = canonical_json(&annotations).unwrap();
        // serde_json's map is a BTreeMap: keys come out sorted no matter
        // the insertion order.
        assert_eq!(first, r#"{"a":"1","b":"2"}"#);
        // And it is stable across processes for the same content.
        let mut other = HashMap::new();
        other.insert("a".to_string(), "1".to_string());
        other.insert("b".to_string(), "2".to_string());
        assert_eq!(canonical_json(&other).unwrap(), first);
    }

    #[test]
    fn kurin_range_of_this_build() {
        let version = semver::Version::parse(env!("CARGO_PKG_VERSION")).unwrap();
        let range = kurin_compat_range();
        assert!(range.starts_with(&format!(">={version}")), "{range}");
        assert!(range.contains("<"), "{range}");
    }

    #[test]
    fn strip_tar_extensions_drops_both() {
        assert_eq!(
            strip_tar_extensions(Path::new("/tmp/base.tar.zst")),
            PathBuf::from("/tmp/base")
        );
        assert_eq!(
            strip_tar_extensions(Path::new("/tmp/base.tar")),
            PathBuf::from("/tmp/base")
        );
        assert_eq!(
            strip_tar_extensions(Path::new("/tmp/base")),
            PathBuf::from("/tmp/base")
        );
    }

    #[test]
    fn spdx_document_covers_files_and_images() {
        let meta = PackageManifest {
            schema: 1,
            name: "base".into(),
            kind: PackageKind::Base,
            version: "1.33.1".into(),
            kurin: ">=0.4.0".into(),
            created: "2026-10-28T00:00:00Z".into(),
            distribution: "kubeadm".into(),
            arch: vec!["amd64".into()],
            components: vec![],
            artifacts: vec![PackageArtifact {
                name: "kubeadm".into(),
                kind: "binary".into(),
                version: "1.33.1".into(),
                arch: "amd64".into(),
                digest: format!("sha256:{:x}", Sha256::digest(b"k")),
                size: 1,
                upstream: "https://dl.k8s.io/".into(),
            }],
            images: vec![PackageImageMeta {
                reference: "registry.k8s.io/pause:3.10".into(),
                digest: format!("sha256:{:x}", Sha256::digest(b"i")),
                arch: vec!["amd64".into()],
                used_by: vec!["runtime/containerd".into()],
            }],
            charts: vec![],
            os: vec![],
        };
        let index = digest_of(b"index");
        let doc = spdx::Document::from_package(&meta, &index, "kurin-test");
        assert_eq!(doc.spdx_version, "SPDX-2.3");
        assert_eq!(doc.packages.len(), 2);
        assert_eq!(doc.packages[0].name, "kubeadm");
        assert_eq!(doc.packages[0].download_location, "https://dl.k8s.io/");
        assert_eq!(
            doc.packages[0].external_refs[0].reference_locator,
            "pkg:generic/kubeadm@1.33.1"
        );
        assert_eq!(doc.packages[1].name, "registry.k8s.io/pause");
        assert_eq!(doc.packages[1].version_info, "3.10");
        assert_eq!(
            doc.packages[1].external_refs[0].reference_locator,
            "pkg:docker/registry.k8s.io/pause@3.10"
        );
        assert_eq!(doc.relationships.len(), 2);
        assert!(doc.document_namespace.ends_with(index.digest()));
        let json = canonical_json(&doc).unwrap();
        assert!(json.contains(r#""spdxVersion":"SPDX-2.3""#), "{json}");
    }

    #[test]
    fn builder_assembles_the_documented_index_shape() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = tempfile::tempdir().unwrap();
        let cache = BlobStore::open(cache_dir.path());

        let kubeadm = b"kubeadm-bytes";
        let yaml = b"apiVersion: v1\n";
        let k_digest = cache.put_bytes(kubeadm).unwrap();
        let y_digest = cache.put_bytes(yaml).unwrap();

        // A tiny multi-arch "image": one index, one platform manifest,
        // one config and one layer blob.
        let config = br#"{"config":true}"#;
        let layer = b"layer-bytes";
        let c_digest = cache.put_bytes(config).unwrap();
        let l_digest = cache.put_bytes(layer).unwrap();
        let platform_manifest = serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": { "mediaType": "application/vnd.oci.image.config.v1+json",
                        "digest": c_digest.to_string(), "size": config.len() },
            "layers": [ { "mediaType": "application/vnd.oci.image.layer.v1.tar",
                          "digest": l_digest.to_string(), "size": layer.len() } ]
        })
        .to_string();
        let p_digest = cache.put_bytes(platform_manifest.as_bytes()).unwrap();
        let index = serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.index.v1+json",
            "manifests": [ { "mediaType": "application/vnd.oci.image.manifest.v1+json",
                             "digest": p_digest.to_string(), "size": platform_manifest.len(),
                             "platform": { "architecture": "amd64", "os": "linux" } } ]
        })
        .to_string();
        let i_digest = cache.put_bytes(index.as_bytes()).unwrap();

        let mut builder = builder();
        builder.add_component("runtime/containerd", "2.1.4");
        builder.add_file(PackageFile {
            name: "kubeadm".into(),
            kind: ArtifactKind::Binary,
            version: "1.33.1".into(),
            arch: crate::artifact::Arch::Amd64,
            digest: k_digest.clone(),
            size: kubeadm.len() as u64,
            title: "kubeadm".into(),
            upstream: "https://dl.k8s.io/kubeadm".into(),
        });
        builder.add_file(PackageFile {
            name: "calico-operator".into(),
            kind: ArtifactKind::Manifest,
            version: "3.30.2".into(),
            arch: crate::artifact::Arch::Arm64,
            digest: y_digest.clone(),
            size: yaml.len() as u64,
            title: "tigera-operator.yaml".into(),
            upstream: "https://example/tigera-operator.yaml".into(),
        });
        builder.add_image(PackageImage {
            reference: "registry.k8s.io/pause:3.10".into(),
            digest: i_digest.clone(),
            size: index.len() as u64,
            media_type: "application/vnd.oci.image.index.v1+json".into(),
            arch: vec![crate::artifact::Arch::Amd64],
            used_by: vec!["runtime/containerd".into()],
            blobs: vec![p_digest.clone(), c_digest.clone(), l_digest.clone()],
        });

        let out = PackageOutput::Layout(dir.path().join("pkg"));
        let report = builder.build(&cache, &out).unwrap();
        assert_eq!(report.tag, "v1.33.1-kubeadm");
        assert!(dir.path().join("pkg/oci-layout").is_file());
        assert!(dir.path().join("pkg.spdx.json").is_file());
        assert_eq!(report.sbom, dir.path().join("pkg.spdx.json"));

        // index.json → [package index, SBOM referrer] → the documented
        // shape below the package index.
        let layout = Layout::open(dir.path().join("pkg")).unwrap();
        let root = layout.read_index().unwrap();
        assert_eq!(root.manifests().len(), 2, "root + SBOM referrer");
        assert_eq!(root.manifests()[0].digest(), &report.index);
        assert_eq!(
            root.manifests()[0]
                .annotations()
                .as_ref()
                .unwrap()
                .get(REF_NAME_ANNOTATION)
                .unwrap(),
            "v1.33.1-kubeadm"
        );
        // the second entry is the SPDX SBOM referrer of the index
        let sbom_entry = &root.manifests()[1];
        assert_eq!(
            sbom_entry.artifact_type().as_ref().unwrap().as_ref(),
            MEDIA_TYPE_SPDX
        );
        assert_eq!(
            root.manifests()[0]
                .annotations()
                .as_ref()
                .unwrap()
                .get(REF_NAME_ANNOTATION)
                .unwrap(),
            "v1.33.1-kubeadm"
        );

        let mut bytes = Vec::new();
        layout
            .blobs()
            .open_blob(&report.index)
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        let package_index: ImageIndex = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            package_index.artifact_type().as_ref().unwrap().as_ref(),
            MEDIA_TYPE_PACKAGE
        );
        // meta + one files manifest per arch + one image.
        assert_eq!(package_index.manifests().len(), 4);
        let meta_desc = &package_index.manifests()[0];
        assert_eq!(
            meta_desc.artifact_type().as_ref().unwrap().as_ref(),
            MEDIA_TYPE_META
        );
        let files_desc = &package_index.manifests()[1];
        assert_eq!(
            files_desc.artifact_type().as_ref().unwrap().as_ref(),
            MEDIA_TYPE_FILES
        );
        assert_eq!(
            files_desc.platform().as_ref().unwrap().architecture(),
            &OciArch::Amd64
        );
        let image_desc = &package_index.manifests()[3];
        assert_eq!(image_desc.digest(), &i_digest);
        assert_eq!(
            image_desc
                .annotations()
                .as_ref()
                .unwrap()
                .get(IMAGE_REF_ANNOTATION)
                .unwrap(),
            "registry.k8s.io/pause:3.10"
        );

        // meta manifest: empty config + kurin-package.json layer.
        let mut bytes = Vec::new();
        layout
            .blobs()
            .open_blob(meta_desc.digest())
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        let meta_manifest: ImageManifest = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            meta_manifest.artifact_type().as_ref().unwrap().as_ref(),
            MEDIA_TYPE_META
        );
        assert_eq!(meta_manifest.config().media_type(), &MediaType::EmptyJSON);
        let mut bytes = Vec::new();
        layout
            .blobs()
            .open_blob(meta_manifest.layers()[0].digest())
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        let meta: PackageManifest = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(meta.name, "base");
        assert_eq!(meta.arch, vec!["amd64".to_string(), "arm64".to_string()]);
        assert_eq!(meta.artifacts.len(), 2);
        assert_eq!(meta.images.len(), 1);
        assert_eq!(meta.images[0].used_by, vec!["runtime/containerd"]);

        // files manifest layers carry the documented annotations.
        let mut bytes = Vec::new();
        layout
            .blobs()
            .open_blob(files_desc.digest())
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        let files_manifest: ImageManifest = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            files_manifest.artifact_type().as_ref().unwrap().as_ref(),
            MEDIA_TYPE_FILES
        );
        let layer0 = &files_manifest.layers()[0];
        assert_eq!(layer0.digest(), &k_digest);
        let annotations = layer0.annotations().as_ref().unwrap();
        assert_eq!(
            annotations.get("org.opencontainers.image.title").unwrap(),
            "kubeadm"
        );
        assert_eq!(
            annotations.get(ARTIFACT_NAME_ANNOTATION).unwrap(),
            "kubeadm"
        );

        // Deterministic: the same inputs rebuild byte-identical output.
        let second = dir.path().join("pkg2");
        let report2 = builder
            .build(&cache, &PackageOutput::Layout(second.clone()))
            .unwrap();
        assert_eq!(report2.index, report.index);
        let a = std::fs::read(dir.path().join("pkg/index.json")).unwrap();
        let b = std::fs::read(second.join("index.json")).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn build_packs_tar_zstd_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = tempfile::tempdir().unwrap();
        let cache = BlobStore::open(cache_dir.path());
        let payload = b"containerd-tarball";
        let digest = cache.put_bytes(payload).unwrap();

        let mut builder = builder();
        builder.add_file(PackageFile {
            name: "containerd".into(),
            kind: ArtifactKind::Binary,
            version: "2.1.4".into(),
            arch: crate::artifact::Arch::Amd64,
            digest,
            size: payload.len() as u64,
            title: "containerd-2.1.4-linux-amd64.tar.gz".into(),
            upstream: "https://github.com/containerd/".into(),
        });

        let tar_path = dir.path().join("base.tar.zst");
        let out = PackageOutput::TarZstd(tar_path.clone());
        let report = builder.build(&cache, &out).unwrap();
        assert!(tar_path.is_file());
        assert!(dir.path().join("base.spdx.json").is_file());

        // Reading it back through the tar transport yields the same
        // package identity and manifest.
        let source = PackageSource::open(&format!("file://{}", tar_path.display())).unwrap();
        assert_eq!(source.index_digest().unwrap(), report.index);
        let meta = source.manifest().unwrap();
        assert_eq!(meta.name, "base");
        assert_eq!(meta.artifacts.len(), 1);
        assert_eq!(meta.artifacts[0].name, "containerd");
        // Layout::resolve finds the tag.
        assert_eq!(
            source.layout().resolve("v1.33.1-kubeadm").unwrap().digest(),
            &report.index
        );
    }

    #[test]
    fn inspect_summary_lists_the_package() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = tempfile::tempdir().unwrap();
        let cache = BlobStore::open(cache_dir.path());
        let payload = b"kubelet";
        let digest = cache.put_bytes(payload).unwrap();
        let mut builder = builder();
        builder.add_file(PackageFile {
            name: "kubelet".into(),
            kind: ArtifactKind::Binary,
            version: "1.33.1".into(),
            arch: crate::artifact::Arch::Amd64,
            digest,
            size: payload.len() as u64,
            title: "kubelet".into(),
            upstream: "https://dl.k8s.io/kubelet".into(),
        });
        let out = dir.path().join("pkg");
        let report = builder
            .build(&cache, &PackageOutput::Layout(out.clone()))
            .unwrap();
        let source = PackageSource::open(&format!("oci-layout://{}", out.display())).unwrap();
        let summary = inspect_summary(&source).unwrap();
        assert!(
            summary.contains("package:   base 1.33.1 (kubeadm)"),
            "{summary}"
        );
        assert!(
            summary.contains(&format!("index:     {}", report.index)),
            "{summary}"
        );
        assert!(summary.contains("files (amd64, 1)"), "{summary}");
        assert!(summary.contains("kubelet 1.33.1 binary"), "{summary}");
    }

    #[test]
    fn missing_metadata_is_reported_as_not_a_package() {
        let dir = tempfile::tempdir().unwrap();
        Layout::create(dir.path()).unwrap();
        let err = PackageSource::open(&format!("oci-layout://{}", dir.path().display()))
            .unwrap()
            .manifest()
            .unwrap_err();
        assert!(err.to_string().contains("is not a kurin package"), "{err}");
    }

    #[test]
    fn empty_json_digest_matches_the_spec() {
        assert_eq!(digest_of(EMPTY_JSON).to_string(), EMPTY_JSON_DIGEST);
    }

    #[test]
    fn os_package_builder_records_rows_and_tag() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = tempfile::tempdir().unwrap();
        let cache = BlobStore::open(cache_dir.path());
        let payload = b"repo-tar-bytes";
        let digest = cache.put_bytes(payload).unwrap();

        let mut builder = PackageBuilder::new(PackageKind::Os, "rocky-9", "2026.10", "any")
            .with_created("2026-10-28T00:00:00Z")
            .with_kurin(">=0.4.0, <0.6.0");
        builder.add_os_repo(
            "rocky-9",
            crate::artifact::Arch::Amd64,
            PackageFile {
                name: "os/rocky-9".into(),
                kind: ArtifactKind::Binary,
                version: "2026.10".into(),
                arch: crate::artifact::Arch::Amd64,
                digest: digest.clone(),
                size: payload.len() as u64,
                title: "os/rocky-9/repo.tar".into(),
                upstream: "container:rockylinux:9".into(),
            },
        );
        let out = dir.path().join("os");
        let report = builder
            .build(&cache, &PackageOutput::Layout(out.clone()))
            .unwrap();
        assert_eq!(report.tag, "v2026.10");

        let source = PackageSource::open(&format!("oci-layout://{}", out.display())).unwrap();
        let meta = source.manifest().unwrap();
        assert_eq!(meta.kind, PackageKind::Os);
        assert_eq!(meta.distribution, "any");
        assert_eq!(meta.os.len(), 1);
        assert_eq!(meta.os[0].os, "rocky-9");
        assert_eq!(meta.os[0].arch, "amd64");
        assert_eq!(meta.os[0].digest, digest.to_string());
        assert_eq!(meta.artifacts[0].name, "os/rocky-9");

        let summary = inspect_summary(&source).unwrap();
        assert!(summary.contains("os:        1"), "{summary}");
        assert!(summary.contains("rocky-9 (amd64)"), "{summary}");
    }
}
