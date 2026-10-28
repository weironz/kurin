//! Plan execution: dry-run rendering, `--check` probing, the executor call
//! and the per-operation terminal summary.

use std::sync::Arc;

use kurin_core::engine::Event;
use kurin_core::executor::{self, RunOptions, SessionPool};
use kurin_core::plan::Plan;
use kurin_core::render;
use kurin_core::run::RunContext;
use miette::{IntoDiagnostic, Result};

use crate::output::{OpOutcome, OpResult, OutputOpts};
use crate::{Params, Prepared};

/// Which terminal lines the executor's v1 event adapter prints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Emit {
    /// `skip/done/ok/FAIL/-- stage` lines (create, add nodes).
    Progress,
    /// Only the `--json` event lines (upgrade, teardown, certs).
    JsonOnly,
}

pub(crate) async fn run(prepared: Prepared, mut plan: Plan) -> Result<OpResult> {
    let Prepared {
        run,
        lock: _lock,
        facts: _facts,
        pool,
        spec,
        params,
        session,
        out,
    } = prepared;
    let fingerprint = plan.fingerprint();
    session.begin(params.op_name(), &spec.name, &fingerprint);

    let result = run_op(&run, &pool, &spec, &params, &mut plan, out).await;
    // The seed registry (04 §5.3) is per-operation: stop it before the
    // operation is reported done, so a resident server does not leak
    // loopback listeners. The SSH forwards die with the session pool.
    kurin_components::seed::finish_op(&run.op_id);
    session.finish(&result);
    let outcome = result?;
    Ok(OpResult {
        kind: plan.op,
        cluster: spec.name,
        fingerprint,
        outcome,
    })
}

async fn run_op(
    run: &Arc<RunContext>,
    pool: &Arc<dyn SessionPool>,
    spec: &kurin_config::Cluster,
    params: &Params,
    plan: &mut Plan,
    out: OutputOpts,
) -> Result<OpOutcome> {
    match params {
        Params::Create {
            dry_run,
            check,
            resume,
            ..
        } => {
            if *check {
                executor::probe(plan, pool).await?;
            }
            if *dry_run {
                render_plan(plan, out)?;
                return Ok(OpOutcome::Planned);
            }
            let mut opts = RunOptions::default().with_default_state(&spec.name);
            opts.resume = *resume;
            run_plan(plan, pool, run, out, Emit::Progress, opts).await?;
            if !out.json_enabled() {
                eprintln!("cluster {} ready", spec.name);
            }
            Ok(OpOutcome::Completed)
        }
        Params::AddNodes { .. } => {
            let opts = RunOptions::default().with_default_state(&spec.name);
            run_plan(plan, pool, run, out, Emit::Progress, opts).await?;
            if !out.json_enabled() {
                eprintln!("nodes reconciled for cluster {}", spec.name);
            }
            Ok(OpOutcome::Completed)
        }
        Params::DeleteNode { dry_run, check, .. }
        | Params::DeleteCluster { dry_run, check, .. } => {
            if *check {
                executor::probe(plan, pool).await?;
            }
            if *dry_run {
                render_plan(plan, out)?;
                return Ok(OpOutcome::Planned);
            }
            // Teardown steps are best-effort and report through `tracing`;
            // the executor's per-task lines would duplicate them and a
            // `FAIL` line would claim the operation failed when it did not.
            let warnings = kurin_components::teardown::WarningSummary::new();
            run.events.add_sink(warnings.clone());
            let opts = RunOptions {
                plan_fingerprint: plan.fingerprint(),
                ..Default::default()
            };
            run_plan(plan, pool, run, out, Emit::JsonOnly, opts).await?;
            if let Some(summary) = warnings.render() {
                eprintln!("{summary}");
            }
            Ok(OpOutcome::Completed)
        }
        Params::Upgrade { dry_run, .. } => {
            if *dry_run {
                render_plan(plan, out)?;
                return Ok(OpOutcome::Planned);
            }
            let opts = RunOptions {
                plan_fingerprint: plan.fingerprint(),
                ..Default::default()
            };
            run_plan(plan, pool, run, out, Emit::JsonOnly, opts).await?;
            Ok(OpOutcome::Completed)
        }
        Params::CertsRenew { .. } => {
            let opts = RunOptions {
                plan_fingerprint: plan.fingerprint(),
                ..Default::default()
            };
            run_plan(plan, pool, run, out, Emit::JsonOnly, opts).await?;
            Ok(OpOutcome::Completed)
        }
        Params::AddonInstall { dry_run } | Params::AddonRemove { dry_run, .. } => {
            if *dry_run {
                render_plan(plan, out)?;
                return Ok(OpOutcome::Planned);
            }
            let opts = RunOptions {
                plan_fingerprint: plan.fingerprint(),
                ..Default::default()
            };
            run_plan(plan, pool, run, out, Emit::Progress, opts).await?;
            Ok(OpOutcome::Completed)
        }
    }
}

/// Render the plan tree (`--dry-run`), or `{"plan": "<tree>"}` under v1
/// `--json`.
fn render_plan(plan: &Plan, out: OutputOpts) -> Result<()> {
    let tree = render::render_tree(plan);
    if out.json_enabled() && !out.v2() {
        let plan = serde_json::json!({ "plan": tree });
        println!("{}", serde_json::to_string_pretty(&plan).into_diagnostic()?);
    } else if !out.v2() {
        print!("{tree}");
    }
    Ok(())
}

async fn run_plan(
    plan: &mut Plan,
    pool: &Arc<dyn SessionPool>,
    run: &Arc<RunContext>,
    out: OutputOpts,
    emit: Emit,
    mut opts: RunOptions,
) -> Result<()> {
    if opts.plan_fingerprint.is_empty() {
        opts.plan_fingerprint = plan.fingerprint();
    }
    let mut sink = |event: Event| {
        if out.v2() {
            return;
        }
        if out.json_enabled() {
            println!("{}", render::event_json_line(&event));
            return;
        }
        if emit != Emit::Progress {
            return;
        }
        use kurin_core::engine::Event::*;
        match &event {
            TaskSkipped { id, reason, .. } => match reason {
                Some(reason) => eprintln!("skip  {id} ({reason})"),
                None => eprintln!("skip  {id}"),
            },
            TaskDone { id, ms, .. } => eprintln!("done  {id} ({ms}ms)"),
            TaskReconciled { id, .. } => eprintln!("ok    {id} (reconciled)"),
            TaskFailed { id, reason, .. } => eprintln!("FAIL  {id}: {reason}"),
            StageDone { stage, .. } => eprintln!("-- stage {stage} complete"),
            _ => {}
        }
    };
    executor::run_with_bus(plan, pool, run, &run.events, &mut sink, opts).await
}
