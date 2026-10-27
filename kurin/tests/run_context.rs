//! WP-07: per-run context isolation and Blackboard persistence.
//!
//! Two runs with different zones/manifests must never see each other's
//! artifact URLs (the pre-WP-07 code kept both in process globals), and the
//! join material / k3s token must survive `--resume` through the 0600
//! run-state file.

use std::sync::Arc;
use std::thread;

use kurin::artifact::{install_manifest, ArtifactManifest};
use kurin::blackboard::{Blackboard, JoinMaterial, JOIN_MATERIAL, K3S_TOKEN};
use kurin::run::{ArtifactSources, RunContext};
use kurin::zone::{rewrite_image_ref, Zone};

fn image_url(run: &RunContext, name: &str) -> String {
    let manifest = install_manifest(&run.sources).expect("run manifest parses");
    let spec = manifest
        .spec(name, kurin::arch::Arch::Amd64)
        .expect("image spec resolves");
    rewrite_image_ref(&spec.url(), run.sources.zone)
}

#[test]
fn parallel_runs_do_not_share_zone_or_manifest() {
    let mut modified = ArtifactManifest::embedded().expect("embedded manifest");
    let entry = modified
        .artifacts
        .iter_mut()
        .find(|a| a.name == "pause")
        .expect("pause entry");
    entry.version = "9.9.9".to_string();

    let cn = Arc::new(RunContext::new(
        "op-cn",
        "cluster-cn",
        ArtifactSources::embedded().with_zone(Zone::Cn),
    ));
    let custom = Arc::new(RunContext::new(
        "op-custom",
        "cluster-custom",
        ArtifactSources {
            zone: Zone::Global,
            manifest_json: Some(serde_json::to_string(&modified).expect("manifest encodes")),
            offline_dir: None,
        },
    ));

    // Resolve in parallel: a process-global zone/manifest would make one
    // thread observe the other's values.
    let a = {
        let cn = Arc::clone(&cn);
        thread::spawn(move || image_url(&cn, "pause"))
    };
    let b = {
        let custom = Arc::clone(&custom);
        thread::spawn(move || image_url(&custom, "pause"))
    };
    let (cn_url, custom_url) = (a.join().unwrap(), b.join().unwrap());

    assert!(
        cn_url.contains("k8s.m.daocloud.io"),
        "cn run uses the mirror: {cn_url}"
    );
    assert!(
        !cn_url.contains("9.9.9"),
        "cn run must keep the embedded version: {cn_url}"
    );
    assert!(
        custom_url.contains("registry.k8s.io"),
        "global run stays upstream: {custom_url}"
    );
    assert!(
        custom_url.contains("9.9.9"),
        "custom run uses its own manifest: {custom_url}"
    );
    assert_ne!(cn_url, custom_url);

    // The contexts themselves stay independent too.
    assert_eq!(cn.sources.zone, Zone::Cn);
    assert_eq!(custom.sources.zone, Zone::Global);
}

#[test]
fn join_material_round_trips_and_resumes_from_the_bb_file() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("runs/demo.bb.json");
    let material = JoinMaterial {
        join_command: "kubeadm join 10.0.0.1:6443 --token fake.token".into(),
        certificate_key: "0123456789abcdef".into(),
    };

    let bb = Blackboard::open(&path, false).expect("fresh blackboard");
    bb.set(JOIN_MATERIAL, material.clone())
        .expect("store join material");
    bb.set(K3S_TOKEN, "K10fake::node:abc".to_string())
        .expect("store k3s token");

    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "run-state secrets must be 0600");

    // `--resume` opens the same file and hands downstream tasks the values.
    let resumed = Blackboard::open(&path, true).expect("resume blackboard");
    assert_eq!(resumed.get(&JOIN_MATERIAL), Some(material));
    assert_eq!(
        resumed.get(&K3S_TOKEN).as_deref(),
        Some("K10fake::node:abc")
    );
}

#[test]
fn resume_without_a_file_is_an_empty_blackboard() {
    let dir = tempfile::tempdir().unwrap();
    let bb = Blackboard::open(dir.path().join("missing.bb.json"), true).unwrap();
    assert!(bb.get(&JOIN_MATERIAL).is_none());
    assert!(bb.get(&K3S_TOKEN).is_none());
}
