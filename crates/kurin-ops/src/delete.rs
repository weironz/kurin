//! `kurin delete node|cluster`: prepare a teardown run.

use std::sync::Arc;

use kurin_config::Cluster;
use kurin_core::executor::SessionPool;
use kurin_core::lock::ClusterLockGuard;
use miette::Result;

use crate::output::{OpSession, OutputOpts};
use crate::{Params, Prepared};

/// Prepare a teardown run. `--dry-run` runs without the mutation lock.
pub(crate) async fn prepare(
    spec: Cluster,
    params: Params,
    pool: Arc<dyn SessionPool>,
    op_id: String,
    out: OutputOpts,
    lock: Option<ClusterLockGuard>,
) -> Result<Prepared> {
    if let Params::DeleteNode { node, .. } = &params {
        if !spec.nodes.iter().any(|n| &n.name == node) {
            return Err(miette::miette!(
                "node {node:?} is not declared in the config"
            ));
        }
    }
    let run = crate::ephemeral_run_with_op_id(&spec, op_id)?;
    let session = OpSession::start(&run, out)?;
    session.register_cluster(&spec);
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
