//! WP-22: the exported JSON Schema is committed, drift-checked, and every
//! sample config validates against the schema of its own version (and,
//! after migration, against the v1alpha2 schema).

use std::path::{Path, PathBuf};

use kurin_config::schema;
use serde_json::Value;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root")
}

fn committed_schema_path() -> PathBuf {
    repo_root().join("docs/schema/cluster.v1alpha2.json")
}

/// The committed document is the snapshot: regenerating the schema must
/// reproduce it byte for byte. Fix drift with `just schema`.
#[test]
fn committed_v1alpha2_schema_matches_generated() {
    let generated = schema::schema_json("v1alpha2").expect("schema exports");
    let committed = std::fs::read_to_string(committed_schema_path())
        .expect("docs/schema/cluster.v1alpha2.json is committed");
    assert_eq!(
        committed, generated,
        "docs/schema/cluster.v1alpha2.json is stale; run `just schema`"
    );
}

#[test]
fn both_versions_export() {
    for version in schema::SCHEMA_VERSIONS {
        let value = schema::schema(version).expect("known version exports");
        assert_eq!(value["type"], "object", "{version}");
        assert!(value["properties"]["version"].is_object(), "{version}");
    }
    assert!(schema::schema("v0").is_err());
}

fn validate(version: &str, document: &Value, name: &str) {
    let schema = schema::schema(version).expect("schema exports");
    let validator = jsonschema::validator_for(&schema).expect("schema compiles");
    if let Err(error) = validator.validate(document) {
        panic!("{name} does not satisfy the {version} schema: {error}");
    }
}

fn toml_document(text: &str, name: &str) -> Value {
    let parsed: toml::Value = toml::from_str(text).unwrap_or_else(|e| panic!("{name}: {e}"));
    serde_json::to_value(parsed).expect("toml converts to json")
}

/// The WP-00 fixtures address the offline bundle with a placeholder; the
/// loader (and migration) require the directory to exist.
const OFFLINE_DIR_PLACEHOLDER: &str = "@OFFLINE_DIR@";

fn read_sample(path: &Path) -> String {
    let raw = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("read sample {}: {e}", path.display()));
    raw.replace(
        OFFLINE_DIR_PLACEHOLDER,
        &repo_root()
            .join("kurin/tests/fixtures/offline-bundle")
            .display()
            .to_string(),
    )
}

fn sample_paths() -> Vec<PathBuf> {
    let root = repo_root();
    let mut paths = Vec::new();
    for dir in ["kurin/tests/fixtures/configs", "kurin/examples"] {
        let mut found: Vec<PathBuf> = std::fs::read_dir(root.join(dir))
            .unwrap_or_else(|e| panic!("{dir}: {e}"))
            .map(|entry| entry.expect("dir entry").path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "toml"))
            .collect();
        found.sort();
        paths.extend(found);
    }
    assert!(!paths.is_empty(), "no sample configs found");
    paths
}

/// Every v1alpha1 sample validates against the v1alpha1 schema, and its
/// migrated v1alpha2 form validates against the v1alpha2 schema.
#[test]
fn every_sample_config_validates() {
    for path in sample_paths() {
        let name = path
            .strip_prefix(repo_root())
            .unwrap_or(&path)
            .display()
            .to_string();
        let text = read_sample(&path);
        let any = kurin_config::parse_any(&path, &text).expect("sample parses");
        let version = any.version().to_string();
        validate(&version, &toml_document(&text, &name), &name);

        if version == kurin_config::CONFIG_VERSION {
            let migrated = kurin_config::migrate::migrate_str(&path, &text)
                .unwrap_or_else(|e| panic!("{name}: migrate failed: {e}"));
            validate(
                kurin_config::v1alpha2::CONFIG_VERSION,
                &toml_document(&migrated, &name),
                &format!("{name} (migrated)"),
            );
        }
    }
}

/// A document the loader rejects must also be rejected by the schema for
/// the same reason: unknown keys are closed (`deny_unknown_fields`).
#[test]
fn unknown_field_fails_schema_validation() {
    let schema = schema::schema("v1alpha2").expect("schema exports");
    let validator = jsonschema::validator_for(&schema).expect("schema compiles");
    let document = serde_json::json!({
        "version": "v1alpha2",
        "name": "x",
        "not-a-field": true,
    });
    assert!(validator.validate(&document).is_err());
}
