//! JSON Schema export (WP-40): `docs/schema/addon.v1alpha1.json` is the
//! committed copy of the schema derived from the same Rust types the
//! loader parses into, so it cannot drift from the accepted document
//! shape. The drift test in `tests/addon_schema.rs` fails when the two
//! disagree.

use serde_json::Value;

/// JSON Schema for `addon.yaml` as a [`serde_json::Value`].
pub fn schema() -> Value {
    schemars::schema_for!(crate::addon::spec::AddonManifest).to_value()
}

/// Pretty-printed JSON Schema for `addon.yaml`, newline-terminated.
///
/// This is the exact text written to `docs/schema/addon.v1alpha1.json`,
/// so redirection reproduces the committed file byte for byte.
pub fn schema_json() -> String {
    let mut text = serde_json::to_string_pretty(&schema()).expect("schema serializes");
    text.push('\n');
    text
}
