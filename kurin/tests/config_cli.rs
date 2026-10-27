//! CLI surface for the two schema versions: `create config` defaults and
//! `kurin config migrate`.

use std::process::Command;

fn kurin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_kurin"))
}

const OLD: &str = r#"# a comment that must survive
version = "v1alpha1"
name = "cli-migrate"

[kubernetes]
distribution = "k8s"
runtime = "crio"

[ssh.auth]
method = "password"
password = "fake"

[storage]
kind = "local-path"
default = true

[nodes.n1]
address = "10.0.0.11"
role = "control-plane"
"#;

#[test]
fn create_config_defaults_to_v1alpha2() {
    let out = kurin().args(["create", "config"]).output().unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("version = \"v1alpha2\""), "output: {text}");
}

#[test]
fn create_config_schema_version_v1alpha1_prints_the_old_example() {
    let out = kurin()
        .args(["create", "config", "--schema-version", "v1alpha1"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("version = \"v1alpha1\""), "output: {text}");
}

#[test]
fn config_migrate_prints_to_stdout() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("old.toml");
    std::fs::write(&file, OLD).unwrap();
    let out = kurin()
        .args(["config", "migrate", "-f"])
        .arg(&file)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("version = \"v1alpha2\""), "output: {text}");
    assert!(
        text.contains("# a comment that must survive"),
        "output: {text}"
    );
    assert!(
        text.contains("[runtime]\nkind = \"crio\""),
        "output: {text}"
    );
    assert!(text.contains("[addons.local-path]"), "output: {text}");
}

#[test]
fn config_migrate_writes_an_equivalent_file() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("old.toml");
    let out_file = dir.path().join("new.toml");
    std::fs::write(&file, OLD).unwrap();
    let out = kurin()
        .args(["config", "migrate", "-f"])
        .arg(&file)
        .args(["-o"])
        .arg(&out_file)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let before = kurin_config::load(&file).expect("original loads");
    let after = kurin_config::load(&out_file).expect("migrated loads");
    assert!(before.same_spec(&after));
}

#[test]
fn config_migrate_rejects_an_already_migrated_file() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("new.toml");
    std::fs::write(&file, "version = \"v1alpha2\"\nname = \"x\"\n").unwrap();
    let out = kurin()
        .args(["config", "migrate", "-f"])
        .arg(&file)
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("already v1alpha2"), "stderr: {stderr}");
}
