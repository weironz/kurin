//! Offline bundle manifest + air-gapped upgrade entry point.
//!
//! No network, no SSH: the bundle manifest is exercised as a file format and
//! `run_offline` must fail on a bundle problem *before* touching any node.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use miette::Result;
use semver::Version;
use sha2::{Digest, Sha256};

use kurin::artifact::{Arch, ArtifactEntry, ArtifactKind, ArtifactManifest};
use kurin::upgrade::artifact_bundle::{read_bundle_manifest, write_bundle_manifest};
use kurin::Session;

// ---------------------------------------------------------------------------
// bundle manifest: write / read
// ---------------------------------------------------------------------------

#[test]
fn bundle_manifest_round_trips_the_pins() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = ArtifactManifest::embedded().unwrap();

    let path = write_bundle_manifest(dir.path(), &manifest).unwrap();
    assert_eq!(path, dir.path().join("kurin-manifest.json"));
    assert!(path.is_file());

    let back = read_bundle_manifest(dir.path()).unwrap();
    assert_eq!(
        serde_json::to_value(&back).unwrap(),
        serde_json::to_value(&manifest).unwrap(),
        "the bundle manifest must round-trip every pin"
    );
}

#[test]
fn missing_manifest_names_the_rebuild_command() {
    let dir = tempfile::tempdir().unwrap();
    let err = read_bundle_manifest(dir.path()).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("kurin-manifest.json missing in bundle"),
        "unexpected error: {msg}"
    );
    assert!(
        msg.contains("rebuild with `kurin artifact --with-kubernetes vX.Y.Z --out <dir>`"),
        "must point at the exact rebuild command: {msg}"
    );
}

#[test]
fn missing_bundle_dir_reports_the_same_hint() {
    let err = read_bundle_manifest(Path::new("/nonexistent/kurin-bundle")).unwrap_err();
    assert!(
        err.to_string()
            .contains("kurin-manifest.json missing in bundle"),
        "unexpected error: {err}"
    );
}

/// Build the smallest valid bundle: one verified payload keyed `linux-amd64`
/// for `name`/`version`, plus the matching `kurin-manifest.json`.
fn write_tiny_bundle(dir: &Path, name: &str, version: &str, file: &str, payload: &[u8]) {
    let digest = format!("{:x}", Sha256::digest(payload));
    let adir = dir.join(format!("{name}-{version}-amd64"));
    std::fs::create_dir_all(&adir).unwrap();
    std::fs::write(adir.join(file), payload).unwrap();
    std::fs::write(adir.join(format!("{file}.sha256")), &digest).unwrap();
    let manifest = ArtifactManifest {
        version: 1,
        artifacts: vec![ArtifactEntry {
            name: name.into(),
            kind: ArtifactKind::Binary,
            version: version.into(),
            file: file.into(),
            url_template: format!("https://example.invalid/{{version}}/{file}"),
            sha256: BTreeMap::from([("linux-amd64".into(), digest)]),
            optional: false,
        }],
    };
    write_bundle_manifest(dir, &manifest).unwrap();
}

#[test]
fn bundle_payloads_verify_against_the_written_manifest() {
    let dir = tempfile::tempdir().unwrap();
    let payload = b"#!/bin/sh\necho kubeadm\n";
    write_tiny_bundle(dir.path(), "kubeadm", "1.34.12", "kubeadm", payload);

    // What `run_offline` does before any node SSH: read the pins back, then
    // re-verify every payload against them.
    let read = read_bundle_manifest(dir.path()).unwrap();
    let found = ArtifactManifest::from_dir_with(dir.path(), &read).unwrap();
    let art = found
        .find("kubeadm", Arch::Amd64)
        .expect("kubeadm in bundle");
    assert_eq!(std::fs::read(&art.file).unwrap(), payload);
    assert_eq!(art.version, "1.34.12");
}

// ---------------------------------------------------------------------------
// run_offline: bundle problems fail on the control host
// ---------------------------------------------------------------------------

struct NoSessions;

#[async_trait::async_trait]
impl kurin::SessionPool for NoSessions {
    async fn get(&self, node: &str) -> Result<Arc<dyn Session>> {
        panic!("run_offline must fail before opening a session to {node}")
    }
}

fn cluster() -> kurin_config::Cluster {
    kurin_config::parse_str(
        Path::new("x.toml"),
        r##"version = "v1alpha1"
name = "t"
[ssh]
user = "root"
[ssh.auth]
method = "password"
password = "x"
[nodes.cp1]
address = "10.0.0.1"
role = "control-plane"
[nodes.w1]
address = "10.0.0.2"
role = "worker"
"##,
    )
    .unwrap()
}

#[tokio::test]
async fn run_offline_requires_the_bundle_manifest() {
    let dir = tempfile::tempdir().unwrap();
    let pool: Arc<dyn kurin::SessionPool> = Arc::new(NoSessions);
    let err = kurin::upgrade::run_offline(
        &cluster(),
        &Version::parse("1.34.12").unwrap(),
        dir.path(),
        &pool,
        1,
    )
    .await
    .unwrap_err();
    assert!(
        err.to_string()
            .contains("kurin-manifest.json missing in bundle"),
        "unexpected error: {err}"
    );
}

#[tokio::test]
async fn run_offline_reports_a_bundle_without_kubeadm() {
    // Integrity passes (one verified payload) but kubeadm is absent: the
    // arch probe must fail before any node session is opened.
    let dir = tempfile::tempdir().unwrap();
    write_tiny_bundle(
        dir.path(),
        "calico-operator",
        "3.30.3",
        "calico.yaml",
        b"manifest",
    );

    let pool: Arc<dyn kurin::SessionPool> = Arc::new(NoSessions);
    let err = kurin::upgrade::run_offline(
        &cluster(),
        &Version::parse("1.34.12").unwrap(),
        dir.path(),
        &pool,
        1,
    )
    .await
    .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("contains no kubeadm binary")
            && msg.contains("kurin artifact --with-kubernetes"),
        "unexpected error: {msg}"
    );
}
