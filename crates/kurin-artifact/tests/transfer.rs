//! WP-33: package transfer between the three transport forms (04 §3.3).
//!
//! The local half (layout / tar / tar.zst pairwise, blob skipping,
//! digest verification, corrupt-blob rejection) always runs. The
//! registry half runs only when `KURIN_TEST_ZOT` points at a zot
//! container, e.g.:
//!
//! ```sh
//! docker run -d --name kurin-wp33-zot -p 127.0.0.1:15123:5000 \
//!   ghcr.io/project-zot/zot-linux-amd64:latest
//! KURIN_TEST_ZOT=127.0.0.1:15123 cargo test -p kurin-artifact --test transfer
//! ```
//!
//! `auth failure` needs a registry that rejects bad credentials: start
//! zot with `--config` htpasswd auth and set `KURIN_TEST_ZOT_USER` /
//! `KURIN_TEST_ZOT_PASS` / `KURIN_TEST_ZOT_PASS_FILE` (the file the
//! env var points at must contain the real password).

use kurin_artifact::package::{PackageBuilder, PackageFile, PackageImage, PackageKind};
use kurin_artifact::store::BlobStore;
use kurin_artifact::transfer::{transfer, verify_package, PackageRef, TransferOptions};
use oci_spec::image::Digest;
use sha2::{Digest as _, Sha256};

fn hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn digest(bytes: &[u8]) -> Digest {
    Digest::try_from(format!("sha256:{}", hex(bytes))).unwrap()
}

/// A tiny package: one file per arch, one image, built into `dir` and
/// returned as its layout. Same shape as the WP-32 fixture.
fn build_package(dir: &std::path::Path) -> PackageRef {
    let cache = BlobStore::open(dir.join("cache"));
    cache.put_bytes(b"k").unwrap();
    cache.put_bytes(b"y").unwrap();
    // the image chain must be real manifest documents: the closure
    // walker parses them to find the next layer of children
    let cfg = br#"{"architecture":"amd64","os":"linux","rootfs":{"type":"layers","diff_ids":[]}}"#
        .as_slice();
    let lyr = b"lyr".as_slice();
    let mft = serde_json::to_vec(&serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "digest": format!("sha256:{}", hex(cfg)),
            "size": cfg.len()
        },
        "layers": [{
            "mediaType": "application/vnd.oci.image.layer.v1.tar",
            "digest": format!("sha256:{}", hex(lyr)),
            "size": lyr.len()
        }]
    }))
    .unwrap();
    let idx = serde_json::to_vec(&serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.index.v1+json",
        "manifests": [{
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "digest": format!("sha256:{}", hex(&mft)),
            "size": mft.len(),
            "platform": { "architecture": "amd64", "os": "linux" }
        }]
    }))
    .unwrap();
    for bytes in [idx.as_slice(), mft.as_slice(), cfg, lyr] {
        cache.put_bytes(bytes).unwrap();
    }
    let mut builder = PackageBuilder::new(PackageKind::Base, "base", "1.33.1", "kubeadm")
        .with_created("2026-10-28T00:00:00Z")
        .with_kurin(">=0.4.0, <0.6.0")
        .with_tag("v1.33.1-kubeadm");
    builder.add_component("distro/kubeadm", "1.33.1");
    builder.add_file(PackageFile {
        name: "kubeadm".into(),
        kind: kurin_artifact::artifact::ArtifactKind::Binary,
        version: "1.33.1".into(),
        arch: kurin_artifact::artifact::Arch::Amd64,
        digest: digest(b"k"),
        size: 1,
        title: "kubeadm".into(),
        upstream: "https://dl.k8s.io/kubeadm".into(),
    });
    builder.add_file(PackageFile {
        name: "calico-operator".into(),
        kind: kurin_artifact::artifact::ArtifactKind::Manifest,
        version: "3.30.2".into(),
        arch: kurin_artifact::artifact::Arch::Arm64,
        digest: digest(b"y"),
        size: 1,
        title: "tigera-operator.yaml".into(),
        upstream: "https://example/tigera-operator.yaml".into(),
    });
    builder.add_image(PackageImage {
        reference: "registry.k8s.io/pause:3.10".into(),
        digest: digest(&idx),
        size: idx.len() as u64,
        media_type: "application/vnd.oci.image.index.v1+json".into(),
        arch: vec![kurin_artifact::artifact::Arch::Amd64],
        used_by: vec!["runtime/containerd".into()],
        blobs: vec![digest(&mft), digest(cfg), digest(lyr)],
    });
    let out = kurin_artifact::package::PackageOutput::Layout(dir.join("layout"));
    builder.build(&cache, &out).unwrap();
    PackageRef::parse(&format!("oci-layout://{}", dir.join("layout").display())).unwrap()
}

fn no_auth() -> TransferOptions {
    TransferOptions::default()
}

/// Every local pairwise conversion (layout / tar / tar.zst) carries the
/// same index digest, and a second copy into an already-populated
/// destination moves nothing.
#[tokio::test]
async fn local_forms_convert_pairwise_and_skip_present_blobs() {
    let tmp = tempfile::tempdir().unwrap();
    let src = build_package(tmp.path());
    let first = verify_package(&src, &no_auth()).await.unwrap();
    let forms = [
        PackageRef::Layout(tmp.path().join("dst-layout")),
        PackageRef::Tar(tmp.path().join("dst.tar")),
        PackageRef::TarZstd(tmp.path().join("dst.tar.zst")),
    ];
    for dst in &forms {
        let report = transfer(&src, dst, &no_auth()).await.unwrap();
        assert_eq!(report.index, first.index, "index digest survives -> {dst}");
        assert_eq!(
            report.counts.blobs_moved, first.blobs,
            "everything moved once -> {dst}"
        );
        assert_eq!(report.counts.blobs_present, 0, "-> {dst}");
        let verified = verify_package(dst, &no_auth()).await.unwrap();
        assert_eq!(verified.index, first.index);
    }
    // re-transfer into the layout destination: nothing moves again
    // (a tar destination always re-packs a fresh staging layout, so
    // skipping only has meaning for a directory / a registry)
    let again = transfer(&src, &forms[0], &no_auth()).await.unwrap();
    assert_eq!(
        again.counts.blobs_moved, 0,
        "second copy into the same layout sends nothing"
    );
    assert_eq!(
        again.counts.blobs_present, first.blobs,
        "second copy sees every blob present"
    );
    assert_eq!(again.index, first.index);

    // and back: tar.zst -> fresh layout round-trips the digest too
    let round = PackageRef::Layout(tmp.path().join("round-layout"));
    let report = transfer(&forms[2], &round, &no_auth()).await.unwrap();
    assert_eq!(report.index, first.index);
    verify_package(&round, &no_auth()).await.unwrap();
}

/// A corrupted blob is rejected by `verify` in every form.
#[tokio::test]
async fn corrupt_blob_is_rejected_by_verify() {
    let tmp = tempfile::tempdir().unwrap();
    let src = build_package(tmp.path());
    let layout_dir = tmp.path().join("layout");
    // flip one byte of the "kubeadm" payload blob
    let blob = layout_dir.join("blobs").join("sha256").join(hex(b"k"));
    let mut bytes = std::fs::read(&blob).unwrap();
    bytes[0] ^= 0xff;
    std::fs::write(&blob, bytes).unwrap();
    let err = verify_package(&src, &no_auth()).await.unwrap_err();
    let msg = format!("{err:?}");
    assert!(
        msg.contains("corrupt") || msg.contains("digest") || msg.contains("sha256"),
        "{msg}"
    );
    // and it must not be pushable either
    let dst = PackageRef::Layout(tmp.path().join("never"));
    assert!(transfer(&src, &dst, &no_auth()).await.is_err());
}

// -- registry half (needs a zot; gated behind KURIN_TEST_ZOT) ---------------

fn zot() -> Option<(String, String, String)> {
    let host = std::env::var("KURIN_TEST_ZOT").ok()?;
    let user = std::env::var("KURIN_TEST_ZOT_USER").unwrap_or_else(|_| "testuser".into());
    let pass_env = "KURIN_TEST_ZOT_PASS";
    std::env::set_var(
        pass_env,
        std::env::var("KURIN_TEST_ZOT_PASS").unwrap_or_else(|_| "kurin-test-pass".into()),
    );
    Some((host, user, pass_env.into()))
}

fn zot_opts(user: &str, pass_env: &str) -> TransferOptions {
    TransferOptions {
        user: Some(user.to_string()),
        password_env: Some(pass_env.to_string()),
    }
}

/// The full manual e2e as a test: push layout→zot, pull zot→tar, verify
/// and inspect both; the second push sends nothing; wrong credentials
/// fail with an auth error.
#[tokio::test]
async fn zot_round_trip_and_auth_failure() {
    let Some((host, user, pass_env)) = zot() else {
        eprintln!("skipping: KURIN_TEST_ZOT is not set");
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let src = build_package(tmp.path());
    let first = verify_package(&src, &no_auth()).await.unwrap();
    let opts = zot_opts(&user, &pass_env);

    // 1. push layout -> zot
    let repo = format!(
        "kurin-wp33/base-{}",
        &hex(&std::process::id().to_le_bytes())[..6]
    );
    let reg = PackageRef::parse(&format!("oci://{host}/{repo}:v1.33.1-kubeadm")).unwrap();
    let pushed = transfer(&src, &reg, &opts).await.unwrap();
    assert_eq!(pushed.index, first.index);
    assert_eq!(
        pushed.counts.blobs_moved, first.blobs,
        "first push sends every blob"
    );

    // 2. second push: the registry already has everything -> nothing sent
    let again = transfer(&src, &reg, &opts).await.unwrap();
    assert_eq!(again.counts.blobs_moved, 0, "second push sends nothing");
    assert_eq!(again.counts.blobs_present, first.blobs);
    assert_eq!(again.index, first.index);

    // 3. pull zot -> tar, verify and inspect
    let tar = PackageRef::Tar(tmp.path().join("from-zot.tar"));
    let pulled = transfer(&reg, &tar, &opts).await.unwrap();
    assert_eq!(pulled.index, first.index);
    let verified = verify_package(&tar, &no_auth()).await.unwrap();
    assert_eq!(verified.index, first.index);
    assert_eq!(verified.blobs, first.blobs);
    let source = kurin_artifact::package::PackageSource::open(&format!(
        "file://{}",
        tmp.path().join("from-zot.tar").display()
    ))
    .unwrap();
    let meta = source.manifest().unwrap();
    assert_eq!(meta.name, "base");
    assert_eq!(meta.version, "1.33.1");
    assert_eq!(meta.kind.as_str(), "base");

    // 4. bad credentials are refused
    std::env::set_var("KURIN_TEST_ZOT_BAD", "definitely-wrong");
    let bad = zot_opts(&user, "KURIN_TEST_ZOT_BAD");
    let missing = PackageRef::parse(&format!("oci://{host}/{repo}:missing")).unwrap();
    let err = transfer(&src, &missing, &bad).await.unwrap_err();
    let msg = format!("{err:?}");
    assert!(
        msg.contains("401")
            || msg.to_lowercase().contains("auth")
            || msg.to_lowercase().contains("unauthorized"),
        "expected an auth failure, got: {msg}"
    );
}
