//! OCI image-spec v1.1 referrers of the package index (04 §3.1, WP-39).
//!
//! The SPDX SBOM and the cosign signature travel as **referrers**:
//! image manifests whose `subject` is the package index descriptor.
//! The image layout spec has no dedicated referrers section — a
//! layout's `index.json` MAY carry auxiliary descriptors next to the
//! tagged root — so kurin files every referrer manifest it stores as
//! an extra `index.json` entry and discovers referrers by scanning
//! those entries for a matching `subject`. Registries use the
//! referrers API (or the `sha256-<hex>` fallback tag) instead; see
//! [`crate::oci::remote`].
//!
//! A referrer manifest is the canonical artifact shape (OCI 1.1):
//!
//! ```text
//! mediaType:    application/vnd.oci.image.manifest.v1+json
//! artifactType: e.g. application/spdx+json (the SBOM)
//! config:       application/vnd.oci.empty.v1+json (the shared {} blob)
//! layers:       [ the payload, annotated as needed ]
//! subject:      the package index descriptor
//! ```

use std::collections::HashMap;
use std::io::Read;

use oci_spec::image::{
    Descriptor, DescriptorBuilder, Digest, ImageIndex, ImageIndexBuilder, ImageManifest,
    ImageManifestBuilder, MediaType,
};
use thiserror::Error;

use crate::oci::is_manifest_media_type;
use crate::oci::layout::Layout;

/// Errors raised while attaching or discovering referrers.
#[derive(Debug, Error)]
pub enum ReferrerError {
    /// The backing layout could not be read or written.
    #[error(transparent)]
    Layout(#[from] crate::oci::layout::LayoutError),

    /// A referrer manifest did not parse.
    #[error("referrer manifest {digest}: {detail}")]
    InvalidManifest { digest: String, detail: String },

    /// The blob store rejected a read or write.
    #[error(transparent)]
    Store(#[from] crate::store::StoreError),

    /// Reading a blob failed.
    #[error("referrer blob I/O: {0}")]
    Io(#[from] std::io::Error),

    /// Manifest serialization failed.
    #[error(transparent)]
    Json(#[from] serde_json::Error),

    /// Building an OCI descriptor failed (a constant went wrong).
    #[error("building {what}: {source}")]
    Build {
        what: &'static str,
        source: oci_spec::OciSpecError,
    },
}

/// One referrer of the package index: its manifest (already
/// digest-verified on read) plus helpers for the canonical parts.
#[derive(Debug, Clone)]
pub struct Referrer {
    /// The referrer manifest's own descriptor, as filed in `index.json`.
    pub descriptor: Descriptor,
    /// The parsed referrer manifest.
    pub manifest: ImageManifest,
}

impl Referrer {
    /// The `artifactType`: what kind of referrer this is.
    pub fn artifact_type(&self) -> &str {
        self.manifest
            .artifact_type()
            .as_ref()
            .map(|t| t.as_ref())
            .unwrap_or_default()
    }

    /// The digest this referrer points at (its `subject`).
    pub fn subject_digest(&self) -> Option<&Digest> {
        self.manifest.subject().as_ref().map(|s| s.digest())
    }

    /// The single payload layer's descriptor.
    ///
    /// # Errors
    ///
    /// A referrer with no layers — kurin's referrers always carry
    /// exactly one payload layer; anything else is not consumed.
    pub fn payload(&self) -> Result<&Descriptor, ReferrerError> {
        self.manifest
            .layers()
            .first()
            .ok_or_else(|| ReferrerError::InvalidManifest {
                digest: self.descriptor.digest().to_string(),
                detail: "no payload layer".to_string(),
            })
    }

    /// The payload layer's annotations (signature storage, titles).
    pub fn payload_annotations(&self) -> Option<&HashMap<String, String>> {
        self.manifest
            .layers()
            .first()
            .and_then(|l| l.annotations().as_ref())
    }
}

/// Attach one referrer to `subject` in `layout`: store the payload as
/// a blob, write the referrer manifest pointing at it, and file the
/// manifest as an extra `index.json` entry (the tagged root stays
/// first). Returns the referrer's descriptor.
///
/// `layer_annotations` travel on the payload descriptor — the cosign
/// signature rides there as `dev.cosignproject.cosign/signature`.
///
/// # Errors
///
/// A blob-store or layout error, or a manifest build failure.
pub fn attach(
    layout: &Layout,
    subject: &Descriptor,
    artifact_type: &str,
    payload_media_type: &str,
    payload: &[u8],
    layer_annotations: HashMap<String, String>,
) -> Result<Descriptor, ReferrerError> {
    let payload_digest = layout.put_blob(payload)?;
    let payload_size = layout.blobs().size(&payload_digest)?;
    let mut layer = DescriptorBuilder::default()
        .media_type(MediaType::Other(payload_media_type.to_string()))
        .digest(payload_digest)
        .size(payload_size);
    if !layer_annotations.is_empty() {
        layer = layer.annotations(layer_annotations);
    }
    let manifest = ImageManifestBuilder::default()
        .schema_version(2u32)
        .media_type(MediaType::ImageManifest)
        .artifact_type(MediaType::Other(artifact_type.to_string()))
        .config(crate::package::empty_descriptor())
        .layers(vec![layer.build().map_err(|e| ReferrerError::Build {
            what: "payload layer descriptor",
            source: e,
        })?])
        .subject(bare_subject(subject))
        .build()
        .map_err(|e| ReferrerError::Build {
            what: "referrer manifest",
            source: e,
        })?;
    append_manifest(layout, &manifest, artifact_type)
}

/// Store an already-built referrer `manifest` (with its `subject` set)
/// in `layout` and file it in `index.json`. The manifest's exact bytes
/// are preserved, so a referrer pulled from a registry keeps its
/// digest.
///
/// # Errors
///
/// A blob-store or layout error.
pub fn append_manifest(
    layout: &Layout,
    manifest: &ImageManifest,
    artifact_type: &str,
) -> Result<Descriptor, ReferrerError> {
    let bytes = serde_json::to_vec(manifest)?;
    let digest = layout.put_blob(&bytes)?;
    let size = layout.blobs().size(&digest)?;
    let descriptor = DescriptorBuilder::default()
        .media_type(MediaType::ImageManifest)
        .artifact_type(MediaType::Other(artifact_type.to_string()))
        .digest(digest)
        .size(size)
        .build()
        .map_err(|e| ReferrerError::Build {
            what: "referrer descriptor",
            source: e,
        })?;
    add_index_entry(layout, descriptor.clone())?;
    Ok(descriptor)
}

/// Every referrer of `subject` filed in `layout`'s `index.json`.
/// Manifest documents are read through the blob store, so each one is
/// digest-verified on the way in; payloads are verified when read.
///
/// # Errors
///
/// A layout or blob-store error (including a tampered manifest).
pub fn find(layout: &Layout, subject: &Digest) -> Result<Vec<Referrer>, ReferrerError> {
    let index = layout.read_index()?;
    let mut found = Vec::new();
    for entry in index.manifests() {
        // The package index itself is not a referrer of itself.
        if entry.digest() == subject {
            continue;
        }
        if !is_manifest_media_type(entry.media_type().as_ref()) {
            continue;
        }
        let mut bytes = Vec::new();
        layout
            .blobs()
            .open_blob(entry.digest())?
            .read_to_end(&mut bytes)?;
        let Ok(manifest) = serde_json::from_slice::<ImageManifest>(&bytes) else {
            // An auxiliary manifest entry that is not a kurin
            // referrer (another tool's content filed in the same
            // layout): not ours to validate, not ours to return.
            continue;
        };
        if manifest
            .subject()
            .as_ref()
            .is_some_and(|s| s.digest() == subject)
        {
            found.push(Referrer {
                descriptor: entry.clone(),
                manifest,
            });
        }
    }
    Ok(found)
}

/// File an already-stored referrer `descriptor` in `layout`'s
/// `index.json` (digest-duplicated entries are skipped). The manifest
/// blob must already be in the layout's store.
///
/// # Errors
///
/// A layout error.
pub fn file_referrer(layout: &Layout, descriptor: Descriptor) -> Result<(), ReferrerError> {
    add_index_entry(layout, descriptor)
}

/// Append `descriptor` to `layout`'s `index.json` unless an entry with
/// the same digest is already filed.
fn add_index_entry(layout: &Layout, descriptor: Descriptor) -> Result<(), ReferrerError> {
    let mut index = layout.read_index()?;
    if index
        .manifests()
        .iter()
        .any(|d| d.digest() == descriptor.digest())
    {
        return Ok(());
    }
    let mut manifests = index.manifests().clone();
    manifests.push(descriptor);
    index.set_manifests(manifests);
    layout.write_index(&index)?;
    Ok(())
}

/// The subject descriptor without the tag annotation: `subject`
/// carries the identity (mediaType, digest, size), not where the
/// package happened to be filed.
fn bare_subject(subject: &Descriptor) -> Descriptor {
    let mut bare = subject.clone();
    if let Some(annotations) = bare.annotations() {
        let mut annotations = annotations.clone();
        annotations.remove(crate::oci::layout::REF_NAME_ANNOTATION);
        bare.set_annotations(if annotations.is_empty() {
            None
        } else {
            Some(annotations)
        });
    }
    bare
}

/// Merge referrer descriptors into the fallback-tag index document
/// (the `sha256-<hex>` tag of the distribution-spec referrers tag
/// schema): existing entries are kept, new ones appended, duplicates
/// by digest dropped. Pure function — the tag update itself lives in
/// [`crate::oci::remote`].
///
/// # Errors
///
/// Manifest build failure (both indexes are constant shapes).
pub fn merge_referrers_tag_index(
    existing: &ImageIndex,
    referrers: &[Descriptor],
) -> Result<ImageIndex, ReferrerError> {
    let mut manifests = existing.manifests().clone();
    for referrer in referrers {
        if !manifests.iter().any(|d| d.digest() == referrer.digest()) {
            manifests.push(referrer.clone());
        }
    }
    ImageIndexBuilder::default()
        .schema_version(2u32)
        .media_type(MediaType::ImageIndex)
        .manifests(manifests)
        .build()
        .map_err(|e| ReferrerError::Build {
            what: "referrers tag index",
            source: e,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn subject() -> Descriptor {
        let digest = Digest::try_from(
            "sha256:44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a",
        )
        .unwrap();
        DescriptorBuilder::default()
            .media_type(MediaType::ImageIndex)
            .digest(digest)
            .size(2u64)
            .build()
            .unwrap()
    }

    #[test]
    fn attach_and_find_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = Layout::create(tmp.path()).unwrap();
        let subject = subject();
        let descriptor = attach(
            &layout,
            &subject,
            "application/vnd.example.sbom",
            "application/vnd.example.sbom+json",
            br#"{"sbom":true}"#,
            HashMap::from([(
                "org.opencontainers.image.title".to_string(),
                "sbom.json".to_string(),
            )]),
        )
        .unwrap();
        let found = find(&layout, subject.digest()).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].descriptor.digest(), descriptor.digest());
        assert_eq!(found[0].artifact_type(), "application/vnd.example.sbom");
        assert_eq!(found[0].subject_digest(), Some(subject.digest()));
        let annotations = found[0].payload_annotations().unwrap();
        assert_eq!(
            annotations.get("org.opencontainers.image.title"),
            Some(&"sbom.json".to_string())
        );

        // attaching the same payload again is idempotent
        attach(
            &layout,
            &subject,
            "application/vnd.example.sbom",
            "application/vnd.example.sbom+json",
            br#"{"sbom":true}"#,
            HashMap::from([(
                "org.opencontainers.image.title".to_string(),
                "sbom.json".to_string(),
            )]),
        )
        .unwrap();
        let again = find(&layout, subject.digest()).unwrap();
        assert_eq!(again.len(), 1, "no duplicate index entry");
    }

    #[test]
    fn other_subjects_are_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = Layout::create(tmp.path()).unwrap();
        let subject = subject();
        attach(
            &layout,
            &subject,
            "application/vnd.example.sbom",
            "application/vnd.example.sbom+json",
            b"payload",
            HashMap::new(),
        )
        .unwrap();
        let other = Digest::try_from(
            "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        )
        .unwrap();
        let found = find(&layout, &other).unwrap();
        assert!(found.is_empty());
    }

    #[test]
    fn merge_dedups_by_digest() {
        let first = DescriptorBuilder::default()
            .media_type(MediaType::ImageManifest)
            .digest(
                Digest::try_from(
                    "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                )
                .unwrap(),
            )
            .size(1u64)
            .build()
            .unwrap();
        let second = DescriptorBuilder::default()
            .media_type(MediaType::ImageManifest)
            .digest(
                Digest::try_from(
                    "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                )
                .unwrap(),
            )
            .size(2u64)
            .build()
            .unwrap();
        let existing = ImageIndexBuilder::default()
            .schema_version(2u32)
            .media_type(MediaType::ImageIndex)
            .manifests(vec![first.clone()])
            .build()
            .unwrap();
        let merged = merge_referrers_tag_index(&existing, &[first, second]).unwrap();
        assert_eq!(merged.manifests().len(), 2);
    }

    #[test]
    fn subject_annotation_is_stripped() {
        let mut tagged = subject();
        tagged.set_annotations(Some(HashMap::from([(
            crate::oci::layout::REF_NAME_ANNOTATION.to_string(),
            "v1.33.1-kubeadm".to_string(),
        )])));
        let bare = bare_subject(&tagged);
        assert!(bare.annotations().is_none());
        assert_eq!(bare.digest(), tagged.digest());
    }
}
