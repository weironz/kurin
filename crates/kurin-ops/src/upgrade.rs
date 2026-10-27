//! `kurin upgrade`: prepare the run (target resolution + facts).

use std::sync::Arc;

use kurin_config::{Cluster, ResolvedSecrets};
use kurin_core::executor::SessionPool;
use kurin_core::lock::ClusterLockGuard;
use miette::Result;

use crate::output::{OpSession, OutputOpts};
use crate::{Params, Prepared};

/// Prepare an upgrade run. The target version is resolved here (before any
/// session work) so an invalid `--with-kubernetes` fails fast.
pub(crate) async fn prepare(
    spec: Cluster,
    mut params: Params,
    pool: Arc<dyn SessionPool>,
    op_id: String,
    out: OutputOpts,
    lock: Option<ClusterLockGuard>,
    secrets: Arc<ResolvedSecrets>,
) -> Result<Prepared> {
    let Params::Upgrade { target, .. } = &mut params else {
        return Err(miette::miette!(
            "upgrade::prepare called for another operation"
        ));
    };
    let resolved = match target.take() {
        Some(version) => version,
        None => spec.kubernetes_version.clone(),
    };
    *target = Some(resolved);

    let run = crate::ephemeral_run_with_op_id(&spec, op_id)?;
    let session = OpSession::start(&run, out)?;
    session.register_cluster(&spec);
    crate::register_resolved_secrets(&run, &secrets);

    // Facts first: upgrades resolve binaries per discovered arch.
    let facts = kurin_components::discover::run(&spec, &pool).await?;
    run.set_facts(facts.clone());

    Ok(Prepared {
        run,
        lock,
        facts,
        pool,
        spec,
        params,
        session,
        out,
    })
}
