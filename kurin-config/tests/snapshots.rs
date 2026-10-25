//! Snapshot tests for the error paths: every rejection must carry a
//! human-actionable hint. serde_yml sometimes reports no source location;
//! we snapshot only the hint/message text, never the span.

use std::path::Path;

use kurin_config::{parse_str, ConfigError};

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

/// Reduce an error to its stable, span-free text (hint or parse message).
fn reason(e: &ConfigError) -> String {
    match e {
        ConfigError::Invalid { hint, .. } => hint.clone(),
        ConfigError::Parse { message, .. } => message.clone(),
        other => other.to_string(),
    }
}

#[test]
fn snapshot_unknown_field_hint() {
    let typo = GOOD.replace("pod-cidr", "pod-cider");
    let err = parse_str(Path::new("x.toml"), &typo).unwrap_err();
    insta::assert_snapshot!("error_unknown_field", reason(&err));
}

#[test]
fn snapshot_version_gate_hint() {
    let old = GOOD.replace("v1alpha1", "v1alpha0");
    let err = parse_str(Path::new("x.toml"), &old).unwrap_err();
    insta::assert_snapshot!("error_version_gate", reason(&err));
}

#[test]
fn snapshot_duplicate_address_hint() {
    let bad = GOOD.replace(
        "[nodes.worker01]\naddress = \"10.99.0.21\"",
        "[nodes.worker01]\naddress = \"10.99.0.21\"\ninternal-address = \"10.99.0.11\"",
    );
    let err = parse_str(Path::new("x.toml"), &bad).unwrap_err();
    insta::assert_snapshot!("error_duplicate_address", reason(&err));
}

#[test]
fn snapshot_bad_node_name_hint() {
    let bad = GOOD.replace("[nodes.worker01]", "[nodes.Worker_01]");
    let err = parse_str(Path::new("x.toml"), &bad).unwrap_err();
    insta::assert_snapshot!("error_bad_node_name", reason(&err));
}

#[test]
fn snapshot_pod_cidr_out_of_range_hint() {
    let bad = GOOD.replace("10.233.64.0/18", "10.233.64.0/7");
    let err = parse_str(Path::new("x.toml"), &bad).unwrap_err();
    insta::assert_snapshot!("error_pod_cidr_out_of_range", reason(&err));
}

#[test]
fn snapshot_vip_inside_pod_cidr_hint() {
    let bad = GOOD
        .replace(
            "[nodes.master01]",
            "[ha]\nvip = \"10.233.64.5\"\n\n[nodes.master01]",
        )
        .replace(
            "[nodes.worker01]",
            "[nodes.master02]\naddress = \"10.99.0.12\"\nrole = \"control-plane\"\n\n[nodes.worker01]",
        );
    let err = parse_str(Path::new("x.toml"), &bad).unwrap_err();
    insta::assert_snapshot!("error_vip_inside_pod_cidr", reason(&err));
}

#[test]
fn snapshot_yaml_syntax_error_hint() {
    // serde_yml may fail to locate the span; the message must still be stable.
    let broken = "version: v1alpha1\nnodes: [unclosed\n";
    let err = parse_str(Path::new("x.yaml"), broken).unwrap_err();
    insta::assert_snapshot!("error_yaml_syntax", reason(&err));
}

#[test]
fn snapshot_missing_auth_hint() {
    let noauth = GOOD.replace("[ssh.auth]\nmethod = \"password\"\npassword = \"s3cret\"\n", "");
    let err = parse_str(Path::new("x.toml"), &noauth).unwrap_err();
    insta::assert_snapshot!("error_missing_auth", reason(&err));
}

#[test]
fn snapshot_example_roundtrips() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    for (name, path) in [
        ("example_roundtrip_toml", root.join("kurin/examples/cluster.example.toml")),
        (
            "example_roundtrip_yaml",
            root.join("kurin-config/examples/cluster.example.yaml"),
        ),
    ] {
        let src = std::fs::read_to_string(&path).unwrap();
        let cfg = kurin_config::parse(&path, &src).unwrap();
        let serialized = match path.extension().and_then(|e| e.to_str()) {
            Some("toml") => toml::to_string(&cfg).unwrap(),
            _ => serde_yml::to_string(&cfg).unwrap(),
        };
        let again = kurin_config::parse(&path, &serialized).unwrap();
        assert_eq!(format!("{cfg:?}"), format!("{again:?}"));
        insta::assert_snapshot!(name, serialized);
    }
}
