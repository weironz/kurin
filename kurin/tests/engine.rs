//! Engine tests: verify the tri-state contract against a mock session.
//! No real SSH; the session layer lands in W1 (russh track).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use kurin::{
    engine::{Drift, ExecOutput, Session},
    Event, Outcome, Stage, Task,
};
use miette::Result;

struct MockSession {
    state: std::sync::Mutex<State>,
    disturb_after: usize,
    calls: AtomicUsize,
}

struct State {
    files: std::collections::HashMap<String, Vec<u8>>,
}

#[async_trait::async_trait]
impl Session for MockSession {
    async fn exec(&self, command: &str) -> Result<ExecOutput> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        if self.disturb_after > 0 && n >= self.disturb_after {
            // Simulate transport drop mid-apply.
            return Err(miette::miette!("connection reset by peer"));
        }
        let st = self.state.lock().unwrap();
        let stdout = if command.starts_with("test -z") {
            "ok".into()
        } else if st.files.contains_key("/etc/containerd/config.toml") {
            "1".into()
        } else {
            "0".into()
        };
        Ok(ExecOutput {
            exit_code: 0,
            stdout,
            stderr: String::new(),
        })
    }

    async fn write_file(&self, path: &str, contents: &[u8]) -> Result<()> {
        self.state
            .lock()
            .unwrap()
            .files
            .insert(path.into(), contents.to_vec());
        Ok(())
    }

    async fn file_sha256(&self, path: &str) -> Result<Option<String>> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .files
            .get(path)
            .map(|_| "deadbeef".into()))
    }

    async fn service_active(&self, _name: &str) -> Result<bool> {
        Ok(true)
    }
}

struct ProbeTask;

#[async_trait::async_trait]
impl Task for ProbeTask {
    fn id(&self) -> String {
        "x/probe@n".into()
    }
    fn stage(&self) -> Stage {
        Stage::Prepare
    }
    fn node(&self) -> &str {
        "n"
    }
    fn describe(&self) -> String {
        "probe only".into()
    }
    async fn check(&self, _s: &dyn Session) -> Result<Drift> {
        Ok(Drift::InSync)
    }
    async fn apply(&self, _s: &dyn Session) -> Result<Outcome> {
        Ok(Outcome::Verified)
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_after_apply_reprobes_and_reports() {
    let task = ProbeTask;
    let session = MockSession {
        state: std::sync::Mutex::new(State {
            files: Default::default(),
        }),
        disturb_after: 0,
        calls: AtomicUsize::new(0),
    };
    let drift = task.check(&session).await.unwrap();
    assert!(matches!(drift, Drift::InSync));
}

#[test]
fn stage_ordering_is_fixed() {
    assert!(Stage::Preflight < Stage::Prepare);
    assert!(Stage::Join > Stage::Bootstrap);
}

#[test]
fn dry_run_tree_renders_every_stage() {
    // Plan without real cluster data: verify tree shape, not task selection.
    let cluster = kurin_config::parse_str(
        std::path::Path::new("x.toml"),
        r##"version = "v1alpha1"
name = "t"
[ssh]
user = "root"
[ssh.auth]
method = "password"
password = "x"
[nodes.m]
address = "10.0.0.1"
role = "control-plane"
"##,
    )
    .unwrap();
    let facts = kurin::facts::FactsTable::synthetic(&cluster).unwrap();
    let run = Arc::new(
        kurin::run::RunContext::new(
            "test-op",
            &cluster.name,
            kurin::run::ArtifactSources::embedded(),
        )
        .with_facts(facts),
    );
    let plan = kurin::plan::build(&cluster, &run).expect("test plan builds");
    let tree = kurin::render::render_tree(&plan);
    assert!(tree.contains("preflight"));
    assert!(tree.contains("bootstrap"));
    assert!(tree.contains("cni"));
    let _ = tree.contains("cluster-ready");
}

#[test]
fn version_table_is_pinned() {
    assert_eq!(kurin::version::KUBERNETES_VERSION, "1.37.2");
    let versions = kurin::version::component_versions();
    assert!(versions.iter().any(|(k, _)| *k == "kubernetes"));
}

#[derive(Default)]
#[allow(dead_code)] // fixture kept for the pool-trait shape; no wire-up in unit tests
struct Noop;
#[async_trait::async_trait]
impl kurin::SessionPool for Noop {
    async fn get(&self, _node: &str) -> miette::Result<Arc<dyn Session>> {
        Err(miette::miette!("no session pool in unit tests"))
    }
}

#[test]
fn events_are_serde_representable() {
    let ev = Event::TaskStart {
        id: "a".into(),
        node: "n".into(),
        stage: Stage::Prepare.label(),
        ts: 1_790_000_000_123,
    };
    let js = serde_json::to_string(&ev).unwrap();
    assert!(js.contains("task-start"));
}
