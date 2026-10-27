//! `kurin create cluster`: prepare (facts, HA warnings) and plan via the
//! component registry.

use std::sync::Arc;

use kurin_config::{Cluster, ResolvedSecrets};
use kurin_core::executor::SessionPool;
use kurin_core::lock::ClusterLockGuard;
use miette::Result;

use crate::output::{OpSession, OutputOpts};
use crate::{warn, Params, Prepared};

/// Prepare a create run: blackboard (resume-aware), event session, HA
/// warnings and node facts.
pub(crate) async fn prepare(
    mut spec: Cluster,
    params: Params,
    pool: Arc<dyn SessionPool>,
    seed: crate::RunSeed,
    out: OutputOpts,
    lock: Option<ClusterLockGuard>,
    secrets: Arc<ResolvedSecrets>,
) -> Result<Prepared> {
    let (dry_run, check, resume) = match &params {
        Params::Create {
            dry_run,
            check,
            resume,
            ..
        } => (*dry_run, *check, *resume),
        _ => {
            return Err(miette::miette!(
                "create::prepare called for another operation"
            ))
        }
    };
    // Resolve the payload source before the run context is built: its
    // artifact sources record the outcome (WP-34). A legacy bundle
    // keeps its behaviour; a base package is opened for validation.
    let payload = crate::packages::resolve_payload(&mut spec).await?;
    let run = crate::run_with_blackboard(&spec, resume, &seed)?;
    let session = OpSession::start(&run, out, seed.cli_output)?;
    session.register_cluster(&spec);
    crate::register_resolved_secrets(&run, &secrets);

    // HA topology warnings: published as warn-level log events, so the
    // terminal text is unchanged and `--json`/JSONL consumers see them too.
    let cp = spec.control_plane().count();
    if cp.is_multiple_of(2) && cp > 1 {
        warn(
            &run,
            format!("warn: even control-plane count ({cp}); etcd quorum prefers odd"),
        );
    }
    if cp > 1 && spec.vip.is_none() {
        warn(
            &run,
            format!(
                "warn: {cp} control-plane nodes without ha.vip: the first master becomes \
                 the API single point of failure"
            ),
        );
    }

    // Facts before the plan: per-node artifact selection is facts-driven.
    // `--dry-run` stays offline (zero network): synthetic facts come from
    // `nodes.<n>.arch` or the control machine's arch.
    let facts = if dry_run && !check {
        let facts = kurin_core::facts::FactsTable::synthetic(&spec)?;
        let missing = spec
            .nodes
            .iter()
            .filter(|n| n.arch.is_none())
            .map(|n| n.name.as_str())
            .collect::<Vec<_>>();
        if !missing.is_empty() {
            warn(
                &run,
                format!(
                    "warn: arch not probed for {} (dry-run without --check): \
                     using nodes.<n>.arch or the control machine's arch",
                    missing.join(", ")
                ),
            );
        }
        facts
    } else {
        kurin_components::discover::run(&spec, &pool).await?
    };
    run.set_facts(facts.clone());
    // With the architectures known, the base package must cover the
    // cluster (WP-34 step 3); a mutating run additionally stops at the
    // delivery boundary (WP-35 lands preload).
    crate::packages::validate_payload(&spec, &facts, &payload, !dry_run).await?;
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
