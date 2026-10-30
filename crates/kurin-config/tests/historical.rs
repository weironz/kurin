//! v1.0 gate §1: historical v1alpha1 samples must keep parsing unchanged
//! and migrate to an equivalent v1alpha2 document.
//!
//! The samples are the released `kurin/examples/cluster.example.toml`
//! from v0.4.0, v0.5.0 and v0.5.1; they are frozen here so a schema change
//! that breaks an old file fails `just check` instead of only a live
//! upgrade.

use std::path::Path;

use kurin_config::migrate::migrate_str;
use kurin_config::parse_str;

const SAMPLES: &[(&str, &str)] = &[
    (
        "v0.4.0",
        include_str!("fixtures/historical/v0.4.0-cluster.example.toml"),
    ),
    (
        "v0.5.0",
        include_str!("fixtures/historical/v0.5.0-cluster.example.toml"),
    ),
    (
        "v0.5.1",
        include_str!("fixtures/historical/v0.5.1-cluster.example.toml"),
    ),
];

#[test]
fn released_v1alpha1_samples_load_and_migrate() {
    for (release, src) in SAMPLES {
        let path = Path::new("historical.toml");
        let before = parse_str(path, src)
            .unwrap_or_else(|e| panic!("{release}: historical sample must load: {e:?}"));

        let migrated = migrate_str(path, src)
            .unwrap_or_else(|e| panic!("{release}: historical sample must migrate: {e:?}"));
        let after = parse_str(path, &migrated)
            .unwrap_or_else(|e| panic!("{release}: migrated sample must load: {e:?}"));

        assert!(
            before.same_spec(&after),
            "{release}: migrated document changed the cluster spec"
        );
        assert_eq!(before.name, "prod");
        assert_eq!(before.control_plane().count(), 3);
        assert_eq!(before.nodes.len(), 4);
    }
}
