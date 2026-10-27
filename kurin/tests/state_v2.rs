//! RunState v2 integration: v1 compatibility and the `--resume` fingerprint
//! gate at the executor boundary.

mod support;

use std::fs;
use std::sync::Arc;

use kurin::executor::{run_with, RunOptions, SessionPool};
use kurin::plan::Plan;
use kurin::state::{RunState, StateFile};
use petgraph::stable_graph::StableDiGraph;
use support::ScriptedPool;

fn empty_plan() -> Plan {
    Plan {
        graph: StableDiGraph::new(),
        stages: Vec::new(),
    }
}

fn pool() -> Arc<dyn SessionPool> {
    Arc::new(ScriptedPool::new(Vec::new()))
}

#[tokio::test]
async fn resume_rejects_a_changed_plan_fingerprint() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("runs").join("tier1.json");
    {
        let mut state = RunState::open(&path, "tier1", false)
            .unwrap()
            .with_identity("op-old", "fingerprint-old");
        state.mark_done("prepare/sysctl@n1");
        state.flush().unwrap();
    }

    let mut plan = empty_plan();
    let err = run_with(
        &mut plan,
        &pool(),
        &mut |_| {},
        RunOptions {
            cluster: "tier1".into(),
            state_file: Some(path.clone()),
            resume: true,
            op_id: "op-new".into(),
            plan_fingerprint: "fingerprint-new".into(),
        },
    )
    .await
    .unwrap_err();
    assert_eq!(
        err.to_string(),
        "计划已变化，请去掉 --resume 重新运行（已收敛的任务会被检查跳过）"
    );

    // The failed resume must not rewrite the state file.
    let file: StateFile = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(file.plan_fingerprint, "fingerprint-old");
    assert_eq!(file.done.len(), 1);
}

#[tokio::test]
async fn v1_state_file_resumes_and_is_upgraded_to_v2() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("runs").join("tier1.json");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        &path,
        r#"{"version":1,"cluster":"tier1","tasks":{
            "prepare/sysctl@n1":{"state":"done","finished_at":7}
        }}"#,
    )
    .unwrap();

    let mut plan = empty_plan();
    run_with(
        &mut plan,
        &pool(),
        &mut |_| {},
        RunOptions {
            cluster: "tier1".into(),
            state_file: Some(path.clone()),
            resume: true,
            op_id: "op-new".into(),
            plan_fingerprint: "fingerprint-new".into(),
        },
    )
    .await
    .unwrap();

    let file: StateFile = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(file.version, 2);
    assert_eq!(file.op_id, "op-new");
    assert_eq!(file.plan_fingerprint, "fingerprint-new");
    assert_eq!(file.done.len(), 1);
    assert_eq!(file.done[0].key, "prepare/sysctl@n1");
    assert_eq!(file.done[0].finished_at, 7);
}
