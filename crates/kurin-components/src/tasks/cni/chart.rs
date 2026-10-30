//! Shared pieces of the chart-rendered CNI installs (WP-48): resolving
//! the pinned chart archive, the rendered-image integrity check (03
//! §4.2 rule 1, first-layer edition) and the legacy-install probes
//! that decide `--take-ownership`.

use std::collections::BTreeSet;

use kurin_artifact::artifact::{install_manifest, Arch};
use kurin_core::run::ArtifactSources;
use kurin_core::zone::rewrite_image_ref;
use kurin_kube::{ClusterApi, DynamicObject, Gvk, ObjRef};
use miette::Result;

use crate::addon::apply::resolve_file_payload;

/// The pinned chart archive for manifest entry `name` (e.g.
/// `cilium-chart`), resolved from the run's payload set: online cache,
/// offline bundle or base package — the same sources the install tasks
/// use, so air-gapped installs carry the chart like any other payload.
pub(super) async fn chart_payload(
    sources: &ArtifactSources,
    name: &str,
    arch: Arch,
) -> Result<(String, Vec<u8>)> {
    let file = resolve_file_payload(sources, name, arch)
        .await
        .map_err(|e| miette::miette!("{e}"))?;
    Ok((file.file_name, file.bytes))
}

/// Every `image:` reference the rendered objects carry.
pub(super) fn rendered_images(objects: &[DynamicObject]) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for object in objects {
        if let Ok(value) = serde_json::to_value(object) {
            collect(&value, &mut out);
        }
    }
    out
}

fn collect(value: &serde_json::Value, out: &mut BTreeSet<String>) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, item) in map {
                if key == "image" {
                    if let Some(reference) = item.as_str() {
                        out.insert(reference.to_string());
                    }
                }
                collect(item, out);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                collect(item, out);
            }
        }
        _ => {}
    }
}

/// Image-integrity gate for the first layer: every rendered reference
/// must be one of the manifest-pinned images in `names` — the upstream
/// reference or, in the CN zone, its mirror rewrite — and a rendered
/// digest must equal the pin. `what` names the CNI in diagnostics.
pub(super) fn check_rendered_images(
    sources: &ArtifactSources,
    arch: Arch,
    names: &[&str],
    images: &BTreeSet<String>,
    what: &str,
) -> Result<()> {
    let zone = sources.zone;
    let manifest = install_manifest(sources)?;
    let mut allowed: Vec<(String, Option<String>)> = Vec::new();
    for name in names {
        let spec = manifest.spec(name, arch)?;
        let upstream = spec.url();
        allowed.push((upstream.clone(), spec.expected_sha256()));
        let mirrored = rewrite_image_ref(&upstream, zone);
        if mirrored != upstream {
            allowed.push((mirrored, spec.expected_sha256()));
        }
    }
    for image in images {
        let covered = allowed.iter().any(|(reference, digest)| {
            let (repo, tag, rendered_digest) = split(image);
            let (want_repo, want_tag) = split_tag(reference);
            // Manifest pins are bare hex; rendered refs carry `sha256:`.
            let rendered_digest = rendered_digest.map(|d| d.strip_prefix("sha256:").unwrap_or(d));
            repo == want_repo
                && tag == want_tag
                && rendered_digest.is_none_or(|d| Some(d) == digest.as_deref())
        });
        if !covered {
            return Err(miette::miette!(
                "{what} renders image {image}, which the artifact manifest does not pin; \
                 a CNI the offline set cannot carry would half-install"
            ));
        }
    }
    Ok(())
}

/// `<repo>[:tag][@digest]` → repo, tag, digest.
fn split(reference: &str) -> (&str, Option<&str>, Option<&str>) {
    let (rest, digest) = match reference.split_once('@') {
        Some((rest, digest)) => (rest, Some(digest)),
        None => (reference, None),
    };
    let (repo, tag) = split_tag(rest);
    (repo, tag, digest)
}

/// `<repo>[:tag]` → repo, tag.
fn split_tag(reference: &str) -> (&str, Option<&str>) {
    match reference.rfind(':') {
        Some(at) if !reference[at + 1..].contains('/') => {
            (&reference[..at], Some(&reference[at + 1..]))
        }
        _ => (reference, None),
    }
}

/// Whether a helm release named `release` exists in `kube-system`
/// (helm's secret storage driver labels its records `owner=helm`).
pub(super) async fn release_exists(api: &dyn ClusterApi, release: &str) -> Result<bool> {
    let selector = format!("owner=helm,name={release}");
    let secrets = api
        .list_json(
            &Gvk::core("v1", "Secret"),
            Some("kube-system"),
            Some(&selector),
        )
        .await?;
    Ok(!secrets.is_empty())
}

/// Whether the legacy (non-helm) install left `reference` behind — the
/// takeover signal: resources exist that no release owns.
pub(super) async fn legacy_object_exists(api: &dyn ClusterApi, reference: &ObjRef) -> Result<bool> {
    Ok(api.get_json(reference).await?.is_some())
}

/// The takeover decision (WP-48): adopt exactly when the resources
/// exist but the release does not — a fresh install adopts nothing, an
/// existing release upgrades in place.
pub(super) fn take_ownership(release_exists: bool, legacy_exists: bool) -> bool {
    !release_exists && legacy_exists
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn takeover_happens_only_for_unowned_resources() {
        assert!(take_ownership(false, true));
        assert!(!take_ownership(true, true));
        assert!(!take_ownership(false, false));
        assert!(!take_ownership(true, false));
    }

    #[test]
    fn rendered_image_matching_covers_digest_and_mirror_forms() {
        let _sources = ArtifactSources::embedded();
        let pinned = "quay.io/cilium/cilium:v1.20.4";
        let digest = "20fe92e4c86641233a450098c158d2d29085f3419931ab6bf681d2485dedf16e";
        let allowed = [(pinned.to_string(), Some(digest.to_string()))];
        let ok = |image: &str| {
            allowed.iter().any(|(reference, digest)| {
                let (repo, tag, rendered) = split(image);
                let (want_repo, want_tag) = split_tag(reference);
                let rendered = rendered.map(|d| d.strip_prefix("sha256:").unwrap_or(d));
                repo == want_repo
                    && tag == want_tag
                    && rendered.is_none_or(|d| Some(d) == digest.as_deref())
            })
        };
        assert!(ok("quay.io/cilium/cilium:v1.20.4"));
        assert!(ok(&format!("quay.io/cilium/cilium:v1.20.4@{digest}")));
        assert!(ok(&format!(
            "quay.io/cilium/cilium:v1.20.4@sha256:{digest}"
        )));
        assert!(!ok("quay.io/cilium/cilium:v1.20.4@sha256:deadbeef"));
        assert!(!ok("quay.io/cilium/cilium:v1.20.5"));
        assert!(!ok("quay.io/other/cilium:v1.20.4"));
    }
}
