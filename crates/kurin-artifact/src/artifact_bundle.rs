//! Offline bundle manifest: persist the dynamic [`ArtifactManifest`] a
//! bundle was built with, inside the bundle itself (`kurin-manifest.json`
//! at the bundle root). `kurin artifact --with-kubernetes vX.Y.Z` writes
//! it; `kurin upgrade --offline <dir>` reads the *same* pins back, so an
//! air-gapped upgrade never re-resolves versions or checksums and never
//! falls back to the embedded manifest of a different release.

use std::path::{Path, PathBuf};

use miette::{IntoDiagnostic, Result};

use crate::artifact::ArtifactManifest;

/// File name of the manifest written at the bundle root.
pub const BUNDLE_MANIFEST: &str = "kurin-manifest.json";

/// Write `manifest` as JSON to `<dir>/kurin-manifest.json`; returns the
/// written path. Pretty-printed so inspecting or diffing a bundle stays
/// readable.
pub fn write_bundle_manifest(dir: &Path, manifest: &ArtifactManifest) -> Result<PathBuf> {
    let path = dir.join(BUNDLE_MANIFEST);
    let mut json = serde_json::to_string_pretty(manifest).into_diagnostic()?;
    json.push('\n');
    std::fs::write(&path, json).into_diagnostic()?;
    Ok(path)
}

/// Read the manifest back. A bundle without one was built by an older
/// `kurin artifact` that never recorded its pins, so its target version is
/// unknowable — refuse to guess and point at the rebuild instead.
pub fn read_bundle_manifest(dir: &Path) -> Result<ArtifactManifest> {
    let path = dir.join(BUNDLE_MANIFEST);
    let json = std::fs::read_to_string(&path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            miette::miette!(
                "kurin-manifest.json missing in bundle; rebuild with `kurin artifact --with-kubernetes vX.Y.Z --out <dir>`"
            )
        } else {
            miette::miette!("read {}: {e}", path.display())
        }
    })?;
    ArtifactManifest::from_json(&json).map_err(|e| miette::miette!("parse {}: {e}", path.display()))
}
