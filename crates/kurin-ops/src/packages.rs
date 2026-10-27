//! Deploy-side payload sources (WP-34, 04 §4): resolve what one run's
//! payloads come from, keep legacy offline bundles working through the
//! same interface, and validate a base package covers the cluster.
//!
//! Resolution happens before the run context is built (its
//! [`ArtifactSources`] record the outcome); validation happens after
//! `discover`, when the node architectures are known. Installing *from*
//! a package (preload or registry delivery) is WP-35/36: this build
//! validates and plans, and refuses to silently fall back to online
//! downloads for a run that selected a package.

use std::path::PathBuf;

use kurin_artifact::deploy::{open_payload_source, PayloadSource};
use kurin_config::quick::is_os_package;
use kurin_config::Cluster;
use kurin_core::facts::FactsTable;
use miette::Result;

/// What the run's payloads come from, after the config and the CLI
/// overrides resolved through the compatibility reader.
pub(crate) enum Payload {
    /// No offline source: fetch from the network as before.
    Online,
    /// A legacy offline bundle directory — the pre-package path, with
    /// unchanged behaviour (the canonical `packages.offline-dir`
    /// carries the directory).
    Legacy,
    /// A kurin package (`oci://`, `oci-layout://`, `file://`), opened and
    /// digest-verified once per process.
    Package {
        uri: String,
        package: std::sync::Arc<kurin_artifact::package::PackageSource>,
    },
}

impl std::fmt::Debug for Payload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Payload::Online => f.write_str("Payload::Online"),
            Payload::Legacy => f.write_str("Payload::Legacy"),
            Payload::Package { uri, .. } => {
                write!(f, "Payload::Package({uri})")
            }
        }
    }
}

/// Resolve the run's payload source (WP-34 step 1/2): `[artifacts]
/// offline-dir` and `[packages] base` — plus whatever `--offline`
/// recorded — lower into one canonical form. A legacy bundle directory
/// (`kurin-manifest.json`, no `oci-layout`) is recognised by the
/// compatibility reader and stays on the bundle path; a package (or a
/// bare path that turns out to be one) is opened and recorded as the
/// base package URI.
///
/// `spec` is canonicalised in place so [`crate::sources_for`] sees one
/// of `offline_dir` (legacy) or `base` (package), never both.
///
/// `oci://` sources are pulled through the shared package opener (WP-35),
/// so the registry is read once per run and the install tasks stream from
/// the verified local copy.
pub(crate) async fn resolve_payload(spec: &mut Cluster) -> Result<Payload> {
    let offline = spec.packages.offline_dir.clone();
    let base = spec.packages.base.clone();
    let input = match (offline, base) {
        (Some(_), Some(uri)) => {
            return Err(miette::miette!(
                "both an offline bundle directory and the base package {uri:?} are selected; \
                 keep exactly one payload source"
            ));
        }
        (Some(dir), None) => dir.display().to_string(),
        (None, Some(uri)) => uri,
        (None, None) => return Ok(Payload::Online),
    };
    // A bare directory is the only input the compatibility reader has to
    // classify (legacy bundle vs layout); everything else — `oci://`,
    // `oci-layout://`, `file://X.tar[.zst]`, a bare archive path — opens
    // through the shared package cache, so the install tasks reuse this
    // exact package instead of unpacking or pulling it per node.
    let is_dir = !input.contains("://") && std::path::Path::new(input.trim()).is_dir();
    if is_dir {
        match open_payload_source(&input).map_err(|e| miette::miette!("{e}"))? {
            PayloadSource::LegacyBundle { dir } => {
                spec.packages.offline_dir = Some(dir.clone());
                spec.packages.base = None;
                return Ok(Payload::Legacy);
            }
            PayloadSource::Package { uri, .. } => {
                let package = kurin_artifact::preload::open_package(&uri)
                    .await
                    .map_err(|e| miette::miette!("{e}"))?;
                spec.packages.offline_dir = None;
                spec.packages.base = Some(uri.clone());
                return Ok(Payload::Package { uri, package });
            }
        }
    }
    let package = kurin_artifact::preload::open_package(&input)
        .await
        .map_err(|e| miette::miette!("{e}"))?;
    spec.packages.offline_dir = None;
    spec.packages.base = Some(input.clone());
    Ok(Payload::Package {
        uri: input,
        package,
    })
}

/// Apply `--offline DIR|URI` (WP-34 step 4): equivalent to `--package`.
///
/// A URI selects package sources (an `/os/` path segment files it under
/// `packages.os`, everything else is the base package) and replaces any
/// payload source the config declared. A bare path keeps the legacy
/// meaning — whatever the compatibility reader finds there — and is
/// resolved by [`resolve_payload`].
pub(crate) fn apply_offline_override(spec: &mut Cluster, value: &str) {
    let value = value.trim();
    if !value.contains("://") {
        spec.packages.base = None;
        spec.packages.offline_dir = Some(PathBuf::from(value));
        return;
    }
    spec.packages.offline_dir = None;
    if is_os_package(value) {
        if !spec.packages.os.iter().any(|uri| uri == value) {
            spec.packages.os.push(value.to_string());
        }
    } else {
        spec.packages.base = Some(value.to_string());
    }
}

/// The delivery strategy this run would use (04 §5.5): an explicit
/// `[packages] delivery` wins, `auto` picks `registry` when the cluster
/// configures one (a `role = "registry"` node or `kind = "external"`) and
/// `preload` otherwise.
pub(crate) fn effective_delivery(spec: &Cluster) -> kurin_config::v1alpha2::Delivery {
    use kurin_config::v1alpha2::Delivery;
    match spec.packages.delivery {
        Delivery::Auto => {
            let registry = spec.registry.kind.is_some()
                || spec
                    .nodes
                    .iter()
                    .any(|n| n.role == kurin_config::NodeRole::Registry);
            if registry {
                Delivery::Registry
            } else {
                Delivery::Preload
            }
        }
        other => other,
    }
}

/// Validate the base package against the cluster it is about to install
/// (WP-34 step 3), then enforce the delivery boundary: preload delivery
/// (WP-35) executes from the package, registry delivery still refuses
/// instead of silently fetching online (WP-36).
///
/// `mutating` is false for `--dry-run` runs, which plan and validate
/// without touching a node.
pub(crate) fn validate_payload(
    spec: &Cluster,
    facts: &FactsTable,
    payload: &Payload,
    mutating: bool,
) -> Result<()> {
    let Payload::Package { uri, package } = payload else {
        return Ok(());
    };
    let meta = package
        .manifest()
        .map_err(|e| miette::miette!("{uri}: {e}"))?;
    let gaps = kurin_components::component::coverage::coverage_gaps(spec, facts, &meta);
    if !gaps.is_empty() {
        let report = kurin_components::component::coverage::render_gaps(uri, &meta, &gaps);
        return Err(miette::miette!("{}", report.trim_end()));
    }
    if mutating && effective_delivery(spec) == kurin_config::v1alpha2::Delivery::Registry {
        return Err(miette::miette!(
            "the base package {uri} covers this cluster, but registry delivery is not \
             implemented yet (WP-36); set [packages] delivery = \"preload\" to stream \
             payloads to the nodes"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> Cluster {
        kurin_config::parse_str(
            std::path::Path::new("payload.toml"),
            r#"version = "v1alpha1"
name = "pay"
[kubernetes]
version = "1.33.1"
[network]
pod-cidr = "10.233.64.0/18"
service-cidr = "10.233.0.0/18"
[ssh]
user = "root"
[ssh.auth]
method = "password"
password = "fake"
[nodes.cp1]
address = "10.99.0.11"
role = "control-plane"
"#,
        )
        .expect("cluster parses")
    }

    /// A minimal legacy bundle directory.
    fn bundle(dir: &std::path::Path) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join("kurin-manifest.json"),
            r#"{"version":1,"artifacts":[]}"#,
        )
        .unwrap();
    }

    #[tokio::test]
    async fn a_legacy_bundle_stays_on_the_bundle_path() {
        let tmp = tempfile::tempdir().unwrap();
        bundle(tmp.path());
        let mut spec = spec();
        spec.packages.offline_dir = Some(tmp.path().to_path_buf());
        match resolve_payload(&mut spec).await.unwrap() {
            Payload::Legacy => {}
            other => panic!("expected the legacy path, got {other:?}"),
        }
        // Canonicalised for the run sources: bundle dir, no base.
        assert_eq!(spec.packages.offline_dir.as_deref(), Some(tmp.path()));
        assert!(spec.packages.base.is_none());
    }

    #[tokio::test]
    async fn a_bare_bundle_path_via_base_is_adapted_to_the_bundle_path() {
        // `--package <legacy dir>`: the compatibility reader adapts the
        // old layout to the same interface.
        let tmp = tempfile::tempdir().unwrap();
        bundle(tmp.path());
        let mut spec = spec();
        spec.packages.base = Some(tmp.path().display().to_string());
        match resolve_payload(&mut spec).await.unwrap() {
            Payload::Legacy => {}
            other => panic!("expected the legacy path, got {other:?}"),
        }
        assert!(spec.packages.base.is_none());
    }

    #[tokio::test]
    async fn a_bundle_and_a_package_at_once_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        bundle(tmp.path());
        let mut spec = spec();
        spec.packages.offline_dir = Some(tmp.path().to_path_buf());
        spec.packages.base = Some("file:///media/pkg.tar.zst".to_string());
        let err = resolve_payload(&mut spec).await.unwrap_err();
        assert!(
            err.to_string().contains("exactly one payload source"),
            "{err}"
        );
    }

    #[test]
    fn offline_uri_overrides_the_config_bundle() {
        let tmp = tempfile::tempdir().unwrap();
        bundle(tmp.path());
        let mut spec = spec();
        spec.packages.offline_dir = Some(tmp.path().to_path_buf());
        apply_offline_override(&mut spec, "file:///media/base.tar.zst");
        assert_eq!(
            spec.packages.base.as_deref(),
            Some("file:///media/base.tar.zst")
        );
        assert!(spec.packages.offline_dir.is_none());
    }

    #[test]
    fn offline_os_uris_are_filed_under_os() {
        let mut spec = spec();
        apply_offline_override(&mut spec, "oci://harbor.local/kurin/os/rocky-9:2026.10");
        assert!(spec.packages.base.is_none());
        assert_eq!(spec.packages.os.len(), 1);
        assert!(spec.packages.offline_dir.is_none());
    }

    #[tokio::test]
    async fn a_bare_offline_path_replaces_a_config_base() {
        let tmp = tempfile::tempdir().unwrap();
        bundle(tmp.path());
        let mut spec = spec();
        spec.packages.base = Some("file:///media/base.tar.zst".to_string());
        apply_offline_override(&mut spec, tmp.path().to_str().unwrap());
        assert!(spec.packages.base.is_none());
        assert_eq!(spec.packages.offline_dir.as_deref(), Some(tmp.path()));
        // And it resolves to the legacy path.
        match resolve_payload(&mut spec).await.unwrap() {
            Payload::Legacy => {}
            other => panic!("expected the legacy path, got {other:?}"),
        }
    }

    #[test]
    fn online_runs_have_nothing_to_validate() {
        let spec = spec();
        let facts = FactsTable::new();
        assert!(validate_payload(&spec, &facts, &Payload::Online, true).is_ok());
        // The legacy path is untouched by validation (no behaviour
        // change for old bundles).
        assert!(validate_payload(&spec, &facts, &Payload::Legacy, true).is_ok());
    }
}
