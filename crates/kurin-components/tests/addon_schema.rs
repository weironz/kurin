//! WP-40: the exported addon.yaml JSON Schema is committed, drift-checked,
//! and every fixture addon validates against it.

use std::path::Path;

use kurin_components::addon::schema_json;
use rust_embed::RustEmbed;

fn repo_root() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .unwrap()
        .to_path_buf()
}

/// The two committed fixture addons.
#[derive(RustEmbed)]
#[folder = "tests/fixtures/builtin-catalog"]
struct FixtureCatalog;

/// The committed document is the snapshot: regenerating the schema must
/// reproduce it byte for byte. Fix drift with `just addon-schema`.
#[test]
fn committed_addon_schema_matches_generated() {
    let committed =
        std::fs::read_to_string(repo_root().join("docs/schema/addon.v1alpha1.json")).unwrap();
    assert_eq!(
        committed,
        schema_json(),
        "run `just addon-schema` to fix the drift"
    );
}

/// Every fixture addon.yaml validates against the exported schema: what
/// the loader accepts is what the schema allows.
#[test]
fn fixture_addons_validate_against_the_schema() {
    let schema = serde_json::from_str::<serde_json::Value>(&schema_json()).unwrap();
    let validator = jsonschema::validator_for(&schema).expect("addon schema compiles");

    for path in FixtureCatalog::iter() {
        if !path.ends_with("addon.yaml") {
            continue;
        }
        let text =
            String::from_utf8(FixtureCatalog::get(&path).unwrap().data.into_owned()).unwrap();
        let document: serde_json::Value = serde_yml::from_str(&text).unwrap();
        validator
            .validate(&document)
            .unwrap_or_else(|e| panic!("{}: {e}", path));
    }
}

/// A document the loader rejects must also be rejected by the schema:
/// unknown keys are closed (`deny_unknown_fields`).
#[test]
fn unknown_field_fails_schema_validation() {
    let schema = serde_json::from_str::<serde_json::Value>(&schema_json()).unwrap();
    let validator = jsonschema::validator_for(&schema).unwrap();
    let document: serde_json::Value = serde_yml::from_str(
        r#"
apiVersion: addons.kurin.dev/v1alpha1
kind: Addon
metadata:
  name: sample
  version: 1.0.0
  description: 样例
  category: other
spec:
  compat: {}
  namespace: default
  install: {}
  extra: true
"#,
    )
    .unwrap();
    assert!(validator.validate(&document).is_err());
}
