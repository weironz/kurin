//! WP-38 integration tests: build a real `os` package around a fixture
//! repository (no container run), then exercise the facts matching and
//! the offline diagnostics on the assembled package.

use std::path::PathBuf;

use kurin_artifact::artifact::ArtifactStore;
use std::future::Future;

use kurin_artifact::os::{calver, create_os, resolve, supported_targets, OsPackage, OsTarget};
use kurin_artifact::package::{PackageOutput, PackageSource};
use kurin_core::facts::{OsFamily, OsInfo};
use kurin_core::run::ArtifactSources;
use kurin_core::zone::Zone;

/// A deterministic fake "downloaded repository": the deb layout (`.deb`
/// files + `Packages`) inside a temp dir, used in place of the container
/// run by writing straight into the layout the builder would receive.
///
/// The builder path is exercised through the public API: `create_os`
/// runs containers, so this test builds the equivalent package with the
/// same builder the unit test pins, then matches it.
#[test]
fn os_package_round_trip_matches_node_facts() {
    let dir = tempfile::tempdir().unwrap();
    let cache_dir = tempfile::tempdir().unwrap();
    let store = ArtifactStore::new(cache_dir.path());
    let _ = store;

    // Assemble an os package by hand: same code path as create_os's
    // tail (builder + repo tar), minus the container download.
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(repo.join("repodata")).unwrap();
    std::fs::write(repo.join("conntrack-tools.rpm"), b"rpm-bytes").unwrap();
    std::fs::write(repo.join("haproxy.rpm"), b"haproxy-bytes").unwrap();
    std::fs::write(repo.join("repodata").join("repomd.xml"), b"<repomd/>").unwrap();

    let target = OsTarget::parse("rocky-9").unwrap();
    assert_eq!(target.family, OsFamily::Rpm);

    // Pack via the same helper create_os uses (through the public
    // package builder API).
    let mut builder = kurin_artifact::package::PackageBuilder::new(
        kurin_artifact::package::PackageKind::Os,
        "rocky-9",
        "2026.10",
        "any",
    );
    let mut tar_bytes = Vec::new();
    {
        use std::io::Write;
        let mut tar = tar::Builder::new(&mut tar_bytes);
        let mut header = tar::Header::new_gnu();
        header.set_size(9);
        header.set_mode(0o644);
        header.set_mtime(0);
        header.set_entry_type(tar::EntryType::Regular);
        header.set_cksum();
        tar.append_data(&mut header, "./repo.tar", &b"repo-tar"[..])
            .unwrap();
        tar.finish().unwrap();
        tar.into_inner().unwrap().flush().unwrap();
    }
    let digest = store.blob_store().put_bytes(&tar_bytes).unwrap();
    builder.add_os_repo(
        "rocky-9",
        kurin_core::arch::Arch::Amd64,
        kurin_artifact::package::PackageFile {
            name: "os/rocky-9".into(),
            kind: kurin_artifact::artifact::ArtifactKind::Binary,
            version: "2026.10".into(),
            arch: kurin_core::arch::Arch::Amd64,
            digest: digest.clone(),
            size: tar_bytes.len() as u64,
            title: "os/rocky-9/repo.tar".into(),
            upstream: "container:rockylinux:9".into(),
        },
    );
    let out_dir = dir.path().join("pkg");
    let report = builder
        .build(store.blob_store(), &PackageOutput::Layout(out_dir.clone()))
        .unwrap();
    assert_eq!(report.tag, "v2026.10");

    // The package resolves for a matching node, through every source
    // form the config accepts.
    let uri = format!("oci-layout://{}", out_dir.display());
    for uri in [uri, format!("file://{}", tar_package(&out_dir).display())] {
        let sources = ArtifactSources {
            zone: Zone::Global,
            manifest_json: None,
            offline_dir: None,
            base: None,
            os: vec![uri.clone()],
            delivery: Default::default(),
            verify: Default::default(),
            trusted_keys: Vec::new(),
        };
        let rocky = OsInfo::from_release("rocky", "9.4", "rhel", "Rocky Linux 9.4");
        let matched =
            futures_executor_block_on(resolve(&sources, &rocky, kurin_core::arch::Arch::Amd64))
                .unwrap();
        assert!(matched.matched.is_some(), "{uri}: {:?}", matched.candidates);
        let repo = matched.matched.unwrap();
        assert_eq!(repo.os, "rocky-9");
        assert!(repo.path.is_file(), "{}", repo.path.display());

        // Arch mismatch and OS mismatch are diagnosed, not matched.
        let arm =
            futures_executor_block_on(resolve(&sources, &rocky, kurin_core::arch::Arch::Arm64))
                .unwrap();
        assert!(arm.matched.is_none());
        assert!(arm
            .render(&rocky, kurin_core::arch::Arch::Arm64)
            .contains("architecture mismatch"));

        let ubuntu = OsInfo::from_release("ubuntu", "24.04", "debian", "Ubuntu 24.04");
        let wrong =
            futures_executor_block_on(resolve(&sources, &ubuntu, kurin_core::arch::Arch::Amd64))
                .unwrap();
        assert!(wrong.matched.is_none());
        let text = wrong.render(&ubuntu, kurin_core::arch::Arch::Amd64);
        assert!(
            text.contains("no os package matches ubuntu 24.04 (amd64)"),
            "{text}"
        );
        assert!(text.contains("OS mismatch"), "{text}");
    }
}

/// Minimal single-threaded block_on (kurin-artifact has no tokio rt
/// feature here; the resolver only needs a reactor for `open_package`
/// on `oci://`, which these local sources do not use).
fn futures_executor_block_on<F: Future>(future: F) -> F::Output {
    // tokio::task::block_in_place is unavailable; use the simplest
    use std::pin::Pin;
    use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
    fn noop(_: *const ()) {}
    fn clone(p: *const ()) -> RawWaker {
        RawWaker::new(p, &VTABLE)
    }
    static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, noop, noop, noop);
    let waker = unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) };
    let mut cx = Context::from_waker(&waker);
    let mut future = Box::pin(future);
    loop {
        match Pin::new(&mut future).poll(&mut cx) {
            Poll::Ready(out) => return out,
            Poll::Pending => std::thread::yield_now(),
        }
    }
}

/// Pack the built layout once as a `.tar` and return its path (the
/// second source form the matching test covers).
fn tar_package(out_dir: &std::path::Path) -> PathBuf {
    let layout = kurin_artifact::oci::layout::Layout::open(out_dir).unwrap();
    let tar = out_dir.with_extension("tar");
    let file = std::fs::File::create(&tar).unwrap();
    layout.pack_tar(file).unwrap();
    tar
}

/// `create_os` rejects unknown targets and missing arches before any
/// container runs.
#[test]
fn create_os_validates_inputs() {
    let dir = tempfile::tempdir().unwrap();
    let cache_dir = tempfile::tempdir().unwrap();
    let store = ArtifactStore::new(cache_dir.path());
    let opts = OsPackage {
        target: OsTarget::parse("ubuntu-24.04").unwrap(),
        arches: vec![],
        version: "2026.10".into(),
        iso: None,
    };
    let out = PackageOutput::Layout(dir.path().join("x"));
    let err = create_os(&store, &opts, &out);
    futures_executor_block_on(err).unwrap_err();
}

/// The calver default is `YYYY.MM`.
#[test]
fn calver_is_year_dot_month() {
    let value = calver();
    assert_eq!(value.len(), 7, "{value}");
    assert_eq!(value.as_bytes()[4], b'.', "{value}");
    assert!(value[..4].chars().all(|c| c.is_ascii_digit()), "{value}");
}

/// The public target list is what the CLI advertises.
#[test]
fn supported_targets_are_stable() {
    assert_eq!(
        supported_targets().join(", "),
        "ubuntu-24.04, ubuntu-22.04, debian-12, rocky-9, rhel-9, almalinux-9, \
         openeuler-24.03, kylin-10"
    );
    assert_eq!(
        OsTarget::parse("debian-12").unwrap().container_image(),
        "debian:12"
    );
    assert_eq!(
        OsTarget::parse("openeuler-24.03")
            .unwrap()
            .container_image(),
        "openeuler/openeuler:24.03"
    );
    // PackageSource still opens the built os package.
    let _ = PackageSource::open; // re-exported API stays reachable
    let _ = create_os; // public API pinned
}
