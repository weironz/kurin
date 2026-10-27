//! WP-44: `kurin addon` (03 §4.6).
//!
//! Covers the catalog listing and `info` rendering, the `--schema` output,
//! the `[addons.<name>]` rewrite (`toml_edit`, comments preserved), the
//! operation scoping (one addon per run, plus its `requires`), the
//! values-schema rejection, and the `--dry-run` plan printed by the real
//! binary.
//!
//! Every addon comes from a `source = "dir://…"` fixture, so the tests do
//! not depend on the built-in catalog (WP-46).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use kurin::cli::addon::{self, InfoArgs, InstallArgs, RemoveArgs, UpgradeArgs};
use tempfile::TempDir;

/// The addon fixtures (`kurin/tests/fixtures/addons/<name>/`).
fn addons_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/addons")
}

/// Write the config fixture into `dir`, with the addon directory filled in.
fn config(dir: &Path) -> PathBuf {
    let template = include_str!("fixtures/configs/addons.toml");
    let text = template.replace("@ADDONS_DIR@", &addons_dir().display().to_string());
    let path = dir.join("cluster.toml");
    std::fs::write(&path, text).expect("write config");
    path
}

/// Replace the machine-specific fixture path so snapshots are portable.
fn normalize(text: &str) -> String {
    text.replace(&addons_dir().display().to_string(), "@ADDONS_DIR@")
}

fn install_args(file: &Path) -> InstallArgs {
    InstallArgs {
        name: "metrics-server".to_string(),
        file: file.to_path_buf(),
        version: None,
        set: Vec::new(),
        values: None,
        dry_run: false,
        no_save: false,
    }
}

#[test]
fn list_renders_the_catalog_and_the_inventory() {
    let tmp = TempDir::new().unwrap();
    let file = config(tmp.path());
    let cluster = kurin_config::load(&file).expect("config loads");
    let catalog = addon::catalog(Some(&cluster)).expect("catalog resolves");
    let mut installed = BTreeMap::new();
    installed.insert("metrics-server".to_string(), "0.7.2".to_string());
    let rows = addon::rows(&catalog, Some(&cluster), &installed);
    insta::assert_snapshot!("list", addon::render_list(&rows));
}

#[test]
fn list_marks_configured_addons_without_a_cluster() {
    let tmp = TempDir::new().unwrap();
    let file = config(tmp.path());
    let cluster = kurin_config::load(&file).expect("config loads");
    let catalog = addon::catalog(Some(&cluster)).expect("catalog resolves");
    let rows = addon::rows(&catalog, Some(&cluster), &BTreeMap::new());
    let text = addon::render_list(&rows);
    assert!(text.contains("demo") && text.contains("disabled"), "{text}");
    assert!(text.contains("metrics-server"), "{text}");
}

#[test]
fn info_renders_the_metadata() {
    let tmp = TempDir::new().unwrap();
    let file = config(tmp.path());
    let cluster = kurin_config::load(&file).expect("config loads");
    let catalog = addon::catalog(Some(&cluster)).expect("catalog resolves");
    let addon = catalog.get("metrics-server").expect("fixture is loaded");
    insta::assert_snapshot!("info", normalize(&addon::render_info(addon)));
}

#[test]
fn info_schema_prints_the_values_schema() {
    let tmp = TempDir::new().unwrap();
    let file = config(tmp.path());
    let cluster = kurin_config::load(&file).expect("config loads");
    let catalog = addon::catalog(Some(&cluster)).expect("catalog resolves");
    let addon = catalog.get("metrics-server").expect("fixture is loaded");
    let schema = addon
        .files()
        .read("values.schema.json")
        .expect("schema exists");
    let schema: serde_json::Value = serde_json::from_slice(&schema).unwrap();
    insta::assert_json_snapshot!("values_schema", schema);
}

#[test]
fn install_rewrites_the_config_and_keeps_comments() {
    let tmp = TempDir::new().unwrap();
    let file = config(tmp.path());
    let change = addon::install(&install_args(&file)).expect("install plans");
    assert!(change.save, "install saves by default");
    // The file is untouched until `commit`: a failed operation must not
    // claim the addon is enabled.
    let before = std::fs::read_to_string(&file).unwrap();
    assert!(before.contains("enabled = false"));
    insta::assert_snapshot!("install_document", normalize(&change.document));

    change.commit(&file).expect("commit writes the config");
    let after = std::fs::read_to_string(&file).unwrap();
    assert!(after.contains("# Disabled on purpose"), "comment survives");
    assert!(after.contains("[addons.metrics-server]\nenabled = true"));
    assert_eq!(change.scoped.addons.len(), 1, "one addon per operation");
    assert!(change.scoped.addons["metrics-server"].enabled);
}

#[test]
fn install_merges_set_and_values_file() {
    let tmp = TempDir::new().unwrap();
    let file = config(tmp.path());
    let values = tmp.path().join("values.yaml");
    std::fs::write(&values, "args:\n  - --kubelet-insecure-tls\n").unwrap();
    let change = addon::install(&InstallArgs {
        set: vec!["replicas=3".to_string()],
        values: Some(values),
        ..install_args(&file)
    })
    .expect("install plans");
    insta::assert_snapshot!("install_values", normalize(&change.document));
    let entry = &change.spec.addons["metrics-server"];
    let values = entry.values.as_ref().expect("values are set");
    assert_eq!(values.get("replicas").and_then(|v| v.as_integer()), Some(3));
}

#[test]
fn install_rejects_values_outside_the_schema() {
    let tmp = TempDir::new().unwrap();
    let file = config(tmp.path());
    let err = addon::install(&InstallArgs {
        set: vec!["replicas=0".to_string()],
        ..install_args(&file)
    })
    .expect_err("replicas must be >= 1");
    let text = format!("{err:?}");
    assert!(text.contains("replicas"), "{text}");
    assert!(text.contains("minimum"), "{text}");
}

#[test]
fn install_rejects_an_unknown_addon() {
    let tmp = TempDir::new().unwrap();
    let file = config(tmp.path());
    let err = addon::install(&InstallArgs {
        name: "nope".to_string(),
        ..install_args(&file)
    })
    .expect_err("unknown addon");
    assert!(format!("{err:?}").contains("unknown addon `nope`"));
}

#[test]
fn install_rejects_a_version_the_catalog_lacks() {
    let tmp = TempDir::new().unwrap();
    let file = config(tmp.path());
    let err = addon::install(&InstallArgs {
        version: Some("9.9.9".to_string()),
        ..install_args(&file)
    })
    .expect_err("the catalog has 0.7.2 only");
    assert!(format!("{err:?}").contains("not provided by the catalog"));
}

#[test]
fn install_scopes_the_operation_to_the_addon_and_its_requires() {
    let tmp = TempDir::new().unwrap();
    let file = config(tmp.path());
    let change = addon::install(&InstallArgs {
        name: "demo".to_string(),
        ..install_args(&file)
    })
    .expect("install plans");
    let mut names: Vec<&str> = change.scoped.addons.keys().map(String::as_str).collect();
    names.sort();
    assert_eq!(names, ["demo", "metrics-server"]);
    assert!(
        change.spec.addons["metrics-server"].enabled,
        "the requires entry is enabled in the saved config"
    );
}

#[test]
fn upgrade_pins_the_catalog_version() {
    let tmp = TempDir::new().unwrap();
    let file = config(tmp.path());
    addon::install(&install_args(&file))
        .expect("install plans")
        .commit(&file)
        .expect("install saves");
    let change = addon::upgrade(&UpgradeArgs {
        name: "metrics-server".to_string(),
        file: file.clone(),
        version: None,
        no_save: false,
    })
    .expect("upgrade plans");
    insta::assert_snapshot!("upgrade_document", normalize(&change.document));
    assert_eq!(
        change.spec.addons["metrics-server"].version.as_deref(),
        Some("0.7.2")
    );
}

#[test]
fn upgrade_rejects_a_disabled_addon() {
    let tmp = TempDir::new().unwrap();
    let file = config(tmp.path());
    let err = addon::upgrade(&UpgradeArgs {
        name: "metrics-server".to_string(),
        file,
        version: None,
        no_save: false,
    })
    .expect_err("the fixture entry is disabled");
    assert!(format!("{err:?}").contains("is disabled"));
}

#[test]
fn remove_disables_the_entry_and_scopes_the_operation() {
    let tmp = TempDir::new().unwrap();
    let file = config(tmp.path());
    addon::install(&install_args(&file))
        .expect("install plans")
        .commit(&file)
        .expect("install saves");
    let change = addon::remove(&RemoveArgs {
        name: "metrics-server".to_string(),
        file,
        keep_crds: false,
        no_save: false,
    })
    .expect("remove plans");
    insta::assert_snapshot!("remove_document", normalize(&change.document));
    assert_eq!(change.scoped.addons.len(), 1);
    assert!(!change.scoped.addons["metrics-server"].enabled);
    assert_eq!(
        change.scoped.addons["metrics-server"]
            .values
            .as_ref()
            .unwrap()["replicas"]
            .as_integer(),
        Some(1),
        "values survive the remove so a reinstall keeps them"
    );
}

#[test]
fn remove_rejects_an_undeclared_addon() {
    let tmp = TempDir::new().unwrap();
    let file = config(tmp.path());
    let err = addon::remove(&RemoveArgs {
        name: "nope".to_string(),
        file,
        keep_crds: false,
        no_save: false,
    })
    .expect_err("not declared");
    assert!(format!("{err:?}").contains("is not declared"));
}

// ---------------------------------------------------------------------------
// the real binary: `--dry-run` and the config file staying untouched
// ---------------------------------------------------------------------------

fn run_cli(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_kurin"))
        .args(args)
        .env("XDG_STATE_HOME", dir.join("state"))
        .output()
        .expect("the binary runs")
}

#[test]
fn dry_run_prints_the_plan_and_leaves_the_config_alone() {
    let tmp = TempDir::new().unwrap();
    let file = config(tmp.path());
    let before = std::fs::read_to_string(&file).unwrap();
    let output = run_cli(
        tmp.path(),
        &[
            "addon",
            "install",
            "metrics-server",
            "-f",
            file.to_str().unwrap(),
            "--dry-run",
        ],
    );
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    insta::assert_snapshot!("dry_run_plan", String::from_utf8_lossy(&output.stdout));
    assert_eq!(std::fs::read_to_string(&file).unwrap(), before);
}

#[test]
fn list_without_a_config_prints_the_header() {
    let tmp = TempDir::new().unwrap();
    let output = run_cli(tmp.path(), &["addon", "list"]);
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "NAME  VERSION  CATEGORY  STATUS  DESCRIPTION\n"
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("built-in addon catalog is empty"));
}

#[test]
fn info_schema_is_pipeable_from_the_binary() {
    let tmp = TempDir::new().unwrap();
    let file = config(tmp.path());
    let output = run_cli(
        tmp.path(),
        &[
            "addon",
            "info",
            "metrics-server",
            "-f",
            file.to_str().unwrap(),
            "--schema",
        ],
    );
    assert!(output.status.success());
    let schema: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    assert_eq!(schema["title"], "metrics-server values");
    // `--schema` prints the schema only: no metadata block.
    assert!(!String::from_utf8_lossy(&output.stdout).contains("description:"));
}

#[test]
fn info_reports_an_unknown_addon() {
    let tmp = TempDir::new().unwrap();
    let file = config(tmp.path());
    let err = addon::info(&InfoArgs {
        name: "nope".to_string(),
        file: Some(file),
        schema: false,
    })
    .expect_err("unknown addon");
    assert!(format!("{err:?}").contains("unknown addon `nope`"));
}

#[test]
fn help_lists_every_subcommand() {
    let tmp = TempDir::new().unwrap();
    let output = run_cli(tmp.path(), &["addon", "--help"]);
    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stdout);
    for command in ["list", "info", "install", "upgrade", "remove"] {
        assert!(text.contains(command), "{command} missing from:\n{text}");
    }
}
