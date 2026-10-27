//! Addon packaging (WP-45, 04 §3.4 kind `addon`, 03 §4.7).
//!
//! `kurin package create addon -f addons/<name>/addon.yaml` turns one
//! addon directory into an OCI package that installs offline:
//!
//! - every addon file (`addon.yaml`, values templates, schema, README,
//!   local charts) becomes a file-set layer, titled with its
//!   package-relative path;
//! - an `oci://` chart is pulled into the package and `addon.yaml` is
//!   rewritten to point at the in-package archive (an air-gapped install
//!   must never reach a registry for the chart);
//! - every `spec.images` reference is pulled at its pinned digest — the
//!   index (or single-platform manifest) plus the selected architectures'
//!   sub-manifests and blobs — so the image set is closed.
//!
//! The same payload builder feeds `kurin package create base --addons`:
//! the addon's files land under `addons/<name>/`, and its images join the
//! base package's image list, so one base package installs the selected
//! addons offline ([`crate::addon::load::Addon::from_base_package`]).
//!
//! Validation is the WP-40 static rule set: an addon that would install
//! half-way (an undeclared image, a missing chart, a broken schema) is
//! rejected before anything is written.

use std::path::Path;

use kurin_artifact::artifact::{Arch, ArtifactKind, ArtifactStore};
use kurin_artifact::oci::layout::Layout;
use kurin_artifact::oci::remote;
use kurin_artifact::package::{
    PackageBuilder, PackageChart, PackageComponent, PackageError, PackageExtras, PackageFile,
    PackageImage, PackageKind, PackageOutput, PackageReport,
};
use kurin_artifact::transfer::walk_closure;
use kurin_artifact::zone::Zone;
use oci_spec::image::{Digest, ImageIndex, ImageManifest};
use thiserror::Error;

use crate::addon::error::AddonError;
use crate::addon::load::Addon;
use crate::addon::spec::ADDON_FILE;

/// Media-type fragment of a Helm chart payload layer.
const HELM_CHART_LAYER: &str = "helm.chart";

/// What an addon package covers.
#[derive(Debug, Clone, Default)]
pub struct AddonPackageOptions {
    /// Architectures whose image sub-manifests to carry. Empty uses the
    /// addon's declared `spec.compat.arch`, else the host architecture.
    pub arches: Vec<Arch>,
}

/// Build one addon package (04 §3.4 kind `addon`).
///
/// `source` is the addon directory or its `addon.yaml` (the CLI's
/// `-f addons/<name>/addon.yaml`); `out` is any [`PackageOutput`] form.
///
/// # Errors
///
/// [`AddonPackageError`] when the addon does not load or validate, a
/// chart or image cannot be pulled, an image does not cover a requested
/// architecture, or the package cannot be written.
pub async fn create_addon(
    store: &ArtifactStore,
    source: &str,
    opts: &AddonPackageOptions,
    zone: Zone,
    out: &PackageOutput,
) -> Result<PackageReport, AddonPackageError> {
    let addon = load_addon(source)?;
    let diagnostics = addon.validate();
    if !diagnostics.is_empty() {
        return Err(AddonPackageError::Invalid {
            addon: addon.name().to_string(),
            diagnostics: diagnostics.iter().map(|d| d.summary()).collect(),
        });
    }
    let arches = if opts.arches.is_empty() {
        default_arches(&addon)?
    } else {
        opts.arches.clone()
    };
    let extras = addon_extras(store, &addon, &arches, zone, "").await?;
    let version = addon.manifest().metadata.version.to_string();
    let mut builder = PackageBuilder::new(PackageKind::Addon, addon.name(), &version, "addon")
        .with_tag(version.clone())
        .with_arches(arches);
    builder.add_component(format!("addon/{}", addon.name()), version);
    for file in extras.files {
        builder.add_file(file);
    }
    for image in extras.images {
        builder.add_image(image);
    }
    for chart in extras.charts {
        builder.add_chart(chart);
    }
    Ok(builder.build(store.blob_store(), out)?)
}

/// Load an addon from a directory, an `addon.yaml` path, or an addon
/// package source (`dir://`, `oci-layout://`, `file://`).
fn load_addon(source: &str) -> Result<Addon, AddonError> {
    let path = Path::new(source);
    if path.is_file() && path.file_name().is_some_and(|name| name == ADDON_FILE) {
        return Addon::from_dir(path.parent().unwrap_or_else(|| Path::new(".")));
    }
    Addon::from_source(source)
}

/// The arches an addon package covers by default: the addon's declared
/// compatibility list, else the host architecture.
fn default_arches(addon: &Addon) -> Result<Vec<Arch>, AddonPackageError> {
    let declared = &addon.manifest().spec.compat.arch;
    if !declared.is_empty() {
        return declared
            .iter()
            .map(|a| {
                Arch::parse(a.as_str()).ok_or_else(|| AddonPackageError::NoArch {
                    detail: format!("unknown architecture `{}`", a.as_str()),
                })
            })
            .collect();
    }
    Arch::host()
        .map(|arch| vec![arch])
        .map_err(|e| AddonPackageError::NoArch {
            detail: e.to_string(),
        })
}

/// Build the payload one addon contributes to a package: its files
/// (namespaced under `prefix`), its chart rows and its images. Blobs are
/// imported into `store`'s content-addressed cache, ready for
/// [`PackageBuilder::build`].
///
/// `prefix` is empty for a standalone addon package and `addons/<name>/`
/// when embedding the addon in a base package.
pub async fn addon_extras(
    store: &ArtifactStore,
    addon: &Addon,
    arches: &[Arch],
    zone: Zone,
    prefix: &str,
) -> Result<PackageExtras, AddonPackageError> {
    let first_arch = *arches.first().ok_or_else(|| AddonPackageError::NoArch {
        detail: "pass --arch (e.g. --arch amd64,arm64)".to_string(),
    })?;
    let mut manifest = addon.manifest().clone();
    let version = manifest.metadata.version.to_string();
    let mut extras = PackageExtras {
        component: Some(PackageComponent {
            id: format!("addon/{}", addon.name()),
            version: version.clone(),
        }),
        ..PackageExtras::default()
    };

    // 1. The chart: pull an `oci://` chart into the package and rewrite
    //    addon.yaml to the in-package path; a local chart is already one
    //    of the addon's files.
    if let Some(helm) = manifest.spec.install.helm().cloned() {
        if helm.chart.starts_with("oci://") {
            let chart = pull_chart(store, &helm.chart, zone).await?;
            let rel = format!("charts/{}", chart.file_name);
            if let Some(helm) = manifest.spec.install.helm.as_mut() {
                helm.chart = rel.clone();
            }
            extras.files.push(file_from_bytes(
                store,
                &format!("{prefix}{rel}"),
                &chart.bytes,
                &version,
                first_arch,
                &helm.chart,
            )?);
            extras.charts.push(PackageChart {
                name: chart.name,
                version: chart.version,
                digest: chart.digest,
            });
        } else {
            let bytes = addon.files().read(&helm.chart)?;
            let digest = store.blob_store().put_bytes(&bytes)?;
            let (name, chart_version) = chart_identity_from_path(&helm.chart, &version);
            extras.charts.push(PackageChart {
                name,
                version: chart_version,
                digest: digest.to_string(),
            });
        }
    }

    // 2. addon.yaml (with the rewritten chart path).
    let yaml = serde_yml::to_string(&manifest).map_err(|e| AddonPackageError::Yaml {
        detail: format!("serialize addon.yaml: {e}"),
    })?;
    extras.files.push(file_from_bytes(
        store,
        &format!("{prefix}{ADDON_FILE}"),
        yaml.as_bytes(),
        &version,
        first_arch,
        &addon.origin(),
    )?);

    // 3. Every other addon file, verbatim.
    for rel in addon.files().list() {
        if rel == ADDON_FILE {
            continue;
        }
        let bytes = addon.files().read(&rel)?;
        extras.files.push(file_from_bytes(
            store,
            &format!("{prefix}{rel}"),
            &bytes,
            &version,
            first_arch,
            &addon.origin(),
        )?);
    }

    // 4. Every declared image, at its pinned digest.
    for reference in &manifest.spec.images {
        extras
            .images
            .push(pull_image(store, reference, addon.name(), arches, zone).await?);
    }
    Ok(extras)
}

/// One addon file as a package file-set entry.
fn file_from_bytes(
    store: &ArtifactStore,
    title: &str,
    bytes: &[u8],
    version: &str,
    arch: Arch,
    upstream: &str,
) -> Result<PackageFile, AddonPackageError> {
    let digest = store.blob_store().put_bytes(bytes)?;
    Ok(PackageFile {
        name: title.to_string(),
        kind: ArtifactKind::Manifest,
        version: version.to_string(),
        arch,
        digest,
        size: bytes.len() as u64,
        title: title.to_string(),
        upstream: upstream.to_string(),
    })
}

/// A pulled Helm chart.
struct PulledChart {
    file_name: String,
    name: String,
    version: String,
    digest: String,
    bytes: Vec<u8>,
}

/// Pull an `oci://` chart artifact and return its payload archive plus
/// its identity from the chart config blob.
async fn pull_chart(
    store: &ArtifactStore,
    reference: &str,
    _zone: Zone,
) -> Result<PulledChart, AddonPackageError> {
    let temp = tempfile::tempdir()?;
    let layout = Layout::create(temp.path())?;
    let root = remote::pull_artifact(reference, &layout)
        .await
        .map_err(|e| AddonPackageError::Pull {
            reference: reference.to_string(),
            detail: e.to_string(),
        })?;
    import_closure(store, &layout, &root)?;

    let root_bytes = read_blob(&layout, root.digest())?;
    let manifest: ImageManifest =
        serde_json::from_slice(&root_bytes).map_err(|e| AddonPackageError::Pull {
            reference: reference.to_string(),
            detail: format!("chart manifest: {e}"),
        })?;
    let layer = manifest
        .layers()
        .iter()
        .find(|layer| layer.media_type().as_ref().contains(HELM_CHART_LAYER))
        .or_else(|| manifest.layers().first())
        .ok_or_else(|| AddonPackageError::Pull {
            reference: reference.to_string(),
            detail: "the chart artifact carries no layer".to_string(),
        })?;
    let bytes = read_blob(&layout, layer.digest())?;

    // The chart's config is `{"name": …, "version": …}` (Helm's OCI
    // config media type); fall back to the reference's tail.
    let config_bytes = read_blob(&layout, manifest.config().digest()).unwrap_or_default();
    let config: serde_json::Value = serde_json::from_slice(&config_bytes).unwrap_or_default();
    let fallback = reference.rsplit('/').next().unwrap_or("chart");
    let name = config
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or(fallback)
        .to_string();
    let version = config
        .get("version")
        .and_then(|v| v.as_str())
        .unwrap_or("0.0.0")
        .to_string();
    Ok(PulledChart {
        file_name: format!("{name}-{version}.tgz"),
        name,
        version,
        digest: layer.digest().to_string(),
        bytes,
    })
}

/// Pull one digest-pinned addon image for every requested architecture.
async fn pull_image(
    store: &ArtifactStore,
    reference: &str,
    addon: &str,
    arches: &[Arch],
    zone: Zone,
) -> Result<PackageImage, AddonPackageError> {
    let pull_reference = kurin_artifact::zone::rewrite_image_ref(reference, zone);
    let temp = tempfile::tempdir()?;
    let layout = Layout::create(temp.path())?;
    let root = remote::pull_artifact(&pull_reference, &layout)
        .await
        .map_err(|e| AddonPackageError::Pull {
            reference: reference.to_string(),
            detail: e.to_string(),
        })?;
    import_closure(store, &layout, &root)?;

    let root_bytes = read_blob(&layout, root.digest())?;
    let is_index = serde_json::from_slice::<serde_json::Value>(&root_bytes)
        .ok()
        .and_then(|v| {
            v.get("manifests")
                .and_then(|m| m.as_array())
                .map(|m| !m.is_empty())
        })
        .unwrap_or(false);

    let mut blobs: Vec<Digest> = Vec::new();
    let mut covered: Vec<Arch> = Vec::new();
    if is_index {
        let index: ImageIndex =
            serde_json::from_slice(&root_bytes).map_err(|e| AddonPackageError::Pull {
                reference: reference.to_string(),
                detail: format!("image index: {e}"),
            })?;
        for arch in arches {
            let entry = index
                .manifests()
                .iter()
                .find(|d| {
                    d.platform().as_ref().is_some_and(|p| {
                        p.architecture().to_string() == arch.as_str()
                            && p.os().to_string() == "linux"
                    })
                })
                .ok_or_else(|| AddonPackageError::MissingPlatform {
                    reference: reference.to_string(),
                    arch: arch.as_str().to_string(),
                })?;
            blobs.push(entry.digest().clone());
            let manifest_bytes = read_blob(&layout, entry.digest())?;
            let manifest: ImageManifest =
                serde_json::from_slice(&manifest_bytes).map_err(|e| AddonPackageError::Pull {
                    reference: reference.to_string(),
                    detail: format!("image manifest: {e}"),
                })?;
            blobs.push(manifest.config().digest().clone());
            for layer in manifest.layers() {
                blobs.push(layer.digest().clone());
            }
            covered.push(*arch);
        }
    } else {
        let manifest: ImageManifest =
            serde_json::from_slice(&root_bytes).map_err(|e| AddonPackageError::Pull {
                reference: reference.to_string(),
                detail: format!("image manifest: {e}"),
            })?;
        let config_bytes = read_blob(&layout, manifest.config().digest())?;
        let config: serde_json::Value = serde_json::from_slice(&config_bytes).unwrap_or_default();
        let arch = config
            .get("architecture")
            .and_then(|v| v.as_str())
            .and_then(Arch::parse)
            .ok_or_else(|| AddonPackageError::MissingPlatform {
                reference: reference.to_string(),
                arch: arches
                    .iter()
                    .map(|a| a.as_str())
                    .collect::<Vec<_>>()
                    .join(","),
            })?;
        if !arches.contains(&arch) {
            return Err(AddonPackageError::MissingPlatform {
                reference: reference.to_string(),
                arch: arch.as_str().to_string(),
            });
        }
        blobs.push(manifest.config().digest().clone());
        for layer in manifest.layers() {
            blobs.push(layer.digest().clone());
        }
        covered.push(arch);
    }

    Ok(PackageImage {
        reference: reference.to_string(),
        digest: root.digest().clone(),
        size: root.size(),
        media_type: root.media_type().as_ref().to_string(),
        arch: covered,
        used_by: vec![format!("addon/{addon}")],
        blobs,
    })
}

/// Import every manifest and blob of a pulled artifact into the cache
/// store, so the package builder can link them into the target layout.
fn import_closure(
    store: &ArtifactStore,
    layout: &Layout,
    root: &oci_spec::image::Descriptor,
) -> Result<(), AddonPackageError> {
    let closure = walk_closure(layout, root)?;
    for manifest in &closure.manifests {
        store.blob_store().put_bytes(&manifest.bytes)?;
    }
    for blob in &closure.blobs {
        let mut reader = layout.blobs().open_blob(blob.digest())?;
        store.blob_store().put_stream(&mut reader)?;
    }
    Ok(())
}

/// Read one blob out of a pulled layout.
fn read_blob(layout: &Layout, digest: &Digest) -> Result<Vec<u8>, AddonPackageError> {
    use std::io::Read as _;
    let mut bytes = Vec::new();
    layout.blobs().open_blob(digest)?.read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// Chart name/version from a local chart path
/// (`charts/metrics-server-3.13.0.tgz` → `metrics-server`, `3.13.0`).
fn chart_identity_from_path(path: &str, addon_version: &str) -> (String, String) {
    let stem = path
        .rsplit('/')
        .next()
        .unwrap_or(path)
        .trim_end_matches(".tgz")
        .trim_end_matches(".tar.gz");
    match stem.rsplit_once('-') {
        Some((name, version)) if semver::Version::parse(version).is_ok() && !name.is_empty() => {
            (name.to_string(), version.to_string())
        }
        _ => (stem.to_string(), addon_version.to_string()),
    }
}

/// Errors raised while packaging an addon (WP-45).
#[derive(Debug, Error, miette::Diagnostic)]
pub enum AddonPackageError {
    /// The addon does not load or parse.
    #[error(transparent)]
    Addon(#[from] AddonError),

    /// The addon violates a static rule: packaging would ship it broken.
    #[error("addon `{addon}` is not packageable:\n  - {}", diagnostics.join("\n  - "))]
    Invalid {
        /// The addon's name.
        addon: String,
        /// One line per violation.
        diagnostics: Vec<String>,
    },

    /// A chart or image could not be pulled.
    #[error("cannot pull {reference}: {detail}")]
    Pull {
        /// The reference as the addon declared it.
        reference: String,
        /// What failed.
        detail: String,
    },

    /// The image does not carry one of the requested architectures.
    #[error("image {reference} does not cover {arch}")]
    MissingPlatform {
        /// The image reference.
        reference: String,
        /// The architecture the package was asked to cover.
        arch: String,
    },

    /// No architecture to package for.
    #[error("no architecture to package: {detail}")]
    NoArch {
        /// What the caller should do.
        detail: String,
    },

    /// `addon.yaml` could not be re-serialized.
    #[error("{detail}")]
    Yaml {
        /// What failed.
        detail: String,
    },

    /// The package builder rejected the payload.
    #[error(transparent)]
    Package(#[from] PackageError),

    /// The content-addressed cache rejected a write.
    #[error(transparent)]
    Store(#[from] kurin_artifact::store::StoreError),

    /// A temporary OCI layout could not be created.
    #[error(transparent)]
    Layout(#[from] kurin_artifact::oci::layout::LayoutError),

    /// Local I/O (temporary layouts).
    #[error("package I/O error: {message}")]
    Io {
        /// What failed.
        message: String,
        #[source]
        source: std::io::Error,
    },
}

impl From<std::io::Error> for AddonPackageError {
    fn from(source: std::io::Error) -> Self {
        AddonPackageError::Io {
            message: source.to_string(),
            source,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chart_identity_comes_from_the_archive_name() {
        assert_eq!(
            chart_identity_from_path("charts/metrics-server-3.13.0.tgz", "0.8.0"),
            ("metrics-server".to_string(), "3.13.0".to_string())
        );
        assert_eq!(
            chart_identity_from_path("charts/weird.tgz", "0.8.0"),
            ("weird".to_string(), "0.8.0".to_string())
        );
    }
}
