//! `kurin certs`: prepare a renewal run and run the read-only inventory.

use std::sync::Arc;

use kurin_config::{Cluster, ResolvedSecrets};
use kurin_core::executor::SessionPool;
use kurin_core::lock::ClusterLockGuard;
use miette::Result;

use crate::output::{OpSession, OutputOpts};
use crate::{Params, Prepared};

/// Prepare a `certs renew` run (the plan is built by
/// [`kurin_components::certs::build_plan`]).
pub(crate) async fn prepare(
    spec: Cluster,
    params: Params,
    pool: Arc<dyn SessionPool>,
    op_id: String,
    out: OutputOpts,
    lock: Option<ClusterLockGuard>,
    secrets: Arc<ResolvedSecrets>,
) -> Result<Prepared> {
    if !matches!(params, Params::CertsRenew { .. }) {
        return Err(miette::miette!(
            "certs::prepare called for another operation"
        ));
    }
    let run = crate::ephemeral_run_with_op_id(&spec, op_id)?;
    let session = OpSession::start(&run, out)?;
    session.register_cluster(&spec);
    crate::register_resolved_secrets(&run, &secrets);
    Ok(Prepared {
        run,
        lock,
        facts: kurin_core::facts::FactsTable::new(),
        pool,
        spec,
        params,
        session,
        out,
    })
}

/// `certs check`: probe every node and print the expiry table.
pub(crate) async fn certs_check(
    spec: &Cluster,
    warn_days: i64,
    out: OutputOpts,
) -> Result<Vec<kurin_components::certs::NodeCerts>> {
    let run = crate::ephemeral_run(spec)?;
    let secrets = spec.resolve_secrets()?;
    crate::register_resolved_secrets(&run, &secrets);
    let pool: Arc<dyn SessionPool> =
        Arc::new(kurin_ssh::SshSessionPool::from_cluster(spec, &secrets)?);
    let session = OpSession::start(&run, out)?;
    session.register_cluster(spec);
    session.begin("certs-check", &spec.name, "");
    let result = async {
        let reports = kurin_components::certs::check(spec, &pool, warn_days).await?;
        kurin_components::certs::render(&reports, warn_days);
        Ok(reports)
    }
    .await;
    session.finish(&result);
    result
}
