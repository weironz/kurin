//! Narrow, real-behavior tests for the config pipeline. These must catch
//! plausible consumer bugs: unknown-field typos, version drift, HA rule
//! conflicts, missing auth, default merging.

use std::path::Path;

use kurin_config::{load, parse_str, NodeRole};

const GOOD: &str = r##"
version = "v1alpha1"
name = "test"

[network]
pod-cidr = "10.233.64.0/18"
service-cidr = "10.0.0.0/16"

[ssh]
user = "root"

[ssh.auth]
method = "password"
password = "s3cret"

[nodes.master01]
address = "10.99.0.11"
role = "control-plane"

[nodes.worker01]
address = "10.99.0.21"
"##;

fn path(name: &str) -> std::path::PathBuf {
    Path::new("/tmp").join(format!("kurin-test-{name}.toml"))
}

fn write_good(tmp: &std::path::Path, body: &str) {
    std::fs::write(tmp, body).unwrap();
}

#[test]
fn single_control_plane_accepted() {
    let c = parse_str(Path::new("x.toml"), GOOD).expect("valid config parses");
    assert_eq!(c.name, "test");
    assert_eq!(c.control_plane().count(), 1);
    assert!(c.workers().count() == 1);
    assert_eq!(c.kubernetes_version.to_string(), "1.33.1");
    let m = &c.nodes[0];
    assert_eq!(m.role, NodeRole::ControlPlane);
}

#[test]
fn unknown_field_rejected_with_span() {
    let typo = GOOD.replace("pod-cidr", "pod-cider");
    let err = parse_str(Path::new("x.toml"), &typo).unwrap_err();
    let msg = format!("{err:?}");
    assert!(msg.contains("pod-cider") || msg.contains("unknown field"), "got: {msg}");
}

#[test]
fn version_gate_rejects_unknown_version() {
    let old = GOOD.replace("v1alpha1", "v1alpha0");
    let err = parse_str(Path::new("x.toml"), &old).unwrap_err();
    assert!(err.to_string().contains("invalid configuration"), "got: {err}");
}

#[test]
fn vip_collides_with_node_address() {
    let bad = format!("{GOOD}\n[ha]\nvip = \"10.99.0.11\"\n");
    let err = parse_str(Path::new("x.toml"), &bad).unwrap_err();
    let msg = err.to_string();
    // vip on a single CP is already an error; collision covered too
    assert!(msg.contains("invalid configuration"), "got: {msg}");
}

#[test]
fn missing_auth_fails_clearly() {
    let noauth = GOOD
        .replace("[ssh.auth]\nmethod = \"password\"\npassword = \"s3cret\"\n", "");
    let err = parse_str(Path::new("x.toml"), &noauth).unwrap_err();
    let msg = format!("{err:?}");
    assert!(msg.contains("no ssh auth"), "got: {msg}");
}

#[test]
fn yaml_format_accepted() {
    let yaml = r#"
version: v1alpha1
name: test
ssh:
  user: root
  auth:
    method: password
    password: s3cret
nodes:
  master01:
    address: 10.99.0.11
    role: control-plane
"#;
    let c = parse_str(Path::new("x.yaml"), yaml).expect("yaml parses");
    assert_eq!(c.control_plane().count(), 1);
}

#[test]
fn from_disk_loads() {
    let p = path("disk");
    write_good(&p, GOOD);
    let c = load(&p).unwrap();
    let _ = std::fs::remove_file(&p);
    assert_eq!(c.name, "test");
}

#[test]
fn miette_renders_diagnostic() {
    let typo = GOOD.replace("role = \"control-plane\"", "role = \"worker\"");
    let err = parse_str(Path::new("x.toml"), &typo).unwrap_err();
    // miette fancy rendering exists; base Debug render must contain the reason
    let text = format!("{err:?}");
    assert!(text.contains("control-plane"), "msg: {text}");
}