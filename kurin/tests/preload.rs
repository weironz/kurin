//! WP-35: preload distribution from packages (04 §5.1).
//!
//! Covers the two halves of the work package against the public API:
//!
//! - **File payloads** — `ArtifactsInstall` resolves each artifact from
//!   the run's package and streams it as `UploadSource::Blob`, picking the
//!   node's `facts.arch` (amd64 and arm64 packages carry different bytes).
//! - **Image payloads** — `ImagesImport` re-hosts the selected platform as
//!   a single-image OCI layout tar and streams it; the tar carries
//!   `oci-layout` + `index.json` for that architecture only.
//!
//! The scripted session records every upload with its source kind, so a
//! regression back to whole-file buffering (`UploadSource::File`) or to
//! the wrong architecture fails here.

mod support;

use std::path::Path;
use std::sync::Arc;

use kurin::arch::Arch;
use kurin::artifact::{ArtifactKind, ArtifactManifest, ArtifactStore};
use kurin::engine::{Drift, Task};
use kurin::facts::FactsTable;
use kurin::package::{PackageBuilder, PackageFile, PackageImage, PackageKind, PackageOutput};
use kurin::run::{ArtifactSources, RunContext};
use kurin::tasks::images::ImagesImport;
use kurin::tasks::install::ArtifactsInstall;
use sha2::{Digest as _, Sha256};

use support::ScriptedSession;

/// Artifacts `ArtifactsInstall` installs on a node (the manifest's
/// installable set: images and plain manifests are not node installs).
const INSTALLABLE: &[&str] = &[
    "kubeadm",
    "kubelet",
    "kubectl",
    "runc",
    "containerd",
    "crictl",
    "cni-plugins",
    "k3s",
    "crio",
    "cilium-cli",
];

fn cluster() -> kurin_config::Cluster {
    kurin_config::parse_str(
        Path::new("preload.toml"),
        r#"version = "v1alpha1"
name = "preload"
[kubernetes]
version = "1.33.1"
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
[nodes.cp1]
address = "10.99.0.11"
role = "control-plane"
"#,
    )
    .expect("cluster parses")
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// The payload a package carries for one artifact and architecture: the
/// bytes differ per arch, so the upload proves which one was selected.
fn file_payload(name: &str, arch: Arch) -> Vec<u8> {
    format!("{name}-{}-payload", arch.as_str()).into_bytes()
}

/// One multi-arch image: an index with an amd64 and an arm64 manifest, each
/// with its own config and layer, all archived into the cache blob store.
fn image_payload(cache: &ArtifactStore, reference: &str) -> PackageImage {
    let mut entries = Vec::new();
    let mut blobs = Vec::new();
    for arch in [Arch::Amd64, Arch::Arm64] {
        let layer = format!("layer:{reference}:{}", arch.as_str()).into_bytes();
        let config = format!("config:{reference}:{}", arch.as_str()).into_bytes();
        let layer_digest = cache.blob_store().put_bytes(&layer).unwrap();
        let config_digest = cache.blob_store().put_bytes(&config).unwrap();
        let manifest = serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": {
                "mediaType": "application/vnd.oci.image.config.v1+json",
                "digest": config_digest.to_string(),
                "size": config.len(),
            },
            "layers": [{
                "mediaType": "application/vnd.oci.image.layer.v1.tar",
                "digest": layer_digest.to_string(),
                "size": layer.len(),
            }],
        });
        let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
        let manifest_digest = cache.blob_store().put_bytes(&manifest_bytes).unwrap();
        entries.push(serde_json::json!({
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "digest": manifest_digest.to_string(),
            "size": manifest_bytes.len(),
            "platform": { "architecture": arch.as_str(), "os": "linux" },
        }));
        blobs.extend([manifest_digest, config_digest, layer_digest]);
    }
    let index = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.index.v1+json",
        "manifests": entries,
    });
    let index_bytes = serde_json::to_vec(&index).unwrap();
    let index_digest = cache.blob_store().put_bytes(&index_bytes).unwrap();
    PackageImage {
        reference: reference.to_string(),
        digest: index_digest,
        size: index_bytes.len() as u64,
        media_type: "application/vnd.oci.image.index.v1+json".into(),
        arch: vec![Arch::Amd64, Arch::Arm64],
        used_by: Vec::new(),
        blobs,
    }
}

/// Build a base package into `dir` covering both architectures: every
/// installable artifact plus the cluster's bootstrap images.
fn build_package(dir: &Path) -> String {
    let cache = ArtifactStore::new(dir.parent().unwrap().join("cache"));
    let mut builder = PackageBuilder::new(PackageKind::Base, "base", "1.33.1", "kubeadm")
        .with_created("2026-10-28T00:00:00Z");
    for arch in [Arch::Amd64, Arch::Arm64] {
        for name in INSTALLABLE {
            let payload = file_payload(name, arch);
            let digest = cache.blob_store().put_bytes(&payload).unwrap();
            builder.add_file(PackageFile {
                name: (*name).to_string(),
                kind: ArtifactKind::Binary,
                version: "1.33.1".into(),
                arch,
                digest,
                size: payload.len() as u64,
                title: (*name).to_string(),
                upstream: "https://example.invalid".into(),
            });
        }
    }
    let cluster = cluster();
    let manifest = ArtifactManifest::embedded().unwrap();
    let mut seen = Vec::new();
    for arch in [Arch::Amd64, Arch::Arm64] {
        for artifact in kurin::components::bootstrap_images(&cluster, arch) {
            if seen.contains(&artifact.name) {
                continue;
            }
            seen.push(artifact.name.clone());
            let spec = manifest.spec(&artifact.name, arch).unwrap();
            builder.add_image(image_payload(&cache, &spec.url()));
        }
    }
    builder
        .build(
            cache.blob_store(),
            &PackageOutput::Layout(dir.to_path_buf()),
        )
        .expect("package builds");
    format!("oci-layout://{}", dir.display())
}

fn run_for(uri: String) -> Arc<RunContext> {
    let mut facts = FactsTable::new();
    facts.insert("cp1", kurin::facts::NodeFacts::minimal(Arch::Amd64));
    Arc::new(
        RunContext::new(
            "preload-test",
            "preload",
            ArtifactSources::embedded().with_base_package(uri),
        )
        .with_facts(facts),
    )
}

/// `ArtifactsInstall` streams package blobs and selects them by arch.
#[tokio::test]
async fn artifacts_install_streams_the_selected_arch_blob() {
    let tmp = tempfile::tempdir().unwrap();
    let uri = build_package(&tmp.path().join("pkg"));
    let run = run_for(uri);
    let cluster = cluster();
    let node = cluster.nodes[0].clone();

    for arch in [Arch::Amd64, Arch::Arm64] {
        let task = ArtifactsInstall::new(&node, arch, kurin_config::RuntimeKind::Containerd, &run);
        let session = ScriptedSession::new(&node.name, Vec::new());
        assert!(matches!(
            task.check(&session).await.unwrap(),
            Drift::Drifted(_)
        ));
        task.apply(&session).await.expect("apply succeeds");

        let uploads = session.uploads();
        assert_eq!(
            uploads.len(),
            INSTALLABLE.len(),
            "one upload per installable artifact"
        );
        for upload in &uploads {
            assert_eq!(
                upload.source, "blob",
                "{} must be streamed as a blob, not buffered",
                upload.path
            );
        }

        // The direct-install binaries land at their final path via a
        // staged temp name; the payload must be this arch's bytes.
        let kubeadm = uploads
            .iter()
            .find(|u| u.path == "/usr/local/bin/kubeadm.kurin-new")
            .expect("kubeadm upload");
        let expected = file_payload("kubeadm", arch);
        assert_eq!(kubeadm.sha256, sha256_hex(&expected));
        assert_eq!(
            kubeadm.declared.as_deref(),
            Some(format!("sha256:{}", sha256_hex(&expected)).as_str())
        );
        assert_eq!(kubeadm.bytes, expected.len());

        // The tar payloads keep their staged name and carry this arch too.
        let containerd = uploads
            .iter()
            .find(|u| u.path == "/tmp/kurin-containerd.tgz")
            .expect("containerd upload");
        assert_eq!(
            containerd.sha256,
            sha256_hex(&file_payload("containerd", arch))
        );
    }
}

/// `ImagesImport` streams a single-image OCI layout tar for the node's
/// architecture, then imports it with `ctr`.
#[tokio::test]
async fn images_import_streams_a_single_arch_layout_tar() {
    let tmp = tempfile::tempdir().unwrap();
    let uri = build_package(&tmp.path().join("pkg"));
    let run = run_for(uri);
    let cluster = cluster();
    let node = cluster.nodes[0].clone();
    let task = ImagesImport::new(&node, Arch::Amd64, &cluster, &run);
    let session = ScriptedSession::new(&node.name, Vec::new());

    // The scripted node reports an empty `ctr images list`, so `check`
    // sees every image missing and `apply` is what the executor would run.
    task.apply(&session).await.expect("apply succeeds");

    let uploads = session.uploads();
    let expected = kurin::components::bootstrap_images(&cluster, Arch::Amd64).len();
    assert_eq!(uploads.len(), expected, "one tar per bootstrap image");
    for upload in &uploads {
        assert_eq!(upload.source, "blob");
        assert!(
            upload.path.starts_with("/tmp/kurin-img-"),
            "kubeadm imports from /tmp: {}",
            upload.path
        );
        let tar = session.file(&upload.path).expect("tar captured");
        assert!(
            tar.windows(b"oci-layout".len()).any(|w| w == b"oci-layout"),
            "{} is an OCI layout tar",
            upload.path
        );
        assert!(
            tar.windows(b"\"architecture\":\"amd64\"".len())
                .any(|w| w == b"\"architecture\":\"amd64\""),
            "{} names the amd64 manifest",
            upload.path
        );
        assert!(
            !tar.windows(b"arm64".len()).any(|w| w == b"arm64"),
            "{} must not carry the arm64 sub-manifest",
            upload.path
        );
    }

    // Every import ran through containerd's k8s.io namespace.
    let commands = session.unmatched();
    assert_eq!(
        commands
            .iter()
            .filter(|c| c.contains("ctr -n k8s.io images import"))
            .count(),
        expected
    );
}

/// k3s does not import through `ctr`: the tar goes to the agent images
/// directory, which k3s reads when its service starts.
#[tokio::test]
async fn k3s_puts_the_layout_tar_in_the_agent_images_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let uri = build_package(&tmp.path().join("pkg"));
    let run = run_for(uri);
    let k3s = kurin_config::parse_str(
        Path::new("preload-k3s.toml"),
        r#"version = "v1alpha1"
name = "preload-k3s"
[kubernetes]
version = "1.33.1"
distribution = "k3s"
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
[nodes.cp1]
address = "10.99.0.11"
role = "control-plane"
"#,
    )
    .expect("k3s cluster parses");
    let node = k3s.nodes[0].clone();
    let task = ImagesImport::new(&node, Arch::Amd64, &k3s, &run);
    let session = ScriptedSession::new(&node.name, Vec::new());
    task.apply(&session).await.expect("apply succeeds");

    let uploads = session.uploads();
    assert!(!uploads.is_empty());
    for upload in &uploads {
        assert_eq!(upload.source, "blob");
        assert!(
            upload
                .path
                .starts_with("/var/lib/rancher/k3s/agent/images/kurin-"),
            "k3s image tar path: {}",
            upload.path
        );
    }
    assert!(
        !session
            .unmatched()
            .iter()
            .any(|c| c.contains("ctr -n k8s.io images import")),
        "k3s must not shell out to ctr"
    );
}
