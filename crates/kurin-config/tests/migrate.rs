//! `kurin config migrate` integration tests: every v1alpha1 sample in the
//! WP-00 matrix migrates, the migrated document lowers to the *same*
//! `ClusterSpec` as the original, and comments survive the rewrite.

use std::path::{Path, PathBuf};

use kurin_config::migrate::migrate_str;
use kurin_config::parse_str;

/// The WP-00 fixture matrix (`kurin/tests/fixtures/configs`).
const FIXTURES: &[&str] = &[
    "single.toml",
    "ha-kubevip-3cp.toml",
    "kurin-lb.toml",
    "external-endpoint.toml",
    "k3s-single.toml",
    "k3s-ha.toml",
    "k3s-kurin-lb.toml",
    "crio.toml",
    "cni-flannel.toml",
    "cni-cilium.toml",
    "cni-kube-ovn.toml",
    "cni-none.toml",
    "registry-zot.toml",
    "registry-docker.toml",
    "registry-harbor.toml",
    "storage-local-path.toml",
    "storage-nfs.toml",
    "offline.toml",
    "zone-cn.toml",
    "labels.toml",
    "mixed-arch.toml",
];

const OFFLINE_DIR_PLACEHOLDER: &str = "@OFFLINE_DIR@";

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn fixture(name: &str) -> PathBuf {
    root().join("kurin/tests/fixtures/configs").join(name)
}

fn offline_bundle_dir() -> PathBuf {
    root().join("kurin/tests/fixtures/offline-bundle")
}

/// Read a fixture, injecting the offline-dir placeholder the same way the
/// plan snapshots do.
fn read_fixture(path: &Path) -> String {
    let raw = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("read fixture {}: {e}", path.display()));
    raw.replace(
        OFFLINE_DIR_PLACEHOLDER,
        &offline_bundle_dir().display().to_string(),
    )
}

#[test]
fn every_v1alpha1_sample_migrates_and_is_equivalent() {
    for name in FIXTURES {
        let path = fixture(name);
        let src = read_fixture(&path);
        let out = migrate_str(&path, &src)
            .unwrap_or_else(|e| panic!("fixture {name} must migrate: {e:?}"));

        let before = parse_str(&path, &src).expect("original loads");
        let after = parse_str(&path, &out).expect("migrated document loads");
        assert!(
            before.same_spec(&after),
            "fixture {name}: migrated ClusterSpec differs from the v1alpha1 original\n{out}"
        );

        // Every comment line survives verbatim (toml_edit preserves decor).
        for line in src.lines().filter(|l| l.trim_start().starts_with('#')) {
            assert!(
                out.contains(line.trim_end()),
                "fixture {name}: comment lost: {line:?}"
            );
        }

        // Normalize the machine-specific offline path before snapshotting.
        let snapshot = out.replace(
            &offline_bundle_dir().display().to_string(),
            OFFLINE_DIR_PLACEHOLDER,
        );
        insta::assert_snapshot!(
            format!("migrated__{}", name.trim_end_matches(".toml")),
            snapshot
        );
    }
}

#[test]
fn example_config_migrates() {
    let path = root().join("kurin/examples/cluster.example.toml");
    let src = std::fs::read_to_string(&path).expect("read example");
    let out = migrate_str(&path, &src).expect("example migrates");
    let before = parse_str(&path, &src).expect("original loads");
    let after = parse_str(&path, &out).expect("migrated loads");
    assert!(before.same_spec(&after));
    insta::assert_snapshot!("migrated__example", out);
}

#[test]
fn v1alpha2_example_loads() {
    let path = root().join("kurin/examples/cluster.example.v1alpha2.toml");
    let spec = kurin_config::load(&path).expect("v1alpha2 example loads");
    assert_eq!(spec.name, "prod");
    assert_eq!(spec.distribution, kurin_config::Distribution::K8s);
    assert_eq!(spec.control_plane().count(), 3);
}

#[test]
fn v1alpha1_storage_maps_to_addons() {
    let path = fixture("storage-local-path.toml");
    let src = read_fixture(&path);
    let spec = parse_str(&path, &src).expect("loads");
    let addon = spec.addons.get("local-path").expect("local-path addon");
    assert!(addon.enabled);
    assert_eq!(
        addon
            .values
            .as_ref()
            .and_then(|v| v.get("default-class"))
            .and_then(|v| v.as_bool()),
        Some(true)
    );
}

#[test]
fn unknown_addon_names_are_rejected() {
    let toml = r#"version = "v1alpha2"
name = "x"

[ssh.auth]
method = "password"
password = "fake"

[nodes.n1]
address = "10.0.0.11"
role = "control-plane"

[addons.definitely-not-an-addon]
enabled = true
"#;
    let err = parse_str(Path::new("x.toml"), toml).expect_err("must be rejected");
    let msg = format!("{err}");
    assert!(msg.contains("unknown addon"), "message: {msg}");
    assert!(msg.contains("metrics-server"), "message: {msg}");
}

#[test]
fn v1alpha1_storage_and_its_v1alpha2_form_are_the_same_spec() {
    // WP-47: `[storage]` is only a v1alpha1 spelling. Both versions must
    // lower to exactly the same addon entry (names + values), so the
    // addon engine cannot tell which file it came from.
    for (name, addon, values) in [
        (
            "storage-local-path.toml",
            "local-path",
            vec![("default-class", toml::Value::Boolean(true))],
        ),
        (
            "storage-nfs.toml",
            "nfs-subdir",
            vec![
                ("default-class", toml::Value::Boolean(false)),
                ("server", toml::Value::String("192.168.10.50".to_string())),
                ("path", toml::Value::String("/export/kurin".to_string())),
            ],
        ),
    ] {
        let path = fixture(name);
        let src = read_fixture(&path);
        let v1 = parse_str(&path, &src).expect("v1alpha1 loads");
        let migrated = migrate_str(&path, &src).expect("migrates");
        let v2 = parse_str(&path, &migrated).expect("v1alpha2 loads");

        assert!(v1.same_spec(&v2), "{name}: specs differ");
        let entry = v1.addons.get(addon).expect("storage addon present");
        assert!(entry.enabled, "{name}: addon must be enabled");
        let table = entry
            .values
            .as_ref()
            .and_then(|v| v.as_table())
            .expect("values table");
        for (key, expected) in values {
            assert_eq!(table.get(key), Some(&expected), "{name}: {key}");
        }
        assert_eq!(v1.addons, v2.addons, "{name}: addon maps differ");
    }
}
