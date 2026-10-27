//! Plan-only runs (WP-54): build (and optionally drift-probe) the create
//! plan for a cluster without executing it — the Web server's
//! `POST /clusters/{name}/plan`.
//!
//! The run context is **ephemeral** (like `precheck`): a plan preview must
//! never touch the cluster's persisted Blackboard, so previewing a live
//! cluster cannot truncate its join materials or secrets. With `check`
//! the live facts are discovered and every task is probed for drift
//! (`--dry-run --check` semantics); without it the run stays offline,
//! planning against synthetic facts.

use std::sync::Arc;

use kurin_config::{Cluster, ResolvedSecrets};
use kurin_core::executor::SessionPool;
use kurin_core::plan::Plan;
use miette::Result;

use crate::output::{OpSession, OutputOpts};
use crate::{RunContext, RunSeed};

/// Build the create plan for `spec`; with `check`, discover live facts
/// first and annotate every task with its drift.
///
/// # Errors
///
/// A diagnostic when the payload source cannot be resolved, discovery
/// fails (`check` only), the plan cannot be built or a drift probe fails.
pub async fn plan_only(
    spec: &Cluster,
    check: bool,
    out: OutputOpts,
    seed: RunSeed,
) -> Result<Plan> {
    let run: Arc<RunContext> = crate::ephemeral_run_seeded(spec, &seed)?;
    // A pure offline preview resolves no secrets at all; `check` opens
    // sessions, so it needs them exactly like any read-only operation.
    let secrets = Arc::new(if check {
        spec.resolve_secrets()?
    } else {
        ResolvedSecrets::default()
    });
    crate::register_resolved_secrets(&run, &secrets);
    let pool: Arc<dyn SessionPool> =
        Arc::new(kurin_ssh::SshSessionPool::from_cluster(spec, &secrets)?);
    let session = OpSession::start(&run, out, seed.cli_output)?;
    session.register_cluster(spec);

    let result = async {
        let mut spec = spec.clone();
        let payload = crate::packages::resolve_payload(&mut spec).await?;
        let facts = if check {
            kurin_components::discover::run(&spec, &pool).await?
        } else {
            let facts = kurin_core::facts::FactsTable::synthetic(&spec)?;
            let missing = spec
                .nodes
                .iter()
                .filter(|n| n.arch.is_none())
                .map(|n| n.name.as_str())
                .collect::<Vec<_>>();
            if !missing.is_empty() {
                crate::warn(
                    &run,
                    format!(
                        "warn: arch not probed for {} (plan without check): using \
                         nodes.<n>.arch or the control machine's arch",
                        missing.join(", ")
                    ),
                );
            }
            facts
        };
        run.set_facts(facts.clone());
        crate::packages::validate_payload(&spec, &facts, &payload, false)?;
        let mut plan = kurin_components::plan::build_with_pool(&spec, Some(pool.clone()), &run)?;
        if check {
            kurin_core::executor::probe(&mut plan, &pool).await?;
        }
        Ok(plan)
    }
    .await;

    match &result {
        Ok(plan) => session.begin("plan", &spec.name, &plan.fingerprint()),
        Err(_) => session.begin("plan", &spec.name, ""),
    }
    session.finish(&result);
    result
}
