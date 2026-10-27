//! WP-34: deploy-side package access.
//!
//! Covers the three deliverables of the work package against the public
//! API surface:
//!
//! - **Coverage diagnostics** — a base package checked against the
//!   cluster it is asked to install (Kubernetes version, distribution,
//!   CNI, every node architecture); the rendered report is pinned as a
//!   snapshot, and a fully-covering package produces no gaps.
//! - **Old-bundle compatibility** — a legacy bundle directory
//!   (`kurin-manifest.json`, no `oci-layout`) is recognised by the
//!   compatibility reader and adapted to the same
//!   [`kurin::run::ArtifactSources`] interface the package URI forms
//!   use.
//! - **`--offline <URI>` is `--package <URI>`** — both quick-create
//!   spellings lower to the same config document.

use std::path::Path;

use clap::Parser;
use kurin::arch::Arch;
use kurin::artifact::{ArtifactKind, ArtifactManifest, ArtifactStore};
use kurin::cli::quick::{self, QuickArgs, SaveDefault};
use kurin::components::coverage::{coverage_gaps, render_gaps};
use kurin::deploy::{open_payload_source, PayloadSource};
use kurin::facts::{FactsTable, NodeFacts};
use kurin::package::{
    PackageBuilder, PackageFile, PackageImage, PackageKind, PackageManifest, PackageOutput,
};
use kurin::run::{ArtifactSources, RunContext};
use kurin::zone::Zone;
use oci_spec::image::Digest;
use sha2::{Digest as _, Sha256};

// ---------------------------------------------------------------------------
// fixtures
// ---------------------------------------------------------------------------

/// A kubeadm 1.33.1 cluster, calico, one amd64 and one arm64 node.
fn cluster() -> kurin_config::Cluster {
    kurin_config::parse_str(
        Path::new("package_deploy.toml"),
        r#"version = "v1alpha1"
name = "deploy"
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
[nodes.w1]
address = "10.99.0.21"
role = "worker"
arch = "arm64"
"#,
    )
    .expect("cluster parses")
}

/// Discovered facts: every node amd64 (w1's configured arch is ignored
/// because discovery is the authority).
fn amd64_facts() -> FactsTable {
    let mut facts = FactsTable::new();
    facts.insert("cp1", NodeFacts::minimal(Arch::Amd64));
    facts.insert("w1", NodeFacts::minimal(Arch::Amd64));
    facts
}

fn hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn digest_of(bytes: &[u8]) -> Digest {
    Digest::try_from(format!("sha256:{}", hex(bytes))).unwrap()
}

/// Build a real base package into `dir` covering `cluster()`: every
/// enabled component's artifacts, aggregated the way
/// `kurin package create base` aggregates them (a fake payload blob per
/// artifact, content-addressed like the real ones).
fn build_package(dir: &Path) {
    let spec = cluster();
    let cache = ArtifactStore::new(dir.parent().unwrap().join("cache"));
    for bytes in [b"mft".as_slice(), b"cfg".as_slice(), b"lyr".as_slice()] {
        cache.blob_store().put_bytes(bytes).unwrap();
    }
    let mut builder = PackageBuilder::new(PackageKind::Base, "base", "1.33.1", "kubeadm")
        .with_created("2026-10-28T00:00:00Z");
    for component in kurin::components::registry()
        .iter()
        .filter(|c| c.enabled(&spec))
    {
        let refs = component.artifacts(&spec, Arch::Amd64);
        if refs.is_empty() {
            continue;
        }
        builder.add_component(component.id(), "1");
        for r in refs {
            let payload = format!("{}:{}", r.name, r.arch);
            cache.blob_store().put_bytes(payload.as_bytes()).unwrap();
            let digest = digest_of(payload.as_bytes());
            let size = payload.len() as u64;
            match r.kind {
                ArtifactKind::Image => builder.add_image(PackageImage {
                    reference: format!("registry.local/{}", r.name),
                    digest,
                    size,
                    media_type: "application/vnd.oci.image.index.v1+json".into(),
                    arch: vec![r.arch],
                    used_by: vec![component.id().to_string()],
                    blobs: vec![digest_of(b"mft"), digest_of(b"cfg"), digest_of(b"lyr")],
                }),
                _ => builder.add_file(PackageFile {
                    name: r.name.clone(),
                    kind: r.kind,
                    version: r.version.clone(),
                    arch: r.arch,
                    digest,
                    size,
                    title: r.name,
                    upstream: "https://example.invalid".into(),
                }),
            }
        }
    }
    builder
        .build(
            cache.blob_store(),
            &PackageOutput::Layout(dir.to_path_buf()),
        )
        .expect("package builds");
}

// ---------------------------------------------------------------------------
// coverage diagnostics
// ---------------------------------------------------------------------------

/// A package that does not cover the cluster: the report lists exactly
/// what is missing (the snapshot is the contract for the diagnostic
/// wording).
#[test]
fn coverage_diagnostics_snapshot() {
    let meta = mismatching_manifest();
    let gaps = coverage_gaps(&cluster(), &amd64_facts(), &meta);
    let report = render_gaps("oci-layout:///srv/base", &meta, &gaps);
    insta::assert_snapshot!(report);
}

/// A real package built for the cluster's selection resolves with no
/// gaps: the file-level and image-level requirements the enabled
/// components declare are all carried.
#[test]
fn a_real_package_that_matches_has_no_gaps() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("base");
    build_package(&dir);
    let meta = match open_payload_source(dir.to_str().unwrap()).unwrap() {
        PayloadSource::Package { package, .. } => package.manifest().unwrap(),
        other => panic!("expected a package, got {other:?}"),
    };
    assert!(
        coverage_gaps(&cluster(), &amd64_facts(), &meta).is_empty(),
        "a matching package must cover the cluster"
    );
}

/// A hand-written manifest missing every category the validation
/// covers. Keeping it as data (not a built package) lets the snapshot
/// test exercise the diagnostic wording deterministically.
fn mismatching_manifest() -> PackageManifest {
    PackageManifest {
        schema: 1,
        name: "base".into(),
        kind: PackageKind::Base,
        version: "1.34.0".into(),
        kurin: ">=0.6.0, <0.8.0".into(),
        created: "2026-10-28T00:00:00Z".into(),
        distribution: "k3s".into(),
        arch: vec!["arm64".into()],
        components: vec![kurin::package::PackageComponent {
            id: "cni/flannel".into(),
            version: "1".into(),
        }],
        artifacts: Vec::new(),
        images: Vec::new(),
        charts: Vec::new(),
        os: Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// old-bundle compatibility
// ---------------------------------------------------------------------------

/// A legacy bundle directory is recognised by the compatibility reader
/// and lands on the bundle path — the interface `--offline`/`--package`
/// share with the package URI forms.
#[test]
fn a_legacy_bundle_directory_is_adapted_to_the_same_interface() {
    let tmp = tempfile::tempdir().unwrap();
    write_legacy_bundle(tmp.path());
    match open_payload_source(tmp.path().to_str().unwrap()).unwrap() {
        PayloadSource::LegacyBundle { dir } => {
            assert_eq!(dir, tmp.path());
            // The run sources carry it exactly where the pre-package
            // path expects it: unchanged behaviour for old bundles.
            let run = RunContext::new(
                "op",
                "deploy",
                ArtifactSources::embedded().with_offline_dir(Some(dir)),
            );
            assert!(run.sources.is_offline());
            assert!(run.sources.offline_dir.is_some());
            assert!(run.sources.base.is_none());
        }
        other => panic!("expected a legacy bundle, got {other:?}"),
    }
}

/// A package URI reaches the same interface as the base package field.
#[test]
fn a_package_reaches_the_same_interface() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("base");
    build_package(&dir);
    match open_payload_source(dir.to_str().unwrap()).unwrap() {
        PayloadSource::Package { uri, .. } => {
            let run = RunContext::new(
                "op",
                "deploy",
                ArtifactSources::embedded().with_base_package(uri),
            );
            assert!(run.sources.is_offline());
            assert!(run.sources.base.is_some());
            assert!(run.sources.offline_dir.is_none());
        }
        other => panic!("expected a package, got {other:?}"),
    }
}

/// The `[packages]` settings all reach the run context (WP-34 step 1).
#[test]
fn package_settings_reach_the_run_context() {
    let sources = ArtifactSources {
        zone: Zone::Cn,
        base: Some("oci-layout:///srv/base".into()),
        os: vec!["oci://harbor.local/kurin/os/rocky-9:2026.10".into()],
        delivery: kurin_config::v1alpha2::Delivery::Registry,
        verify: kurin_config::v1alpha2::Verify::Signature,
        trusted_keys: vec!["/etc/kurin/cosign.pub".into()],
        ..ArtifactSources::embedded()
    };
    let run = RunContext::new("op", "deploy", sources);
    assert_eq!(run.sources.base.as_deref(), Some("oci-layout:///srv/base"));
    assert_eq!(run.sources.os.len(), 1);
    assert_eq!(
        run.sources.delivery,
        kurin_config::v1alpha2::Delivery::Registry
    );
    assert_eq!(
        run.sources.verify,
        kurin_config::v1alpha2::Verify::Signature
    );
    assert_eq!(run.sources.trusted_keys.len(), 1);
    assert!(run.sources.is_offline());
}

fn write_legacy_bundle(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join("kurin-manifest.json"),
        serde_json::to_string(&ArtifactManifest::embedded().unwrap()).unwrap(),
    )
    .unwrap();
}

// ---------------------------------------------------------------------------
// --offline is --package
// ---------------------------------------------------------------------------

/// The binary flattens `QuickArgs` into a subcommand; tests parse it the
/// same way so clap's conflicts/defaults are exercised too.
#[derive(Parser)]
struct TestCli {
    #[command(flatten)]
    quick: QuickArgs,
}

fn parse(argv: &[&str]) -> QuickArgs {
    TestCli::try_parse_from(std::iter::once("kurin").chain(argv.iter().copied()))
        .expect("arguments parse")
        .quick
}

/// `--offline <URI>` and `--package <URI>` lower to the same config
/// document: the two spellings are one switch (WP-34 step 4).
#[test]
fn offline_uri_and_package_spellings_agree() {
    let base = ["file:///media/usb/kurin-base-v1.33.1.tar.zst"];
    let via_offline = quick::build(
        None,
        &parse(&[
            "--name",
            "eq",
            "--control-plane",
            "10.0.0.1",
            "--offline",
            base[0],
        ]),
        SaveDefault::Stdout,
        false,
    )
    .unwrap();
    let via_package = quick::build(
        None,
        &parse(&[
            "--name",
            "eq",
            "--control-plane",
            "10.0.0.1",
            "--package",
            base[0],
        ]),
        SaveDefault::Stdout,
        false,
    )
    .unwrap();
    assert_eq!(via_offline.document, via_package.document);
    assert_eq!(
        via_offline.spec.packages.base,
        via_package.spec.packages.base
    );
}
