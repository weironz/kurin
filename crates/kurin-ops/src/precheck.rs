//! `kurin precheck`: read-only host audit before `create cluster`.

use std::sync::Arc;

use kurin_config::Cluster;
use kurin_core::executor::SessionPool;
use miette::Result;

use crate::output::{OpSession, OutputOpts};
use crate::PrecheckReport;

/// Run the audit and print the report (JSON lines or the human table).
pub(crate) async fn precheck(spec: &Cluster, out: OutputOpts) -> Result<PrecheckReport> {
    let pool: Arc<dyn SessionPool> = Arc::new(kurin_ssh::SshSessionPool::from_cluster(spec)?);
    let run = crate::ephemeral_run(spec)?;
    let session = OpSession::start(&run, out)?;
    session.register_cluster(spec);
    session.begin("precheck", &spec.name, "");
    let result = async {
        let report = kurin_components::precheck::run(spec, &pool).await?;
        if out.json_enabled() {
            kurin_components::precheck::render_json(&report);
        } else {
            kurin_components::precheck::render(&report);
        }
        Ok(report)
    }
    .await;
    session.finish(&result);
    result
}
