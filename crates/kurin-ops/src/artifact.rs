//! `kurin artifact`: build an offline bundle for the requested arch(es).

use std::path::PathBuf;

use miette::{IntoDiagnostic, Result};

/// Build the bundle in `out`. `arch` is a comma-separated list; the default
/// is the control machine's arch.
pub(crate) async fn build(
    out: PathBuf,
    arch: Option<String>,
    with_kubernetes: Option<String>,
) -> Result<()> {
    let zone = crate::resolve_zone(None)?;
    let arches: Vec<kurin_core::arch::Arch> = match arch.as_deref() {
        Some(list) => list
            .split(',')
            .map(|a| a.trim())
            .filter(|a| !a.is_empty())
            .map(|a| match a {
                "amd64" | "x86_64" => Ok(kurin_core::arch::Arch::Amd64),
                "arm64" | "aarch64" => Ok(kurin_core::arch::Arch::Arm64),
                other => Err(miette::miette!("unknown arch {other:?} (amd64|arm64)")),
            })
            .collect::<miette::Result<Vec<_>>>()?,
        None => vec![kurin_core::arch::Arch::host()?],
    };
    let mut arches = arches;
    arches.dedup();
    for arch in arches {
        let manifest = match &with_kubernetes {
            Some(v) => {
                let target: semver::Version = v
                    .trim_start_matches('v')
                    .parse()
                    .map_err(|e| miette::miette!("--with-kubernetes {v:?}: {e}"))?;
                kurin_artifact::dynamic_manifest::k8s_manifest_for(&target, arch, zone).await?
            }
            None => kurin_artifact::artifact::ArtifactManifest::embedded()?,
        };
        let store = kurin_artifact::artifact::ArtifactStore::open_default()?;
        std::fs::create_dir_all(&out).into_diagnostic()?;
        // The bundle content is the component registry's aggregation (the
        // offline image set is no longer hand-maintained); the manifest
        // itself still resolves versions and pins.
        for name in kurin_components::component::bundle_artifacts(arch)
            .into_iter()
            .map(|r| r.name)
        {
            let spec = manifest.spec(&name, arch)?;
            let dir = store.artifact_dir(&spec);
            std::fs::create_dir_all(&dir).into_diagnostic()?;
            let dest = store.fetch(&spec, zone).await?;
            // Copy the artifact directory into the bundle layout.
            let target = out.join(format!("{}-{}-{}", spec.name, spec.version, arch.as_str()));
            std::fs::create_dir_all(&target).into_diagnostic()?;
            for f in std::fs::read_dir(&dir).into_diagnostic()? {
                let f = f.into_diagnostic()?;
                let name = f.file_name();
                if name.to_string_lossy().ends_with(".part") {
                    continue;
                }
                std::fs::copy(f.path(), target.join(&name)).into_diagnostic()?;
            }
            eprintln!("bundled {} {} ({})", spec.name, spec.version, arch.as_str());
            let _ = &dest;
        }
        // Sidecar checksums are included by the copy above; the dir name
        // layout is exactly what ArtifactManifest::from_dir expects.
        kurin_artifact::artifact_bundle::write_bundle_manifest(&out, &manifest)
            .map_err(|e| miette::miette!("bundle manifest: {e}"))?;
    }
    eprintln!("offline bundle written to {}", out.display());
    Ok(())
}
