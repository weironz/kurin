//! WP-00: the `--json` v1 event contract (docs/v1.0-gates.md §2). For every
//! `Event` variant, the exact JSON field set is frozen: removing or
//! renaming a field breaks every consumer parsing the event stream, adding
//! an *optional* field is allowed. This mirrors the unit test next to
//! `event_json_line` but runs against the public rendering API, so the
//! contract is asserted from the outside, like a consumer would see it.

use kurin::render::event_json_line;
use kurin::Event;
use serde_json::Value;

#[test]
fn every_event_variant_has_the_frozen_field_set() {
    let cases: Vec<(Event, &str, Vec<&str>)> = vec![
        (
            Event::TaskStart {
                id: "prepare/sysctl@n1".into(),
                node: "n1".into(),
                stage: "prepare",
            },
            "task-start",
            vec!["type", "id", "node", "stage"],
        ),
        (
            Event::TaskSkipped {
                id: "preflight/n1".into(),
                node: "n1".into(),
                stage: "preflight",
            },
            "task-skipped",
            vec!["type", "id", "node", "stage"],
        ),
        (
            Event::TaskDone {
                id: "prepare/sysctl@n1".into(),
                node: "n1".into(),
                stage: "prepare",
                ms: 12,
            },
            "task-done",
            vec!["type", "id", "node", "stage", "ms"],
        ),
        (
            Event::TaskFailed {
                id: "cni/calico-operator@n1".into(),
                node: "n1".into(),
                stage: "cni",
                reason: "calico apply failed: connection refused".into(),
            },
            "task-failed",
            vec!["type", "id", "node", "stage", "reason"],
        ),
        (
            Event::TaskReconciled {
                id: "join/kubeadm@n2".into(),
                node: "n2".into(),
                stage: "join",
            },
            "task-reconciled",
            vec!["type", "id", "node", "stage"],
        ),
        (
            Event::StageDone { stage: "post" },
            "stage-done",
            vec!["type", "stage"],
        ),
        (Event::ClusterReady, "cluster-ready", vec!["type"]),
    ];

    for (event, tag, mut fields) in cases {
        let line = event_json_line(&event);
        assert!(!line.contains('\n'), "{tag}: one JSON object per line");
        let value: Value = serde_json::from_str(&line)
            .unwrap_or_else(|e| panic!("{tag}: line is valid JSON ({e}): {line}"));
        // The discriminant matches both the enum's own view and the wire.
        assert_eq!(value["type"], tag, "{tag}: wire type tag");
        assert_eq!(event.kind(), tag, "{tag}: Event::kind() agrees");
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
