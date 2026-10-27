//! WP-40 integration tests: loading addons from a directory, a package
//! and the embedded catalog, plus every static validation rule with a
//! diagnostic snapshot.
//!
//! Snapshot policy follows the config crate: only the span-free message
//! text, with the machine-dependent roots (tempdir, fixtures dir)
//! replaced by placeholders; the miette render tests additionally prove
//! that parse errors carry the file path and a highlighted line:col.

use std::fs;
use std::path::{Path, PathBuf};

use kurin_components::addon::{Addon, AddonError, Catalog};
use rust_embed::RustEmbed;

fn repo_root() -> PathBuf {
    // crates/kurin-components/tests/addon.rs → repository root.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .unwrap()
        .to_path_buf()
}

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/builtin-catalog")
}

/// The committed two-addon catalog: metrics-server (helm) and local-path
/// (manifests). It doubles as the embed fixture below.
#[derive(RustEmbed)]
#[folder = "tests/fixtures/builtin-catalog"]
struct FixtureCatalog;

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Write one addon directory containing exactly the given files.
fn write_addon(root: &Path, files: &[(&str, &str)]) -> PathBuf {
    let dir = root.join("the-addon");
    for (rel, body) in files {
        let path = dir.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }
    dir
}

/// Load the addon written to `dir` and snapshot every validation
/// diagnostic, normalized and sorted (order is not part of the
/// contract). The snapshot name spells out the rule under test.
fn validate_snapshot(name: &str, dir: &Path) {
    let addon = Addon::from_dir(dir).expect("addon loads");
    let mut summaries: Vec<String> = addon.validate().iter().map(stable).collect();
    summaries.sort();
    insta::assert_snapshot!(name, summaries.join("\n"));
}

/// A span-free rendering of a diagnostic with machine-dependent roots
/// replaced (tempdir → `<root>`, the fixtures dir → `<fixtures>`).
fn stable(error: &AddonError) -> String {
    normalize(&error.summary())
}

/// Machine-independent text: the fixtures dir becomes `<fixtures>`, and
/// every tempdir root (`/tmp/<random>`) becomes `<root>` — a message may
/// mention several different tempdirs at once.
fn normalize(message: &str) -> String {
    let mut out = message.replace(&fixtures().display().to_string(), "<fixtures>");
    while let Some(at) = out.find("/tmp/") {
        let rest = &out[at + "/tmp/".len()..];
        let name_len = rest.find('/').unwrap_or(rest.len());
        out = format!("{}<root>{}", &out[..at], &rest[name_len..]);
    }
    out
}

/// Render a diagnostic the way the CLI would (miette graphical), with the
/// machine-dependent roots replaced.
fn render(error: &AddonError) -> String {
    let mut out = String::new();
    miette::GraphicalReportHandler::new()
        .render_report(&mut out, error)
        .unwrap();
    normalize(&out)
}

fn tempdir() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

// ---------------------------------------------------------------------------
// loading: directory, embedded, package
// ---------------------------------------------------------------------------

#[test]
fn catalog_from_dir_loads_and_validates_both_fixtures() {
    let catalog = Catalog::from_dir(&fixtures()).unwrap();
    assert_eq!(catalog.names(), vec!["local-path", "metrics-server"]);

    let metrics = catalog.get("metrics-server").unwrap();
    assert_eq!(
        metrics.origin(),
        format!("dir {}", fixtures().join("metrics-server").display())
    );
    assert_eq!(metrics.manifest().spec.namespace, "kube-system");
    assert!(metrics.files().carries_charts());
    assert_eq!(metrics.files().list().len(), 4);

    // Every static rule passes for both addons.
    assert!(catalog.validate().is_empty(), "{:?}", catalog.validate());
}

#[test]
fn embedded_catalog_matches_the_directory_scan() {
    let embedded = kurin_components::addon::embedded_from::<FixtureCatalog>().unwrap();
    let from_dir = Catalog::from_dir(&fixtures()).unwrap();
    assert_eq!(embedded.names(), from_dir.names());
    // Embedded sources declare that charts are not carried: they travel
    // in packages or online, so chart paths are not existence-checked.
    let metrics = embedded.get("metrics-server").unwrap();
    assert!(!metrics.files().carries_charts());
    assert!(metrics.files().exists("values.yaml.j2"));
    assert!(embedded.validate().is_empty(), "{:?}", embedded.validate());
}

#[test]
fn real_builtin_catalog_is_empty_until_wp46() {
    let catalog = Catalog::embedded().unwrap();
    assert!(catalog.addons().is_empty(), "{:?}", catalog.names());
    assert!(catalog.validate().is_empty());
}

#[test]
fn package_source_round_trips_through_an_addon_package() {
    let cache_dir = tempdir();
    let cache = kurin_artifact::store::BlobStore::open(cache_dir.path());
    let files: Vec<(&str, String)> = vec![
        (
            "addon.yaml",
            include_str!("fixtures/builtin-catalog/metrics-server/addon.yaml").to_string(),
        ),
        (
            "values.schema.json",
            include_str!("fixtures/builtin-catalog/metrics-server/values.schema.json").to_string(),
        ),
        (
            "values.yaml.j2",
            include_str!("fixtures/builtin-catalog/metrics-server/values.yaml.j2").to_string(),
        ),
        (
            "charts/metrics-server-3.13.0.tgz",
            include_str!(
                "fixtures/builtin-catalog/metrics-server/charts/metrics-server-3.13.0.tgz"
            )
            .to_string(),
        ),
    ];
    let mut builder = kurin_artifact::package::PackageBuilder::new(
        kurin_artifact::package::PackageKind::Addon,
        "metrics-server",
        "0.8.0",
        "any",
    );
    for (title, body) in &files {
        let digest = cache.put_bytes(body.as_bytes()).unwrap();
        builder.add_file(kurin_artifact::package::PackageFile {
            name: (*title).to_string(),
            kind: kurin_artifact::artifact::ArtifactKind::Manifest,
            version: "0.8.0".into(),
            arch: kurin_artifact::artifact::Arch::Amd64,
            digest,
            size: body.len() as u64,
            title: (*title).to_string(),
            upstream: format!("test://{title}"),
        });
    }
    let out_dir = tempdir();
    let layout = out_dir.path().join("pkg");
    builder
        .build(
            &cache,
            &kurin_artifact::package::PackageOutput::Layout(layout.clone()),
        )
        .unwrap();

    let source = format!("oci-layout://{}", layout.display());
    let addon = Addon::from_source(&source).unwrap();
    assert_eq!(addon.name(), "metrics-server");
    assert_eq!(addon.origin(), format!("package {source}"));
    // Package paths render as `source!rel` in diagnostics.
    assert_eq!(
        addon.files().path_for("addon.yaml"),
        format!("{source}!addon.yaml")
    );
    assert!(addon.files().carries_charts());
    // The file map is sorted (BTreeMap), not in insertion order.
    let mut expected: Vec<String> = files.iter().map(|(t, _)| (*t).to_string()).collect();
    expected.sort();
    assert_eq!(addon.files().list(), expected);
    assert!(addon.validate().is_empty(), "{:?}", addon.validate());
}

#[test]
fn package_source_rejects_a_base_package() {
    let cache_dir = tempdir();
    let cache = kurin_artifact::store::BlobStore::open(cache_dir.path());
    let out_dir = tempdir();
    let layout = out_dir.path().join("base");
    kurin_artifact::package::PackageBuilder::new(
        kurin_artifact::package::PackageKind::Base,
        "base",
        "1.33.1",
        "kubeadm",
    )
    .build(
        &cache,
        &kurin_artifact::package::PackageOutput::Layout(layout.clone()),
    )
    .unwrap();

    let error = Addon::from_source(&format!("oci-layout://{}", layout.display())).unwrap_err();
    assert!(
        error
            .summary()
            .contains("package kind is `base`, expected `addon`"),
        "{}",
        error.summary()
    );
}

#[test]
fn registry_sources_get_a_helpful_error() {
    let error = Addon::from_source("oci://harbor.local/kurin/addons/x:1.0.0").unwrap_err();
    assert!(
        error.summary().contains("kurin package pull"),
        "{}",
        error.summary()
    );
    assert!(Addon::from_source("https://example.com/x").is_err());
    assert!(Addon::from_dir(Path::new("/nonexistent-addon-dir")).is_err());
}

// ---------------------------------------------------------------------------
// parse-time rules (schema shape, compat syntax)
// ---------------------------------------------------------------------------

/// A minimal but complete addon.yaml to mutate per test.
const BASE: &str = r#"apiVersion: addons.kurin.dev/v1alpha1
kind: Addon
metadata:
  name: sample
  version: 1.0.0
  description: 样例 addon
  category: other
spec:
  compat:
    kubernetes: ">=1.30.0"
  namespace: default
  install:
    manifests: [manifests/a.yaml.j2]
  images: []
"#;

fn parse_error(yaml: &str, root: &Path) -> AddonError {
    let dir = write_addon(
        root,
        &[
            ("addon.yaml", yaml),
            ("manifests/a.yaml.j2", "kind: ConfigMap\n"),
        ],
    );
    Addon::from_dir(&dir).expect_err("addon must not parse")
}

#[test]
fn parse_error_renders_path_and_span() {
    let root = tempdir();
    let yaml = BASE.replace(">=1.30.0", ">>1.30.0");
    let error = parse_error(&yaml, root.path());
    insta::assert_snapshot!(render(&error));
}

#[test]
fn unknown_fields_are_rejected_at_parse_time() {
    let root = tempdir();
    let yaml = BASE.replace("  images: []", "  images: []\n  extra: true");
    let error = parse_error(&yaml, root.path());
    insta::assert_snapshot!(render(&error));
}

#[test]
fn unknown_hook_actions_are_rejected_at_parse_time() {
    let root = tempdir();
    let yaml = BASE.replace(
        "  images: []",
        "  images: []\n  hooks:\n    post-install:\n      - run: kubectl patch …",
    );
    let error = parse_error(&yaml, root.path());
    insta::assert_snapshot!(stable(&error));
}

#[test]
fn compat_syntax_is_checked_at_parse_time() {
    let root = tempdir();
    // Distributions and arches are closed enums.
    let bad_distro = BASE.replace(
        "    kubernetes:",
        "    distributions: [rke2]\n    kubernetes:",
    );
    let error = parse_error(&bad_distro, root.path());
    insta::assert_snapshot!("compat_bad_distribution", stable(&error));

    let bad_arch = BASE.replace("    kubernetes:", "    arch: [ppc64le]\n    kubernetes:");
    let error = parse_error(&bad_arch, root.path());
    insta::assert_snapshot!("compat_bad_arch", stable(&error));
}

#[test]
fn bad_version_and_timeout_fail_at_parse_time() {
    let root = tempdir();
    let error = parse_error(
        &BASE.replace("version: 1.0.0", "version: latest"),
        root.path(),
    );
    insta::assert_snapshot!("bad_version", stable(&error));

    let error = parse_error(
        &BASE.replace("  images: []", "  images: []\n  timeout: 5x"),
        root.path(),
    );
    insta::assert_snapshot!("bad_timeout", stable(&error));
}

#[test]
fn missing_required_section_fails_at_parse_time() {
    let root = tempdir();
    let yaml = BASE.replace(
        "  install:\n    manifests: [manifests/a.yaml.j2]\n  images: []\n",
        "",
    );
    let error = parse_error(&yaml, root.path());
    insta::assert_snapshot!(stable(&error));
}

// ---------------------------------------------------------------------------
// static validation rules (03 §4.2)
// ---------------------------------------------------------------------------

#[test]
fn install_must_have_exactly_one_of_helm_or_manifests() {
    let root = tempdir();
    let both = BASE.replace(
        "    manifests: [manifests/a.yaml.j2]",
        "    manifests: [manifests/a.yaml.j2]\n    helm:\n      chart: charts/x.tgz",
    );
    let dir = write_addon(
        root.path(),
        &[
            ("addon.yaml", &both),
            ("charts/x.tgz", "x"),
            ("manifests/a.yaml.j2", "y"),
        ],
    );
    validate_snapshot("install_both_helm_and_manifests", &dir);

    let root = tempdir();
    let neither = BASE.replace(
        "  install:\n    manifests: [manifests/a.yaml.j2]",
        "  install: {}",
    );
    let dir = write_addon(
        root.path(),
        &[("addon.yaml", &neither), ("manifests/a.yaml.j2", "y")],
    );
    validate_snapshot("install_neither_helm_nor_manifests", &dir);
}

#[test]
fn referenced_files_must_exist() {
    let root = tempdir();
    let missing_schema = BASE.replace(
        "  images: []",
        "  images: []\n  values-schema: values.schema.json\n  defaults: {}",
    );
    let dir = write_addon(
        root.path(),
        &[
            ("addon.yaml", &missing_schema),
            ("manifests/a.yaml.j2", "y"),
        ],
    );
    validate_snapshot("referenced_values_schema_missing", &dir);

    let root = tempdir();
    let helm = BASE.replace(
        "  install:\n    manifests: [manifests/a.yaml.j2]",
        "  install:\n    helm:\n      chart: charts/missing.tgz",
    );
    let dir = write_addon(
        root.path(),
        &[("addon.yaml", &helm), ("manifests/a.yaml.j2", "y")],
    );
    validate_snapshot("referenced_helm_chart_missing", &dir);

    let root = tempdir();
    let escaping = BASE.replace("manifests/a.yaml.j2", "../outside.yaml");
    let dir = write_addon(
        root.path(),
        &[("addon.yaml", &escaping), ("manifests/a.yaml.j2", "y")],
    );
    validate_snapshot("referenced_path_escapes_the_addon", &dir);
}

#[test]
fn manifest_globs_must_match_something() {
    let root = tempdir();
    let glob = BASE.replace("manifests/a.yaml.j2", "manifests/*.yaml");
    let dir = write_addon(
        root.path(),
        &[("addon.yaml", &glob), ("manifests/a.yaml.j2", "y")],
    );
    validate_snapshot("manifest_glob_matches_nothing", &dir);
    // The fixture addon with a matching glob is covered in
    // catalog_from_dir_loads_and_validates_both_fixtures.
}

#[test]
fn images_must_carry_a_digest() {
    let root = tempdir();
    let pinned = BASE.replace(
        "  images: []",
        "  images:\n    - registry.k8s.io/metrics-server:v0.8.0",
    );
    let dir = write_addon(
        root.path(),
        &[("addon.yaml", &pinned), ("manifests/a.yaml.j2", "y")],
    );
    validate_snapshot("image_without_digest", &dir);
}

#[test]
fn values_schema_must_be_valid_and_closed() {
    // A schema that does not compile: the $ref cannot be resolved.
    let with_schema = BASE.replace(
        "  images: []",
        "  images: []\n  values-schema: values.schema.json",
    );
    let root = tempdir();
    let dir = write_addon(
        root.path(),
        &[
            ("addon.yaml", &with_schema),
            (
                "values.schema.json",
                r##"{"type": "object", "properties": {"x": {"$ref": "#/$defs/missing"}}}"##,
            ),
            ("manifests/a.yaml.j2", "y"),
        ],
    );
    validate_snapshot("values_schema_does_not_compile", &dir);

    // Valid but open at the root: user keys outside the schema must be
    // rejected (03 §4.2 rule 3), so the schema has to close itself.
    let root = tempdir();
    let dir = write_addon(
        root.path(),
        &[
            ("addon.yaml", &with_schema),
            ("values.schema.json", r#"{"type": "object"}"#),
            ("manifests/a.yaml.j2", "y"),
        ],
    );
    validate_snapshot("values_schema_not_closed_at_root", &dir);

    // Not JSON at all.
    let root = tempdir();
    let dir = write_addon(
        root.path(),
        &[
            ("addon.yaml", &with_schema),
            ("values.schema.json", "{not json"),
            ("manifests/a.yaml.j2", "y"),
        ],
    );
    validate_snapshot("values_schema_not_json", &dir);
}

#[test]
fn defaults_must_satisfy_the_values_schema() {
    let schema = r#"{
        "type": "object",
        "additionalProperties": false,
        "properties": {"replicas": {"type": "integer", "minimum": 1}}
    }"#;
    let with_defaults = BASE.replace(
        "  images: []",
        "  images: []\n  values-schema: values.schema.json\n  defaults:\n    replicas: zero",
    );
    let root = tempdir();
    let dir = write_addon(
        root.path(),
        &[
            ("addon.yaml", &with_defaults),
            ("values.schema.json", schema),
            ("manifests/a.yaml.j2", "y"),
        ],
    );
    validate_snapshot("defaults_violate_values_schema", &dir);

    // defaults must be a mapping, not a list.
    let with_list = BASE.replace(
        "  images: []",
        "  images: []\n  values-schema: values.schema.json\n  defaults:\n    - replicas: 1",
    );
    let root = tempdir();
    let dir = write_addon(
        root.path(),
        &[
            ("addon.yaml", &with_list),
            ("values.schema.json", schema),
            ("manifests/a.yaml.j2", "y"),
        ],
    );
    validate_snapshot("defaults_not_a_mapping", &dir);
}

#[test]
fn hooks_must_use_exactly_one_built_in_action() {
    let both = BASE.replace(
        "  images: []",
        "  images: []\n  hooks:\n    post-install:\n      - wait:\n          kind: APIService\n          name: v1\n      - apply:\n          path: manifests/b.yaml",
    );
    let root = tempdir();
    let dir = write_addon(
        root.path(),
        &[
            ("addon.yaml", &both),
            ("manifests/a.yaml.j2", "y"),
            ("manifests/b.yaml", "z"),
        ],
    );
    validate_snapshot("hook_combines_several_actions", &dir);

    let none = BASE.replace(
        "  images: []",
        "  images: []\n  hooks:\n    post-install:\n      - {}",
    );
    let root = tempdir();
    let dir = write_addon(
        root.path(),
        &[("addon.yaml", &none), ("manifests/a.yaml.j2", "y")],
    );
    validate_snapshot("hook_names_no_action", &dir);
}

#[test]
fn names_must_be_dns_labels() {
    let root = tempdir();
    let bad = BASE.replace("  name: sample", "  name: Sample_Addon");
    let dir = write_addon(
        root.path(),
        &[("addon.yaml", &bad), ("manifests/a.yaml.j2", "y")],
    );
    validate_snapshot("metadata_name_not_a_dns_label", &dir);

    let root = tempdir();
    let bad_ns = BASE.replace("  namespace: default", "  namespace: Not A Namespace");
    let dir = write_addon(
        root.path(),
        &[("addon.yaml", &bad_ns), ("manifests/a.yaml.j2", "y")],
    );
    validate_snapshot("namespace_not_a_dns_label", &dir);
}

#[test]
fn requires_and_conflicts_must_reference_catalog_addons() {
    let mut catalog = Catalog::from_dir(&fixtures()).unwrap();
    let root = tempdir();
    let needs = BASE.replace("  name: sample", "  name: needy").replace(
        "  images: []",
        "  images: []\n  requires: [ghost-addon]\n  conflicts: [another-ghost]",
    );
    catalog.push(
        Addon::from_dir(&write_addon(
            root.path(),
            &[("addon.yaml", &needs), ("manifests/a.yaml.j2", "y")],
        ))
        .unwrap(),
    );

    let mut summaries: Vec<String> = catalog.validate().iter().map(stable).collect();
    summaries.sort();
    insta::assert_snapshot!(summaries.join("\n"));
}

#[test]
fn duplicate_names_are_rejected() {
    let mut catalog = Catalog::from_dir(&fixtures()).unwrap();
    let root = tempdir();
    let clone = BASE.replace("  name: sample", "  name: metrics-server");
    catalog.push(
        Addon::from_dir(&write_addon(
            root.path(),
            &[("addon.yaml", &clone), ("manifests/a.yaml.j2", "y")],
        ))
        .unwrap(),
    );

    let mut summaries: Vec<String> = catalog.validate().iter().map(stable).collect();
    summaries.sort();
    insta::assert_snapshot!(summaries.join("\n"));
}

#[test]
fn repo_root_helper_is_sane() {
    assert!(repo_root()
        .join("docs/schema/addon.v1alpha1.json")
        .is_file());
    assert!(repo_root().join("addons/README.md").is_file());
}
