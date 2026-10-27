//! WP-00 behavior baseline: what `kurin create cluster` prints, event by
//! event, for `single.toml` — plus one `--json` line per event variant.
//!
//! The executor runs against `support::ScriptedPool` (no SSH). Commands that
//! matter for control flow are answered by explicit rules; everything else
//! gets the permissive default (exit 0, empty output). The rendered text
//! uses the exact `main.rs` formats (`skip/done/ok/FAIL/-- stage`), with two
//! normalizations: task durations become `Nms`, and lines inside a stage are
//! sorted — the executor parallelizes stage members, so completion order is
//! deliberately not part of the contract (docs/v1.0-gates.md §2).
//!
//! This is the only test in this binary that touches the process
//! environment (join material, kubeconfig, summary marker all redirect into
//! a temp dir); the event-variant snapshot below runs in the same test to
//! keep that env mutation single-threaded.

mod support;

use std::path::PathBuf;
use std::sync::Arc;

use kurin::artifact::Arch;
use kurin::executor::{run_with, RunOptions};
use kurin::render::event_json_line;
use kurin::{plan, Event};
use support::{Reply, Rule, ScriptedPool, ScriptedSession};

/// Fixed timestamp for the `--json` snapshots (the envelope's `ts`).
const TS: u64 = 1_790_000_000_123;

/// The admin.conf a converged control plane serves over `cat` (fake
/// credentials only, rule R8). Valid YAML with the arrays
/// `KubeconfigPull::merge_into` rewrites; >100 bytes so apply accepts it.
const ADMIN_CONF: &str = "apiVersion: v1
kind: Config
clusters:
- cluster:
    server: https://192.168.10.11:6443
  name: kubernetes
contexts:
- context:
    cluster: kubernetes
    user: kubernetes-admin
  name: kubernetes-admin@kubernetes
current-context: kubernetes-admin@kubernetes
users:
- name: kubernetes-admin
  user:
    client-certificate-data: ZmFrZQ==
    client-key-data: ZmFrZQ==
";

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/configs")
        .join(name)
}

/// Same arch mapping the planner uses (`plan.rs`): artifacts are selected
/// for the control machine's architecture.
fn host_arch() -> Arch {
    match std::env::consts::ARCH {
        "x86_64" => Arch::Amd64,
        "aarch64" => Arch::Arm64,
        other => panic!("unsupported host arch {other}"),
    }
}

/// The marker `ArtifactsInstall::apply` would have left behind: every
/// pinned entry as `sha@r{INSTALL_RECIPE}`. Serving it from the session
/// makes the artifacts task report InSync — this test locks the event
/// stream, not the download path.
fn artifacts_marker() -> String {
    let manifest = kurin::artifact::ArtifactManifest::embedded().unwrap();
    let arch = host_arch();
    let mut marker = String::new();
    for entry in manifest.artifacts() {
        let Ok(spec) = manifest.spec(&entry.name, arch) else {
            continue;
        };
        if let Some(sha) = spec.expected_sha256() {
            marker.push_str(&format!(
                "{}={sha}@r{}\n",
                entry.name,
                kurin::tasks::install::INSTALL_RECIPE
            ));
        }
    }
    marker
}

/// Rules where the reply's content is parsed and drives control flow.
fn rules() -> Vec<Rule> {
    vec![
        // ArtifactsInstall::check reads the install marker.
        Rule::new(
            r"cat /etc/kurin/artifacts\.marker",
            Reply::ok(&artifacts_marker()),
        ),
        // KubeadmInit::apply harvests the join command + certificate key
        // (fake values, rule R8).
        Rule::new(
            r"kubeadm token create --print-join-command",
            Reply::ok(concat!(
                "kubeadm join 192.168.10.11:6443 ",
                "--token abcdef.0123456789abcdef ",
                "--discovery-token-ca-cert-hash ",
                "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
            )),
        ),
        Rule::new(
            r"kubeadm init phase upload-certs",
            Reply::ok("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"),
        ),
        // KubeconfigPull::apply reads admin.conf back.
        Rule::new(r"cat /etc/kubernetes/admin\.conf", Reply::ok(ADMIN_CONF)),
        // CalicoOperator::check sees the Installation CR present.
        Rule::new(
            r"get installation default -o name",
            Reply::ok("installation.operator.tigera.io/default"),
        ),
        // KubeadmJoin::apply runs the harvested join command: it did its
        // job on the node, but the connection died before the outcome came
        // back — the executor re-probes, sees kubelet.conf, and reports
        // `ok ... (reconciled)`.
        Rule::new(r"kubeadm join ", Reply::connection_lost())
            .creating("/etc/kubernetes/kubelet.conf", b"# joined\n"),
    ]
}

/// Task durations vary run to run; the CLI contract is the format, not the
/// number. `done  id (1234ms)` -> `done  id (Nms)`.
fn normalize_ms(line: &str) -> String {
    let Some((head, tail)) = line.rsplit_once(" (") else {
        return line.to_string();
    };
    let Some(digits) = tail.strip_suffix("ms)") else {
        return line.to_string();
    };
    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
        return line.to_string();
    }
    format!("{head} (Nms)")
}

/// Render events exactly like `main.rs` does for `create cluster` (non-json
/// branch): the five line formats, nothing for TaskStart/ClusterReady.
/// Lines inside a stage are sorted (see module docs).
fn render_like_main(events: &[Event]) -> String {
    let mut out = String::new();
    let mut stage_lines: Vec<String> = Vec::new();
    for event in events {
        match event {
            Event::TaskSkipped { id, .. } => stage_lines.push(format!("skip  {id}")),
            Event::TaskDone { id, ms, .. } => {
                stage_lines.push(normalize_ms(&format!("done  {id} ({ms}ms)")));
            }
            Event::TaskReconciled { id, .. } => {
                stage_lines.push(format!("ok    {id} (reconciled)"));
            }
            Event::TaskFailed { id, reason, .. } => {
                stage_lines.push(format!("FAIL  {id}: {reason}"));
            }
            Event::StageDone { stage, .. } => {
                stage_lines.sort();
                for line in &stage_lines {
                    out.push_str(line);
                    out.push('\n');
                }
                out.push_str(&format!("-- stage {stage} complete\n"));
                stage_lines.clear();
            }
            _ => {}
        }
    }
    // A failed run ends mid-stage with no StageDone: those lines still
    // print (in-flight siblings settle before the executor returns).
    stage_lines.sort();
    for line in &stage_lines {
        out.push_str(line);
        out.push('\n');
    }
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn create_cluster_output_and_event_lines() {
    // Redirect every control-machine-local path the run touches.
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("XDG_CACHE_HOME", dir.path().join("cache"));
    std::env::set_var("KUBECONFIG", dir.path().join("kubeconfig"));
    std::env::set_var("KURIN_SUMMARY_DIR", dir.path().join("summaries"));

    let cluster =
        kurin_config::load(&fixture_path("single.toml")).expect("single.toml is a valid fixture");

    // Converged control plane (init already ran), fresh worker: the join
    // rule drops the connection after kubelet.conf lands, so the run
    // exercises skip (InSync), done (Drifted -> Verified) and reconciled
    // (Unknown -> re-probe -> InSync) events.
    let master = ScriptedSession::new("master01", rules());
    let worker = ScriptedSession::new("worker01", rules());
    master.seed_file("/etc/kubernetes/admin.conf", ADMIN_CONF.as_bytes());
    let pool = Arc::new(ScriptedPool::new(vec![master, worker]));
    let dyn_pool: Arc<dyn kurin::SessionPool> = pool.clone();
    let facts = kurin::facts::FactsTable::synthetic(&cluster).unwrap();
    let mut plan = plan::build_with_pool(&cluster, Some(dyn_pool.clone()), &facts).unwrap();
    let mut events = Vec::new();
    let mut emit = |event: Event| events.push(event);
    run_with(&mut plan, &dyn_pool, &mut emit, RunOptions::default())
        .await
        .expect("the scripted run converges");

    assert!(matches!(events.last(), Some(Event::ClusterReady { .. })));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::TaskReconciled { id, .. }
            if id == "join/kubeadm@worker01")),
        "the scripted join must end Unknown and reconcile on re-probe"
    );
    insta::assert_snapshot!(render_like_main(&events));

    // What kurin shipped where: every node got the rendered configs
    // (sysctl persists through a shell heredoc, not write_file).
    for node in ["master01", "worker01"] {
        let writes = pool.session(node).writes();
        for path in [
            "/etc/containerd/config.toml",
            "/etc/systemd/system/containerd.service",
            "/etc/systemd/system/kubelet.service",
        ] {
            assert!(
                writes
                    .iter()
                    .any(|w| w.path == path && w.sha256.len() == 64),
                "{node} must have written {path}"
            );
        }
    }
    assert!(pool
        .session("master01")
        .writes()
        .iter()
        .any(|w| w.path == "/etc/kurin/kubeadm-init.yaml"));
    assert!(pool
        .session("worker01")
        .writes()
        .iter()
        .any(|w| w.path == "/etc/kurin/kubeadm-join.yaml"));

    // The merged kubeconfig: our context, pointing at the first master.
    let kubeconfig = std::fs::read_to_string(dir.path().join("kubeconfig")).unwrap();
    assert!(kubeconfig.contains("cluster-single"), "{kubeconfig}");
    assert!(
        kubeconfig.contains("https://192.168.10.11:6443"),
        "{kubeconfig}"
    );

    // The exact remote-command surface: everything the permissive default
    // answered (rules matched a handful; the disconnecting join command is
    // not recorded — it was never answered). Sorted: stage members run
    // concurrently.
    let mut report = String::new();
    for node in ["master01", "worker01"] {
        let mut unmatched = pool.session(node).unmatched();
        unmatched.sort();
        report.push_str(&format!(
            "# {node}: {} command(s) answered by the permissive default\n\n",
            unmatched.len()
        ));
        for cmd in &unmatched {
            for line in cmd.lines() {
                report.push_str("    ");
                report.push_str(line);
                report.push('\n');
            }
            report.push('\n');
        }
    }
    insta::assert_snapshot!("single.unmatched-commands", report);

    // One JSON line per event variant, straight from the renderer the CLI
    // uses for `--json`.
    let representatives = vec![
        Event::TaskStart {
            id: "prepare/sysctl@n1".into(),
            node: "n1".into(),
            stage: "prepare",
            ts: TS,
        },
        Event::TaskSkipped {
            id: "preflight/n1".into(),
            node: "n1".into(),
            stage: "preflight",
            ts: TS,
        },
        Event::TaskDone {
            id: "prepare/sysctl@n1".into(),
            node: "n1".into(),
            stage: "prepare",
            ms: 12,
            ts: TS,
        },
        Event::TaskFailed {
            id: "cni/calico-operator@n1".into(),
            node: "n1".into(),
            stage: "cni",
            reason: "calico apply failed: connection refused".into(),
            ts: TS,
        },
        Event::TaskReconciled {
            id: "join/kubeadm@n2".into(),
            node: "n2".into(),
            stage: "join",
            ts: TS,
        },
        Event::StageDone {
            stage: "post",
            ts: TS,
        },
        Event::ClusterReady { ts: TS },
    ];
    for event in &representatives {
        insta::assert_snapshot!(
            format!("event-json-{}", event.kind()),
            event_json_line(event)
        );
    }

    // The FAIL format and fail-fast: a run where master01's sysctl apply
    // exits non-zero. In-flight siblings settle, the stage never completes,
    // and the executor returns the task's error (no further stages run).
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("XDG_CACHE_HOME", dir.path().join("cache"));
    std::env::set_var("KUBECONFIG", dir.path().join("kubeconfig"));
    std::env::set_var("KURIN_SUMMARY_DIR", dir.path().join("summaries"));
    let mut failing = rules();
    failing.push(Rule::new(
        r"sysctl --system",
        Reply::fail(
            1,
            "error: permission denied on key 'net.bridge.bridge-nf-call-iptables'",
        ),
    ));
    let master = ScriptedSession::new("master01", failing);
    let worker = ScriptedSession::new("worker01", rules());
    let pool = Arc::new(ScriptedPool::new(vec![master, worker]));
    let dyn_pool: Arc<dyn kurin::SessionPool> = pool.clone();
    let facts = kurin::facts::FactsTable::synthetic(&cluster).unwrap();
    let mut plan = plan::build_with_pool(&cluster, Some(dyn_pool.clone()), &facts).unwrap();
    let mut events = Vec::new();
    let mut emit = |event: Event| events.push(event);
    let err = run_with(&mut plan, &dyn_pool, &mut emit, RunOptions::default())
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("sysctl apply failed on master01"),
        "unexpected first error: {err}"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::TaskFailed { id, .. }
            if id == "prepare/sysctl@master01")),
        "the sysctl task must be the recorded failure"
    );
    assert!(
        !events.iter().any(|e| matches!(
            e,
            Event::StageDone {
                stage: "prepare",
                ..
            }
        )),
        "a failed stage never completes"
    );
    assert!(!events
        .iter()
        .any(|e| matches!(e, Event::ClusterReady { .. })));
    insta::assert_snapshot!("single.sysctl-failure", render_like_main(&events));
}
