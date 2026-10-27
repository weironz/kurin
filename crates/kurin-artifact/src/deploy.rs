//! Deploy-side payload sources (WP-34, 04 §4): resolve what a run's
//! payloads come from before anything touches a node.
//!
//! A payload source is one of:
//!
//! - a **kurin package** — the three transport forms of 04 §3.3:
//!   `oci-layout://DIR`, `file://X.tar` / `file://X.tar.zst` (a bare path
//!   with one of those shapes is accepted too);
//! - a **legacy offline bundle directory** — the pre-package layout with
//!   `kurin-manifest.json` at the root and no `oci-layout` marker.
//!
//! The compatibility reader keeps the legacy layout working through the
//! same interface: an existing air-gapped environment does not need to
//! rebuild anything after upgrading kurin (04 §4). `oci://` registry
//! sources are rejected with a pointer at `kurin package pull` (WP-33).

use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::artifact_bundle::BUNDLE_MANIFEST;
use crate::package::{PackageError, PackageSource};

/// The `oci-layout` marker file at an OCI layout root.
const OCI_LAYOUT_MARKER: &str = "oci-layout";

/// What a user-supplied payload source resolved to.
pub enum PayloadSource {
    /// A legacy offline bundle directory (`kurin-manifest.json`, no
    /// `oci-layout`): payloads keep flowing through the bundle path.
    LegacyBundle {
        /// The bundle root, as given.
        dir: PathBuf,
    },
    /// A kurin package, opened and digest-verified on open.
    Package {
        /// Canonical URI the package was opened from (`oci-layout://…`
        /// or `file://…`) — what `[packages] base` / `ArtifactSources`
        /// record.
        uri: String,
        /// The opened package.
        package: PackageSource,
    },
}

impl std::fmt::Debug for PayloadSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PayloadSource::LegacyBundle { dir } => f
                .debug_struct("PayloadSource::LegacyBundle")
                .field("dir", dir)
                .finish(),
            // The opened package is not Debug; its URI identifies it.
            PayloadSource::Package { uri, .. } => f
                .debug_struct("PayloadSource::Package")
                .field("uri", uri)
                .finish_non_exhaustive(),
        }
    }
}

impl PayloadSource {
    /// The URI to record in the run's artifact sources: a package URI,
    /// or the bundle directory as given (the legacy `offline_dir`).
    pub fn source_string(&self) -> String {
        match self {
            PayloadSource::LegacyBundle { dir } => dir.display().to_string(),
            PayloadSource::Package { uri, .. } => uri.clone(),
        }
    }
}

/// Classify and open a payload source: a package URI, a package layout
/// directory or archive, or a legacy offline bundle directory (the
/// compatibility reader of 04 §4).
///
/// # Errors
///
/// [`PayloadError`] when the input names nothing usable: a missing path,
/// a directory that is neither a bundle nor a layout, a file that is not
/// a package archive, or an `oci://` source (needs `kurin package pull`
/// first).
pub fn open_payload_source(input: &str) -> Result<PayloadSource, PayloadError> {
    let input = input.trim();
    if let Some(scheme) = input.split_once("://").map(|(s, _)| s) {
        return match scheme {
            "oci-layout" | "file" => open_package(input),
            "oci" => Err(PayloadError::RegistryUnsupported {
                input: input.to_string(),
            }),
            other => Err(PayloadError::UnknownScheme {
                input: input.to_string(),
                scheme: other.to_string(),
            }),
        };
    }
    // A bare path: whatever it turns out to be on disk.
    let path = Path::new(input);
    if path.is_dir() {
        let bundle = path.join(BUNDLE_MANIFEST);
        let layout = path.join(OCI_LAYOUT_MARKER);
        return match (bundle.is_file(), layout.is_file()) {
            (true, false) => Ok(PayloadSource::LegacyBundle {
                dir: path.to_path_buf(),
            }),
            (false, true) => open_package(&layout_uri(path)),
            // A directory holding both is not a shape any kurin wrote:
            // refuse instead of guessing which one wins.
            (true, true) => Err(PayloadError::AmbiguousDirectory {
                path: path.to_path_buf(),
            }),
            (false, false) => Err(PayloadError::NotAPayloadDirectory {
                path: path.to_path_buf(),
            }),
        };
    }
    if path.is_file() {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        if name.ends_with(".tar") || name.ends_with(".tar.zst") {
            return open_package(&format!("file://{}", absolute(path).display()));
        }
        return Err(PayloadError::NotAPackageFile {
            path: path.to_path_buf(),
        });
    }
    Err(PayloadError::Missing {
        path: path.to_path_buf(),
    })
}

/// Open the package at `uri`.
fn open_package(uri: &str) -> Result<PayloadSource, PayloadError> {
    let package = PackageSource::open(uri).map_err(|e| PayloadError::Open {
        input: uri.to_string(),
        source: e,
    })?;
    // Fail here, not at first use: an index without the package metadata
    // manifest is a layout, but not a kurin package.
    package.manifest().map_err(|e| PayloadError::Open {
        input: uri.to_string(),
        source: e,
    })?;
    Ok(PayloadSource::Package {
        uri: uri.to_string(),
        package,
    })
}

/// `oci-layout://<absolute path>` — the URI form of a layout directory
/// given as a bare path.
fn layout_uri(path: &Path) -> String {
    format!("oci-layout://{}", absolute(path).display())
}

/// Absolute form of `path` without resolving symlinks (the source stays
/// spellable; opening follows links anyway).
fn absolute(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Failures of [`open_payload_source`].
#[derive(Debug, Error, miette::Diagnostic)]
pub enum PayloadError {
    /// `oci://` needs a local copy first.
    #[error(
        "{input}: remote registry packages are not opened directly; pull one first \
         (`kurin package pull`, WP-33) and pass the resulting oci-layout:// or file:// source"
    )]
    RegistryUnsupported { input: String },

    /// A URI scheme this build does not understand.
    #[error("{input}: unknown scheme {scheme:?} (expected oci-layout://, file:// or oci://)")]
    UnknownScheme { input: String, scheme: String },

    /// Path does not exist.
    #[error("{}: no such file or directory", path.display())]
    Missing { path: PathBuf },

    /// Directory without either marker.
    #[error(
        "{}: not a kurin payload source (no {BUNDLE_MANIFEST}, no {OCI_LAYOUT_MARKER}); \
         build one with `kurin artifact --out <dir>` (legacy bundle) or \
         `kurin package create base -o oci-layout://<dir>`",
        path.display()
    )]
    NotAPayloadDirectory { path: PathBuf },

    /// Directory that looks like a bundle and a layout at once.
    #[error(
        "{}: both {BUNDLE_MANIFEST} and {OCI_LAYOUT_MARKER} are present; \
         a source is either a legacy bundle or a package layout, never both",
        path.display()
    )]
    AmbiguousDirectory { path: PathBuf },

    /// File that is not a package archive.
    #[error(
        "{}: not a package (expected a .tar / .tar.zst archive, a package \
         layout directory or a legacy bundle directory)",
        path.display()
    )]
    NotAPackageFile { path: PathBuf },

    /// The package could not be opened or read.
    #[error("{input}: {source}")]
    Open {
        input: String,
        #[source]
        source: PackageError,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The smallest legacy bundle: just the manifest file.
    fn legacy_bundle(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(BUNDLE_MANIFEST), r#"{"version":1,"artifacts":[]}"#).unwrap();
    }

    /// The smallest package layout: marker + empty index.
    fn package_layout(dir: &Path) {
        crate::oci::Layout::create(dir).unwrap();
    }

    /// A real one-file base package, built into `dir` (layout form).
    fn tiny_package(dir: &Path) {
        use crate::artifact::{Arch, ArtifactKind};
        use crate::package::{PackageBuilder, PackageFile, PackageKind, PackageOutput};
        let payload = b"k";
        let digest = oci_spec::image::Digest::try_from(format!("sha256:{}", hex(payload))).unwrap();
        let cache = crate::store::BlobStore::open(dir.parent().unwrap().join("cache"));
        cache.put_bytes(payload).unwrap();
        let mut builder = PackageBuilder::new(PackageKind::Base, "base", "1.33.1", "kubeadm")
            .with_created("2026-10-28T00:00:00Z");
        builder.add_file(PackageFile {
            name: "kubeadm".into(),
            kind: ArtifactKind::Binary,
            version: "1.33.1".into(),
            arch: Arch::Amd64,
            digest,
            size: payload.len() as u64,
            title: "kubeadm".into(),
            upstream: "https://dl.k8s.io/kubeadm".into(),
        });
        builder
            .build(&cache, &PackageOutput::Layout(dir.to_path_buf()))
            .unwrap();
    }

    fn hex(bytes: &[u8]) -> String {
        use sha2::{Digest as _, Sha256};
        format!("{:x}", Sha256::digest(bytes))
    }

    #[test]
    fn a_legacy_bundle_directory_is_recognised() {
        let tmp = tempfile::tempdir().unwrap();
        legacy_bundle(tmp.path());
        match open_payload_source(tmp.path().to_str().unwrap()).unwrap() {
            PayloadSource::LegacyBundle { dir } => assert_eq!(dir, tmp.path()),
            other => panic!("expected a legacy bundle, got {other:?}"),
        }
    }

    #[test]
    fn a_layout_directory_opens_as_a_package() {
        let tmp = tempfile::tempdir().unwrap();
        tiny_package(&tmp.path().join("pkg"));
        let dir = tmp.path().join("pkg");
        match open_payload_source(dir.to_str().unwrap()).unwrap() {
            PayloadSource::Package { uri, .. } => {
                assert!(uri.starts_with("oci-layout://"), "{uri}");
                assert!(uri.ends_with("pkg"), "{uri}");
            }
            other => panic!("expected a package, got {other:?}"),
        }
    }

    #[test]
    fn a_tar_archive_opens_as_a_file_package() {
        use crate::artifact::{Arch, ArtifactKind};
        use crate::package::{PackageBuilder, PackageFile, PackageKind, PackageOutput};
        let tmp = tempfile::tempdir().unwrap();
        let tar = tmp.path().join("base.tar");
        let payload = b"k";
        let digest = oci_spec::image::Digest::try_from(format!("sha256:{}", hex(payload))).unwrap();
        let cache = crate::store::BlobStore::open(tmp.path().join("cache"));
        cache.put_bytes(payload).unwrap();
        let mut builder = PackageBuilder::new(PackageKind::Base, "base", "1.33.1", "kubeadm")
            .with_created("2026-10-28T00:00:00Z");
        builder.add_file(PackageFile {
            name: "kubeadm".into(),
            kind: ArtifactKind::Binary,
            version: "1.33.1".into(),
            arch: Arch::Amd64,
            digest,
            size: payload.len() as u64,
            title: "kubeadm".into(),
            upstream: "https://dl.k8s.io/kubeadm".into(),
        });
        builder
            .build(&cache, &PackageOutput::Tar(tar.clone()))
            .unwrap();
        match open_payload_source(tar.to_str().unwrap()).unwrap() {
            PayloadSource::Package { uri, .. } => assert!(uri.starts_with("file://"), "{uri}"),
            other => panic!("expected a package, got {other:?}"),
        }
    }

    #[test]
    fn package_uris_open_directly() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = tmp.path().join("pkg");
        tiny_package(&layout);
        let uri = format!("oci-layout://{}", layout.display());
        match open_payload_source(&uri).unwrap() {
            PayloadSource::Package { uri: got, .. } => assert_eq!(got, uri),
            other => panic!("expected a package, got {other:?}"),
        }
    }

    #[test]
    fn oci_registry_sources_are_rejected_with_a_pull_hint() {
        let err = open_payload_source("oci://harbor.local/kurin/base:v1").unwrap_err();
        assert!(err.to_string().contains("kurin package pull"), "{err}");
    }

    #[test]
    fn unknown_schemes_are_rejected() {
        let err = open_payload_source("http://example/base.tar").unwrap_err();
        assert!(err.to_string().contains("unknown scheme"), "{err}");
    }

    #[test]
    fn a_directory_without_markers_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let err = open_payload_source(tmp.path().to_str().unwrap()).unwrap_err();
        assert!(
            err.to_string().contains("not a kurin payload source"),
            "{err}"
        );
    }

    #[test]
    fn a_missing_path_is_rejected() {
        let err = open_payload_source("/nonexistent/kurin-bundle").unwrap_err();
        assert!(err.to_string().contains("no such file"), "{err}");
    }

    #[test]
    fn a_non_archive_file_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("payload.bin");
        std::fs::write(&file, b"bytes").unwrap();
        let err = open_payload_source(file.to_str().unwrap()).unwrap_err();
        assert!(err.to_string().contains("not a package"), "{err}");
    }

    #[test]
    fn a_layout_without_package_metadata_is_rejected() {
        // A valid OCI layout that is not a kurin package (no metadata
        // manifest): opening must fail, not return a broken handle.
        let tmp = tempfile::tempdir().unwrap();
        package_layout(&tmp.path().join("pkg"));
        let err = open_payload_source(&format!(
            "oci-layout://{}",
            tmp.path().join("pkg").display()
        ))
        .unwrap_err();
        assert!(err.to_string().contains("is not a kurin package"), "{err}");
    }
}
