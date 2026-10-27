//! `kurin certs`: prepare a renewal run and run the read-only inventory.

use std::sync::Arc;

use kurin_config::{Cluster, ResolvedSecrets};
use kurin_core::executor::SessionPool;
use kurin_core::lock::{ClusterLock, ClusterLockGuard};
use miette::Result;

use crate::output::{OpSession, OutputOpts};
use crate::{Params, Prepared};

/// Prepare a `certs renew` run (the plan is built by
/// [`kurin_components::certs::build_plan`]).
pub(crate) async fn prepare(
    spec: Cluster,
    params: Params,
    pool: Arc<dyn SessionPool>,
    seed: crate::RunSeed,
    out: OutputOpts,
    lock: Option<ClusterLockGuard>,
    secrets: Arc<ResolvedSecrets>,
) -> Result<Prepared> {
    if !matches!(params, Params::CertsRenew { .. }) {
        return Err(miette::miette!(
            "certs::prepare called for another operation"
        ));
    }
    let run = crate::ephemeral_run_seeded(&spec, &seed)?;
    let session = OpSession::start(&run, out, seed.cli_output)?;
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
    let session = OpSession::start(&run, out, true)?;
    session.register_cluster(spec);
    session.begin("certs-check", &spec.name, "");
    let result = async {
        let reports = kurin_components::certs::check(spec, &pool, warn_days).await?;
        kurin_components::certs::render(&reports, warn_days);
        // The scheduled-renewal timer on the first control plane, when
        // `certs schedule` installed one (WP-71).
        let next = match kurin_components::certs::next_renewal(spec, &pool).await {
            Ok(next) => next,
            Err(err) => {
                tracing::warn!("certs check: renewal timer unreadable ({err})");
                None
            }
        };
        kurin_components::certs::render_next_renewal(next.as_deref());
        Ok(reports)
    }
    .await;
    session.finish(&result);
    result
}

/// `kurin certs schedule --every 24h --warn-days 30`: install the
/// renewal timer on every control plane (WP-71).
pub(crate) async fn certs_schedule(
    spec: &Cluster,
    warn_days: i64,
    every: &str,
    out: OutputOpts,
) -> Result<kurin_components::certs::ScheduleReport> {
    let every = kurin_components::certs::parse_every(every)?;
    let run = crate::ephemeral_run(spec)?;
    let secrets = spec.resolve_secrets()?;
    crate::register_resolved_secrets(&run, &secrets);
    let pool: Arc<dyn SessionPool> =
        Arc::new(kurin_ssh::SshSessionPool::from_cluster(spec, &secrets)?);
    let op_id = kurin_core::events::new_op_id();
    let _lock = ClusterLock::acquire_with(&spec.name, &op_id)?;
    let session = OpSession::start(&run, out, true)?;
    session.register_cluster(spec);
    session.begin("certs-schedule", &spec.name, "");
    let result = async {
        let report = kurin_components::certs::schedule(spec, &pool, &every, warn_days).await?;
        if out.json_enabled() {
            println!(
                "{}",
                serde_json::to_string_pretty(&report).map_err(|e| miette::miette!("{e}"))?
            );
        } else {
            kurin_components::certs::render_schedule(&report);
        }
        Ok(report)
    }
    .await;
    session.finish(&result);
    result
}
