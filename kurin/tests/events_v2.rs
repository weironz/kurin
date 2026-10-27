//! WP-06: event v2 envelope shape, timestamps and the terminal renderer.
//!
//! The v1 `--json` contract lives in `event_contract.rs`; here we lock the
//! v2 envelope (`v`, `seq`, `ts`, `op`, `body`) and the two terminal modes:
//! the default one is byte-identical to the pre-v2 output, and
//! `--timestamps` prefixes the RFC 3339 UTC timestamp.

use kurin::events::{Body, Envelope, Level, OpKind, OpResult, Stream, Timestamp, ENVELOPE_VERSION};
use kurin::render::TerminalRenderer;

fn envelope(seq: u64, body: Body) -> Envelope {
    Envelope {
        v: ENVELOPE_VERSION,
        seq,
        ts: Timestamp::from_millis(1_790_000_000_123),
        op: "20260921T141320.123Z-4242".into(),
        body,
    }
}

#[test]
fn v2_envelope_shape_is_stable() {
    let done = envelope(
        7,
        Body::TaskDone {
            key: "prepare/sysctl@n1".into(),
            node: "n1".into(),
            phase: "prepare".into(),
            ms: 42,
        },
    );
    insta::assert_snapshot!(
        "v2-envelope-task-done",
        serde_json::to_string_pretty(&done).unwrap()
    );

    let log = envelope(
        8,
        Body::Log {
            level: Level::Debug,
            node: Some("n1".into()),
            task: Some("bootstrap/kubeadm@n1".into()),
            stream: Stream::Stdout,
            line: "[init] Using Kubernetes version: v1.33.1".into(),
        },
    );
    insta::assert_snapshot!(
        "v2-envelope-remote-log",
        serde_json::to_string_pretty(&log).unwrap()
    );

    let finished = envelope(
        9,
        Body::OpFinished {
            result: OpResult::Succeeded,
            ms: 1234,
            error: None,
        },
    );
    insta::assert_snapshot!(
        "v2-envelope-op-finished",
        serde_json::to_string_pretty(&finished).unwrap()
    );

    let started = envelope(
        1,
        Body::OpStarted {
            kind: OpKind::new("create-cluster"),
            cluster: "demo".into(),
            plan_fingerprint: "a1b2c3".into(),
            kurin: "0.4.0".into(),
        },
    );
    insta::assert_snapshot!(
        "v2-envelope-op-started",
        serde_json::to_string_pretty(&started).unwrap()
    );
}

#[test]
fn default_terminal_output_is_byte_identical_to_v1() {
    // The pre-v2 renderer printed the message text verbatim; the new
    // renderer must not add levels or timestamps unless asked.
    let renderer = TerminalRenderer::with_writer(Vec::new(), false, 0);
    let info = envelope(
        1,
        Body::Log {
            level: Level::Info,
            node: Some("n1".into()),
            task: None,
            stream: Stream::Kurin,
            line: "[n1] ok drain".into(),
        },
    );
    assert_eq!(renderer.render(&info).as_deref(), Some("[n1] ok drain"));

    let warn = envelope(
        2,
        Body::Log {
            level: Level::Warn,
            node: Some("n2".into()),
            task: None,
            stream: Stream::Kurin,
            line: "[n2] !! health gate".into(),
        },
    );
    assert_eq!(
        renderer.render(&warn).as_deref(),
        Some("[n2] !! health gate")
    );

    // Remote output is debug and stays hidden without -vv.
    let remote = envelope(
        3,
        Body::Log {
            level: Level::Debug,
            node: Some("n1".into()),
            task: Some("bootstrap/kubeadm@n1".into()),
            stream: Stream::Stdout,
            line: "[init] Using Kubernetes version".into(),
        },
    );
    assert!(renderer.render(&remote).is_none());
}

#[test]
fn timestamps_mode_has_a_stable_prefix() {
    let renderer = TerminalRenderer::with_writer(Vec::new(), true, 2);
    let remote = envelope(
        3,
        Body::Log {
            level: Level::Debug,
            node: Some("n1".into()),
            task: Some("bootstrap/kubeadm@n1".into()),
            stream: Stream::Stdout,
            line: "[init] Using Kubernetes version: v1.33.1".into(),
        },
    );
    insta::assert_snapshot!("timestamps-remote-log", renderer.render(&remote).unwrap());
}

#[test]
fn seq_is_monotonic_and_ts_is_utc_milliseconds() {
    let bus = kurin::events::EventBus::new("op");
    let first = bus.publish(Body::ClusterReady);
    let second = bus.publish(Body::ClusterReady);
    assert_eq!(first.v, 2);
    assert!(second.seq > first.seq);
    assert!(first.ts.millis() > 1_600_000_000_000);
    assert!(first.ts.format_rfc3339().ends_with('Z'));
}

#[test]
fn configured_ssh_become_and_registry_secrets_are_redacted() {
    use kurin::events::{cluster_secrets, register_cluster_secrets, Redactor, REDACTED};

    const SSH_PASSWORD: &str = "fake-ssh-password-not-real";
    const BECOME_PASSWORD: &str = "fake-become-password-not-real";
    let cluster = kurin_config::parse_str(
        std::path::Path::new("secrets.toml"),
        &format!(
            r#"version = "v1alpha1"
name = "secrets"
[ssh]
user = "ubuntu"
become-password = "{BECOME_PASSWORD}"
[ssh.auth]
method = "password"
password = "{SSH_PASSWORD}"
[registry]
kind = "harbor"
username = "admin"
password = "fake-harbor-password"
[nodes.n1]
address = "10.0.0.11"
role = "control-plane"
[nodes.reg1]
address = "10.0.0.41"
role = "registry"
"#
        ),
    )
    .expect("fake-secret config parses");

    let secrets = cluster_secrets(&cluster);
    assert!(secrets.iter().any(|s| s == SSH_PASSWORD), "{secrets:?}");
    assert!(secrets.iter().any(|s| s == BECOME_PASSWORD), "{secrets:?}");
    assert!(
        secrets.iter().any(|s| s == "fake-harbor-password"),
        "{secrets:?}"
    );

    let redactor = Redactor::new();
    register_cluster_secrets(&redactor, &cluster);
    let bus = kurin::events::EventBus::with_redactor("op", redactor);

    let log = bus.publish(Body::Log {
        level: Level::Info,
        node: Some("n1".into()),
        task: None,
        stream: Stream::Kurin,
        line: format!("connecting with {SSH_PASSWORD} / {BECOME_PASSWORD}"),
    });
    let text = serde_json::to_string(&log).unwrap();
    assert!(!text.contains(SSH_PASSWORD), "{text}");
    assert!(!text.contains(BECOME_PASSWORD), "{text}");
    assert!(text.contains(REDACTED), "{text}");

    let failed = bus.publish(Body::OpFinished {
        result: OpResult::Failed,
        ms: 1,
        error: Some("registry login failed: fake-harbor-password".to_string()),
    });
    let text = serde_json::to_string(&failed).unwrap();
    assert!(!text.contains("fake-harbor-password"), "{text}");
    assert!(text.contains(REDACTED), "{text}");
}
