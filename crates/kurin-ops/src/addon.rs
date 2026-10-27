//! `kurin addon install|upgrade|remove`: prepare the run.
//!
//! The addon operations converge one addon at a time: the CLI hands in a
//! spec whose `[addons]` map holds only the target (plus the entries it
//! `requires`), so the engine's plan is exactly that addon's tasks. The
//! plan itself is built by [`kurin_components::addon::build_addon_plan_with`].

use std::collections::BTreeMap;
use std::sync::Arc;

use kurin_components::addon::AddonRecord;
use kurin_config::{Cluster, ResolvedSecrets};
use kurin_core::executor::SessionPool;
use kurin_core::lock::ClusterLockGuard;
use miette::Result;

use crate::output::{OpSession, OutputOpts};
use crate::{Params, Prepared};

/// Read the installed addon inventory from the cluster's `kurin-state`
/// record (`kurin addon list`): addon name → its [`AddonRecord`].
///
/// # Errors
///
/// A diagnostic when the cluster cannot be reached or its `kurin-state`
/// record is unreadable. A malformed single entry is skipped with a
/// warning instead of failing the whole listing.
pub async fn inventory(spec: &Cluster) -> Result<BTreeMap<String, AddonRecord>> {
    let secrets = spec.resolve_secrets()?;
    let pool: Arc<dyn SessionPool> =
        Arc::new(kurin_ssh::SshSessionPool::from_cluster(spec, &secrets)?);
    let anchors: Vec<String> = spec.control_plane().map(|node| node.name.clone()).collect();
    let mut log = |line: &str| eprintln!("{line}");
    let connected = kurin_kube::connect(
        &pool,
        &anchors,
        spec.distribution.into(),
        None,
        false,
        &mut log,
    )
    .await?;
    let Some(state) = crate::read_state(connected.api.as_ref()).await? else {
        return Ok(BTreeMap::new());
    };
    let mut inventory = BTreeMap::new();
    for (name, value) in state.addons {
        match serde_json::from_value::<AddonRecord>(value) {
            Ok(record) => {
                inventory.insert(name, record);
            }
            Err(err) => tracing::warn!(
                addon = %name,
                "the kurin-state addon entry is invalid: {err}"
            ),
        }
    }
    Ok(inventory)
}

/// Prepare an addon install/remove run.
///
/// A `--dry-run` never connects: it plans against synthetic facts (the
/// declared arches, else the control machine's), exactly like
/// `create cluster --dry-run`. A mutating run discovers the live facts
/// first, so the anchor's helm payload matches its architecture.
pub(crate) async fn prepare(
    spec: Cluster,
    params: Params,
    pool: Arc<dyn SessionPool>,
    seed: crate::RunSeed,
    out: OutputOpts,
    lock: Option<ClusterLockGuard>,
    secrets: Arc<ResolvedSecrets>,
) -> Result<Prepared> {
    let dry_run = match &params {
        Params::AddonInstall { dry_run } | Params::AddonRemove { dry_run, .. } => *dry_run,
        _ => {
            return Err(miette::miette!(
                "addon::prepare called for another operation"
            ))
        }
    };
    let run = crate::ephemeral_run_seeded(&spec, &seed)?;
    let session = OpSession::start(&run, out, seed.cli_output)?;
    session.register_cluster(&spec);
    crate::register_resolved_secrets(&run, &secrets);
    let facts = if dry_run {
        kurin_core::facts::FactsTable::synthetic(&spec)?
    } else {
        kurin_components::discover::run(&spec, &pool).await?
    };
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
