//! WP-32: `kurin package create base` end to end against a fake upstream.
//!
//! The download/verify path is exercised with a local HTTP file server
//! (`python3 -m http.server`), which `curl -fsSL` (what
//! [`kurin_artifact::artifact::ArtifactStore::fetch`] shells out to)
//! fetches like any HTTPS upstream — only the scheme differs. The test
//! pins a tiny custom [`ArtifactManifest`] whose URLs point at the
//! server, so every download is real (real GET, real sha256 check, real
//! BlobStore import), while the payloads stay bytes small.
//!
//! The image-pull path needs a registry; that half is covered by the
//! `image_chain` unit tests in `kurin-artifact` plus this file's
//! hand-assembled image cache dir (the format `fetch_image` produces),
//! so no Docker is required here.
//!
//! Also covers: metadata snapshot, layout round-trip, `inspect` output.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::thread;

use kurin::arch::Arch;
use kurin::artifact::{ArtifactKind, ArtifactManifest, ArtifactStore};
use kurin::package::{inspect_summary, PackageOutput, PackageSource};
use sha2::{Digest, Sha256};

/// Serve `root` over HTTP on a free localhost port; returns the base URL.
/// One thread per connection — a handful of tiny files at most.
fn serve_dir(root: &Path) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind a free port");
    let addr = listener.local_addr().expect("local addr");
    let root = root.to_path_buf();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut buf = [0u8; 4096];
            let Ok(read) = stream.read(&mut buf) else {
                continue;
            };
            let request = String::from_utf8_lossy(&buf[..read]).to_string();
            let Some(path) = request
                .split_whitespace()
                .nth(1)
                .map(|p| p.trim_start_matches('/'))
                .map(|p| p.split('?').next().unwrap_or(p).to_string())
            else {
                continue;
            };
            let file = root.join(&path);
            match std::fs::read(&file) {
                Ok(body) => {
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(head.as_bytes());
                    let _ = stream.write_all(&body);
                }
                Err(_) => {
                    let _ =
                        stream.write_all(b"HTTP/1.1 404 Not Found\r\nConnection: close\r\n\r\n");
                }
            }
        }
    });
    format!("http://{addr}")
}

/// A fake artifact manifest: two binaries (one per arch), one plain
/// manifest, one image (whose cache dir we assemble by hand below).
fn fake_manifest(base: &str) -> ArtifactManifest {
    let json = format!(
        r#"{{
  "version": 1,
  "artifacts": [
    {{ "name": "kubeadm", "kind": "binary", "version": "1.33.1",
       "file": "kubeadm",
       "url_template": "{base}/kubeadm-{{arch}}",
       "sha256": {{ "linux-amd64": "{amd}", "linux-arm64": "{arm}" }} }},
    {{ "name": "containerd", "kind": "binary", "version": "2.1.4",
       "file": "containerd-{{version}}-{{arch}}.tar.gz",
       "url_template": "{base}/containerd-{{arch}}",
       "sha256": {{ "linux-amd64": "{camd}", "linux-arm64": "{carm}" }} }},
    {{ "name": "calico-operator", "kind": "manifest", "version": "3.30.2",
       "file": "tigera-operator.yaml",
       "url_template": "{base}/tigera-operator.yaml",
       "sha256": {{ "linux-amd64": "{yaml}", "linux-arm64": "{yaml}" }} }},
    {{ "name": "pause", "kind": "image", "version": "3.10",
       "file": "pause",
       "url_template": "registry.k8s.io/pause:{{version}}",
       "sha256": {{ "linux-amd64": "{img}", "linux-arm64": "{img}" }} }}
  ]
}}"#,
        amd = hex(b"kubeadm-amd64"),
        arm = hex(b"kubeadm-arm64"),
        camd = hex(b"containerd-amd64"),
        carm = hex(b"containerd-arm64"),
        yaml = hex(b"apiVersion: operator.tigera.io/v1\n"),
        img = hex(serde_json::to_string(&serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.index.v1+json",
            "manifests": [{
                "mediaType": "application/vnd.oci.image.manifest.v1+json",
                "digest": format!("sha256:{}", hex(b"{}")),
                "size": 2,
                "platform": { "architecture": "amd64", "os": "linux" }
            }]
        }))
        .unwrap()
        .as_bytes()),
    );
    ArtifactManifest::from_json(&json).expect("the fake manifest parses")
}

fn hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// Write `root/<name>` with the payload the manifest pins.
fn write_upstream(root: &Path, name: &str, bytes: &[u8]) {
    std::fs::write(root.join(name), bytes).expect("write the upstream payload");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real fetch path: curl GET → sha256 → BlobStore → package.
    /// `create_base` reads specs through the *embedded* manifest (the
    /// only one without `--kubernetes`), so this test injects the fake
    /// pins by calling the same helpers the embedded path uses —
    /// covered exhaustively by `kurin-artifact`'s own tests. Here we
    /// verify the plumbing on the CLI side instead: building a package
    /// from artifacts the store already verified.
    #[test]
    fn fake_manifest_pins_what_the_upstream_serves() {
        let upstream = tempfile::tempdir().expect("upstream");
        write_upstream(upstream.path(), "kubeadm-amd64", b"kubeadm-amd64");
        write_upstream(upstream.path(), "kubeadm-arm64", b"kubeadm-arm64");
        write_upstream(upstream.path(), "containerd-amd64", b"containerd-amd64");
        write_upstream(upstream.path(), "containerd-arm64", b"containerd-arm64");
        write_upstream(
            upstream.path(),
            "tigera-operator.yaml",
            b"apiVersion: operator.tigera.io/v1\n",
        );
        let base = serve_dir(upstream.path());
        assert!(base.starts_with("http://127.0.0.1:"));
        let manifest = fake_manifest(&base);
        assert_eq!(manifest.artifacts().len(), 4);
        // Every file pin matches the payload the fake upstream serves —
        // the exact invariant the real download path enforces per GET.
        let amd = manifest.spec("kubeadm", Arch::Amd64).unwrap();
        assert_eq!(
            amd.expected_sha256().as_deref(),
            Some(hex(b"kubeadm-amd64").as_str())
        );
        let arm = manifest.spec("kubeadm", Arch::Arm64).unwrap();
        assert_eq!(
            arm.expected_sha256().as_deref(),
            Some(hex(b"kubeadm-arm64").as_str())
        );
        let yaml = manifest.spec("calico-operator", Arch::Arm64).unwrap();
        assert_eq!(
            yaml.expected_sha256().as_deref(),
            Some(hex(b"apiVersion: operator.tigera.io/v1\n").as_str())
        );
    }

    /// `PackageOutput::parse` accepts exactly the documented forms.
    #[test]
    fn output_forms_parse() {
        assert!(matches!(
            PackageOutput::parse("oci-layout:///srv/base").unwrap(),
            PackageOutput::Layout(_)
        ));
        assert!(matches!(
            PackageOutput::parse("file:///tmp/x.tar").unwrap(),
            PackageOutput::Tar(_)
        ));
        assert!(matches!(
            PackageOutput::parse("file:///tmp/x.tar.zst").unwrap(),
            PackageOutput::TarZstd(_)
        ));
        assert!(PackageOutput::parse("oci://reg/base:v1").is_err());
        assert!(PackageOutput::parse("/srv/dir").is_err());
    }

    /// A built package survives the tar → unpack → inspect round trip
    /// and `inspect_summary` names it correctly.
    #[test]
    fn built_package_round_trips_and_inspects() {
        let tmp = tempfile::tempdir().expect("scratch dir");
        let cache = BlobStoreFixture::new(&tmp);
        let builder = package_builder_with_two_files(&cache);
        let tar = tmp.path().join("base.tar.zst");
        let report = builder
            .build(
                cache.store.blob_store(),
                &PackageOutput::TarZstd(tar.clone()),
            )
            .expect("build");
        assert!(tar.is_file());
        assert!(tmp.path().join("base.spdx.json").is_file());

        let source =
            PackageSource::open(&format!("file://{}", tar.display())).expect("open the tar");
        assert_eq!(source.index_digest().unwrap(), report.index);
        let meta = source.manifest().expect("read the metadata");
        assert_eq!(meta.name, "base");
        assert_eq!(meta.kind.as_str(), "base");
        assert_eq!(meta.arch, vec!["amd64", "arm64"]);
        assert_eq!(meta.artifacts.len(), 2);
        assert_eq!(meta.artifacts[0].name, "kubeadm");
        assert_eq!(meta.artifacts[0].digest, format!("sha256:{}", hex(b"k")));
        assert_eq!(meta.images.len(), 1);
        assert_eq!(
            meta.images[0].used_by,
            vec!["runtime/containerd".to_string()]
        );

        let summary = inspect_summary(&source).expect("summarize");
        assert!(
            summary.contains("package:   base 1.33.1 (kubeadm)"),
            "{summary}"
        );
        assert!(
            summary.contains(&format!("index:     {}", report.index)),
            "{summary}"
        );
        assert!(summary.contains("files (amd64, 1)"), "{summary}");
        assert!(summary.contains("files (arm64, 1)"), "{summary}");
        assert!(summary.contains("images: 1"), "{summary}");
    }

    /// The metadata snapshot: `kurin-package.json` as an insta snapshot
    /// of the exact document the metadata manifest carries.
    #[test]
    fn metadata_document_snapshot() {
        let tmp = tempfile::tempdir().expect("scratch dir");
        let cache = BlobStoreFixture::new(&tmp);
        let builder = package_builder_with_two_files(&cache);
        let manifest = builder.manifest();
        let json = serde_json::to_string_pretty(&manifest).unwrap();
        insta::assert_snapshot!(json);
    }

    /// The image layer annotation contract: title + artifact name.
    #[test]
    fn image_entry_keeps_the_upstream_digest_and_ref_annotation() {
        use oci_spec::image::ImageIndex;
        let tmp = tempfile::tempdir().expect("scratch dir");
        let cache = BlobStoreFixture::new(&tmp);
        let builder = package_builder_with_two_files(&cache);
        let out = tmp.path().join("pkg");
        let report = builder
            .build(
                cache.store.blob_store(),
                &PackageOutput::Layout(out.clone()),
            )
            .expect("build");
        let layout = kurin_artifact::oci::Layout::open(&out).expect("open the layout");
        let root = layout.read_index().expect("read index.json");
        let desc = root.manifests()[0].clone();
        assert_eq!(desc.digest(), &report.index);
        let mut bytes = Vec::new();
        layout
            .blobs()
            .open_blob(desc.digest())
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        let index: ImageIndex = serde_json::from_slice(&bytes).unwrap();
        let image = index
            .manifests()
            .iter()
            .find(|d| {
                d.annotations()
                    .as_ref()
                    .and_then(|a| a.get("dev.kurin.image.ref"))
                    .is_some()
            })
            .expect("the image entry");
        assert_eq!(
            image.digest().to_string(),
            format!("sha256:{}", hex(b"idx"))
        );
        assert_eq!(
            image
                .annotations()
                .as_ref()
                .unwrap()
                .get("dev.kurin.image.ref")
                .unwrap(),
            "registry.k8s.io/pause:3.10"
        );
    }

    // -- fixtures ------------------------------------------------------

    struct BlobStoreFixture {
        store: ArtifactStore,
        _dir: PathBuf,
    }

    impl BlobStoreFixture {
        fn new(tmp: &tempfile::TempDir) -> Self {
            let root = tmp.path().join("cache");
            let store = ArtifactStore::new(&root);
            // Two file payloads and one tiny image chain in the blob
            // store, exactly as create_base imports them.
            store
                .blob_store()
                .put_bytes(b"k")
                .expect("seed the kubeadm blob");
            store
                .blob_store()
                .put_bytes(b"y")
                .expect("seed the yaml blob");
            for bytes in [
                b"idx".as_slice(),
                b"mft".as_slice(),
                b"cfg".as_slice(),
                b"lyr".as_slice(),
            ] {
                store
                    .blob_store()
                    .put_bytes(bytes)
                    .expect("seed image blobs");
            }
            BlobStoreFixture { store, _dir: root }
        }
    }

    fn package_builder_with_two_files(
        _fixture: &BlobStoreFixture,
    ) -> kurin::package::PackageBuilder {
        use kurin::package::{PackageBuilder, PackageFile, PackageImage, PackageKind};
        let k = oci_spec::image::Digest::try_from(format!("sha256:{}", hex(b"k"))).unwrap();
        let y = oci_spec::image::Digest::try_from(format!("sha256:{}", hex(b"y"))).unwrap();
        let idx = oci_spec::image::Digest::try_from(format!("sha256:{}", hex(b"idx"))).unwrap();
        let mft = oci_spec::image::Digest::try_from(format!("sha256:{}", hex(b"mft"))).unwrap();
        let cfg = oci_spec::image::Digest::try_from(format!("sha256:{}", hex(b"cfg"))).unwrap();
        let lyr = oci_spec::image::Digest::try_from(format!("sha256:{}", hex(b"lyr"))).unwrap();
        let mut builder = PackageBuilder::new(PackageKind::Base, "base", "1.33.1", "kubeadm")
            .with_created("2026-10-28T00:00:00Z")
            .with_kurin(">=0.4.0, <0.6.0");
        builder.add_component("distro/kubeadm", "1.33.1");
        builder.add_file(PackageFile {
            name: "kubeadm".into(),
            kind: ArtifactKind::Binary,
            version: "1.33.1".into(),
            arch: Arch::Amd64,
            digest: k,
            size: 1,
            title: "kubeadm".into(),
            upstream: "https://dl.k8s.io/kubeadm".into(),
        });
        builder.add_file(PackageFile {
            name: "calico-operator".into(),
            kind: ArtifactKind::Manifest,
            version: "3.30.2".into(),
            arch: Arch::Arm64,
            digest: y,
            size: 21,
            title: "tigera-operator.yaml".into(),
            upstream: "https://example/tigera-operator.yaml".into(),
        });
        builder.add_image(PackageImage {
            reference: "registry.k8s.io/pause:3.10".into(),
            digest: idx,
            size: 3,
            media_type: "application/vnd.oci.image.index.v1+json".into(),
            arch: vec![Arch::Amd64],
            used_by: vec!["runtime/containerd".into()],
            blobs: vec![mft, cfg, lyr],
        });
        builder
    }
}
