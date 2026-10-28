//! Plan renderer: `--dry-run` text tree + `--json` event lines.
//! The tree shows stage -> node -> task one-liner; check mode annotates
//! `would-change` (drifted) vs `unchanged` after live probes.
//!
//! Also owns the two event sinks the CLI attaches to the bus: the terminal
//! renderer (stderr, byte-compatible with the pre-v2 output) and the
//! `--json=v2` envelope writer (stdout).

use std::io::{self, Write};
use std::sync::Mutex;

use crate::engine::{Drift, Event};
use crate::events::{Body, Envelope, EventSink, Level, Stream};
use crate::plan::Plan;

/// Render the plan as the `--dry-run` tree.
pub fn render_tree(plan: &Plan) -> String {
    let mut out = String::new();
    let groups = plan.phase_groups();
    for (si, group) in groups.iter().enumerate() {
        let last_stage = si + 1 == groups.len();
        let v = if last_stage { "└" } else { "├" };
        out.push_str(&format!(
            "{v}── stage: {} ({} tasks)\n",
            group.phase.label,
            group.tasks.len()
        ));
        for (ti, planned) in group.tasks.iter().enumerate() {
            let last_t = ti + 1 == group.tasks.len();
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

/// Human-readable one-line rendering of an envelope, used by `kurin logs`.
pub fn format_envelope(envelope: &Envelope) -> String {
    let ts = envelope.ts.format_rfc3339();
    match &envelope.body {
        Body::Log {
            level,
            node,
            line,
            stream,
            ..
        } => {
            let mut text = line.clone();
            if let Some(node) = node {
                let prefix = format!("[{node}]");
                if !text.starts_with(&prefix) {
                    text = format!("{prefix} {text}");
                }
            }
            let stream_mark = match stream {
                Stream::Kurin => "",
                Stream::Stdout => " (stdout)",
                Stream::Stderr => " (stderr)",
            };
            format!(
                "{ts} {:<5} {text}{stream_mark}",
                level.as_str().to_ascii_uppercase()
            )
        }
        Body::OpStarted {
            kind,
            cluster,
            kurin,
            ..
        } => format!("{ts} INFO  op {kind} started (cluster {cluster}, kurin {kurin})"),
        Body::PhaseStarted { phase } => format!("{ts} INFO  -- phase {phase}"),
        Body::PhaseDone { phase, ms } => format!("{ts} INFO  -- phase {phase} complete ({ms}ms)"),
        Body::TaskStart { key, .. } => format!("{ts} INFO  start {key}"),
        Body::TaskSkipped { key, reason, .. } => match reason {
            Some(reason) => format!("{ts} INFO  skip  {key} ({reason})"),
            None => format!("{ts} INFO  skip  {key}"),
        },
        Body::TaskDone { key, ms, .. } => format!("{ts} INFO  done  {key} ({ms}ms)"),
        Body::TaskReconciled { key, .. } => format!("{ts} INFO  ok    {key} (reconciled)"),
        Body::TaskFailed { key, reason, .. } => format!("{ts} ERROR FAIL  {key}: {reason}"),
        Body::TaskWarn { key, message, .. } => format!("{ts} WARN  {key}: {message}"),
        Body::Resource {
            kind,
            namespace,
            name,
            ready,
            status,
        } => {
            let ns = namespace
                .as_deref()
                .map(|ns| format!("{ns}/"))
                .unwrap_or_default();
            format!(
                "{ts} INFO  {kind} {ns}{name} ready={ready} {}",
                status.trim()
            )
        }
        Body::ClusterReady => format!("{ts} INFO  cluster ready"),
        Body::OpFinished { result, ms, error } => match error {
            Some(error) => format!("{ts} ERROR op finished: {result:?} ({ms}ms): {error}"),
            None => format!("{ts} INFO  op finished: {result:?} ({ms}ms)"),
        },
    }
}

/// Terminal sink: prints `Body::Log` lines to stderr in the pre-v2 format.
///
/// Visibility follows the old CLI contract:
/// - kurin's own messages (`Stream::Kurin`) at info and above are always
///   shown, so `upgrade`/`teardown`/`certs` progress is unchanged; debug
///   kurin lines (the per-command remote-output summary) appear with `-v`;
/// - remote stdout/stderr lines are debug and appear only with `-vv`;
/// - `--timestamps` prefixes every line with its RFC 3339 UTC timestamp.
///
/// Task/phase events are not printed here: the v1 adapter already renders
/// them (`skip/done/ok/FAIL/-- stage`), and printing both would duplicate.
pub struct TerminalRenderer {
    timestamps: bool,
    verbosity: u8,
    rich: bool,
    color: bool,
    out: Mutex<Box<dyn Write + Send>>,
}

impl TerminalRenderer {
    /// Renderer writing to stderr.
    pub fn stderr(timestamps: bool, verbosity: u8) -> Self {
        Self::with_writer(io::stderr(), timestamps, verbosity)
    }

    /// Rich renderer (levels, compact timestamps, indented remote output).
    pub fn stderr_rich(timestamps: bool, verbosity: u8, color: bool) -> Self {
        Self::with_writer_rich(io::stderr(), timestamps, verbosity, color)
    }

    /// Renderer writing to an arbitrary sink (tests, Web).
    pub fn with_writer<W: Write + Send + 'static>(
        writer: W,
        timestamps: bool,
        verbosity: u8,
    ) -> Self {
        Self {
            timestamps,
            verbosity,
            rich: false,
            color: false,
            out: Mutex::new(Box::new(writer)),
        }
    }

    /// Rich renderer writing to an arbitrary sink.
    pub fn with_writer_rich<W: Write + Send + 'static>(
        writer: W,
        timestamps: bool,
        verbosity: u8,
        color: bool,
    ) -> Self {
        Self {
            timestamps,
            verbosity,
            rich: true,
            color,
            out: Mutex::new(Box::new(writer)),
        }
    }

    /// Format `envelope` for the terminal, or `None` when it is filtered
    /// out for this verbosity.
    pub fn render(&self, envelope: &Envelope) -> Option<String> {
        if self.rich {
            return self.render_rich(envelope);
        }
        let text = match &envelope.body {
            Body::Log {
                level,
                node,
                stream,
                line,
                ..
            } => {
                let visible = match stream {
                    Stream::Kurin => *level >= Level::Info || self.verbosity >= 1,
                    Stream::Stdout | Stream::Stderr => self.verbosity >= 2,
                };
                if !visible {
                    return None;
                }
                let mut text = line.clone();
                if let Some(node) = node {
                    let prefix = format!("[{node}]");
                    if !text.starts_with(&prefix) {
                        text = format!("{prefix} {text}");
                    }
                }
                text
            }
            Body::TaskWarn { key, message, .. } => format!("warn: {key}: {message}"),
            _ => return None,
        };
        if self.timestamps {
            Some(format!("{} {text}", envelope.ts.format_rfc3339()))
        } else {
            Some(text)
        }
    }
}

impl TerminalRenderer {
    fn render_rich(&self, envelope: &Envelope) -> Option<String> {
        let stamp = if self.timestamps {
            envelope.ts.format_rfc3339()
        } else {
            envelope.ts.format_hms()
        };
        match &envelope.body {
            Body::Log {
                level,
                node,
                stream,
                line,
                ..
            } => {
                let visible = match stream {
                    Stream::Kurin => *level >= Level::Info || self.verbosity >= 1,
                    Stream::Stdout | Stream::Stderr => self.verbosity >= 2,
                };
                if !visible {
                    return None;
                }
                let label = match level {
                    Level::Trace => "TRACE",
                    Level::Debug => "DEBUG",
                    Level::Info => "INFO",
                    Level::Warn => "WARN",
                    Level::Error => "ERROR",
                };
                let code = match level {
                    Level::Trace | Level::Debug => Some("2"),
                    Level::Info => None,
                    Level::Warn => Some("33"),
                    Level::Error => Some("31"),
                };
                let label = match (self.color, code) {
                    (true, Some(code)) => format!("\x1b[{code}m{label:<5}\x1b[0m"),
                    _ => format!("{label:<5}"),
                };
                let node = node
                    .as_deref()
                    .map(|node| {
                        let prefix = format!("[{node}]");
                        if line.starts_with(&prefix) {
                            String::new()
                        } else {
                            format!("{prefix} ")
                        }
                    })
                    .unwrap_or_default();
                let prefix = match stream {
                    Stream::Kurin => "",
                    Stream::Stdout | Stream::Stderr => "  │ ",
                };
                let text = if matches!(stream, Stream::Kurin) {
                    line.strip_prefix("warn: ")
                        .or_else(|| line.strip_prefix("error: "))
                        .unwrap_or(line)
                } else {
                    line
                };
                Some(format!("[{stamp}] {label} {node}{prefix}{text}"))
            }
            Body::TaskWarn { key, message, .. } => {
                let label = if self.color {
                    "\x1b[33mWARN \x1b[0m".to_string()
                } else {
                    "WARN ".to_string()
                };
                Some(format!("[{stamp}] {label} {key}: {message}"))
            }
            _ => None,
        }
    }
}

impl EventSink for TerminalRenderer {
    fn handle(&self, envelope: &Envelope) {
        if let Some(line) = self.render(envelope) {
            let mut out = self.out.lock().unwrap_or_else(|e| e.into_inner());
            let _ = writeln!(out, "{line}");
            let _ = out.flush();
        }
    }
}

/// `--json=v2` sink: one envelope per line on stdout.
pub struct JsonSink {
    out: Mutex<Box<dyn Write + Send>>,
}

impl JsonSink {
    /// Sink writing to stdout.
    pub fn stdout() -> Self {
        Self::with_writer(io::stdout())
    }

    /// Sink writing to an arbitrary sink.
    pub fn with_writer<W: Write + Send + 'static>(writer: W) -> Self {
        Self {
            out: Mutex::new(Box::new(writer)),
        }
    }
}

impl EventSink for JsonSink {
    fn handle(&self, envelope: &Envelope) {
        let Ok(line) = serde_json::to_string(envelope) else {
            return;
        };
        let mut out = self.out.lock().unwrap_or_else(|e| e.into_inner());
        let _ = writeln!(out, "{line}");
        let _ = out.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{Body, Envelope, Level, OpKind, OpResult, Stream, Timestamp};

    fn envelope(seq: u64, body: Body) -> Envelope {
        Envelope {
            v: 2,
            seq,
            ts: Timestamp::from_millis(1_790_000_000_123),
            op: "op-1".into(),
            body,
        }
    }

    fn log(level: Level, stream: Stream, line: &str) -> Envelope {
        envelope(
            1,
            Body::Log {
                level,
                node: Some("n1".into()),
                task: Some("bootstrap/kubeadm@n1".into()),
                stream,
                line: line.into(),
            },
        )
    }

    #[test]
    fn event_json_line_is_single_line_with_type_tag_and_ts() {
        let line = event_json_line(&Event::TaskDone {
            id: "prepare/sysctl@n1".into(),
            node: "n1".into(),
            stage: "prepare",
            ms: 12,
            ts: 1_790_000_000_123,
        });
        assert!(!line.contains('\n'));
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["type"], "task-done");
        assert_eq!(value["id"], "prepare/sysctl@n1");
        assert_eq!(value["ms"], 12);
        assert_eq!(value["ts"], 1_790_000_000_123u64);
    }

    #[test]
    fn every_event_kind_matches_kebab_type_tag() {
        let events = [
            Event::TaskStart {
                id: "a".into(),
                node: "n".into(),
                stage: "prepare",
                ts: 1,
            },
            Event::TaskSkipped {
                id: "a".into(),
                node: "n".into(),
                stage: "prepare",
                reason: None,
                ts: 1,
            },
            Event::TaskFailed {
                id: "a".into(),
                node: "n".into(),
                stage: "prepare",
                reason: "x".into(),
                ts: 1,
            },
            Event::TaskReconciled {
                id: "a".into(),
                node: "n".into(),
                stage: "prepare",
                ts: 1,
            },
            Event::StageDone {
                stage: "prepare",
                ts: 1,
            },
            Event::ClusterReady { ts: 1 },
        ];
        for event in &events {
            let value: serde_json::Value = serde_json::from_str(&event_json_line(event)).unwrap();
            assert_eq!(value["type"], event.kind());
        }
    }

    /// v1.0 gate: the exact JSON field set per event kind is the frozen
    /// `--json` contract (docs/v1.0-gates.md §2). A change here is a
    /// breaking change for every consumer parsing the event stream; `ts` is
    /// the one allowed addition.
    #[test]
    fn every_event_kind_has_frozen_field_set_plus_ts() {
        let cases: Vec<(Event, Vec<&str>)> = vec![
            (
                Event::TaskStart {
                    id: "a".into(),
                    node: "n".into(),
                    stage: "prepare",
                    ts: 1,
                },
                vec!["type", "id", "node", "stage", "ts"],
            ),
            (
                Event::TaskSkipped {
                    id: "a".into(),
                    node: "n".into(),
                    stage: "prepare",
                    reason: None,
                    ts: 1,
                },
                vec!["type", "id", "node", "stage", "ts"],
            ),
            (
                Event::TaskDone {
                    id: "a".into(),
                    node: "n".into(),
                    stage: "prepare",
                    ms: 1,
                    ts: 1,
                },
                vec!["type", "id", "node", "stage", "ms", "ts"],
            ),
            (
                Event::TaskFailed {
                    id: "a".into(),
                    node: "n".into(),
                    stage: "prepare",
                    reason: "x".into(),
                    ts: 1,
                },
                vec!["type", "id", "node", "stage", "reason", "ts"],
            ),
            (
                Event::TaskReconciled {
                    id: "a".into(),
                    node: "n".into(),
                    stage: "prepare",
                    ts: 1,
                },
                vec!["type", "id", "node", "stage", "ts"],
            ),
            (
                Event::StageDone {
                    stage: "prepare",
                    ts: 1,
                },
                vec!["type", "stage", "ts"],
            ),
            (Event::ClusterReady { ts: 1 }, vec!["type", "ts"]),
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

    #[test]
    fn terminal_renderer_default_matches_pre_v2_text() {
        let renderer = TerminalRenderer::with_writer(Vec::new(), false, 0);
        let info = renderer
            .render(&log(Level::Info, Stream::Kurin, "[n1] ok step one"))
            .expect("kurin info is visible by default");
        assert_eq!(info, "[n1] ok step one");

        assert!(
            renderer
                .render(&log(Level::Debug, Stream::Stdout, "remote line"))
                .is_none(),
            "remote output is hidden without -vv"
        );
        assert!(renderer
            .render(&log(Level::Debug, Stream::Kurin, "debug"))
            .is_none());
    }

    #[test]
    fn rich_terminal_renderer_prefixes_levels_and_indents_remote_output() {
        let renderer = TerminalRenderer::with_writer_rich(Vec::new(), false, 2, false);
        let info = renderer
            .render(&log(Level::Info, Stream::Kurin, "hello"))
            .expect("info visible");
        assert!(info.contains("INFO"), "{info}");
        assert!(info.contains("[n1] hello"), "{info}");
        assert!(info.starts_with('['), "{info}");

        let remote = renderer
            .render(&log(Level::Debug, Stream::Stdout, "remote line"))
            .expect("-vv shows remote output");
        assert!(remote.contains("DEBUG"), "{remote}");
        assert!(remote.contains("│ remote line"), "{remote}");

        let colored = TerminalRenderer::with_writer_rich(Vec::new(), false, 2, true);
        let line = colored
            .render(&log(Level::Error, Stream::Kurin, "boom"))
            .expect("error visible");
        assert!(line.contains("\x1b[31m"), "{line}");
    }

    #[test]
    fn terminal_renderer_timestamps_and_verbosity() {
        let renderer = TerminalRenderer::with_writer(Vec::new(), true, 2);
        let line = renderer
            .render(&log(Level::Debug, Stream::Stdout, "remote line"))
            .expect("-vv shows remote output");
        assert!(line.starts_with("2026-09-21T"));
        assert!(line.ends_with("[n1] remote line"));
    }

    #[test]
    fn verbosity_one_shows_the_remote_summary_but_not_raw_lines() {
        let renderer = TerminalRenderer::with_writer(Vec::new(), false, 1);
        let summary = log(
            Level::Debug,
            Stream::Kurin,
            "[n1] bootstrap/kubeadm@n1: 3 stdout line(s), 0 stderr line(s)",
        );
        assert!(renderer.render(&summary).is_some(), "-v shows the summary");
        assert!(
            renderer
                .render(&log(Level::Debug, Stream::Stdout, "raw remote line"))
                .is_none(),
            "-v must not stream raw remote output"
        );
    }

    #[test]
    fn format_envelope_covers_every_body() {
        let bodies = [
            Body::OpStarted {
                kind: OpKind::new("create-cluster"),
                cluster: "demo".into(),
                plan_fingerprint: "abc".into(),
                kurin: "0.4.0".into(),
            },
            Body::PhaseStarted {
                phase: "prepare".into(),
            },
            Body::PhaseDone {
                phase: "prepare".into(),
                ms: 3,
            },
            Body::TaskStart {
                key: "prepare/sysctl@n1".into(),
                node: "n1".into(),
                phase: "prepare".into(),
            },
            Body::TaskSkipped {
                key: "prepare/sysctl@n1".into(),
                node: "n1".into(),
                phase: "prepare".into(),
                reason: None,
            },
            Body::TaskDone {
                key: "prepare/sysctl@n1".into(),
                node: "n1".into(),
                phase: "prepare".into(),
                ms: 4,
            },
            Body::TaskReconciled {
                key: "join/kubeadm@n2".into(),
                node: "n2".into(),
                phase: "join".into(),
            },
            Body::TaskFailed {
                key: "cni/calico@n1".into(),
                node: "n1".into(),
                phase: "cni".into(),
                reason: "boom".into(),
            },
            Body::TaskWarn {
                key: "post/summary@n1".into(),
                node: "n1".into(),
                message: "best effort".into(),
            },
            Body::Resource {
                kind: "deployment".into(),
                namespace: Some("kube-system".into()),
                name: "coredns".into(),
                ready: false,
                status: "1/2".into(),
            },
            Body::ClusterReady,
            Body::OpFinished {
                result: OpResult::Succeeded,
                ms: 9,
                error: None,
            },
        ];
        for body in bodies {
            let text = format_envelope(&envelope(1, body));
            assert!(text.starts_with("2026-"), "{text}");
            assert!(!text.contains('\n'), "{text}");
        }
    }
}
