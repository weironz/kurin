//! Kurin operations layer: every CLI operation is prepared, planned and
//! executed here. `kurin/src/main.rs` only parses argv and calls into this
//! crate.
//!
//! The public shape follows the architecture contract:
//!
//! ```text
//! OpRequest { kind, spec, params }
//!    -> prepare(req, out) -> Prepared { run, lock, facts, ... }
//!    -> plan(&prepared)   -> Plan
//!    -> execute(prepared, plan) -> OpResult
//! ```
//!
//! Read-only operations bypass the plan API: [`status`], [`precheck`],
//! [`certs_check`] return structured reports (they still open an operation
//! session so the JSONL log and `tracing` output behave like any other
//! operation), and [`plan_view`] projects a [`Plan`] into its serializable
//! DTO.

#![allow(clippy::result_large_err)]

pub mod add_nodes;
mod artifact;
mod certs;
mod create;
mod delete;
mod execute;
mod logs;
mod output;
mod packages;
mod plan_view;
mod precheck;
mod status;
mod upgrade;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use kurin_config::{Cluster, ResolvedSecrets};
use kurin_core::events::EventBus;
use kurin_core::executor::SessionPool;
use kurin_core::facts::FactsTable;
use kurin_core::lock::{ClusterLock, ClusterLockGuard};
use kurin_core::plan::OpKind;
use kurin_core::run::{ArtifactSources, RunContext, RunSettings};
use miette::{IntoDiagnostic, Result};
use semver::Version;
use tokio_util::sync::CancellationToken;

pub use kurin_components::certs::NodeCerts;
pub use kurin_components::precheck::PrecheckReport;
pub use kurin_components::status::StatusOpts;
pub use kurin_components::status::StatusReport;
pub use kurin_components::tasks::post::ClusterStateRecord;
pub use logs::LogsRequest;
pub use output::{JsonMode, OpOutcome, OpResult, OutputOpts};
pub use plan_view::{plan_view, DriftView, PhaseView, PlanView, TaskView};
pub use precheck::precheck_with_handle;

/// One operation request: what to do, against which cluster, with which
/// operation-specific parameters.
#[derive(Debug, Clone)]
pub struct OpRequest {
    pub kind: OpKind,
    pub spec: Cluster,
    pub params: Params,
    /// Run knobs from global CLI flags (concurrency, upload cap).
    pub settings: RunSettings,
}

/// Operation-specific parameters. The variant must match [`OpRequest::kind`].
#[derive(Debug, Clone)]
pub enum Params {
    Create {
        dry_run: bool,
        check: bool,
        resume: bool,
        /// `--offline DIR|URI`: a legacy bundle directory or a package
        /// source — the same classification `--package` performs.
        offline: Option<String>,
    },
    AddNodes {
        offline: Option<String>,
    },
    DeleteNode {
        node: String,
        dry_run: bool,
        check: bool,
    },
    DeleteCluster {
        dry_run: bool,
        check: bool,
    },
    Upgrade {
        target: Option<Version>,
        dry_run: bool,
        batch: usize,
        offline: Option<String>,
        force_drain_failure: bool,
    },
    CertsRenew {
        force: bool,
    },
}

impl Params {
    /// True when the operation only renders a plan.
    pub fn dry_run(&self) -> bool {
        match self {
            Params::Create { dry_run, .. }
            | Params::DeleteNode { dry_run, .. }
            | Params::DeleteCluster { dry_run, .. }
            | Params::Upgrade { dry_run, .. } => *dry_run,
            Params::AddNodes { .. } | Params::CertsRenew { .. } => false,
        }
    }

    /// True when `--check` should probe live nodes.
    pub fn check(&self) -> bool {
        match self {
            Params::Create { check, .. }
            | Params::DeleteNode { check, .. }
            | Params::DeleteCluster { check, .. } => *check,
            _ => false,
        }
    }

    /// Operation name used for the `OpStarted` event.
    pub fn op_name(&self) -> &'static str {
        match self {
            Params::Create { .. } => "create-cluster",
            Params::AddNodes { .. } => "add-nodes",
            Params::DeleteNode { .. } => "delete-node",
            Params::DeleteCluster { .. } => "delete-cluster",
            Params::Upgrade { .. } => "upgrade",
            Params::CertsRenew { .. } => "certs-renew",
        }
    }
}

/// Handles a resident caller (the Web server's OpRunner) owns for one
/// operation: the id it already persisted, the bus subscribers attached to,
/// the cancellation token, and the cluster lock it acquired at submission.
///
/// The CLI builds the same struct with [`OpHandle::cli`], so both entry
/// points share one code path.
pub struct OpHandle {
    /// Operation id; also the JSONL log name.
    pub op_id: String,
    /// Event bus the caller subscribed to before the first event.
    pub events: Arc<EventBus>,
    /// Cancellation token checked by the executor at stage boundaries.
    pub cancel: CancellationToken,
    /// Cluster mutation lock; `None` for read-only operations.
    pub lock: Option<ClusterLockGuard>,
    /// Attach the CLI's JSONL/terminal sinks and install global tracing.
    pub cli_output: bool,
}

impl OpHandle {
    /// Handles for a CLI run: fresh id/bus/token, CLI output attached.
    pub fn cli(op_id: impl Into<String>, lock: Option<ClusterLockGuard>) -> Self {
        let op_id = op_id.into();
        Self {
            events: Arc::new(EventBus::new(&op_id)),
            op_id,
            cancel: CancellationToken::new(),
            lock,
            cli_output: true,
        }
    }
}

/// The subset of [`OpHandle`] the per-operation `prepare` functions need.
pub struct RunSeed {
    pub op_id: String,
    pub events: Arc<EventBus>,
    pub cancel: CancellationToken,
    pub cli_output: bool,
}

impl RunSeed {
    /// Assemble a seed from its parts (the Web server builds one per run).
    pub fn new(
        op_id: String,
        events: Arc<EventBus>,
        cancel: CancellationToken,
        cli_output: bool,
    ) -> Self {
        Self {
            op_id,
            events,
            cancel,
            cli_output,
        }
    }
}

/// Everything [`plan`] and [`execute`] need, produced by [`prepare`].
///
/// `lock` holds the cluster mutation lock for the whole operation (dropping
/// it releases the lock); it is `None` for read-only (`--dry-run`) runs.
pub struct Prepared {
    pub run: Arc<RunContext>,
    pub lock: Option<ClusterLockGuard>,
    pub facts: FactsTable,
    pub pool: Arc<dyn SessionPool>,
    pub spec: Cluster,
    pub params: Params,
    pub(crate) session: output::OpSession,
    pub(crate) out: OutputOpts,
}

/// Load a config file through the full pipeline: kurin-config's structural
/// checks plus every enabled component's validation rules.
pub fn load_config(file: &Path) -> Result<Cluster> {
    let cluster = kurin_config::load(file)?;
    kurin_components::component::validate(&cluster)?;
    Ok(cluster)
}

/// Resolve the download zone: `KURIN_ZONE` env wins over config's
/// `[artifacts] zone`.
pub(crate) fn resolve_zone(config_zone: Option<&str>) -> Result<kurin_core::zone::Zone> {
    let zone_str = std::env::var("KURIN_ZONE")
        .ok()
        .filter(|z| !z.is_empty())
        .or_else(|| config_zone.map(str::to_string))
        .unwrap_or_else(|| "global".to_string());
    let zone = kurin_core::zone::Zone::parse(&zone_str)
        .ok_or_else(|| miette::miette!(r#"invalid zone {zone_str:?} (expected "cn")"#))?;
    if zone == kurin_core::zone::Zone::Cn {
        eprintln!("[zone] artifact downloads via domestic mirrors");
    }
    Ok(zone)
}

/// Artifact sources for one operation: effective zone, embedded
/// manifest, the payload source (legacy bundle directory or base
/// package) and the `[packages]` settings.
pub(crate) fn sources_for(cluster: &Cluster) -> Result<ArtifactSources> {
    Ok(ArtifactSources {
        zone: resolve_zone(cluster.zone())?,
        manifest_json: None,
        offline_dir: cluster.offline_dir().map(|p| p.to_path_buf()),
        base: cluster.packages.base.clone(),
        os: cluster.packages.os.clone(),
        delivery: cluster.packages.delivery,
        verify: cluster.packages.verify,
        trusted_keys: cluster.packages.trusted_keys.clone(),
    })
}

/// Run context for read-only operations: no persisted Blackboard, so a
/// `status`/`precheck`/`certs` run never truncates another op's secrets.
pub(crate) fn ephemeral_run(cluster: &Cluster) -> Result<Arc<RunContext>> {
    ephemeral_run_with_op_id(cluster, kurin_core::events::new_op_id())
}

/// [`ephemeral_run`] with a caller-chosen operation id.
pub(crate) fn ephemeral_run_with_op_id(
    cluster: &Cluster,
    op_id: String,
) -> Result<Arc<RunContext>> {
    let seed = RunSeed::new(
        op_id.clone(),
        Arc::new(EventBus::new(&op_id)),
        CancellationToken::new(),
        true,
    );
    ephemeral_run_seeded(cluster, &seed)
}

/// Read-only run context seeded with the caller's bus, cancellation token
/// and operation id.
pub(crate) fn ephemeral_run_seeded(cluster: &Cluster, seed: &RunSeed) -> Result<Arc<RunContext>> {
    Ok(Arc::new(
        RunContext::new(seed.op_id.clone(), &cluster.name, sources_for(cluster)?)
            .with_events(Arc::clone(&seed.events))
            .with_cancel(seed.cancel.clone()),
    ))
}

/// Run context carrying the cluster's persisted Blackboard: `resume == true`
/// loads it, `false` starts (and truncates) fresh.
pub(crate) fn run_with_blackboard(
    cluster: &Cluster,
    resume: bool,
    seed: &RunSeed,
) -> Result<Arc<RunContext>> {
    let bb = match kurin_core::blackboard::Blackboard::default_path(&cluster.name) {
        Some(path) => kurin_core::blackboard::Blackboard::open(path, resume)
            .map_err(|e| miette::miette!("{e}"))?,
        None => kurin_core::blackboard::Blackboard::in_memory(),
    };
    Ok(Arc::new(
        RunContext::new(seed.op_id.clone(), &cluster.name, sources_for(cluster)?)
            .with_blackboard(bb)
            .with_events(Arc::clone(&seed.events))
            .with_cancel(seed.cancel.clone()),
    ))
}

/// Publish a warning line on the operation's bus. The terminal renderer
/// prints it verbatim (`warn: ...`), so the text is unchanged from the old
/// direct `eprintln!`; `--json` consumers now see it as a `log` event.
pub(crate) fn warn(run: &RunContext, line: impl Into<String>) {
    run.events.publish(kurin_core::events::Body::Log {
        level: kurin_core::events::Level::Warn,
        node: None,
        task: None,
        stream: kurin_core::events::Stream::Kurin,
        line: line.into(),
    });
}

/// Register every resolved secret value with the run's redactor and make
/// the resolved map available to tasks (SSH pool, registry tasks).
pub(crate) fn register_resolved_secrets(run: &RunContext, secrets: &ResolvedSecrets) {
    for value in secrets.values_str() {
        run.events.redactor().register(value);
    }
    run.set_secrets(secrets.clone());
}

/// Prepare an operation: acquire the lock, build the run context, start the
/// event session, discover facts and run operation-specific preflight work.
pub async fn prepare(req: OpRequest, out: OutputOpts) -> Result<Prepared> {
    // Validate before taking the lock so a bad config never blocks the
    // cluster; `prepare_with_handle` repeats the check (it is cheap).
    kurin_components::component::validate(&req.spec)?;
    let op_id = kurin_core::events::new_op_id();
    let lock = if req.params.dry_run() {
        None
    } else {
        Some(ClusterLock::acquire_with(&req.spec.name, &op_id)?)
    };
    let handle = OpHandle::cli(op_id, lock);
    prepare_with_handle(req, out, handle).await
}

/// [`prepare`] for a caller that already persisted the operation and owns
/// its bus, cancellation token and cluster lock (the Web server's OpRunner).
pub async fn prepare_with_handle(
    req: OpRequest,
    out: OutputOpts,
    handle: OpHandle,
) -> Result<Prepared> {
    let OpRequest {
        kind,
        mut spec,
        params,
        settings,
    } = req;
    // `create`/`add nodes` accept `--offline DIR|URI` — the same
    // classification `--package` performs (WP-34); `upgrade` passes its
    // source to the binary resolver in `plan` instead.
    match &params {
        Params::Create {
            offline: Some(source),
            ..
        }
        | Params::AddNodes {
            offline: Some(source),
        } => packages::apply_offline_override(&mut spec, source),
        _ => {}
    }
    kurin_components::component::validate(&spec)?;

    // Secrets are resolved exactly once, here. A pure `--dry-run` (no
    // `--check`) never opens a session, so it does not read env/file
    // secrets at all; `--check` and every mutating/read-only operation do.
    let secrets = if params.dry_run() && !params.check() {
        ResolvedSecrets::default()
    } else {
        spec.resolve_secrets()?
    };
    let secrets = Arc::new(secrets);

    let OpHandle {
        op_id,
        events,
        cancel,
        lock,
        cli_output,
    } = handle;
    let seed = RunSeed::new(op_id, events, cancel, cli_output);

    let pool: Arc<dyn SessionPool> = match kind {
        // The teardown plan ends with a control-machine step; the wrapper
        // answers the `local` node without an SSH connection.
        OpKind::DeleteCluster => {
            kurin_components::teardown::with_local_node(kurin_core::local::with_controller_node(
                Arc::new(kurin_ssh::SshSessionPool::from_cluster(&spec, &secrets)?),
            ))
        }
        // Registry distribution pushes from the control machine
        // (`Target::Controller`), which the pool answers locally.
        _ => kurin_core::local::with_controller_node(Arc::new(
            kurin_ssh::SshSessionPool::from_cluster(&spec, &secrets)?,
        )),
    };

    let prepared = match kind {
        OpKind::Create => create::prepare(spec, params, pool, seed, out, lock, secrets).await,
        OpKind::AddNodes => add_nodes::prepare(spec, params, pool, seed, out, lock, secrets).await,
        OpKind::DeleteNode | OpKind::DeleteCluster => {
            delete::prepare(spec, params, pool, seed, out, lock, secrets).await
        }
        OpKind::Upgrade => upgrade::prepare(spec, params, pool, seed, out, lock, secrets).await,
        OpKind::CertsRenew => certs::prepare(spec, params, pool, seed, out, lock, secrets).await,
        other => Err(miette::miette!(
            "operation {other:?} is not implemented by this build"
        )),
    }?;
    // Global run knobs are applied once the context exists; the executor
    // reads them when it builds its upload gate.
    prepared
        .run
        .set_max_upload_parallel(settings.max_upload_parallel);
    Ok(prepared)
}

/// Build the operation's plan. Zero or more round trips: `upgrade` resolves
/// its binary source here, exactly like the pre-WP-14 CLI did before it
/// started executing.
pub async fn plan(prepared: &Prepared) -> Result<kurin_core::plan::Plan> {
    let spec = &prepared.spec;
    let run = &prepared.run;
    let pool = &prepared.pool;
    match &prepared.params {
        Params::Create { .. } => {
            kurin_components::plan::build_with_pool(spec, Some(Arc::clone(pool)), run)
        }
        Params::AddNodes { .. } => kurin_components::plan::build_with_pool_for(
            OpKind::AddNodes,
            spec,
            Some(Arc::clone(pool)),
            run,
        ),
        Params::DeleteNode { node, .. } => {
            let target =
                spec.nodes.iter().find(|n| &n.name == node).ok_or_else(|| {
                    miette::miette!("node {node:?} is not declared in the config")
                })?;
            kurin_components::plan::build_delete(spec, run, Some(target))
        }
        Params::DeleteCluster { .. } => kurin_components::plan::build_delete(spec, run, None),
        Params::Upgrade {
            target,
            dry_run,
            batch,
            offline,
            force_drain_failure,
        } => {
            let target = target
                .clone()
                .unwrap_or_else(|| spec.kubernetes_version.clone());
            let opts = kurin_components::upgrade::UpgradeOptions {
                batch: *batch,
                force_drain_failure: *force_drain_failure,
            };
            if *dry_run {
                kurin_components::upgrade::plan(spec, &target, pool, &prepared.facts, opts, run)
                    .await
            } else if let Some(source) = offline.as_deref() {
                // The compatibility reader (WP-34): `upgrade --offline`
                // keeps taking a legacy bundle directory; a package
                // source names the not-yet-landed delivery instead of
                // silently resolving versions online.
                let dir = match kurin_artifact::deploy::open_payload_source(source)
                    .map_err(|e| miette::miette!("{e}"))?
                {
                    kurin_artifact::deploy::PayloadSource::LegacyBundle { dir } => dir,
                    kurin_artifact::deploy::PayloadSource::Package { uri, .. } => {
                        return Err(miette::miette!(
                            "upgrade from the package {uri} is not implemented yet (package \
                             delivery lands in WP-35); `upgrade --offline` takes a legacy bundle \
                             directory built by `kurin artifact --with-kubernetes`"
                        ));
                    }
                };
                kurin_components::upgrade::prepare_offline(
                    spec,
                    &target,
                    &dir,
                    pool,
                    opts,
                    &prepared.facts,
                    run,
                )
                .await
            } else {
                kurin_components::upgrade::prepare_online(
                    spec,
                    &target,
                    pool,
                    opts,
                    &prepared.facts,
                    run,
                )
                .await
            }
        }
        Params::CertsRenew { force } => kurin_components::certs::build_plan(spec, run, *force),
    }
}

/// Execute a prepared plan, publishing `OpStarted`/`OpFinished` and the
/// terminal or `--json` event stream.
pub async fn execute(prepared: Prepared, plan: kurin_core::plan::Plan) -> Result<OpResult> {
    execute::run(prepared, plan).await
}

/// Read the cluster's `kube-system/ConfigMap kurin-state` record.
///
/// `Ok(None)` means the cluster predates the record (or a non-kurin
/// cluster): callers fall back to live probes. A malformed record is an
/// error so `status` can warn instead of silently ignoring it.
pub async fn read_state(api: &dyn kurin_kube::ClusterApi) -> Result<Option<ClusterStateRecord>> {
    kurin_components::tasks::post::read_state(api).await
}

/// `kurin status`: probe the cluster and print the report. Returns the
/// structured report so the caller can map `healthy` to an exit code.
pub async fn status(spec: &Cluster, opts: StatusOpts, out: OutputOpts) -> Result<StatusReport> {
    status::status(spec, opts, out).await
}

/// `kurin precheck`: read-only host audit; prints the report and returns it.
pub async fn precheck(spec: &Cluster, out: OutputOpts) -> Result<PrecheckReport> {
    precheck::precheck(spec, out).await
}

/// `kurin certs check`: certificate inventory + expiry table.
pub async fn certs_check(
    spec: &Cluster,
    warn_days: i64,
    out: OutputOpts,
) -> Result<Vec<NodeCerts>> {
    certs::certs_check(spec, warn_days, out).await
}

/// `kurin logs`: read the JSONL operation logs.
pub async fn logs(args: logs::LogsRequest, out: OutputOpts) -> Result<()> {
    logs::logs(args, out).await
}

/// `kurin artifact`: build an offline bundle.
pub async fn build_artifacts(
    out_dir: PathBuf,
    arch: Option<String>,
    with_kubernetes: Option<String>,
) -> Result<()> {
    artifact::build(out_dir, arch, with_kubernetes).await
}

/// `kurin capabilities --json`: the aggregated component capability table.
pub fn capabilities() -> Result<String> {
    serde_json::to_string_pretty(&kurin_components::component::capabilities_report())
        .into_diagnostic()
        .map_err(|e| miette::miette!("serialize capability table: {e}"))
}
