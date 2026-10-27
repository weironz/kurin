//! WP-00 behavior baseline: the plan for every fixture config, frozen as
//! snapshots. Two views per fixture:
//!
//! - the `--dry-run` text tree (`kurin::render::render_tree`);
//! - a JSON matrix of every stage's tasks (`id` + `describe()`).
//!
//! Any behavioral change to planning shows up here and must be explained
//! in the PR (dev-plan rule R3). `offline.toml` carries an `@OFFLINE_DIR@`
//! placeholder that is replaced with an absolute path to the (empty)
//! fixture bundle directory before parsing — `kurin_config`'s validation
//! requires the directory to exist, and committed fixtures must not embed
//! a machine-specific absolute path.

use std::path::PathBuf;
use std::sync::Arc;

use kurin::plan;
use kurin::render::render_tree;
use kurin::Plan;
use serde_json::json;

/// The config fixture matrix: every file must pass the same load pipeline
/// as `kurin_config::load` (version gate -> defaults -> lowering -> rules).
const FIXTURES: &[&str] = &[
    "single.toml",
    "mixed-arch.toml",
    "external-endpoint.toml",
    "k3s-single.toml",
    "k3s-ha.toml",
    "k3s-kurin-lb.toml",
    "crio.toml",
    "cni-flannel.toml",
    "cni-cilium.toml",
    "cni-kube-ovn.toml",
    "cni-none.toml",
    "registry-zot.toml",
    "registry-docker.toml",
    "registry-harbor.toml",
    "storage-local-path.toml",
    "storage-nfs.toml",
    "offline.toml",
    "zone-cn.toml",
    "labels.toml",
];

/// Placeholder in `offline.toml`; replaced before parsing.
const OFFLINE_DIR_PLACEHOLDER: &str = "@OFFLINE_DIR@";

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/configs")
        .join(name)
}

fn offline_bundle_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/offline-bundle")
}

/// Load a fixture through the same pipeline the CLI runs: kurin-config's
/// structural checks (`parse_str` is the in-memory twin of its `load`)
/// plus the component validation pass, injecting the offline-dir
/// placeholder when present.
fn load_fixture(name: &str) -> kurin_config::Cluster {
    let path = fixture_path(name);
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read fixture {}: {e}", path.display()));
    let src = raw.replace(
        OFFLINE_DIR_PLACEHOLDER,
        &offline_bundle_dir().display().to_string(),
    );
    let cluster = kurin_config::parse_str(&path, &src)
        .unwrap_or_else(|e| panic!("fixture {name} must load through kurin_config: {e:?}"));
    kurin::components::validate(&cluster)
        .unwrap_or_else(|e| panic!("fixture {name} must pass component validation: {e:?}"));
    cluster
}

/// Stage -> task matrix as JSON: the ordered `id` list plus each task's
/// `describe()`, i.e. everything the plan carries besides the tree layout.
fn plan_matrix(plan: &Plan) -> serde_json::Value {
    let stages: Vec<serde_json::Value> = plan
        .stages()
        .into_iter()
        .map(|stage| {
            json!({
                "stage": stage.stage.label(),
                "ids": stage
                    .tasks
                    .iter()
                    .map(|t| t.task.id())
                    .collect::<Vec<String>>(),
                "tasks": stage
                    .tasks
                    .iter()
                    .map(|t| json!({ "id": t.task.id(), "describe": t.task.describe() }))
                    .collect::<Vec<serde_json::Value>>(),
            })
        })
        .collect();
    json!({ "stages": stages })
}

#[test]
fn plan_snapshots_for_the_config_fixture_matrix() {
    for name in FIXTURES {
        let cluster = load_fixture(name);
        let facts = kurin::facts::FactsTable::synthetic(&cluster)
            .expect("synthetic facts build for every fixture");
        let run = Arc::new(
            kurin::run::RunContext::new(
                "test-op",
                &cluster.name,
                kurin::run::ArtifactSources::embedded()
                    .with_offline_dir(cluster.offline_dir.clone()),
            )
            .with_facts(facts),
        );
        let plan = plan::build(&cluster, &run)
            .unwrap_or_else(|e| panic!("fixture {name} must build a plan: {e:?}"));
        insta::assert_snapshot!(format!("{name}.tree"), render_tree(&plan));
        insta::assert_json_snapshot!(format!("{name}.json"), plan_matrix(&plan));
    }
}
