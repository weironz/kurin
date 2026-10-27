//! WP-06: `kurin logs` — JSONL round-trip and the cluster/op/node/level
//! filters, exercised through the real binary with an isolated
//! `XDG_STATE_HOME`.

use std::path::{Path, PathBuf};
use std::process::Command;

use kurin::events::{Body, Envelope, Level, OpKind, OpResult, Stream, Timestamp};

fn envelope(seq: u64, op: &str, body: Body) -> Envelope {
    Envelope {
        v: 2,
        seq,
        ts: Timestamp::from_millis(1_790_000_000_000 + seq),
        op: op.into(),
        body,
    }
}

fn log(level: Level, node: &str, stream: Stream, line: &str) -> Body {
    Body::Log {
        level,
        node: Some(node.into()),
        task: Some(format!("bootstrap/kubeadm@{node}")),
        stream,
        line: line.into(),
    }
}

fn write_log(root: &Path, cluster: &str, op: &str, envelopes: &[Envelope]) {
    let dir = root.join("kurin").join("logs").join(cluster);
    std::fs::create_dir_all(&dir).unwrap();
    let mut out = String::new();
    for envelope in envelopes {
        out.push_str(&serde_json::to_string(envelope).unwrap());
        out.push('\n');
    }
    std::fs::write(dir.join(format!("{op}.jsonl")), out).unwrap();
}

fn fixture(root: &Path) {
    // Older operation: one info line.
    write_log(
        root,
        "demo",
        "20260101T000000.000Z-1",
        &[
            envelope(
                1,
                "20260101T000000.000Z-1",
                Body::OpStarted {
                    kind: OpKind::new("create-cluster"),
                    cluster: "demo".into(),
                    plan_fingerprint: "old".into(),
                    kurin: "0.4.0".into(),
                },
            ),
            envelope(
                2,
                "20260101T000000.000Z-1",
                log(Level::Info, "n1", Stream::Kurin, "old operation line"),
            ),
        ],
    );

    // Newer operation: info on n1, debug remote output on n2, a failure on
    // n2, and a terminal envelope.
    write_log(
        root,
        "demo",
        "20260102T000000.000Z-2",
        &[
            envelope(
                1,
                "20260102T000000.000Z-2",
                Body::OpStarted {
                    kind: OpKind::new("create-cluster"),
                    cluster: "demo".into(),
                    plan_fingerprint: "new".into(),
                    kurin: "0.4.0".into(),
                },
            ),
            envelope(
                2,
                "20260102T000000.000Z-2",
                log(Level::Info, "n1", Stream::Kurin, "new operation line"),
            ),
            envelope(
                3,
                "20260102T000000.000Z-2",
                log(
                    Level::Debug,
                    "n2",
                    Stream::Stdout,
                    "[init] Using Kubernetes version: v1.33.1",
                ),
            ),
            envelope(
                4,
                "20260102T000000.000Z-2",
                log(Level::Debug, "n2", Stream::Stderr, "stderr detail"),
            ),
            envelope(
                5,
                "20260102T000000.000Z-2",
                Body::TaskFailed {
                    key: "join/kubeadm@n2".into(),
                    node: "n2".into(),
                    phase: "join".into(),
                    reason: "join failed".into(),
                },
            ),
            envelope(
                6,
                "20260102T000000.000Z-2",
                Body::OpFinished {
                    result: OpResult::Failed,
                    ms: 99,
                    error: Some("join failed".into()),
                },
            ),
        ],
    );

    // A second cluster proves `--cluster` is required (and honored).
    write_log(
        root,
        "other",
        "20260103T000000.000Z-3",
        &[envelope(
            1,
            "20260103T000000.000Z-3",
            log(Level::Info, "x1", Stream::Kurin, "other cluster line"),
        )],
    );
}

fn run(root: &Path, args: &[&str]) -> (String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_kurin"))
        .args(args)
        .env("XDG_STATE_HOME", root)
        .env("NO_COLOR", "1")
        .output()
        .expect("run kurin");
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn state_dir() -> PathBuf {
    tempfile::tempdir().unwrap().keep()
}

#[test]
fn last_shows_the_newest_operation_only() {
    let root = state_dir();
    fixture(&root);
    let (stdout, stderr) = run(&root, &["logs", "--cluster", "demo", "--last"]);
    assert!(stderr.is_empty(), "stderr: {stderr}");
    assert!(stdout.contains("new operation line"), "{stdout}");
    assert!(!stdout.contains("old operation line"), "{stdout}");
    // Info is the default level: debug remote lines stay hidden.
    assert!(!stdout.contains("Using Kubernetes version"), "{stdout}");
    assert!(stdout.contains("FAIL  join/kubeadm@n2"), "{stdout}");
}

#[test]
fn op_selects_by_prefix() {
    let root = state_dir();
    fixture(&root);
    let (stdout, _) = run(&root, &["logs", "--cluster", "demo", "--op", "20260101"]);
    assert!(stdout.contains("old operation line"), "{stdout}");
    assert!(!stdout.contains("new operation line"), "{stdout}");
}

#[test]
fn node_and_level_filter() {
    let root = state_dir();
    fixture(&root);
    // -vv selects debug and shows the raw remote output for n2 only.
    let (stdout, _) = run(
        &root,
        &["logs", "--cluster", "demo", "--last", "--node", "n2", "-vv"],
    );
    assert!(stdout.contains("Using Kubernetes version"), "{stdout}");
    assert!(stdout.contains("stderr detail"), "{stdout}");
    assert!(!stdout.contains("new operation line"), "{stdout}");
    assert!(!stdout.contains("n1"), "{stdout}");

    // Explicit --level wins over the default.
    let (stdout, _) = run(
        &root,
        &["logs", "--cluster", "demo", "--last", "--level", "error"],
    );
    assert!(stdout.contains("join failed"), "{stdout}");
    assert!(!stdout.contains("new operation line"), "{stdout}");
}

#[test]
fn json_output_round_trips_envelopes() {
    let root = state_dir();
    fixture(&root);
    let (stdout, _) = run(
        &root,
        &[
            "logs",
            "--cluster",
            "demo",
            "--last",
            "--level",
            "debug",
            "--json",
        ],
    );
    let mut count = 0;
    for line in stdout.lines() {
        let envelope: Envelope = serde_json::from_str(line).expect("each line is an envelope");
        assert_eq!(envelope.v, 2);
        assert_eq!(envelope.op, "20260102T000000.000Z-2");
        count += 1;
    }
    assert_eq!(count, 6, "all six envelopes round-trip");
}

#[test]
fn cluster_is_required_when_several_have_logs() {
    let root = state_dir();
    fixture(&root);
    let (_, stderr) = run(&root, &["logs", "--last"]);
    assert!(stderr.contains("--cluster"), "{stderr}");
}

#[test]
fn missing_operation_is_an_error() {
    let root = state_dir();
    fixture(&root);
    let (_, stderr) = run(&root, &["logs", "--cluster", "demo", "--op", "nope"]);
    assert!(stderr.contains("no operation log matching"), "{stderr}");
}

// ── full pipeline: executor -> EventBus -> JSONL -> `kurin logs` ────────

mod pipeline {
    use std::sync::Arc;

    use kurin::engine::{Drift, ExecOutput, Outcome, PlannedTask, Session, Stage, Task};
    use kurin::events::{Body, EventBus, JsonlSink, Level, Stream};
    use kurin::executor::{run_with_bus, RunOptions, SessionPool};
    use kurin::plan::{OpKind, Phase, Plan};

    struct EchoSession;

    #[async_trait::async_trait]
    impl Session for EchoSession {
        async fn exec(&self, _command: &str) -> miette::Result<ExecOutput> {
            Ok(ExecOutput {
                exit_code: 0,
                stdout: "[init] Using Kubernetes version: v1.33.1\n[preflight] running checks\n"
                    .into(),
                stderr: String::new(),
            })
        }
        async fn write_file(&self, _path: &str, _contents: &[u8]) -> miette::Result<()> {
            Ok(())
        }
        async fn file_sha256(&self, _path: &str) -> miette::Result<Option<String>> {
            Ok(None)
        }
        async fn service_active(&self, _name: &str) -> miette::Result<bool> {
            Ok(false)
        }
    }

    struct EchoPool;

    #[async_trait::async_trait]
    impl SessionPool for EchoPool {
        async fn get(&self, _node: &str) -> miette::Result<Arc<dyn Session>> {
            Ok(Arc::new(EchoSession))
        }
    }

    /// Mimics `bootstrap/kubeadm-init`: one mutating command whose output
    /// must reach the JSONL log.
    struct InitTask;

    #[async_trait::async_trait]
    impl Task for InitTask {
        fn id(&self) -> String {
            "bootstrap/kubeadm-init@n1".into()
        }
        fn stage(&self) -> Stage {
            Stage::Bootstrap
        }
        fn node(&self) -> &str {
            "n1"
        }
        fn describe(&self) -> String {
            "kubeadm init".into()
        }
        async fn check(&self, _session: &dyn Session) -> miette::Result<Drift> {
            Ok(Drift::Drifted("not initialized".into()))
        }
        async fn apply(&self, session: &dyn Session) -> miette::Result<Outcome> {
            session
                .exec("kubeadm init --config /etc/kurin/kubeadm-init.yaml")
                .await?;
            Ok(Outcome::Verified)
        }
    }

    #[tokio::test]
    async fn executor_jsonl_then_logs_shows_remote_output() {
        let root = tempfile::tempdir().unwrap();
        let op = "20260104T000000.000Z-9";
        let path = root
            .path()
            .join("kurin")
            .join("logs")
            .join("demo")
            .join(format!("{op}.jsonl"));
        let bus = Arc::new(EventBus::new(op));
        bus.add_sink(Arc::new(JsonlSink::create(&path).unwrap()));

        let mut plan = Plan::from_stage_groups(
            OpKind::Create,
            vec![(
                Phase::new(Stage::Bootstrap.label(), Stage::Bootstrap.label()),
                vec![PlannedTask {
                    task: Arc::new(InitTask),
                    drift: None,
                }],
            )],
        );
        let pool: Arc<dyn SessionPool> = Arc::new(EchoPool);
        let run = Arc::new(kurin::run::RunContext::new(
            op,
            "demo",
            kurin::run::ArtifactSources::embedded(),
        ));
        let mut events = Vec::new();
        run_with_bus(
            &mut plan,
            &pool,
            &run,
            &bus,
            &mut |event| events.push(event),
            RunOptions::default(),
        )
        .await
        .expect("the scripted run converges");

        // The v1 stream is unchanged in shape.
        assert!(events.iter().any(|event| matches!(
            event,
            kurin::Event::TaskDone { id, .. } if id == "bootstrap/kubeadm-init@n1"
        )));

        // Remote stdout landed in the JSONL at debug level.
        let envelopes = kurin::events::read_log(&path).unwrap();
        assert!(
            envelopes.iter().any(|envelope| matches!(
                &envelope.body,
                Body::Log {
                    level: Level::Debug,
                    stream: Stream::Stdout,
                    line,
                    ..
                } if line.contains("[init] Using Kubernetes version")
            )),
            "remote output missing: {envelopes:?}"
        );
        assert!(
            envelopes.iter().any(|envelope| matches!(
                &envelope.body,
                Body::Log {
                    level: Level::Debug,
                    stream: Stream::Kurin,
                    line,
                    ..
                } if line.contains("2 stdout line(s)")
            )),
            "remote summary missing: {envelopes:?}"
        );

        // `kurin logs --last -vv` prints it, timestamps included.
        let (stdout, stderr) =
            super::run(root.path(), &["logs", "--cluster", "demo", "--last", "-vv"]);
        assert!(stderr.is_empty(), "{stderr}");
        assert!(
            stdout.contains("[init] Using Kubernetes version: v1.33.1"),
            "{stdout}"
        );
        assert!(stdout.contains("2 stdout line(s)"), "{stdout}");
        assert!(
            stdout.lines().any(|line| line.starts_with("2026-")),
            "timestamps missing: {stdout}"
        );
    }
}
