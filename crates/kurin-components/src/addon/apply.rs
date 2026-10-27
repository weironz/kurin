//! Addon execution (WP-42, 03 §4.5): the Helm path and the manifests path.
//!
//! Both consume a [`RenderedAddon`] (WP-41) and report the object list the
//! readiness tracker (WP-18) follows:
//!
//! - **Helm**: the pinned `helm` binary, the chart archive and the merged
//!   `values.yaml` are uploaded into a work dir on the anchor control
//!   plane. `helm template` renders the exact object set *before* anything
//!   touches the cluster — that output is what the image-integrity check
//!   (03 §4.2 rule 1) validates, so an undeclared image fails the install
//!   instead of reaching the apiserver. `helm upgrade --install
//!   --wait=false` then records the release; readiness is kurin's job.
//! - **Manifests**: rendered on the control machine by WP-41, applied with
//!   server-side apply through [`ClusterApi`].
//!
//! Nothing here waits, writes `kurin-state` or runs hooks — that is the
//! `addon-engine` component (WP-43).

use std::collections::BTreeSet;

use kurin_artifact::artifact::{install_manifest, Arch};
use kurin_artifact::preload::PayloadSet;
use kurin_core::engine::Session;
use kurin_core::events::Redactor;
use kurin_core::run::ArtifactSources;
use kurin_core::zone::Zone;
use kurin_kube::{
    AppliedObject, ApplyOpts, ClusterApi, DynamicObject, Flavor, HelmRunner, HelmStatus, ObjRef,
};

use crate::addon::error::AddonError;
use crate::addon::load::Addon;
use crate::addon::render::{check_images, collect_images, RenderedAddon};

/// The pinned `helm` archive, resolved for one architecture.
pub struct HelmPayload {
    /// File name inside the remote work dir.
    pub file_name: String,
    /// Archive member holding the binary (`linux-amd64/helm`).
    pub member: String,
    /// The release archive bytes.
    pub bytes: Vec<u8>,
}

impl HelmPayload {
    /// Resolve the pinned `helm` artifact from the run's sources through
    /// the shared payload set (04 §5.1): the offline bundle directory or
    /// base package when the run is air-gapped, else the artifact cache
    /// (downloading and verifying the pinned sha256). Package-based
    /// offline runs resolve the blob from the package instead of the
    /// network — the same three sources the install tasks use.
    pub async fn resolve(sources: &ArtifactSources, arch: Arch) -> Result<Self, AddonError> {
        let file = resolve_file_payload(sources, "helm", arch).await?;
        Ok(Self {
            file_name: file.file_name,
            member: format!("linux-{}/helm", arch.as_str()),
            bytes: file.bytes,
        })
    }
}

/// One file artifact resolved from the run's payload set: its rendered
/// file name plus the payload bytes, verified against the pinned sha256
/// on every source (download checksum, bundle verification, package
/// digest — the property [`kurin_artifact::preload::PayloadSet`] carries).
pub(crate) struct ResolvedFile {
    pub file_name: String,
    pub bytes: Vec<u8>,
}

pub(crate) async fn resolve_file_payload(
    sources: &ArtifactSources,
    name: &str,
    arch: Arch,
) -> Result<ResolvedFile, AddonError> {
    let manifest = install_manifest(sources).map_err(|e| AddonError::io("artifact manifest", e))?;
    let spec = manifest
        .spec(name, arch)
        .map_err(|e| AddonError::io("artifact manifest", e))?;
    let payloads = PayloadSet::open(sources)
        .await
        .map_err(|e| AddonError::io("artifact payloads", e))?;
    let blob = payloads.file(&spec).await.map_err(|e| {
        AddonError::io(
            "artifact payloads",
            format!("resolving {name} ({arch}): {e}"),
        )
    })?;
    let bytes = std::fs::read(&blob.path)
        .map_err(|e| AddonError::io(blob.path.display().to_string(), e))?;
    Ok(ResolvedFile {
        file_name: spec.file_name(),
        bytes,
    })
}

/// Everything one apply needs.
pub struct ApplyRequest<'a> {
    /// The addon definition (declared images, namespace, install kind).
    pub addon: &'a Addon,
    /// The rendered values and manifests (WP-41).
    pub rendered: &'a RenderedAddon,
    /// The anchor control-plane session (helm and uploads run there).
    pub session: &'a dyn Session,
    /// The cluster API used by the manifests path.
    pub api: &'a dyn ClusterApi,
    /// Distribution, which selects the admin kubeconfig.
    pub flavor: Flavor,
    /// The pinned helm archive; required by the helm path, unused by
    /// the manifests path.
    pub helm: Option<&'a HelmPayload>,
    /// Effective download zone: `oci://` and `https://` chart
    /// references helm pulls itself follow the CN mirror table.
    pub zone: kurin_core::zone::Zone,
    /// Secrets to scrub from error messages (R8).
    pub redactor: &'a Redactor,
}

/// What an apply produced: the objects the readiness tracker follows and,
/// for chart addons, the Helm release name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AddonApply {
    /// Applied (or, for Helm, rendered) objects, in render order.
    pub objects: Vec<AppliedObject>,
    /// Helm release name; `None` for the manifests path.
    pub release: Option<String>,
}

impl AddonApply {
    /// The object references the tracker takes.
    pub fn refs(&self) -> Vec<ObjRef> {
        self.objects
            .iter()
            .map(|o| ObjRef {
                api_version: o.api_version.clone(),
                kind: o.kind.clone(),
                name: o.name.clone(),
                namespace: o.namespace.clone(),
            })
            .collect()
    }

    pub fn len(&self) -> usize {
        self.objects.len()
    }

    pub fn is_empty(&self) -> bool {
        self.objects.is_empty()
    }
}

/// Apply one rendered addon through its declared install path.
pub async fn apply_addon(request: &ApplyRequest<'_>) -> Result<AddonApply, AddonError> {
    if request.rendered.is_helm() {
        apply_helm(request).await
    } else {
        apply_manifests(request).await
    }
}

/// `helm status -o json` for the release (the `check` half of WP-43).
pub async fn release_status(
    session: &dyn Session,
    flavor: Flavor,
    release: &str,
    namespace: &str,
    helm: &HelmPayload,
    redactor: &Redactor,
) -> Result<HelmStatus, AddonError> {
    let runner = prepared_runner(session, flavor, release, namespace, helm, redactor).await?;
    runner
        .status()
        .await
        .map_err(|e| apply_error(release, e, redactor))
}

/// `helm uninstall` the release.
pub async fn uninstall_addon(
    session: &dyn Session,
    flavor: Flavor,
    release: &str,
    namespace: &str,
    helm: &HelmPayload,
    redactor: &Redactor,
) -> Result<(), AddonError> {
    let runner = prepared_runner(session, flavor, release, namespace, helm, redactor).await?;
    runner
        .uninstall()
        .await
        .map_err(|e| apply_error(release, e, redactor))?;
    cleanup(&runner).await;
    Ok(())
}

async fn prepared_runner<'a>(
    session: &'a dyn Session,
    flavor: Flavor,
    release: &str,
    namespace: &str,
    helm: &HelmPayload,
    redactor: &Redactor,
) -> Result<HelmRunner<'a>, AddonError> {
    let mut runner = HelmRunner::new(session, flavor, release, namespace);
    runner
        .install_helm(&helm.bytes, &helm.file_name, &helm.member)
        .await
        .map_err(|e| apply_error(release, e, redactor))?;
    Ok(runner)
}

async fn apply_helm(request: &ApplyRequest<'_>) -> Result<AddonApply, AddonError> {
    let rendered = request.rendered;
    let release = rendered
        .release
        .clone()
        .unwrap_or_else(|| rendered.name.clone());
    let chart_ref = rendered.chart.as_deref().ok_or_else(|| {
        AddonError::invalid(
            format!("addon {}", rendered.name),
            "spec.install.helm.chart is missing",
        )
    })?;
    let helm = request.helm.ok_or_else(|| {
        AddonError::apply(
            &release,
            "the helm payload was not resolved (required by the helm install path)",
        )
    })?;
    let chart = chart_payload(request.addon, chart_ref, request.zone)?;

    let mut runner = HelmRunner::new(
        request.session,
        request.flavor,
        &release,
        &rendered.namespace,
    );
    runner
        .install_helm(&helm.bytes, &helm.file_name, &helm.member)
        .await
        .map_err(|e| apply_error(&release, e, request.redactor))?;
    match chart {
        ChartPayload::Archive { file_name, bytes } => runner
            .upload_chart(&file_name, &bytes)
            .await
            .map_err(|e| apply_error(&release, e, request.redactor))?,
        ChartPayload::Oci(reference) => runner.use_oci_chart(&reference),
        ChartPayload::Url(url) => runner.use_url_chart(&url),
    }
    runner
        .write_values(&rendered.values.yaml)
        .await
        .map_err(|e| apply_error(&release, e, request.redactor))?;

    // Render first: the image check must reject an undeclared image
    // before the release reaches the cluster.
    let objects = runner
        .template()
        .await
        .map_err(|e| apply_error(&release, e, request.redactor))?;
    let mut images = BTreeSet::new();
    for object in &objects {
        let json = serde_json::to_value(object)
            .map_err(|e| AddonError::apply(&release, format!("serialize rendered object: {e}")))?;
        collect_images(&json, &mut images);
    }
    check_images(
        &request.addon.manifest().spec,
        &format!("helm template {chart_ref}"),
        &images,
        request.redactor,
    )?;

    runner
        .install(false)
        .await
        .map_err(|e| apply_error(&release, e, request.redactor))?;
    cleanup(&runner).await;

    Ok(AddonApply {
        objects: applied_objects(&objects, &rendered.namespace),
        release: Some(release),
    })
}

async fn apply_manifests(request: &ApplyRequest<'_>) -> Result<AddonApply, AddonError> {
    let rendered = request.rendered;
    let mut objects = Vec::new();
    for manifest in &rendered.manifests {
        for value in &manifest.objects {
            let object: DynamicObject = serde_json::from_value(value.clone())
                .map_err(|e| AddonError::invalid(&manifest.path, format!("decode object: {e}")))?;
            objects.push(object);
        }
    }
    let report = request
        .api
        .apply(&objects, ApplyOpts::default())
        .await
        .map_err(|e| apply_error(&rendered.name, e, request.redactor))?;
    Ok(AddonApply {
        objects: report.applied,
        release: None,
    })
}

/// Where the chart comes from.
enum ChartPayload {
    /// A chart archive carried by the addon (uploaded to the anchor).
    Archive { file_name: String, bytes: Vec<u8> },
    /// An `oci://` reference helm pulls itself.
    Oci(String),
    /// An `https://` chart archive URL helm downloads itself.
    Url(String),
}

fn chart_payload(addon: &Addon, chart: &str, zone: Zone) -> Result<ChartPayload, AddonError> {
    if chart.starts_with("oci://") {
        // The CN zone mirrors the registry host of the reference; the
        // digest (when pinned) still governs what helm pulls.
        let reference = chart
            .strip_prefix("oci://")
            .expect("checked the prefix above");
        return Ok(ChartPayload::Oci(format!(
            "oci://{}",
            kurin_core::zone::rewrite_image_ref(reference, zone)
        )));
    }
    if let Some(url) = chart.strip_prefix("https://") {
        // A URL chart archive: helm downloads it, through the mirror
        // table in the CN zone (github.com -> gh-proxy.com).
        let _ = url;
        return Ok(ChartPayload::Url(kurin_core::zone::rewrite(chart, zone)));
    }
    let path = addon.files().path_for(chart);
    if !chart.ends_with(".tgz") {
        return Err(AddonError::invalid(
            &path,
            "spec.install.helm.chart must be a .tgz archive, an oci:// reference or an https:// URL",
        ));
    }
    let bytes = addon.files().read(chart)?;
    let file_name = chart.rsplit('/').next().unwrap_or(chart).to_string();
    Ok(ChartPayload::Archive { file_name, bytes })
}

/// Convert rendered objects into the tracker's shape. Namespaced objects
/// rendered without an explicit `metadata.namespace` belong to the
/// addon's namespace (helm applies them there).
fn applied_objects(objects: &[DynamicObject], namespace: &str) -> Vec<AppliedObject> {
    objects
        .iter()
        .filter_map(|object| {
            let types = object.types.as_ref()?;
            let name = object.metadata.name.clone().filter(|n| !n.is_empty())?;
            let namespace = object
                .metadata
                .namespace
                .clone()
                .or_else(|| (!is_cluster_scoped(&types.kind)).then(|| namespace.to_string()));
            Some(AppliedObject {
                api_version: types.api_version.clone(),
                kind: types.kind.clone(),
                namespace,
                name,
            })
        })
        .collect()
}

/// Cluster-scoped built-in kinds (the ones addons commonly ship). A
/// namespaced object must not be tracked in the addon namespace when it
/// has none of its own.
pub(crate) fn is_cluster_scoped(kind: &str) -> bool {
    matches!(
        kind,
        "Namespace"
            | "Node"
            | "PersistentVolume"
            | "ClusterRole"
            | "ClusterRoleBinding"
            | "CustomResourceDefinition"
            | "StorageClass"
            | "PriorityClass"
            | "IngressClass"
            | "RuntimeClass"
            | "CSIDriver"
            | "CSINode"
            | "VolumeAttachment"
            | "APIService"
            | "MutatingWebhookConfiguration"
            | "ValidatingWebhookConfiguration"
            | "ValidatingAdmissionPolicy"
            | "ValidatingAdmissionPolicyBinding"
            | "ClusterIssuer"
            | "ClusterSecretStore"
            | "ClusterPolicy"
    )
}

fn apply_error(
    path: &str,
    error: impl std::fmt::Display + std::fmt::Debug,
    redactor: &Redactor,
) -> AddonError {
    AddonError::apply(path, redactor.redact(&format!("{error:?}")))
}

async fn cleanup(runner: &HelmRunner<'_>) {
    if let Err(err) = runner.cleanup().await {
        tracing::warn!(workdir = runner.workdir(), %err, "helm work dir cleanup failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kurin_kube::dynamic_object;
    use kurin_kube::Gvk;

    #[test]
    fn cluster_scoped_kinds_keep_no_namespace() {
        let ns = dynamic_object(&Gvk::core("v1", "Namespace"), "demo", None);
        let sa = dynamic_object(
            &Gvk::core("v1", "ServiceAccount"),
            "demo",
            Some("kube-system"),
        );
        let bare = dynamic_object(&Gvk::core("v1", "ConfigMap"), "demo", None);
        let applied = applied_objects(&[ns, sa, bare], "addons");
        assert_eq!(applied.len(), 3);
        assert_eq!(applied[0].namespace, None);
        assert_eq!(applied[1].namespace.as_deref(), Some("kube-system"));
        assert_eq!(applied[2].namespace.as_deref(), Some("addons"));
        assert_eq!(applied[2].api_version, "v1");
    }

    #[test]
    fn applied_objects_skips_nameless_objects() {
        let mut object = dynamic_object(&Gvk::core("v1", "ConfigMap"), "x", None);
        object.metadata.name = None;
        assert!(applied_objects(&[object], "ns").is_empty());
    }

    #[test]
    fn refs_carry_the_tracker_shape() {
        let applied = AddonApply {
            objects: vec![AppliedObject {
                api_version: "apps/v1".into(),
                kind: "Deployment".into(),
                namespace: Some("kube-system".into()),
                name: "metrics-server".into(),
            }],
            release: Some("metrics-server".into()),
        };
        assert_eq!(
            applied.refs(),
            vec![ObjRef::namespaced(
                "apps/v1",
                "Deployment",
                "kube-system",
                "metrics-server"
            )]
        );
    }
}
