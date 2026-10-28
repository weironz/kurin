//! Rich terminal progress for `create cluster` / `add nodes`.
//!
//! The historical renderer printed one bare line per terminal task event
//! (`skip/done/ok/FAIL`). This renderer keeps the same event source but
//! presents it as an operator-friendly stream:
//!
//! ```text
//! [09:42:10] 🚀 Starting cluster execution for [prod] (op 20261028T...)
//!
//! ━━▶ Phase 1/9: Preflight ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
//! [09:42:11] [master01] [CHECK ] preflight  ✔ satisfied -> skipped
//! [09:42:11] [master02] [CHECK ] preflight  checking…
//! [09:42:12] [master02] [VERIFY] preflight  ✔ passed (0.31s)
//! ```
//!
//! Only the frozen v1 [`Event`] stream is used: `TaskStart` is buffered
//! until its terminal event, so skipped tasks render one line while applied
//! tasks render CHECK + VERIFY. `APPLY` and upload progress need new events
//! and are intentionally out of scope for this pass.

use std::collections::HashMap;
use std::io::Write;
use std::time::Instant;

use kurin_core::engine::Event;
use kurin_core::events::Timestamp;
use kurin_core::plan::Plan;

const RESET: &str = "\x1b[0m";
const PHASE_WIDTH: usize = 78;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Level {
    Check,
    Verify,
}

impl Level {
    fn label(self) -> &'static str {
        match self {
            Level::Check => "CHECK",
            Level::Verify => "VERIFY",
        }
    }

    fn color(self) -> &'static str {
        match self {
            Level::Check => "\x1b[36m",  // cyan
            Level::Verify => "\x1b[32m", // green
        }
    }
}

#[derive(Debug, Clone)]
struct Pending {
    ts: u64,
    node: String,
    kind: String,
}

/// Stateful terminal renderer. One instance per operation.
pub struct ProgressRenderer<W: Write> {
    out: W,
    color: bool,
    timestamps: bool,
    cluster: String,
    op_id: String,
    stages: Vec<&'static str>,
    current_stage: Option<&'static str>,
    phase_no: usize,
    node_width: usize,
    kind_width: usize,
    pending: HashMap<String, Pending>,
    started: Instant,
    ready: bool,
}

impl<W: Write> ProgressRenderer<W> {
    /// Renderer writing to an arbitrary sink (tests, embedding).
    pub fn with_writer(
        out: W,
        plan: &Plan,
        cluster: &str,
        op_id: &str,
        color: bool,
        timestamps: bool,
    ) -> Self {
        let stages: Vec<&'static str> = plan
            .phase_groups()
            .iter()
            .map(|group| group.phase.label)
            .collect();
        let node_width = plan
            .tasks
            .iter()
            .map(|planned| planned.task.node().len())
            .max()
            .unwrap_or(8)
            .max(8);
        let kind_width = plan
            .tasks
            .iter()
            .map(|planned| kind_of(&planned.task.id()).len())
            .max()
            .unwrap_or(12)
            .max(12);
        Self {
            out,
            color,
            timestamps,
            cluster: cluster.to_string(),
            op_id: op_id.to_string(),
            stages,
            current_stage: None,
            phase_no: 0,
            node_width,
            kind_width,
            pending: HashMap::new(),
            started: Instant::now(),
            ready: false,
        }
    }

    /// Print the operation header. Call once, before the executor starts.
    pub fn start(&mut self) {
        let ts = Timestamp::now();
        let line = format!(
            "[{}] 🚀 Starting cluster execution for [{}] (op {})",
            self.stamp(ts),
            self.cluster,
            self.op_id
        );
        self.write_line(&line);
        self.write_line("");
    }

    /// Consume one v1 event.
    pub fn handle(&mut self, event: Event) {
        match event {
            Event::TaskStart {
                id,
                node,
                stage,
                ts,
            } => {
                self.ensure_phase(stage);
                let kind = kind_of(&id).to_string();
                self.pending.insert(id, Pending { ts, node, kind });
            }
            Event::TaskSkipped {
                id,
                node,
                stage,
                reason,
                ts,
            } => {
                self.ensure_phase(stage);
                self.pending.remove(&id);
                let mut message = String::from("✔ satisfied");
                if let Some(reason) = reason {
                    message.push_str(&format!(" ({reason})"));
                }
                message.push_str(" -> skipped");
                self.task_line(ts, &node, Level::Check, kind_of(&id), &message);
            }
            Event::TaskDone {
                id,
                node,
                stage,
                ms,
                ts,
            } => {
                self.ensure_phase(stage);
                if let Some(pending) = self.pending.remove(&id) {
                    self.task_line(
                        pending.ts,
                        &pending.node,
                        Level::Check,
                        &pending.kind,
                        "checking…",
                    );
                }
                self.task_line(
                    ts,
                    &node,
                    Level::Verify,
                    kind_of(&id),
                    &format!("✔ passed ({})", duration(ms)),
                );
            }
            Event::TaskFailed {
                id,
                node,
                stage,
                reason,
                ts,
            } => {
                self.ensure_phase(stage);
                if let Some(pending) = self.pending.remove(&id) {
                    self.task_line(
                        pending.ts,
                        &pending.node,
                        Level::Check,
                        &pending.kind,
                        "checking…",
                    );
                }
                self.task_line(
                    ts,
                    &node,
                    Level::Verify,
                    kind_of(&id),
                    &format!("✘ failed: {reason}"),
                );
            }
            Event::TaskReconciled {
                id,
                node,
                stage,
                ts,
            } => {
                self.ensure_phase(stage);
                self.pending.remove(&id);
                self.task_line(ts, &node, Level::Check, kind_of(&id), "↻ reconciled");
            }
            Event::StageDone { .. } => {}
            Event::ClusterReady { ts } => {
                if !self.ready {
                    self.ready = true;
                    let elapsed = self.started.elapsed();
                    self.write_line("");
                    self.write_line(&format!(
                        "[{}] 🎉 CLUSTER [{}] IS READY! (Total elapsed: {})",
                        self.stamp(Timestamp::from_millis(ts)),
                        self.cluster,
                        elapsed_text(elapsed)
                    ));
                }
            }
        }
    }

    /// Flush the writer (the caller owns the final error line on failure).
    pub fn finish(&mut self) {
        let _ = self.out.flush();
    }

    /// Test hook: consume the writer.
    #[cfg(test)]
    pub fn into_inner(self) -> W {
        self.out
    }

    fn ensure_phase(&mut self, stage: &'static str) {
        if self.current_stage == Some(stage) {
            return;
        }
        self.current_stage = Some(stage);
        self.phase_no += 1;
        let total = self.stages.len().max(self.phase_no);
        let prefix = format!("━━▶ Phase {}/{}: {stage} ", self.phase_no, total);
        let pad = PHASE_WIDTH.saturating_sub(prefix.chars().count());
        let line = format!("{prefix}{}", "━".repeat(pad));
        self.write_line("");
        self.write_line(&self.paint("1;36", &line));
    }

    fn task_line(&mut self, ts: u64, node: &str, level: Level, kind: &str, message: &str) {
        let stamp = self.stamp(Timestamp::from_millis(ts));
        let node = pad(node, self.node_width);
        let kind = pad(kind, self.kind_width);
        let label = pad(level.label(), 6);
        let label = self.paint(level.color(), &label);
        self.write_line(&format!("[{stamp}] [{node}] [{label}] {kind} {message}"));
    }

    fn stamp(&self, ts: Timestamp) -> String {
        if self.timestamps {
            ts.format_rfc3339()
        } else {
            ts.format_hms()
        }
    }

    fn paint(&self, code: &str, text: &str) -> String {
        if self.color {
            format!("{code}{text}{RESET}")
        } else {
            text.to_string()
        }
    }

    fn write_line(&mut self, line: &str) {
        let _ = writeln!(self.out, "{line}");
        let _ = self.out.flush();
    }
}

fn kind_of(id: &str) -> &str {
    let head = id.rsplit_once('@').map(|(head, _)| head).unwrap_or(id);
    head.rsplit_once('/').map(|(_, kind)| kind).unwrap_or(head)
}

fn pad(text: &str, width: usize) -> String {
    let mut out = text.to_string();
    let len = out.chars().count();
    if len < width {
        out.push_str(&" ".repeat(width - len));
    }
    out
}

fn duration(ms: u128) -> String {
    format!("{:.2}s", ms as f64 / 1000.0)
}

fn elapsed_text(elapsed: std::time::Duration) -> String {
    let secs = elapsed.as_secs();
    if secs >= 60 {
        format!("{}m {:02}s", secs / 60, secs % 60)
    } else {
        format!("{}.{}s", secs, elapsed.subsec_millis() / 100)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn renderer() -> ProgressRenderer<Vec<u8>> {
        // A tiny plan is not needed for formatting tests: build the state
        // through the same fields the production constructor derives.
        ProgressRenderer {
            out: Vec::new(),
            color: false,
            timestamps: false,
            cluster: "demo".into(),
            op_id: "op-1".into(),
            stages: vec!["Preflight", "Bootstrap"],
            current_stage: None,
            phase_no: 0,
            node_width: 8,
            kind_width: 12,
            pending: HashMap::new(),
            started: Instant::now(),
            ready: false,
        }
    }

    fn ts() -> u64 {
        1_790_000_000_000
    }

    #[test]
    fn renders_phase_headers_and_skipped_tasks() {
        let mut r = renderer();
        r.handle(Event::TaskStart {
            id: "preflight/x@master01".into(),
            node: "master01".into(),
            stage: "Preflight",
            ts: ts(),
        });
        r.handle(Event::TaskSkipped {
            id: "preflight/x@master01".into(),
            node: "master01".into(),
            stage: "Preflight",
            reason: Some("already in sync".into()),
            ts: ts(),
        });
        let out = String::from_utf8(r.into_inner()).unwrap();
        assert!(out.contains("Phase 1/2: Preflight"), "{out}");
        assert!(out.contains("[master01]"), "{out}");
        assert!(out.contains("[CHECK ]"), "{out}");
        assert!(
            out.contains("✔ satisfied (already in sync) -> skipped"),
            "{out}"
        );
        assert!(
            !out.contains("checking…"),
            "start is buffered for skips: {out}"
        );
    }

    #[test]
    fn renders_check_then_verify_for_applied_tasks() {
        let mut r = renderer();
        r.handle(Event::TaskStart {
            id: "bootstrap/kubeadm@master01".into(),
            node: "master01".into(),
            stage: "Bootstrap",
            ts: ts(),
        });
        r.handle(Event::TaskDone {
            id: "bootstrap/kubeadm@master01".into(),
            node: "master01".into(),
            stage: "Bootstrap",
            ms: 310,
            ts: ts() + 310,
        });
        let out = String::from_utf8(r.into_inner()).unwrap();
        assert!(out.contains("Phase 1/2: Bootstrap"), "{out}");
        assert!(out.contains("checking…"), "{out}");
        assert!(out.contains("[VERIFY]"), "{out}");
        assert!(out.contains("✔ passed (0.31s)"), "{out}");
    }

    #[test]
    fn renders_failures_and_ready_footer() {
        let mut r = renderer();
        r.handle(Event::TaskFailed {
            id: "bootstrap/kubeadm@master01".into(),
            node: "master01".into(),
            stage: "Bootstrap",
            reason: "API server did not start".into(),
            ts: ts(),
        });
        r.handle(Event::ClusterReady { ts: ts() + 1000 });
        let out = String::from_utf8(r.into_inner()).unwrap();
        assert!(out.contains("✘ failed: API server did not start"), "{out}");
        assert!(out.contains("🎉 CLUSTER [demo] IS READY!"), "{out}");
    }
}
