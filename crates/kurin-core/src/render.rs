//! Plan renderer: `--dry-run` text tree + `--json` event lines.
//! The tree shows stage -> node -> task one-liner; check mode annotates
//! `would-change` (drifted) vs `unchanged` after live probes.

use crate::engine::{Drift, Event};
use crate::plan::Plan;

pub fn render_tree(plan: &Plan) -> String {
    let mut out = String::new();
    for (si, &idx) in plan.stages.iter().enumerate() {
        let stage = &plan.graph[idx];
        let last_stage = si + 1 == plan.stages.len();
        let v = if last_stage { "└" } else { "├" };
        out.push_str(&format!(
            "{v}── stage: {} ({} tasks)\n",
            stage.stage.label(),
            stage.tasks.len()
        ));
        for (ti, planned) in stage.tasks.iter().enumerate() {
            let last_t = ti + 1 == stage.tasks.len();
            let branch = if last_stage { "    " } else { "│   " };
            let tv = if last_t { "└" } else { "├" };
            let drift = match &planned.drift {
                None => String::new(),
                Some(Drift::InSync) => " [unchanged]".to_string(),
                Some(Drift::Drifted(r)) => format!(" [would-change: {r}]"),
            };
            out.push_str(&format!(
                "{branch}{tv}── {} :: {}{drift}\n",
                planned.task.id(),
                planned.task.describe()
            ));
        }
    }
    out
}

/// One JSON object per line, for `--json` event streaming. The caller owns
/// the sink (stdout) and flushing; this only guarantees a single line per
/// event.
pub fn event_json_line(event: &Event) -> String {
    serde_json::to_string(event).expect("Event serialization is infallible")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_json_line_is_single_line_with_type_tag() {
        let line = event_json_line(&Event::TaskDone {
            id: "prepare/sysctl@n1".into(),
            node: "n1".into(),
            stage: "prepare",
            ms: 12,
        });
        assert!(!line.contains('\n'));
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["type"], "task-done");
        assert_eq!(value["id"], "prepare/sysctl@n1");
        assert_eq!(value["ms"], 12);
    }

    #[test]
    fn every_event_kind_matches_kebab_type_tag() {
        let events = [
            Event::TaskStart {
                id: "a".into(),
                node: "n".into(),
                stage: "prepare",
            },
            Event::TaskSkipped {
                id: "a".into(),
                node: "n".into(),
                stage: "prepare",
            },
            Event::TaskFailed {
                id: "a".into(),
                node: "n".into(),
                stage: "prepare",
                reason: "x".into(),
            },
            Event::TaskReconciled {
                id: "a".into(),
                node: "n".into(),
                stage: "prepare",
            },
            Event::StageDone { stage: "prepare" },
            Event::ClusterReady,
        ];
        for event in &events {
            let value: serde_json::Value = serde_json::from_str(&event_json_line(event)).unwrap();
            assert_eq!(value["type"], event.kind());
        }
    }

    /// v1.0 gate: the exact JSON field set per event kind is the frozen
    /// `--json` contract (docs/v1.0-gates.md §2). A change here is a
    /// breaking change for every consumer parsing the event stream.
    #[test]
    fn every_event_kind_has_frozen_field_set() {
        let cases: Vec<(Event, Vec<&str>)> = vec![
            (
                Event::TaskStart {
                    id: "a".into(),
                    node: "n".into(),
                    stage: "prepare",
                },
                vec!["type", "id", "node", "stage"],
            ),
            (
                Event::TaskSkipped {
                    id: "a".into(),
                    node: "n".into(),
                    stage: "prepare",
                },
                vec!["type", "id", "node", "stage"],
            ),
            (
                Event::TaskDone {
                    id: "a".into(),
                    node: "n".into(),
                    stage: "prepare",
                    ms: 1,
                },
                vec!["type", "id", "node", "stage", "ms"],
            ),
            (
                Event::TaskFailed {
                    id: "a".into(),
                    node: "n".into(),
                    stage: "prepare",
                    reason: "x".into(),
                },
                vec!["type", "id", "node", "stage", "reason"],
            ),
            (
                Event::TaskReconciled {
                    id: "a".into(),
                    node: "n".into(),
                    stage: "prepare",
                },
                vec!["type", "id", "node", "stage"],
            ),
            (Event::StageDone { stage: "prepare" }, vec!["type", "stage"]),
            (Event::ClusterReady, vec!["type"]),
        ];
        for (event, fields) in &cases {
            let value: serde_json::Value = serde_json::from_str(&event_json_line(event)).unwrap();
            let mut got: Vec<&str> = value
                .as_object()
                .unwrap()
                .keys()
                .map(|k| k.as_str())
                .collect();
            got.sort_unstable();
            let mut want = fields.clone();
            want.sort_unstable();
            assert_eq!(got, want, "field set for {}", event.kind());
        }
    }
}
