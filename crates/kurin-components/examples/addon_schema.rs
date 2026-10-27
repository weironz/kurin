//! Print the `addon.yaml` JSON Schema on stdout.
//!
//! `just addon-schema` redirects this into `docs/schema/addon.v1alpha1.json`;
//! the drift test in `tests/addon_schema.rs` fails when the committed file
//! disagrees with the types.

fn main() {
    print!("{}", kurin_components::addon::schema_json());
}
