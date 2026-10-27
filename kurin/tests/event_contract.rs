//! WP-00/WP-06: the `--json` v1 event contract (docs/v1.0-gates.md §2).
//! For every `Event` variant, the exact JSON field set is frozen: removing
//! or renaming a field breaks every consumer parsing the event stream.
//! WP-06 added exactly one field, `ts` (UTC milliseconds, from the v2
//! envelope), which the gate explicitly allows as an addition.

use kurin::render::event_json_line;
use kurin::Event;
use serde_json::Value;

/// Fixed timestamp so the assertions never depend on the wall clock.
const TS: u64 = 1_790_000_000_123;

#[test]
fn every_event_variant_has_the_frozen_field_set_plus_ts() {
    let cases: Vec<(Event, &str, Vec<&str>)> = vec![
        (
            Event::TaskStart {
                id: "prepare/sysctl@n1".into(),
                node: "n1".into(),
                stage: "prepare",
                ts: TS,
            },
            "task-start",
            vec!["type", "id", "node", "stage", "ts"],
        ),
        (
            Event::TaskSkipped {
                id: "preflight/n1".into(),
                node: "n1".into(),
                stage: "preflight",
                reason: None,
                ts: TS,
            },
            "task-skipped",
            vec!["type", "id", "node", "stage", "ts"],
        ),
        // WP-11: the only addition is the optional `reason` on
        // `task-skipped` (why a task did not run). Present only when the
        // task was blocked, so the in-sync shape above is unchanged.
        (
            Event::TaskSkipped {
                id: "join/kubeadm@w1".into(),
                node: "w1".into(),
                stage: "join",
                reason: Some("blocked by bootstrap/kubeadm-init@cp1".into()),
                ts: TS,
            },
            "task-skipped",
            vec!["type", "id", "node", "stage", "reason", "ts"],
        ),
        (
            Event::TaskDone {
                id: "prepare/sysctl@n1".into(),
                node: "n1".into(),
                stage: "prepare",
                ms: 12,
                ts: TS,
            },
            "task-done",
            vec!["type", "id", "node", "stage", "ms", "ts"],
        ),
        (
            Event::TaskFailed {
                id: "cni/calico-operator@n1".into(),
                node: "n1".into(),
                stage: "cni",
                reason: "calico apply failed: connection refused".into(),
                ts: TS,
            },
            "task-failed",
            vec!["type", "id", "node", "stage", "reason", "ts"],
        ),
        (
            Event::TaskReconciled {
                id: "join/kubeadm@n2".into(),
                node: "n2".into(),
                stage: "join",
                ts: TS,
            },
            "task-reconciled",
            vec!["type", "id", "node", "stage", "ts"],
        ),
        (
            Event::StageDone {
                stage: "post",
                ts: TS,
            },
            "stage-done",
            vec!["type", "stage", "ts"],
        ),
        (
            Event::ClusterReady { ts: TS },
            "cluster-ready",
            vec!["type", "ts"],
        ),
    ];

    for (event, tag, mut fields) in cases {
        let line = event_json_line(&event);
        assert!(!line.contains('\n'), "{tag}: one JSON object per line");
        let value: Value = serde_json::from_str(&line)
            .unwrap_or_else(|e| panic!("{tag}: line is valid JSON ({e}): {line}"));
        // The discriminant matches both the enum's own view and the wire.
        assert_eq!(value["type"], tag, "{tag}: wire type tag");
        assert_eq!(event.kind(), tag, "{tag}: Event::kind() agrees");
        assert_eq!(value["ts"], TS, "{tag}: envelope timestamp is carried");
        let mut got: Vec<&str> = value
            .as_object()
            .unwrap_or_else(|| panic!("{tag}: event must serialize to an object"))
            .keys()
            .map(|k| k.as_str())
            .collect();
        got.sort_unstable();
        fields.sort_unstable();
        assert_eq!(got, fields, "{tag}: frozen field set");
    }
}
