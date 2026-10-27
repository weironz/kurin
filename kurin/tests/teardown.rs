//! WP-12: teardown as a plan. Snapshots pin the `delete node` /
//! `delete cluster` plans for both distributions; the executor tests cover
//! the two behaviors the imperative teardown used to hide: best-effort
//! failures are warnings with a per-node summary, and an unreachable anchor
//! control plane skips the API steps without stopping the wipe.
//!
//! The executor runs against `support::ScriptedPool` (no SSH). Convergence
//! probes treat exit 0 as "still drifted", so the permissive default reply
//! makes every step run; tests for the already-clean path script non-zero
//! probe exits explicitly.

mod support;

use std::path::PathBuf;
use std::sync::Arc;

use kurin::executor::run_with_bus;
use kurin::plan::build_delete;
use kurin::render::render_tree;
use kurin::teardown::{with_local_node, WarningSummary};
use kurin::{Event, SessionPool};
use serde_json::json;
use support::{Reply, Rule, ScriptedPool, ScriptedSession};

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/configs")
        .join(name)
}

fn load_fixture(name: &str) -> kurin_config::Cluster {
    let path = fixture_path(name);
    let cluster =
        kurin_config::load(&path).unwrap_or_else(|e| panic!("fixture {name} must load: {e:?}"));
    kurin::components::validate(&cluster)
        .unwrap_or_else(|e| panic!("fixture {name} must validate: {e:?}"));
    cluster
}

fn run_for(cluster: &kurin_config::Cluster) -> Arc<kurin::run::RunContext> {
    let facts = kurin::facts::FactsTable::synthetic(cluster).expect("synthetic facts");
    support::run(cluster, facts)
}

/// Plan matrix: phase -> ordered task ids, descriptions and failure policy,
/// plus the dependency edges (as `from -> to` task ids).
fn plan_matrix(plan: &kurin::Plan) -> serde_json::Value {
    let phases: Vec<serde_json::Value> = plan
        .phase_groups()
        .into_iter()
        .map(|group| {
            json!({
                "phase": group.phase.id,
                "tasks": group
                    .tasks
                    .iter()
                    .map(|t| json!({
                        "id": t.task.id(),
                        "node": t.task.node(),
                        "describe": t.task.describe(),
                        "policy": format!("{:?}", t.task.failure_policy()),
                    }))
                    .collect::<Vec<_>>(),
            })
        })
        .collect();
    let edges: Vec<String> = plan
        .edges
        .iter()
        .map(|&(from, to)| {
            format!(
                "{} -> {}",
                plan.tasks[from].task.id(),
                plan.tasks[to].task.id()
            )
        })
        .collect();
    json!({
        "op": format!("{:?}", plan.op),
        "phases": phases,
        "edges": edges,
    })
}

/// The fixture matrix for teardown snapshots: kubeadm and k3s, one control
/// plane plus one worker each.
const CASES: &[(&str, &str)] = &[("single.toml", "worker01"), ("k3s-single.toml", "agent01")];

#[test]
fn delete_node_plan_snapshots() {
    for (fixture, target) in CASES {
        let cluster = load_fixture(fixture);
        let node = cluster
            .nodes
            .iter()
            .find(|n| n.name == *target)
            .unwrap_or_else(|| panic!("{fixture} has no node {target}"));
        let run = run_for(&cluster);
        let plan = build_delete(&cluster, &run, Some(node)).expect("delete node plan builds");
        insta::assert_snapshot!(format!("delete-node-{fixture}.tree"), render_tree(&plan));
        insta::assert_snapshot!(
            format!("delete-node-{fixture}.json"),
            serde_json::to_string_pretty(&plan_matrix(&plan)).unwrap()
        );
    }
}

#[test]
fn delete_cluster_plan_snapshots() {
    for (fixture, _) in CASES {
        let cluster = load_fixture(fixture);
        let run = run_for(&cluster);
        let plan = build_delete(&cluster, &run, None).expect("delete cluster plan builds");
        insta::assert_snapshot!(format!("delete-cluster-{fixture}.tree"), render_tree(&plan));
        insta::assert_snapshot!(
            format!("delete-cluster-{fixture}.json"),
            serde_json::to_string_pretty(&plan_matrix(&plan)).unwrap()
        );
    }
}

/// Scripted sessions for `single.toml`: the anchor answers the API steps,
/// the worker answers the local ones.
fn single_pool(worker_rules: Vec<Rule>) -> ScriptedPool {
    ScriptedPool::new(vec![
        ScriptedSession::new(
            "master01",
            vec![
                Rule::new("get node worker01", Reply::ok("node/worker01")),
                Rule::new("drain worker01", Reply::ok("")),
                Rule::new("delete node worker01", Reply::ok("")),
            ],
        ),
        ScriptedSession::new("worker01", worker_rules),
    ])
}

async fn run_plan(
    cluster: &kurin_config::Cluster,
    pool: &Arc<dyn SessionPool>,
    target: Option<&kurin_config::Node>,
) -> (miette::Result<()>, Vec<Event>, Arc<WarningSummary>) {
    let run = run_for(cluster);
    let mut plan = build_delete(cluster, &run, target).expect("teardown plan builds");
    let warnings = WarningSummary::new();
    run.events.add_sink(warnings.clone());
    let mut events = Vec::new();
    let result = run_with_bus(
        &mut plan,
        pool,
        &run,
        &run.events,
        &mut |event| events.push(event),
        kurin::executor::RunOptions::default(),
    )
    .await;
    (result, events, warnings)
}

#[tokio::test]
async fn cleanup_failures_warn_and_are_summarized_per_node() {
    // The wipe fails (a busy mountpoint survived all three passes); every
    // other step still runs.
    let pool = single_pool(vec![Rule::new(
        "for i in 1 2 3",
        Reply::fail(
            1,
            "rm: cannot remove '/var/lib/kubelet': Device or resource busy",
        ),
    )]);
    let cluster = load_fixture("single.toml");
    let target = cluster.nodes.iter().find(|n| n.name == "worker01").unwrap();
    let worker = Arc::clone(pool.session("worker01"));
    let pool: Arc<dyn SessionPool> = Arc::new(pool);
    let (result, events, warnings) = run_plan(&cluster, &pool, Some(target)).await;

    // Best-effort: the operation still succeeds.
    assert!(result.is_ok(), "{result:?}");
    // The frozen v1 stream still reports the failing task; the executor's
    // `TaskWarn` (and the summary below) is what tells consumers it did not
    // stop the run.
    assert!(
        events.iter().any(
            |e| matches!(e, Event::TaskFailed { id, .. } if id == "cleanup/wipe-dirs@worker01")
        ),
        "{events:?}"
    );

    let summary = warnings.render().expect("a warning was recorded");
    assert!(summary.contains("[worker01]"), "{summary}");
    assert!(summary.contains("cleanup/wipe-dirs@worker01"), "{summary}");
    assert!(summary.contains("Device or resource busy"), "{summary}");

    // Successors of the failed wipe still ran: the iptables cleanup is the
    // last step of the chain.
    assert!(
        worker
            .unmatched()
            .iter()
            .any(|c| c.contains("for t in filter nat mangle raw")),
        "iptables cleanup must run after the wipe failed: {:?}",
        worker.unmatched()
    );
}

#[tokio::test]
async fn unreachable_anchor_skips_the_api_steps_and_still_wipes() {
    // No session for master01: the anchor is unreachable.
    let pool = ScriptedPool::new(vec![ScriptedSession::new("worker01", vec![])]);
    let worker = Arc::clone(pool.session("worker01"));
    let cluster = load_fixture("single.toml");
    let target = cluster.nodes.iter().find(|n| n.name == "worker01").unwrap();
    let pool: Arc<dyn SessionPool> = Arc::new(pool);
    let (result, _, warnings) = run_plan(&cluster, &pool, Some(target)).await;

    assert!(result.is_ok(), "{result:?}");
    let summary = warnings.render().expect("the unreachable anchor warns");
    assert!(summary.contains("[master01]"), "{summary}");
    assert!(summary.contains("drain/drain@worker01"), "{summary}");
    assert!(summary.contains("delete-node-object@worker01"), "{summary}");

    // The node itself is still reset and wiped.
    let commands = worker.unmatched();
    assert!(
        commands.iter().any(|c| c.contains("kubeadm reset")),
        "the wipe must continue without the anchor: {commands:?}"
    );
    assert!(
        commands.iter().any(|c| c.contains("for i in 1 2 3")),
        "the artifact wipe must run: {commands:?}"
    );
}

#[tokio::test]
async fn converged_node_is_skipped_without_running_any_step() {
    let pool = ScriptedPool::new(vec![
        ScriptedSession::new(
            "server01",
            vec![Rule::new(
                "get node agent01",
                Reply::fail(
                    1,
                    "Error from server (NotFound): nodes \"agent01\" not found",
                ),
            )],
        ),
        ScriptedSession::new(
            "agent01",
            vec![
                Rule::new("systemctl is-active", Reply::fail(3, "")),
                Rule::new("ls -d", Reply::fail(2, "")),
                Rule::new("ip -o link show", Reply::fail(1, "")),
                Rule::new("iptables -t", Reply::fail(1, "")),
            ],
        ),
    ]);
    let cluster = load_fixture("k3s-single.toml");
    let target = cluster.nodes.iter().find(|n| n.name == "agent01").unwrap();
    let pool: Arc<dyn SessionPool> = Arc::new(pool);
    let (result, events, warnings) = run_plan(&cluster, &pool, Some(target)).await;

    assert!(result.is_ok(), "{result:?}");
    assert!(warnings.render().is_none(), "nothing failed");
    assert!(
        events.iter().all(|e| !matches!(e, Event::TaskDone { .. })),
        "a converged node must not apply anything: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::TaskSkipped { .. })),
        "converged tasks are skipped: {events:?}"
    );
}

#[tokio::test]
async fn delete_cluster_removes_local_state_after_the_nodes() {
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("XDG_CACHE_HOME", dir.path().join("cache"));
    std::env::set_var("XDG_STATE_HOME", dir.path().join("state"));

    let cluster = load_fixture("single.toml");
    let join_dir = kurin::tasks::bootstrap::join_material_path(&cluster.name);
    std::fs::create_dir_all(&join_dir).unwrap();
    let state = kurin::RunState::default_path(&cluster.name).unwrap();
    std::fs::create_dir_all(state.parent().unwrap()).unwrap();
    std::fs::write(&state, b"{}").unwrap();
    let bb = kurin::blackboard::Blackboard::default_path(&cluster.name).unwrap();
    std::fs::create_dir_all(bb.parent().unwrap()).unwrap();
    std::fs::write(&bb, b"{}").unwrap();

    let pool = ScriptedPool::new(vec![
        ScriptedSession::new(
            "master01",
            vec![Rule::new("get node worker01", Reply::ok("node/worker01"))],
        ),
        ScriptedSession::new("worker01", vec![]),
    ]);
    let pool: Arc<dyn SessionPool> = with_local_node(Arc::new(pool));
    let (result, events, warnings) = run_plan(&cluster, &pool, None).await;

    assert!(result.is_ok(), "{result:?}");
    assert!(warnings.render().is_none(), "{:?}", warnings.render());
    assert!(!join_dir.exists(), "join cache must be removed");
    assert!(!state.exists(), "run state must be removed");
    assert!(!bb.exists(), "blackboard must be removed");
    assert!(
        events.iter().any(|e| matches!(
            e,
            Event::TaskDone { id, .. } if id == "local/local-state-cleanup@local"
        )),
        "the local cleanup runs as a plan task: {events:?}"
    );
}
