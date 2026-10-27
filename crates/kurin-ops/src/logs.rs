//! `kurin logs`: read the JSONL operation logs with cluster/op/node/level
//! filters.

use std::time::Duration;

use kurin_core::events::{self, Level, LogQuery};
use miette::{IntoDiagnostic, Result};

use crate::output::OutputOpts;

/// `kurin logs` filters. Without `op` the newest log is shown.
#[derive(Debug, Clone, Default)]
pub struct LogsRequest {
    pub cluster: Option<String>,
    pub op: Option<String>,
    pub last: bool,
    pub follow: bool,
    pub node: Option<String>,
    pub level: Option<String>,
}

pub(crate) async fn logs(args: LogsRequest, out: OutputOpts) -> Result<()> {
    let min_level = match args.level.as_deref() {
        Some(level) => Level::parse(level).ok_or_else(|| {
            miette::miette!("invalid --level {level:?} (trace|debug|info|warn|error)")
        })?,
        None if out.verbose >= 2 => Level::Debug,
        None => Level::Info,
    };
    let query = LogQuery {
        min_level,
        node: args.node.clone(),
    };
    let cluster = resolve_cluster(args.cluster)?;
    // `--op` wins; otherwise (including `--last`) the newest log is read.
    let _ = args.last;
    let path = match &args.op {
        Some(op) => events::find_log(&cluster, op).ok_or_else(|| {
            miette::miette!("no operation log matching {op:?} for cluster {cluster}")
        })?,
        None => events::latest_log(&cluster)
            .ok_or_else(|| miette::miette!("no operation logs for cluster {cluster}"))?,
    };

    let mut offset = 0u64;
    loop {
        let envelopes = events::read_log_from(&path, &mut offset)
            .map_err(|e| miette::miette!("read {}: {e}", path.display()))?;
        for envelope in envelopes {
            if !query.matches(&envelope) {
                continue;
            }
            if out.json_enabled() {
                println!("{}", serde_json::to_string(&envelope).into_diagnostic()?);
            } else {
                println!("{}", kurin_core::render::format_envelope(&envelope));
            }
        }
        if !args.follow {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    Ok(())
}

fn resolve_cluster(explicit: Option<String>) -> Result<String> {
    if let Some(cluster) = explicit {
        return Ok(cluster);
    }
    let mut clusters = events::list_clusters().into_diagnostic()?;
    match clusters.len() {
        1 => Ok(clusters.remove(0)),
        0 => Err(miette::miette!(
            "no operation logs yet; run a kurin operation first"
        )),
        _ => Err(miette::miette!(
            "logs exist for multiple clusters ({}); pass --cluster",
            clusters.join(", ")
        )),
    }
}
