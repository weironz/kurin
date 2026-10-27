//! `PlanView` DTO snapshots: the serializable plan shape (phases -> tasks
//! with key/node/describe/drift) and the `certs renew` plan.

use std::path::PathBuf;
use std::sync::Arc;

use kurin::ops::plan_view;
use kurin::{certs, facts, plan, run};

fn fixture(name: &str) -> kurin_config::Cluster {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/configs")
        .join(name);
    let cluster = kurin_config::load(&path).expect("fixture loads");
    kurin::components::validate(&cluster).expect("fixture validates");
    cluster
}

fn run_for(cluster: &kurin_config::Cluster) -> Arc<run::RunContext> {
    let facts = facts::FactsTable::synthetic(cluster).expect("synthetic facts");
    Arc::new(
        run::RunContext::new("test-op", &cluster.name, run::ArtifactSources::embedded())
            .with_facts(facts),
    )
}

#[test]
fn plan_view_projects_the_create_plan() {
    let cluster = fixture("single.toml");
    let run = run_for(&cluster);
    let plan = plan::build(&cluster, &run).expect("create plan builds");
    insta::assert_json_snapshot!(plan_view(&plan));
}

#[test]
fn certs_renew_plan_is_serial_per_control_plane() {
    let cluster = fixture("ha-kubevip-3cp.toml");
    let run = run_for(&cluster);
    let plan = certs::build_plan(&cluster, &run, false).expect("certs plan builds");

    // One chain per control plane, in config order, rolling one at a time.
    let view = plan_view(&plan);
    assert_eq!(view.op, "certs-renew");
    let renew = view
        .phases
        .iter()
        .find(|p| p.id == "renew")
        .expect("renew phase");
    let keys: Vec<&str> = renew.tasks.iter().map(|t| t.key.as_str()).collect();
    assert_eq!(
        keys,
        vec![
            "renew/certs-renew@master01",
            "restart-static-pods/restart-static-pods@master01",
            "apiready-gate/apiready-gate@master01",
            "renew/certs-renew@master02",
            "restart-static-pods/restart-static-pods@master02",
            "apiready-gate/apiready-gate@master02",
            "renew/certs-renew@master03",
            "restart-static-pods/restart-static-pods@master03",
            "apiready-gate/apiready-gate@master03",
        ]
    );
    assert_eq!(plan.rolling.len(), 1);
    assert_eq!(plan.rolling[0].max_parallel, 1);
    insta::assert_json_snapshot!(view);
}
