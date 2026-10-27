//! Output plumbing shared by every operation: the `--json` mode, the
//! per-operation event session (JSONL log + terminal/JSON sinks) and the
//! result type the CLI maps to an exit code.

use std::sync::Arc;
use std::time::Instant;

use kurin_core::events::{self, Body, EventBus, JsonlSink, OpKind, OpResult as EventOpResult};
use kurin_core::executor::is_cancelled;
use kurin_core::render::{JsonSink, TerminalRenderer};
use kurin_core::run::RunContext;
use miette::Result;

/// Version of the `--json` stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsonMode {
    /// Frozen v1 event objects, now carrying `ts`.
    V1,
    /// Full v2 envelopes (`v`, `seq`, `ts`, `op`, `body`).
    V2,
}

/// Output knobs shared by every operation.
#[derive(Debug, Clone, Copy, Default)]
pub struct OutputOpts {
    pub json: Option<JsonMode>,
    pub timestamps: bool,
    pub verbose: u8,
}

impl OutputOpts {
    /// True for `--json=v2` (the envelope stream owns stdout).
    pub fn v2(self) -> bool {
        self.json == Some(JsonMode::V2)
    }

    /// True for any `--json` mode.
    pub fn json_enabled(self) -> bool {
        self.json.is_some()
    }
}

/// How an operation ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpOutcome {
    /// The plan was executed.
    Completed,
    /// `--dry-run`: the plan was rendered and nothing was changed.
    Planned,
}

/// What [`crate::execute`] reports back to its caller.
#[derive(Debug, Clone)]
pub struct OpResult {
    pub kind: kurin_core::plan::OpKind,
    pub cluster: String,
    pub fingerprint: String,
    pub outcome: OpOutcome,
}

/// One CLI operation: its event bus, JSONL log and start time.
///
/// The bus is created before the operation does any work so discovery and
/// remote output land in the log; `begin` publishes `OpStarted` once the
/// plan fingerprint is known.
pub(crate) struct OpSession {
    bus: Arc<EventBus>,
    started: Instant,
}

impl OpSession {
    /// Attach the JSONL writer and the terminal/JSON sink to the run's bus,
    /// install the `tracing` bridge and print the log path once. The log
    /// name is the run's `op_id`.
    ///
    /// `cli_output == false` is the resident-server mode: the caller owns
    /// the bus sinks (database + JSONL under the data directory) and the
    /// process-wide tracing router, so this only records the start time.
    pub(crate) fn start(run: &RunContext, out: OutputOpts, cli_output: bool) -> Result<Self> {
        let bus = Arc::clone(&run.events);
        if cli_output {
            let path = events::log_path(&run.cluster, &run.op_id);
            let sink = JsonlSink::create(&path)
                .map_err(|e| miette::miette!("create log {}: {e}", path.display()))?;
            bus.add_sink(Arc::new(sink));
            if out.v2() {
                bus.add_sink(Arc::new(JsonSink::stdout()));
            } else {
                bus.add_sink(Arc::new(TerminalRenderer::stderr(
                    out.timestamps,
                    out.verbose,
                )));
            }
            events::install_tracing(Arc::clone(&bus));
            eprintln!("[logs] {}", path.display());
        }
        Ok(Self {
            bus,
            started: Instant::now(),
        })
    }

    /// Register the cluster's configured secrets before anything runs.
    pub(crate) fn register_cluster(&self, cluster: &kurin_config::Cluster) {
        events::register_cluster_secrets(self.bus.redactor(), cluster);
    }

    /// Publish `OpStarted` with the plan fingerprint (empty when the
    /// operation has no plan).
    pub(crate) fn begin(&self, kind: &str, cluster: &str, plan_fingerprint: &str) {
        self.bus.publish(Body::OpStarted {
            kind: OpKind::new(kind),
            cluster: cluster.to_string(),
            plan_fingerprint: plan_fingerprint.to_string(),
            kurin: env!("CARGO_PKG_VERSION").to_string(),
        });
    }

    /// Publish `OpFinished` and report the operation's outcome. A cancelled
    /// run (the executor returns [`kurin_core::executor::Cancelled`]) is
    /// reported as `cancelled`, not as a failure.
    pub(crate) fn finish<T>(&self, result: &Result<T>) {
        let (result, error) = match result {
            Ok(_) => (EventOpResult::Succeeded, None),
            Err(e) if is_cancelled(e) => (EventOpResult::Cancelled, None),
            Err(e) => (EventOpResult::Failed, Some(format!("{e:?}"))),
        };
        self.bus.publish(Body::OpFinished {
            result,
            ms: self.started.elapsed().as_millis() as u64,
            error,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A cancelled run is reported as `OpFinished { result: cancelled }`
    /// with no error text, not as a failure.
    #[test]
    fn cancelled_run_finishes_as_cancelled() {
        let bus = Arc::new(EventBus::new("op-test"));
        let mut rx = bus.subscribe();
        let session = OpSession {
            bus: Arc::clone(&bus),
            started: Instant::now(),
        };
        let result: Result<()> = Err(miette::Report::new(kurin_core::executor::Cancelled));
        session.finish(&result);

        let envelope = rx.try_recv().expect("op-finished was published");
        match envelope.body {
            Body::OpFinished { result, error, .. } => {
                assert_eq!(result, EventOpResult::Cancelled);
                assert!(error.is_none());
            }
            other => panic!("expected op-finished, got {other:?}"),
        }
    }
}
