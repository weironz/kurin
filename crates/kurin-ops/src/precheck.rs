//! `kurin precheck`: read-only host audit before `create cluster`.

use std::sync::Arc;

use kurin_config::Cluster;
use kurin_core::executor::SessionPool;
use miette::Result;

use crate::output::{OpSession, OutputOpts};
use crate::PrecheckReport;

/// Run the audit and print the report (JSON lines or the human table).
pub(crate) async fn precheck(spec: &Cluster, out: OutputOpts) -> Result<PrecheckReport> {
    let op_id = kurin_core::events::new_op_id();
    let seed = crate::RunSeed::new(
        op_id.clone(),
        Arc::new(kurin_core::events::EventBus::new(&op_id)),
        tokio_util::sync::CancellationToken::new(),
        true,
    );
    precheck_with_handle(spec, out, seed).await
}

/// [`precheck`] with caller-owned bus/cancellation (the Web server).
pub async fn precheck_with_handle(
    spec: &Cluster,
    out: OutputOpts,
    seed: crate::RunSeed,
) -> Result<PrecheckReport> {
    let run = crate::ephemeral_run_seeded(spec, &seed)?;
    let secrets = spec.resolve_secrets()?;
    crate::register_resolved_secrets(&run, &secrets);
    let pool: Arc<dyn SessionPool> =
        Arc::new(kurin_ssh::SshSessionPool::from_cluster(spec, &secrets)?);
    let session = OpSession::start(&run, out, seed.cli_output)?;
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
