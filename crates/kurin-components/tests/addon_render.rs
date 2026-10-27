//! WP-41 integration tests: rendering (`03` §4.3/§4.4).
//!
//! Covered here: the MiniJinja context, the values merge order, JSON
//! Schema diagnostics mapped back to the config file, secret resolution
//! and redaction, and image integrity for rendered manifests.
//!
//! Snapshots follow the addon tests: machine-dependent roots are
//! replaced and only the stable text is asserted.

use std::fs;
use std::path::{Path, PathBuf};

use kurin_components::addon::{Addon, Catalog, RenderRequest};
use kurin_config::{parse_str, AddonSpec, ClusterSpec};
use kurin_core::events::Redactor;
use serde_json::json;

const BASE: &str = r#"version = "v1alpha2"
name = "demo"

[ssh.auth]
method = "password"
password = "fake"

[kubernetes]
version = "1.33.1"
distribution = "kubeadm"

[network]
pod-cidr = "10.244.0.0/16"
service-cidr = "10.96.0.0/12"

[network.cni]
kind = "calico"

[ha]
vip = "10.0.0.100"

[registry]
kind = "external"
address = "harbor.local"

[artifacts]
zone = "cn"

[nodes.cp-1]
address = "10.0.0.11"
role = "control-plane"
arch = "amd64"
labels = { zone = "a" }

[nodes.w-1]
address = "10.0.0.21"
role = "worker"
"#;

fn cluster(extra: &str) -> ClusterSpec {
    let text = format!("{BASE}\n{extra}");
    parse_str(Path::new("cluster.toml"), &text).expect("test config parses")
}

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/builtin-catalog")
}

fn no_config() -> AddonSpec {
    AddonSpec {
        enabled: true,
        version: None,
        source: None,
        values: None,
        raw_values: None,
    }
}

fn write_addon(root: &Path, files: &[(&str, &str)]) -> PathBuf {
    let dir = root.join("the-addon");
    for (rel, body) in files {
        let path = dir.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }
    dir
}

fn tempdir() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

/// A manifest addon whose template dumps the whole 03 §4.4 context.
const CONTEXT_ADDON: &[(&str, &str)] = &[
    (
        "addon.yaml",
        r#"apiVersion: addons.kurin.dev/v1alpha1
kind: Addon
metadata:
  name: context-demo
  version: 1.0.0
  description: context rendering test
  category: other
spec:
  compat: {}
  namespace: demo
  install:
    manifests:
      - manifests/*.yaml.j2
"#,
    ),
    (
        "manifests/context.yaml.j2",
        r#"apiVersion: v1
kind: ConfigMap
metadata:
  name: context
  namespace: {{ addon.namespace }}
data:
  cluster: "{{ cluster.name }}"
  domain: "{{ cluster.domain }}"
  kubernetes: "{{ kubernetes.version }}/{{ kubernetes.distribution }}"
  pod-cidr: "{{ network.pod_cidr }}"
  service-cidr: "{{ network.service_cidr }}"
  cni: "{{ network.cni }}"
  vip: "{{ ha.vip }}"
  ha-mode: "{{ ha.mode }}"
  mirror: "{{ registry.mirror }}"
  zone: "{{ zone }}"
  nodes: "{% for n in nodes %}{{ n.name }}={{ n.role }}@{{ n.address }}/{{ n.arch }}/{{ n.labels.zone | default('-') }};{% endfor %}"
  values: "{% for k, v in addon.values | items %}{{ k }}={{ v }};{% endfor %}"
"#,
    ),
];

#[test]
fn renders_the_documented_context() {
    let dir = tempdir();
    let addon = Addon::from_dir(&write_addon(dir.path(), CONTEXT_ADDON)).unwrap();
    let cluster = cluster("");
    let redactor = Redactor::new();
    let rendered = addon
        .render(
            &RenderRequest::new(&cluster, &no_config()).config_path("cluster.toml"),
            &redactor,
        )
        .unwrap();
    assert_eq!(rendered.manifests.len(), 1);
    insta::assert_snapshot!("render_context", rendered.manifests[0].text);
}

#[test]
fn renders_manifest_templates_from_the_fixture_catalog() {
    let addon = Addon::from_dir(&fixtures().join("local-path")).unwrap();
    let cluster = cluster("");
    let redactor = Redactor::new();
    let rendered = addon
        .render(&RenderRequest::new(&cluster, &no_config()), &redactor)
        .unwrap();
    assert_eq!(rendered.manifests.len(), 1);
    assert_eq!(rendered.manifests[0].path, "manifests/config.yaml.j2");
    insta::assert_snapshot!("render_local_path_manifest", rendered.manifests[0].text);
}

#[test]
fn embedded_storage_addons_render_their_storage_classes() {
    // WP-47: the shipped local-path / nfs-subdir templates are what the
    // addon engine renders on a real cluster; both must produce their
    // StorageClass (`kurin-local-path` / `kurin-nfs`) and the provisioner
    // Deployment.
    let catalog = Catalog::embedded().expect("embedded catalog loads");
    for (name, class, extra) in [
        (
            "local-path",
            "kurin-local-path",
            "[addons.local-path]\nenabled = true\nvalues = { default-class = true }\n",
        ),
        (
            "nfs-subdir",
            "kurin-nfs",
            "[addons.nfs-subdir]\nenabled = true\nvalues = { server = \"10.0.0.5\", path = \"/exports\" }\n",
        ),
    ] {
        let cluster = cluster(extra);
        let user = cluster.addons.get(name).expect("addon configured");
        let addon = catalog
            .addons()
            .iter()
            .find(|addon| addon.name() == name)
            .unwrap_or_else(|| panic!("{name} is in the embedded catalog"));
        let rendered = addon
            .render(
                &RenderRequest::new(&cluster, user).zone("global"),
                &Redactor::new(),
            )
            .unwrap_or_else(|err| panic!("{name} renders: {err}"));
        let text = &rendered.manifests[0].text;
        assert!(text.contains(&format!("name: {class}")), "{name}: {text}");
        assert!(text.contains("kind: Deployment"), "{name}: {text}");
    }
}

#[test]
fn helm_addon_renders_values_and_leaves_manifests_to_wp42() {
    let addon = Addon::from_dir(&fixtures().join("metrics-server")).unwrap();
    let cluster = cluster("[addons.metrics-server]\nvalues = { replicas = 3 }\n");
    let config = cluster.addons.get("metrics-server").unwrap();
    let redactor = Redactor::new();
    let rendered = addon
        .render(&RenderRequest::new(&cluster, config), &redactor)
        .unwrap();
    assert!(rendered.is_helm());
    assert_eq!(
        rendered.chart.as_deref(),
        Some("charts/metrics-server-3.13.0.tgz")
    );
    assert_eq!(rendered.release.as_deref(), Some("metrics-server"));
    assert!(rendered.manifests.is_empty());
    assert_eq!(rendered.values.values, json!({"replicas": 3}));
    insta::assert_snapshot!("render_helm_values", rendered.values.yaml);
}

/// The values template is the built-in recommended layer: it may derive
/// values from the cluster, but explicit user/raw values win.
#[test]
fn values_merge_defaults_recommended_user_raw() {
    let dir = tempdir();
    let addon = write_addon(
        dir.path(),
        &[
            (
                "addon.yaml",
                r#"apiVersion: addons.kurin.dev/v1alpha1
kind: Addon
metadata:
  name: merge-demo
  version: 1.0.0
  description: merge order test
  category: other
spec:
  compat: {}
  namespace: demo
  install:
    helm:
      chart: charts/demo.tgz
      values: values.yaml.j2
  values-schema: values.schema.json
  defaults:
    replicas: 1
    mode: default
"#,
            ),
            (
                "values.yaml.j2",
                "replicas: {{ nodes | length }}\nmode: recommended\n",
            ),
            (
                "values.schema.json",
                r#"{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "type": "object",
  "additionalProperties": false,
  "properties": {
    "replicas": { "type": "integer" },
    "mode": { "type": "string" }
  }
}
"#,
            ),
        ],
    );
    let addon = Addon::from_dir(&addon).unwrap();
    let cluster = cluster("");
    let redactor = Redactor::new();

    // defaults → recommended: the template's node count beats the default.
    let rendered = addon
        .render(&RenderRequest::new(&cluster, &no_config()), &redactor)
        .unwrap();
    assert_eq!(
        rendered.values.values,
        json!({"replicas": 2, "mode": "recommended"})
    );

    // user values beat the recommended layer; raw-values beat everything.
    let config = AddonSpec {
        values: Some(toml::from_str("replicas = 3").unwrap()),
        raw_values: Some(toml::from_str("replicas = 9\nmode = \"raw\"").unwrap()),
        ..no_config()
    };
    let rendered = addon
        .render(&RenderRequest::new(&cluster, &config), &redactor)
        .unwrap();
    assert_eq!(
        rendered.values.values,
        json!({"replicas": 9, "mode": "raw"})
    );
    assert!(rendered.values.raw_values_unvalidated);
}

#[test]
fn schema_violation_names_the_config_location() {
    let addon = Addon::from_dir(&fixtures().join("metrics-server")).unwrap();
    let cluster = cluster("[addons.metrics-server]\nvalues = { replicas = 0 }\n");
    let config = cluster.addons.get("metrics-server").unwrap();
    let redactor = Redactor::new();
    let error = addon
        .render(
            &RenderRequest::new(&cluster, config).config_path("cluster.toml"),
            &redactor,
        )
        .unwrap_err();
    let summary = error.summary();
    assert!(
        summary.contains("addons.metrics-server.values.replicas"),
        "{summary}"
    );
    assert!(summary.contains("cluster.toml"), "{summary}");
    insta::assert_snapshot!("render_schema_violation", summary);

    // miette points at the offending line of the config file.
    let mut rendered = String::new();
    miette::GraphicalReportHandler::new()
        .render_report(&mut rendered, &error)
        .unwrap();
    assert!(rendered.contains("replicas = 0"), "{rendered}");
}

#[test]
fn unknown_values_key_is_rejected() {
    let addon = Addon::from_dir(&fixtures().join("metrics-server")).unwrap();
    let cluster = cluster("[addons.metrics-server]\nvalues = { replicas = 1, nope = true }\n");
    let config = cluster.addons.get("metrics-server").unwrap();
    let error = addon
        .render(&RenderRequest::new(&cluster, config), &Redactor::new())
        .unwrap_err();
    let summary = error.summary();
    assert!(
        summary.contains("addons.metrics-server.values"),
        "{summary}"
    );
    assert!(summary.contains("nope"), "{summary}");
}

#[test]
fn raw_values_pass_through_unvalidated() {
    let addon = Addon::from_dir(&fixtures().join("metrics-server")).unwrap();
    let cluster =
        cluster("[addons.metrics-server]\nraw-values = { replicas = 2, not-in-schema = true }\n");
    let config = cluster.addons.get("metrics-server").unwrap();
    let rendered = addon
        .render(&RenderRequest::new(&cluster, config), &Redactor::new())
        .unwrap();
    assert_eq!(
        rendered.values.values,
        json!({"replicas": 2, "not-in-schema": true})
    );
    assert!(rendered.values.raw_values_unvalidated);
}

// ---------------------------------------------------------------------------
// secrets
// ---------------------------------------------------------------------------

const SECRET_ADDON: &[(&str, &str)] = &[
    (
        "addon.yaml",
        r#"apiVersion: addons.kurin.dev/v1alpha1
kind: Addon
metadata:
  name: secret-demo
  version: 1.0.0
  description: secret rendering test
  category: other
spec:
  compat: {}
  namespace: demo
  install:
    manifests:
      - manifests/*.yaml.j2
  values-schema: values.schema.json
"#,
    ),
    (
        "values.schema.json",
        r#"{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "type": "object",
  "additionalProperties": false,
  "properties": {
    "password": { "type": "string" }
  }
}
"#,
    ),
    (
        "manifests/config.yaml.j2",
        r#"apiVersion: v1
kind: ConfigMap
metadata:
  name: demo
  namespace: {{ addon.namespace }}
data:
  password: "{{ addon.values.password }}"
"#,
    ),
];

fn secret_values(var: &str) -> AddonSpec {
    AddonSpec {
        values: Some(toml::from_str(&format!("password = {{ env = \"{var}\" }}")).unwrap()),
        ..no_config()
    }
}

#[test]
fn secret_references_resolve_and_are_redacted() {
    const FAKE: &str = "fake-addon-password-not-real";
    const VAR: &str = "KURIN_TEST_ADDON_SECRET";
    std::env::set_var(VAR, FAKE);

    let dir = tempdir();
    let addon = Addon::from_dir(&write_addon(dir.path(), SECRET_ADDON)).unwrap();
    let cluster = cluster("");
    let redactor = Redactor::new();
    let rendered = addon
        .render(
            &RenderRequest::new(&cluster, &secret_values(VAR)),
            &redactor,
        )
        .unwrap();

    // The secret reaches the rendered manifest (it must), but the
    // redactor masks it everywhere else.
    assert!(rendered.manifests[0].text.contains(FAKE));
    assert_eq!(rendered.values.values["password"], json!(FAKE));
    let masked = redactor.redact(&rendered.manifests[0].text);
    assert!(!masked.contains(FAKE), "{masked}");
    assert!(masked.contains("******"), "{masked}");
    assert!(!format!("{rendered:?}").contains(FAKE));
    std::env::remove_var(VAR);
}

#[test]
fn a_secret_inside_a_render_error_is_redacted() {
    const FAKE: &str = "fake-image-secret-not-real";
    const VAR: &str = "KURIN_TEST_ADDON_IMAGE_SECRET";
    std::env::set_var(VAR, FAKE);

    // The secret is rendered into an image reference, which is not
    // declared: the resulting diagnostic would otherwise print it.
    let dir = tempdir();
    let addon = Addon::from_dir(&write_addon(
        dir.path(),
        &[
            SECRET_ADDON[0],
            SECRET_ADDON[1],
            (
                "manifests/pod.yaml.j2",
                r#"apiVersion: v1
kind: Pod
metadata:
  name: demo
spec:
  containers:
    - name: demo
      image: "{{ addon.values.password }}"
"#,
            ),
        ],
    ))
    .unwrap();
    let cluster = cluster("");
    let error = addon
        .render(
            &RenderRequest::new(&cluster, &secret_values(VAR)),
            &Redactor::new(),
        )
        .unwrap_err();
    let summary = error.summary();
    assert!(!summary.contains(FAKE), "{summary}");
    assert!(summary.contains("******"), "{summary}");
    assert!(summary.contains("not declared"), "{summary}");
    std::env::remove_var(VAR);
}

#[test]
fn missing_secret_env_names_the_variable_not_a_value() {
    const VAR: &str = "KURIN_TEST_ADDON_MISSING_SECRET";
    std::env::remove_var(VAR);

    let dir = tempdir();
    let addon = Addon::from_dir(&write_addon(dir.path(), SECRET_ADDON)).unwrap();
    let cluster = cluster("");
    let error = addon
        .render(
            &RenderRequest::new(&cluster, &secret_values(VAR)),
            &Redactor::new(),
        )
        .unwrap_err();
    let summary = error.summary();
    assert!(summary.contains(VAR), "{summary}");
    assert!(summary.contains("not set"), "{summary}");
}

// ---------------------------------------------------------------------------
// image integrity
// ---------------------------------------------------------------------------

fn image_addon(root: &Path, image: &str, declared: &str) -> Addon {
    let addon_yaml = format!(
        r#"apiVersion: addons.kurin.dev/v1alpha1
kind: Addon
metadata:
  name: image-demo
  version: 1.0.0
  description: image integrity test
  category: other
spec:
  compat: {{}}
  namespace: demo
  install:
    manifests:
      - manifests/*.yaml.j2
  images:
    - {declared}
"#
    );
    let manifest = format!(
        r#"apiVersion: v1
kind: Pod
metadata:
  name: demo
spec:
  containers:
    - name: demo
      image: "{image}"
"#
    );
    let dir = write_addon(
        root,
        &[
            ("addon.yaml", &addon_yaml),
            ("manifests/pod.yaml.j2", &manifest),
        ],
    );
    Addon::from_dir(&dir).unwrap()
}

const DIGEST_A: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

#[test]
fn undeclared_image_is_rejected() {
    let dir = tempdir();
    let declared = format!("registry.k8s.io/demo/demo:v1@{DIGEST_A}");
    let addon = image_addon(dir.path(), "registry.k8s.io/other/other:v1", &declared);
    let cluster = cluster("");
    let error = addon
        .render(
            &RenderRequest::new(&cluster, &no_config()),
            &Redactor::new(),
        )
        .unwrap_err();
    let summary = error.summary();
    assert!(
        summary.contains("registry.k8s.io/other/other:v1"),
        "{summary}"
    );
    assert!(summary.contains("not declared"), "{summary}");
    assert!(summary.contains("manifests/pod.yaml.j2"), "{summary}");
}

#[test]
fn declared_image_passes_integrity_check() {
    let dir = tempdir();
    let declared = format!("registry.k8s.io/demo/demo:v1@{DIGEST_A}");
    let addon = image_addon(dir.path(), "registry.k8s.io/demo/demo:v1", &declared);
    let cluster = cluster("");
    let rendered = addon
        .render(
            &RenderRequest::new(&cluster, &no_config()),
            &Redactor::new(),
        )
        .unwrap();
    assert_eq!(
        rendered.images,
        ["registry.k8s.io/demo/demo:v1".to_string()]
            .into_iter()
            .collect()
    );
}

#[test]
fn a_rendered_digest_must_match_the_declared_one() {
    let dir = tempdir();
    let declared = format!("registry.k8s.io/demo/demo:v1@{DIGEST_A}");
    let rendered_ref = "registry.k8s.io/demo/demo:v1@sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    let addon = image_addon(dir.path(), rendered_ref, &declared);
    let cluster = cluster("");
    let error = addon
        .render(
            &RenderRequest::new(&cluster, &no_config()),
            &Redactor::new(),
        )
        .unwrap_err();
    assert!(
        error.summary().contains("not declared"),
        "{}",
        error.summary()
    );
}
