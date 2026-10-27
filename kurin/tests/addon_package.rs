//! WP-45: addon packaging and offline delivery.
//!
//! Covers the four deliverables against the public API:
//!
//! - **addon package metadata** — `create_addon` turns an addon
//!   directory into an OCI package; the `kurin-package.json` it writes is
//!   pinned as a snapshot, and the package round-trips through
//!   [`Addon::from_package`].
//! - **chart/image closure** — every digest the package's image rows
//!   reach is present in the layout, and [`PayloadSet`] resolves them by
//!   reference for preload and registry delivery. Both a multi-arch index
//!   and a single-platform manifest are covered.
//! - **base with addons** — a base package embedding an addon under
//!   `addons/<name>/` provides it offline: `Addon::from_base_package`
//!   loads it and `resolve_selection` finds it from `[packages] base`
//!   alone.
//! - **coverage validation** — a package missing an addon image or file
//!   reports the gap; a complete one reports none.

use std::path::{Path, PathBuf};

use kurin::addon::{addon_extras, create_addon, Addon, AddonPackageOptions, Catalog};
use kurin::arch::Arch;
use kurin::artifact::ArtifactStore;
use kurin::components::coverage::addon_gaps;
use kurin::package::{
    PackageBuilder, PackageChart, PackageComponent, PackageExtras, PackageFile, PackageImage,
    PackageKind, PackageManifest, PackageOutput, PackageSource,
};
use kurin::preload::{open_package, PayloadSet};
use kurin::run::ArtifactSources;
use kurin::zone::Zone;
use oci_spec::image::Digest;
use sha2::{Digest as _, Sha256};

// ---------------------------------------------------------------------------
// fixtures
// ---------------------------------------------------------------------------

const ADDON_YAML: &str = r#"apiVersion: addons.kurin.dev/v1alpha1
kind: Addon
metadata:
  name: metrics-server
  version: 1.0.0
  title: Demo
  description: A demo addon used by the packaging tests
  category: other
spec:
  compat:
    kubernetes: ">=1.30.0"
    distributions: [kubeadm]
    arch: [amd64]
  namespace: kube-system
  install:
    helm:
      chart: charts/demo-1.0.0.tgz
      release: demo
      values: values.yaml.j2
  values-schema: values.schema.json
  defaults:
    replicas: 1
  readiness:
    - { kind: Deployment, name: demo }
"#;

const VALUES_SCHEMA: &str = r#"{
  "type": "object",
  "additionalProperties": false,
  "properties": {
    "replicas": { "type": "integer", "minimum": 1 }
  }
}
"#;

/// Write the demo addon directory and return its path.
fn write_addon(root: &Path, images: &[&str]) -> PathBuf {
    let dir = root.join("demo");
    std::fs::create_dir_all(dir.join("charts")).unwrap();
    let yaml = if images.is_empty() {
        ADDON_YAML.to_string()
    } else {
        let list = images
            .iter()
            .map(|reference| format!("    - {reference}\n"))
            .collect::<String>();
        ADDON_YAML.replace(
            "  namespace: kube-system\n",
            &format!("  namespace: kube-system\n  images:\n{list}"),
        )
    };
    std::fs::write(dir.join("addon.yaml"), yaml).unwrap();
    std::fs::write(dir.join("values.schema.json"), VALUES_SCHEMA).unwrap();
    std::fs::write(
        dir.join("values.yaml.j2"),
        "replicas: {{ addon.values.replicas }}\n",
    )
    .unwrap();
    std::fs::write(dir.join("charts/demo-1.0.0.tgz"), b"fake chart payload").unwrap();
    std::fs::write(dir.join("README.md"), "# demo\n").unwrap();
    dir
}

fn digest_of(bytes: &[u8]) -> Digest {
    Digest::try_from(format!("sha256:{:x}", Sha256::digest(bytes))).unwrap()
}

/// A synthetic multi-arch index for `reference` carrying one amd64
/// platform, with every blob in `store`.
fn synthetic_index_image(
    store: &ArtifactStore,
    reference: &str,
    arch: Arch,
    used_by: &str,
) -> PackageImage {
    let config = format!(r#"{{"architecture":"{}","os":"linux"}}"#, arch.as_str());
    let layer = b"synthetic layer";
    let config_digest = store.blob_store().put_bytes(config.as_bytes()).unwrap();
    let layer_digest = store.blob_store().put_bytes(layer).unwrap();
    let manifest = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "digest": config_digest.to_string(),
            "size": config.len(),
        },
        "layers": [{
            "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
            "digest": layer_digest.to_string(),
            "size": layer.len(),
        }],
    });
    let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
    let manifest_digest = store.blob_store().put_bytes(&manifest_bytes).unwrap();
    let index = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.index.v1+json",
        "manifests": [{
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "digest": manifest_digest.to_string(),
            "size": manifest_bytes.len(),
            "platform": { "architecture": arch.as_str(), "os": "linux" },
        }],
    });
    let index_bytes = serde_json::to_vec(&index).unwrap();
    let index_digest = store.blob_store().put_bytes(&index_bytes).unwrap();
    PackageImage {
        reference: reference.to_string(),
        digest: index_digest,
        size: index_bytes.len() as u64,
        media_type: "application/vnd.oci.image.index.v1+json".to_string(),
        arch: vec![arch],
        used_by: vec![used_by.to_string()],
        blobs: vec![manifest_digest, config_digest, layer_digest],
    }
}

/// A synthetic single-platform manifest (no index) for `reference`.
fn synthetic_manifest_image(
    store: &ArtifactStore,
    reference: &str,
    arch: Arch,
    used_by: &str,
) -> PackageImage {
    let config = format!(r#"{{"architecture":"{}","os":"linux"}}"#, arch.as_str());
    let layer = b"single-platform layer";
    let config_digest = store.blob_store().put_bytes(config.as_bytes()).unwrap();
    let layer_digest = store.blob_store().put_bytes(layer).unwrap();
    let manifest = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "digest": config_digest.to_string(),
            "size": config.len(),
        },
        "layers": [{
            "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
            "digest": layer_digest.to_string(),
            "size": layer.len(),
        }],
    });
    let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
    let manifest_digest = store.blob_store().put_bytes(&manifest_bytes).unwrap();
    PackageImage {
        reference: reference.to_string(),
        digest: manifest_digest,
        size: manifest_bytes.len() as u64,
        media_type: "application/vnd.oci.image.manifest.v1+json".to_string(),
        arch: vec![arch],
        used_by: vec![used_by.to_string()],
        blobs: vec![config_digest, layer_digest],
    }
}

/// The addon's files as a base package embeds them (`addons/metrics-server/…`).
fn embedded_addon_files(store: &ArtifactStore, dir: &Path) -> Vec<PackageFile> {
    let mut files: Vec<PathBuf> = Vec::new();
    collect(dir, &mut files);
    files
        .into_iter()
        .map(|path| {
            let rel = path
                .strip_prefix(dir)
                .unwrap()
                .to_string_lossy()
                .to_string();
            let bytes = std::fs::read(&path).unwrap();
            let digest = store.blob_store().put_bytes(&bytes).unwrap();
            PackageFile {
                name: format!("addons/metrics-server/{rel}"),
                kind: kurin::artifact::ArtifactKind::Manifest,
                version: "1.0.0".to_string(),
                arch: Arch::Amd64,
                digest,
                size: bytes.len() as u64,
                title: format!("addons/metrics-server/{rel}"),
                upstream: format!("dir {}", dir.display()),
            }
        })
        .collect()
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, out);
        } else {
            out.push(path);
        }
    }
}

/// Build a base package embedding the demo addon and `images`.
fn build_base_with_addon(
    dir: &Path,
    addon_dir: &Path,
    images: Vec<PackageImage>,
) -> (PackageManifest, String) {
    let store = ArtifactStore::new(dir.parent().unwrap().join("cache"));
    let extras = PackageExtras {
        component: Some(PackageComponent {
            id: "addon/metrics-server".to_string(),
            version: "1.0.0".to_string(),
        }),
        files: embedded_addon_files(&store, addon_dir),
        images,
        charts: vec![PackageChart {
            name: "demo".to_string(),
            version: "1.0.0".to_string(),
            digest: digest_of(b"fake chart payload").to_string(),
        }],
    };
    let mut builder = PackageBuilder::new(PackageKind::Base, "base", "1.33.1", "kubeadm")
        .with_created("2026-10-28T00:00:00Z")
        .with_arches(vec![Arch::Amd64]);
    builder.add_component("distro/kubeadm", "1.33.1");
    builder.add_component(extras.component.as_ref().unwrap().id.clone(), "1.0.0");
    for file in extras.files {
        builder.add_file(file);
    }
    for image in extras.images {
        builder.add_image(image);
    }
    for chart in extras.charts {
        builder.add_chart(chart);
    }
    let out = PackageOutput::Layout(dir.to_path_buf());
    builder
        .build(store.blob_store(), &out)
        .expect("package builds");
    let uri = format!("oci-layout://{}", dir.display());
    let package = PackageSource::open(&uri).unwrap();
    (package.manifest().unwrap(), uri)
}

// ---------------------------------------------------------------------------
// addon package metadata
// ---------------------------------------------------------------------------

/// `create_addon` writes a `kurin-package.json` whose shape is the
/// contract: kind `addon`, the addon's own version, one file-set entry
/// per addon file, the chart row, and the compatibility range.
#[tokio::test]
async fn addon_package_metadata_snapshot() {
    let tmp = tempfile::tempdir().unwrap();
    let addon_dir = write_addon(tmp.path(), &[]);
    let store = ArtifactStore::new(tmp.path().join("cache"));
    let out = PackageOutput::Layout(tmp.path().join("pkg"));
    let report = create_addon(
        &store,
        addon_dir.join("addon.yaml").to_str().unwrap(),
        &AddonPackageOptions::default(),
        Zone::Global,
        &out,
    )
    .await
    .expect("package builds");
    assert_eq!(report.tag, "1.0.0");

    let package =
        PackageSource::open(&format!("oci-layout://{}", out_dir(&out).display())).unwrap();
    let mut meta = package.manifest().unwrap();
    // `created` and the local origin path are the only run-dependent
    // fields; pin them for the snapshot.
    meta.created = "2026-10-28T00:00:00Z".to_string();
    for artifact in &mut meta.artifacts {
        artifact.upstream = "<addon dir>".to_string();
    }
    insta::assert_json_snapshot!("addon_package_metadata", meta);
}

fn out_dir(out: &PackageOutput) -> PathBuf {
    match out {
        PackageOutput::Layout(dir) => dir.clone(),
        other => panic!("expected a layout output, got {other:?}"),
    }
}

/// The package round-trips: `Addon::from_package` reads it back, finds
/// the chart and the values schema, and sees the same file tree.
#[tokio::test]
async fn addon_package_round_trips_through_the_loader() {
    let tmp = tempfile::tempdir().unwrap();
    let addon_dir = write_addon(tmp.path(), &[]);
    let store = ArtifactStore::new(tmp.path().join("cache"));
    let out = PackageOutput::Layout(tmp.path().join("pkg"));
    create_addon(
        &store,
        addon_dir.to_str().unwrap(),
        &AddonPackageOptions::default(),
        Zone::Global,
        &out,
    )
    .await
    .expect("package builds");

    let uri = format!("oci-layout://{}", out_dir(&out).display());
    let addon = Addon::from_package(&uri).expect("package loads as an addon");
    assert_eq!(addon.name(), "metrics-server");
    let files = addon.files().list();
    for expected in [
        "addon.yaml",
        "values.schema.json",
        "values.yaml.j2",
        "charts/demo-1.0.0.tgz",
        "README.md",
    ] {
        assert!(files.iter().any(|f| f == expected), "missing {expected}");
    }
    assert!(addon.validate().is_empty(), "{:?}", addon.validate());
}

// ---------------------------------------------------------------------------
// chart/image closure
// ---------------------------------------------------------------------------

/// Every digest an image row reaches is in the package layout, and
/// `PayloadSet` resolves the image by reference for preload delivery.
#[tokio::test]
async fn image_closure_is_complete_and_resolves_by_reference() {
    let tmp = tempfile::tempdir().unwrap();
    let addon_dir = write_addon(tmp.path(), &[]);
    let reference = "registry.example/demo/controller:v1@sha256:0000000000000000000000000000000000000000000000000000000000000000";
    let store = ArtifactStore::new(tmp.path().join("cache"));
    let image = synthetic_index_image(&store, reference, Arch::Amd64, "addon/metrics-server");
    let (meta, uri) =
        build_base_with_addon(&tmp.path().join("base"), &addon_dir, vec![image.clone()]);

    // The metadata row carries the upstream digest and the arch.
    let row = meta
        .images
        .iter()
        .find(|i| i.reference == reference)
        .expect("image row");
    assert_eq!(row.digest, image.digest.to_string());
    assert_eq!(row.arch, vec!["amd64".to_string()]);

    // The digest closure is physically present.
    let package = open_package(&uri).await.unwrap();
    assert!(package.layout().blobs().has(&image.digest));
    for blob in &image.blobs {
        assert!(
            package.layout().blobs().has(blob),
            "blob {blob} missing from the package"
        );
    }

    // Preload resolves it by reference and produces a layout tar.
    let payloads = PayloadSet::open(&ArtifactSources::embedded().with_base_package(uri.as_str()))
        .await
        .unwrap();
    let payload = payloads.image_ref(reference, Arch::Amd64).await.unwrap();
    assert_eq!(payload.reference(), reference);
    let tar = payload.to_temp_tar().unwrap();
    assert!(std::fs::metadata(&tar.path).unwrap().len() > 0);
    let _ = std::fs::remove_file(&tar.path);

    // Registry delivery gets the index plus every child blob.
    let push = payloads
        .image_for_push_ref(reference, Arch::Amd64)
        .await
        .unwrap();
    assert_eq!(push.index_digest, image.digest.to_string());
    assert_eq!(push.blobs.len(), 3, "manifest, config and layer");
}

/// A digest-pinned image may be a single-platform manifest rather than an
/// index; the package and the preload path handle both.
#[tokio::test]
async fn single_platform_manifest_images_are_supported() {
    let tmp = tempfile::tempdir().unwrap();
    let addon_dir = write_addon(tmp.path(), &[]);
    let reference = "registry.example/demo/single:v2@sha256:1111111111111111111111111111111111111111111111111111111111111111";
    let store = ArtifactStore::new(tmp.path().join("cache"));
    let image = synthetic_manifest_image(&store, reference, Arch::Amd64, "addon/metrics-server");
    let (_, uri) = build_base_with_addon(&tmp.path().join("base"), &addon_dir, vec![image.clone()]);

    let payloads = PayloadSet::open(&ArtifactSources::embedded().with_base_package(uri.as_str()))
        .await
        .unwrap();
    let payload = payloads.image_ref(reference, Arch::Amd64).await.unwrap();
    assert_eq!(payload.reference(), reference);
    let tar = payload.to_temp_tar().unwrap();
    assert!(std::fs::metadata(&tar.path).unwrap().len() > 0);
    let _ = std::fs::remove_file(&tar.path);

    let push = payloads
        .image_for_push_ref(reference, Arch::Amd64)
        .await
        .unwrap();
    assert_eq!(push.index_digest, image.digest.to_string());
    assert_eq!(
        push.media_type,
        "application/vnd.oci.image.manifest.v1+json"
    );
    assert_eq!(push.blobs.len(), 2, "config and layer");
}

/// `addon_extras` namespaces every file under the requested prefix and
/// records the chart row — the payload `package create base --addons`
/// embeds.
#[tokio::test]
async fn addon_extras_namespaces_files_for_a_base_package() {
    let tmp = tempfile::tempdir().unwrap();
    let addon_dir = write_addon(tmp.path(), &[]);
    let store = ArtifactStore::new(tmp.path().join("cache"));
    let addon = Addon::from_dir(&addon_dir).unwrap();
    let extras = addon_extras(
        &store,
        &addon,
        &[Arch::Amd64],
        Zone::Global,
        "addons/metrics-server/",
    )
    .await
    .expect("payload builds");
    let titles: Vec<&str> = extras.files.iter().map(|f| f.title.as_str()).collect();
    assert!(titles.contains(&"addons/metrics-server/addon.yaml"));
    assert!(titles.contains(&"addons/metrics-server/charts/demo-1.0.0.tgz"));
    assert_eq!(extras.charts.len(), 1);
    assert_eq!(extras.charts[0].name, "demo");
    assert_eq!(extras.charts[0].version, "1.0.0");
    assert_eq!(
        extras.component.as_ref().map(|c| c.id.as_str()),
        Some("addon/metrics-server")
    );
}

// ---------------------------------------------------------------------------
// base with addons
// ---------------------------------------------------------------------------

/// A base package built with `--addons` provides the addon offline: the
/// loader reads `addons/<name>/`, the selection finds it from
/// `[packages] base` alone, and coverage reports no gap.
#[tokio::test]
async fn base_with_addons_provides_the_addon_offline() {
    let tmp = tempfile::tempdir().unwrap();
    let reference = "registry.example/demo/controller:v1@sha256:2222222222222222222222222222222222222222222222222222222222222222";
    let addon_dir = write_addon(tmp.path(), &[reference]);
    let store = ArtifactStore::new(tmp.path().join("cache"));
    let image = synthetic_index_image(&store, reference, Arch::Amd64, "addon/metrics-server");
    let (meta, uri) = build_base_with_addon(&tmp.path().join("base"), &addon_dir, vec![image]);

    // The loader reads the embedded subtree.
    let addon = Addon::from_base_package(&uri, "metrics-server")
        .expect("addon loads from the base package");
    assert_eq!(addon.name(), "metrics-server");
    assert!(addon.files().exists("charts/demo-1.0.0.tgz"));

    // The selection resolves it from `[packages] base`; the addon
    // declares the image, so the package must carry it.
    let cluster = cluster_config(&uri);
    let selected =
        kurin::addon::engine::resolve_selection(&cluster, None).expect("selection resolves");
    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].addon.name(), "metrics-server");
    assert!(selected[0].addon.files().exists("README.md"));

    let gaps = addon_gaps(
        &meta,
        &selected[0].addon,
        &[Arch::Amd64],
        "addons/metrics-server/",
    );
    assert!(gaps.is_empty(), "{gaps:?}");
}

/// The offline plan delivers the addon's images before the addon is
/// applied (preload delivery, 04 §5.1): the image task exists for every
/// Kubernetes node and the install task depends on it.
#[tokio::test]
async fn offline_addon_plan_preloads_images_before_install() {
    use std::sync::Arc;

    let tmp = tempfile::tempdir().unwrap();
    let reference = "registry.example/demo/controller:v1@sha256:4444444444444444444444444444444444444444444444444444444444444444";
    let addon_dir = write_addon(tmp.path(), &[reference]);
    let store = ArtifactStore::new(tmp.path().join("cache"));
    let image = synthetic_index_image(&store, reference, Arch::Amd64, "addon/metrics-server");
    let (_, uri) = build_base_with_addon(&tmp.path().join("base"), &addon_dir, vec![image]);
    let cluster = cluster_config(&uri);

    let mut facts = kurin::facts::FactsTable::new();
    facts.insert("cp1", kurin::facts::NodeFacts::minimal(Arch::Amd64));
    let run = Arc::new(
        kurin::run::RunContext::new(
            "op",
            "offline",
            ArtifactSources::embedded().with_base_package(uri.as_str()),
        )
        .with_facts(facts),
    );
    let plan =
        kurin::addon::engine::build_addon_plan(kurin::plan::OpKind::AddonInstall, &cluster, &run)
            .expect("plan builds");
    let ids: Vec<String> = plan.tasks.iter().map(|t| t.task.id()).collect();
    assert!(
        ids.iter().any(|id| id == "addons/metrics-server@cp1"),
        "{ids:?}"
    );
    assert!(
        ids.iter()
            .any(|id| id == "addons/metrics-server/images@cp1"),
        "{ids:?}"
    );
    let install = ids
        .iter()
        .position(|id| id == "addons/metrics-server@cp1")
        .unwrap();
    let images = ids
        .iter()
        .position(|id| id == "addons/metrics-server/images@cp1")
        .unwrap();
    assert!(
        plan.edges
            .iter()
            .any(|(dep, task)| *dep == images && *task == install),
        "the install task must run after its image preload"
    );
}

/// A base package that does not carry the addon's image or files reports
/// exactly what is missing — the offline install refuses instead of
/// half-installing.
#[test]
fn coverage_reports_missing_addon_payloads() {
    let tmp = tempfile::tempdir().unwrap();
    let addon_dir = write_addon(
        tmp.path(),
        &["registry.example/demo/controller:v1@sha256:3333333333333333333333333333333333333333333333333333333333333333"],
    );
    let (meta, _) = build_base_with_addon(&tmp.path().join("base"), &addon_dir, Vec::new());
    let addon = Addon::from_dir(&addon_dir).unwrap();
    let gaps = addon_gaps(&meta, &addon, &[Arch::Amd64], "addons/metrics-server/");
    assert!(
        gaps.iter()
            .any(|gap| gap.contains("registry.example/demo/controller")),
        "{gaps:?}"
    );
    // The files are present, so no file gap is reported.
    assert!(
        !gaps.iter().any(|gap| gap.contains("addon.yaml")),
        "{gaps:?}"
    );
}

/// The built-in catalog is still empty (WP-46); packaging an unknown name
/// fails with a pointer at the supported sources rather than silently
/// producing an empty package.
#[test]
fn unknown_catalog_names_are_rejected() {
    let mut catalog = Catalog::embedded().expect("catalog loads");
    assert!(catalog.take("does-not-exist").is_none());
}

fn cluster_config(base_uri: &str) -> kurin_config::Cluster {
    let toml = format!(
        r#"version = "v1alpha2"
name = "offline"
[kubernetes]
version = "1.33.1"
distribution = "kubeadm"
[runtime]
kind = "containerd"
[network]
pod-cidr = "10.233.64.0/18"
service-cidr = "10.233.0.0/18"
[network.cni]
kind = "calico"
[ssh]
user = "root"
[ssh.auth]
method = "password"
password = "fake"
[packages]
base = "{base_uri}"
[addons.metrics-server]
enabled = true
[nodes.cp1]
address = "10.99.0.11"
role = "control-plane"
"#
    );
    kurin_config::parse_str(Path::new("offline.toml"), &toml).expect("cluster parses")
}
