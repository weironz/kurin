//! `kurin status`: probe the cluster and print the structured report.

use std::sync::Arc;

use kurin_config::Cluster;
use kurin_core::executor::SessionPool;
use miette::{IntoDiagnostic, Result};

use crate::output::{OpSession, OutputOpts};
use crate::{StatusOpts, StatusReport};

/// Probe the cluster and print the report (JSON or the human table).
pub(crate) async fn status(
    spec: &Cluster,
    opts: StatusOpts,
    out: OutputOpts,
) -> Result<StatusReport> {
    let pool: Arc<dyn SessionPool> = Arc::new(kurin_ssh::SshSessionPool::from_cluster(spec)?);
    let run = crate::ephemeral_run(spec)?;
    let session = OpSession::start(&run, out)?;
    session.register_cluster(spec);
    session.begin("status", &spec.name, "");
    let result = async {
        let report = kurin_components::status::collect(spec, &pool, opts).await?;
        if out.json_enabled() {
            // The struct is the contract: serialize it directly so the field
            // set cannot drift from the type.
            println!(
                "{}",
                serde_json::to_string_pretty(&report).into_diagnostic()?
            );
        } else {
            kurin_components::status::render(&report);
        }
        Ok(report)
    }
    .await;
    session.finish(&result);
    result
}
