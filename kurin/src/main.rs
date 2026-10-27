use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::{ArgAction, Parser, Subcommand, ValueEnum};
use miette::{IntoDiagnostic, Result};

use kurin::events::{self, Body, EventBus, JsonlSink, Level, LogQuery, OpKind, OpResult};
use kurin::render::{JsonSink, TerminalRenderer};
use kurin::run::{ArtifactSources, RunContext};

#[derive(Parser)]
#[command(
    name = "kurin",
    about = "Declarative Kubernetes cluster installer",
    version
)]
struct Cli {
    /// Emit machine-readable JSON. Bare `--json` (or `--json=v1`) keeps the
    /// frozen v1 event stream (its field set plus `ts`); `--json=v2` emits
    /// full event envelopes.
    #[arg(
        long,
        global = true,
        value_name = "VERSION",
        num_args = 0..=1,
        default_missing_value = "v1",
        value_enum
    )]
    json: Option<JsonMode>,
    /// Prefix every terminal log line with its UTC timestamp.
    #[arg(long, global = true)]
    timestamps: bool,
    /// `-v` shows info-level progress plus a per-command remote-output
    /// summary; `-vv` also streams every remote stdout/stderr line.
    #[arg(short = 'v', long = "verbose", global = true, action = ArgAction::Count)]
    verbose: u8,
    #[command(subcommand)]
    command: Commands,
}

/// Version of the `--json` stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum JsonMode {
    /// Frozen v1 event objects, now carrying `ts`.
    V1,
    /// Full v2 envelopes (`v`, `seq`, `ts`, `op`, `body`).
    V2,
}

/// Output knobs shared by every operation.
#[derive(Debug, Clone, Copy)]
struct OutputOpts {
    json: Option<JsonMode>,
    timestamps: bool,
    verbose: u8,
}

impl OutputOpts {
    fn v2(self) -> bool {
        self.json == Some(JsonMode::V2)
    }

    fn json_enabled(self) -> bool {
        self.json.is_some()
    }
}

#[derive(Subcommand)]
enum Commands {
    /// Create resources.
    Create(CreateArgs),
    /// Inspect or rotate control-plane certificates.
    Certs {
        #[command(subcommand)]
        what: CertsWhat,
    },
    /// Read-only host audit before `create cluster` (swap, kernel,
    /// ports, disk, time sync...).
    Precheck {
        /// Config file (.toml or .yaml).
        #[arg(short, long)]
        file: PathBuf,
    },
    /// Probe cluster health (nodes, pods, etcd, certificates).
    Status {
        /// Config file (.toml or .yaml).
        #[arg(short, long)]
        file: PathBuf,
    },
    /// Upgrade the cluster's Kubernetes components to a pinned version.
    Upgrade {
        /// Config file (.toml or .yaml) of the running cluster.
        #[arg(short, long)]
        file: PathBuf,
        /// Target Kubernetes version, e.g. v1.33.13.
        #[arg(long)]
        with_kubernetes: Option<String>,
        /// Show the upgrade plan without changing anything.
        #[arg(long)]
        dry_run: bool,
        /// Worker batch size (control-plane nodes always go one at a time).
        #[arg(long, default_value_t = 1)]
        batch: usize,
        /// Offline bundle directory (built by `kurin artifact --with-kubernetes`).
        #[arg(long)]
        offline: Option<PathBuf>,
        /// Continue when `kubectl drain` fails instead of aborting the node's
        /// upgrade (unsafe: the node is upgraded while still running pods).
        #[arg(long)]
        force_drain_failure: bool,
    },
    /// Build an offline artifact bundle for air-gapped installs.
    Artifact {
        /// Output directory (created if missing).
        #[arg(short, long)]
        out: PathBuf,
        /// Architecture(s) to build for, comma-separated (default: this
        /// machine's). A mixed-architecture cluster needs every arch its
        /// nodes use, e.g. `--arch amd64,arm64`.
        #[arg(long)]
        arch: Option<String>,
        /// Kubernetes version to pin binaries and control-plane images for
        /// (writes kurin-manifest.json; default: the compiled-in table).
        #[arg(long)]
        with_kubernetes: Option<String>,
    },
    /// The built-in component capability table (machine-readable). Hidden:
    /// consumed by `just docs-matrix` and the Web UI, not part of the
    /// stable CLI surface.
    #[command(hide = true)]
    Capabilities,
    /// Add nodes from an updated config to an existing cluster.
    Add(AddArgs),
    /// Delete a node or the whole cluster.
    Delete(DeleteArgs),
    /// Read the JSONL operation logs written by every operation.
    Logs(LogsArgs),
}

#[derive(Subcommand)]
enum CertsWhat {
    /// Report certificate expiry on every control-plane node.
    Check {
        #[arg(short, long)]
        file: PathBuf,
        /// Flag certificates expiring within this many days.
        #[arg(long, default_value_t = 30)]
        warn_days: i64,
    },
    /// Rotate control-plane certificates (kubeadm certs renew) with a
    /// health gate per node.
    Renew {
        #[arg(short, long)]
        file: PathBuf,
        /// Renew even when certificates look fresh.
        #[arg(long)]
        force: bool,
    },
}

#[derive(clap::Args)]
struct AddArgs {
    #[command(subcommand)]
    what: AddWhat,
}

#[derive(Subcommand)]
enum AddWhat {
    /// Join every not-yet-joined node from the config.
    Nodes {
        /// Config file (.toml or .yaml) listing the full desired node set.
        #[arg(short, long)]
        file: PathBuf,
        /// Offline artifact bundle directory.
        #[arg(long)]
        offline: Option<PathBuf>,
    },
}

#[derive(clap::Args)]
struct DeleteArgs {
    #[command(subcommand)]
    what: DeleteWhat,
}

#[derive(Subcommand)]
enum DeleteWhat {
    /// Remove one node (drain, delete, reset, wipe artifacts).
    Node {
        /// Node name as declared in the config.
        name: String,
        #[arg(short, long)]
        file: PathBuf,
        /// Print the teardown plan without connecting or changing anything.
        #[arg(long)]
        dry_run: bool,
        /// Additionally probe live nodes, annotating would-change/unchanged.
        #[arg(long)]
        check: bool,
    },
    /// Remove every node and destroy the cluster.
    Cluster {
        #[arg(short, long)]
        file: PathBuf,
        /// Print the teardown plan without connecting or changing anything.
        #[arg(long)]
        dry_run: bool,
        /// Additionally probe live nodes, annotating would-change/unchanged.
        #[arg(long)]
        check: bool,
    },
}

#[derive(clap::Args)]
struct CreateArgs {
    #[command(subcommand)]
    what: CreateWhat,
}

#[derive(Subcommand)]
enum CreateWhat {
    /// Render or run the install plan.
    Cluster {
        /// Config file (.toml or .yaml).
        #[arg(short, long)]
        file: PathBuf,
        /// Print the plan without connecting or changing anything.
        #[arg(long)]
        dry_run: bool,
        /// Additionally probe live nodes, annotating would-change/unchanged.
        #[arg(long)]
        check: bool,
        /// Resume from the run-state file (skips already-done tasks).
        #[arg(long)]
        resume: bool,
        /// Offline artifact bundle directory (overrides [artifacts].offline-dir).
        #[arg(long)]
        offline: Option<PathBuf>,
    },
    /// Print an annotated example config.
    Config {
        /// Print the pinned component versions this build would install.
        #[arg(long)]
        print_versions: bool,
    },
}

/// `kurin logs` filters. Without `--op` the newest log is shown.
#[derive(clap::Args)]
struct LogsArgs {
    /// Cluster whose logs to read (default: the only cluster that has logs).
    #[arg(long)]
    cluster: Option<String>,
    /// Operation id (or unique prefix) to read.
    #[arg(long)]
    op: Option<String>,
    /// Read the newest operation log (the default).
    #[arg(long)]
    last: bool,
    /// Keep printing envelopes as they are appended.
    #[arg(long)]
    follow: bool,
    /// Only lines belonging to this node.
    #[arg(long)]
    node: Option<String>,
    /// Minimum level: trace | debug | info | warn | error (default: info;
    /// `-vv` selects debug).
    #[arg(long)]
    level: Option<String>,
}

/// One CLI operation: its event bus, JSONL log and start time.
///
/// The bus is created before the operation does any work so discovery and
/// remote output land in the log; `begin` publishes `OpStarted` once the
/// plan fingerprint is known.
struct OpSession {
    bus: Arc<EventBus>,
    started: Instant,
}

impl OpSession {
    /// Attach the JSONL writer and the terminal/JSON sink to the run's bus,
    /// install the `tracing` bridge and print the log path once. The log
    /// name is the run's `op_id`.
    fn start(run: &RunContext, out: OutputOpts) -> Result<Self> {
        let path = events::log_path(&run.cluster, &run.op_id);
        let sink = JsonlSink::create(&path)
            .map_err(|e| miette::miette!("create log {}: {e}", path.display()))?;
        let bus = Arc::clone(&run.events);
        bus.add_sink(Arc::new(sink));
        if out.v2() {
            bus.add_sink(Arc::new(JsonSink::stdout()));
        } else {
            bus.add_sink(Arc::new(TerminalRenderer::stderr(
                out.timestamps,
                out.verbose,
            )));
        }
        events::install_tracing(Arc::clone(&bus));
        eprintln!("[logs] {}", path.display());
        Ok(Self {
            bus,
            started: Instant::now(),
        })
    }

    /// Register the cluster's configured secrets before anything runs.
    fn register_cluster(&self, cluster: &kurin_config::Cluster) {
        events::register_cluster_secrets(self.bus.redactor(), cluster);
    }

    /// Publish `OpStarted` with the plan fingerprint (empty when the
    /// operation has no plan).
    fn begin(&self, kind: &str, cluster: &str, plan_fingerprint: &str) {
        self.bus.publish(Body::OpStarted {
            kind: OpKind::new(kind),
            cluster: cluster.to_string(),
            plan_fingerprint: plan_fingerprint.to_string(),
            kurin: env!("CARGO_PKG_VERSION").to_string(),
        });
    }

    /// Publish `OpFinished` and report the operation's outcome. A cancelled
    /// run (the executor returns [`kurin::executor::Cancelled`]) is
    /// reported as `cancelled`, not as a failure.
    fn finish(&self, result: &Result<()>) {
        let (result, error) = match result {
            Ok(()) => (OpResult::Succeeded, None),
            Err(e) if kurin::executor::is_cancelled(e) => (OpResult::Cancelled, None),
            Err(e) => (OpResult::Failed, Some(format!("{e:?}"))),
        };
        self.bus.publish(Body::OpFinished {
            result,
            ms: self.started.elapsed().as_millis() as u64,
            error,
        });
    }
}

/// Resolve the download zone: `KURIN_ZONE` env wins over config's
/// `[artifacts] zone`. The result is carried by the run's
/// [`ArtifactSources`]; there is no process-global zone anymore.
fn resolve_zone(config_zone: Option<&str>) -> Result<kurin::zone::Zone> {
    let zone_str = std::env::var("KURIN_ZONE")
        .ok()
        .filter(|z| !z.is_empty())
        .or_else(|| config_zone.map(str::to_string))
        .unwrap_or_else(|| "global".to_string());
    let zone = kurin::zone::Zone::parse(&zone_str)
        .ok_or_else(|| miette::miette!(r#"invalid zone {zone_str:?} (expected "cn")"#))?;
    if zone == kurin::zone::Zone::Cn {
        eprintln!("[zone] artifact downloads via domestic mirrors");
    }
    Ok(zone)
}

/// Load a config file through the full pipeline: kurin-config's
/// structural checks plus every enabled component's validation rules
/// (the rules that moved out of kurin-config with WP-15).
fn load_config(file: &std::path::Path) -> Result<kurin_config::Cluster> {
    let cluster = kurin_config::load(file)?;
    kurin::components::validate(&cluster)?;
    Ok(cluster)
}

/// Artifact sources for one operation: effective zone, embedded manifest
/// and the cluster's offline dir (a `--offline` flag is applied to the
/// cluster before this is called).
fn sources_for(cluster: &kurin_config::Cluster) -> Result<ArtifactSources> {
    Ok(ArtifactSources {
        zone: resolve_zone(cluster.zone.as_deref())?,
        manifest_json: None,
        offline_dir: cluster.offline_dir.clone(),
    })
}

/// Build the operation's run context with the cluster's persisted
/// Blackboard: `resume == true` loads it, `false` starts (and truncates)
/// fresh. Used by operations that produce or consume run-state secrets.
/// The operation id is caller-chosen so the cluster lock and the operation
/// log share the same id.
fn new_run_with_op_id(
    cluster: &kurin_config::Cluster,
    resume: bool,
    op_id: String,
) -> Result<Arc<RunContext>> {
    let bb = match kurin::blackboard::Blackboard::default_path(&cluster.name) {
        Some(path) => {
            kurin::blackboard::Blackboard::open(path, resume).map_err(|e| miette::miette!("{e}"))?
        }
        None => kurin::blackboard::Blackboard::in_memory(),
    };
    Ok(Arc::new(
        RunContext::new(op_id, &cluster.name, sources_for(cluster)?).with_blackboard(bb),
    ))
}

/// Run context for read-only operations: no persisted Blackboard, so a
/// `status`/`precheck`/`certs` run never truncates another op's secrets.
fn new_ephemeral_run(cluster: &kurin_config::Cluster) -> Result<Arc<RunContext>> {
    new_ephemeral_run_with_op_id(cluster, events::new_op_id())
}

/// [`new_ephemeral_run`] with a caller-chosen operation id.
fn new_ephemeral_run_with_op_id(
    cluster: &kurin_config::Cluster,
    op_id: String,
) -> Result<Arc<RunContext>> {
    Ok(Arc::new(RunContext::new(
        op_id,
        &cluster.name,
        sources_for(cluster)?,
    )))
}

/// Take the cluster's mutation lock. The returned guard must stay alive for
/// the whole operation; dropping it releases the lock.
fn lock_cluster(
    cluster: &kurin_config::Cluster,
    op_id: &str,
) -> Result<kurin::lock::ClusterLockGuard> {
    kurin::lock::ClusterLock::acquire_with(&cluster.name, op_id)
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> ExitCode {
    miette::set_panic_hook();
    let cli = Cli::parse();
    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e:?}");
            ExitCode::FAILURE
        } // `load`'s ConfigError is Diagnostic, so `?` keeps the fancy render.
    }
}

async fn run(cli: Cli) -> Result<()> {
    let out = OutputOpts {
        json: cli.json,
        timestamps: cli.timestamps,
        verbose: cli.verbose,
    };
    match cli.command {
        Commands::Logs(args) => logs_cmd(args, out).await,
        Commands::Upgrade {
            file,
            with_kubernetes,
            dry_run,
            batch,
            offline,
            force_drain_failure,
        } => {
            let cluster = load_config(&file)?;
            let op_id = events::new_op_id();
            // `--dry-run` only reads; everything else mutates the cluster.
            let _lock = if dry_run {
                None
            } else {
                Some(lock_cluster(&cluster, &op_id)?)
            };
            let target = with_kubernetes
                .as_deref()
                .map(|v| v.trim_start_matches('v'))
                .map(semver::Version::parse)
                .transpose()
                .map_err(|e| miette::miette!("invalid --with-kubernetes: {e}"))?
                .unwrap_or_else(|| cluster.kubernetes_version.clone());
            let pool: Arc<dyn kurin::SessionPool> =
                Arc::new(kurin::ssh::SshSessionPool::from_cluster(&cluster)?);
            let run = new_ephemeral_run_with_op_id(&cluster, op_id)?;
            let op = OpSession::start(&run, out)?;
            op.register_cluster(&cluster);
            let result: Result<()> = async {
                // Facts first: upgrades resolve binaries per discovered arch.
                let facts = kurin::discover::run(&cluster, &pool).await?;
                let opts = kurin::upgrade::UpgradeOptions {
                    batch,
                    force_drain_failure,
                };
                // Every path builds the same plan; only the artifact source
                // differs (cache, bundle, or none for a dry run).
                let plan = if dry_run {
                    kurin::upgrade::plan(&cluster, &target, &pool, &facts, opts, &run).await?
                } else if let Some(offline) = offline.as_deref() {
                    kurin::upgrade::prepare_offline(
                        &cluster, &target, offline, &pool, opts, &facts, &run,
                    )
                    .await?
                } else {
                    kurin::upgrade::prepare_online(&cluster, &target, &pool, opts, &facts, &run)
                        .await?
                };
                op.begin("upgrade", &cluster.name, &plan.fingerprint());
                if dry_run {
                    let tree = kurin::render::render_tree(&plan);
                    if out.json_enabled() && !out.v2() {
                        let plan = serde_json::json!({ "plan": tree });
                        println!("{}", serde_json::to_string_pretty(&plan).into_diagnostic()?);
                    } else if !out.v2() {
                        print!("{tree}");
                    }
                    return Ok(());
                }
                let mut emit = |event: kurin::Event| {
                    if out.v2() {
                        return;
                    }
                    if out.json_enabled() {
                        println!("{}", kurin::render::event_json_line(&event));
                    }
                };
                kurin::upgrade::execute(plan, &pool, &run, &mut emit).await
            }
            .await;
            op.finish(&result);
            result
        }
        Commands::Artifact {
            out: dir,
            arch,
            with_kubernetes,
        } => build_artifacts(dir, arch, with_kubernetes).await,
        Commands::Capabilities => capabilities_cmd(out).await,
        Commands::Certs { what } => match what {
            CertsWhat::Check { file, warn_days } => {
                let cluster = load_config(&file)?;
                let pool: Arc<dyn kurin::SessionPool> =
                    Arc::new(kurin::ssh::SshSessionPool::from_cluster(&cluster)?);
                let run = new_ephemeral_run(&cluster)?;
                let op = OpSession::start(&run, out)?;
                op.register_cluster(&cluster);
                op.begin("certs-check", &cluster.name, "");
                let result: Result<()> = async {
                    let reports = kurin::certs::check(&cluster, &pool, warn_days).await?;
                    kurin::certs::render(&reports, warn_days);
                    Ok(())
                }
                .await;
                op.finish(&result);
                result
            }
            CertsWhat::Renew { file, force } => {
                let cluster = load_config(&file)?;
                let op_id = events::new_op_id();
                let _lock = lock_cluster(&cluster, &op_id)?;
                let pool: Arc<dyn kurin::SessionPool> =
                    Arc::new(kurin::ssh::SshSessionPool::from_cluster(&cluster)?);
                let run = new_ephemeral_run_with_op_id(&cluster, op_id)?;
                let op = OpSession::start(&run, out)?;
                op.register_cluster(&cluster);
                op.begin("certs-renew", &cluster.name, "");
                let result = kurin::certs::renew(&cluster, &pool, force).await;
                op.finish(&result);
                result
            }
        },
        Commands::Precheck { file } => {
            let cluster = load_config(&file)?;
            let pool: Arc<dyn kurin::SessionPool> =
                Arc::new(kurin::ssh::SshSessionPool::from_cluster(&cluster)?);
            let run = new_ephemeral_run(&cluster)?;
            let op = OpSession::start(&run, out)?;
            op.register_cluster(&cluster);
            op.begin("precheck", &cluster.name, "");
            let result: Result<()> = async {
                let report = kurin::precheck::run(&cluster, &pool).await?;
                if out.json_enabled() {
                    kurin::precheck::render_json(&report);
                } else {
                    kurin::precheck::render(&report);
                }
                match report.ok() {
                    true => Ok(()),
                    false => Err(miette::miette!(
                        "precheck found {} failing check(s)",
                        report.failures()
                    )),
                }
            }
            .await;
            op.finish(&result);
            result
        }
        Commands::Status { file } => {
            let cluster = load_config(&file)?;
            let pool: Arc<dyn kurin::SessionPool> =
                Arc::new(kurin::ssh::SshSessionPool::from_cluster(&cluster)?);
            let run = new_ephemeral_run(&cluster)?;
            let op = OpSession::start(&run, out)?;
            op.register_cluster(&cluster);
            op.begin("status", &cluster.name, "");
            let result: Result<()> = async {
                let report = kurin::status::collect(&cluster, &pool).await?;
                if out.json_enabled() {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "healthy": report.healthy,
                            "nodes": report.nodes.iter().map(|n| serde_json::json!({
                                "name": n.name, "ready": n.ready,
                                "kubelet_version": n.kubelet_version, "roles": n.roles,
                            })).collect::<Vec<_>>(),
                            "pods_not_running": report.pods_not_running,
                            "etcd_ok": report.etcd_ok,
                            "cert_min_days": report.cert_min_days,
                        }))
                        .into_diagnostic()?
                    );
                } else {
                    kurin::status::render(&report);
                }
                if report.healthy {
                    Ok(())
                } else {
                    Err(miette::miette!("cluster is not healthy"))
                }
            }
            .await;
            op.finish(&result);
            result
        }
        Commands::Add(args) => match args.what {
            AddWhat::Nodes { file, offline } => add_nodes(file, out, offline).await,
        },
        Commands::Delete(args) => match args.what {
            DeleteWhat::Node {
                name,
                file,
                dry_run,
                check,
            } => delete_node_cmd(name, file, out, dry_run, check).await,
            DeleteWhat::Cluster {
                file,
                dry_run,
                check,
            } => delete_cluster_cmd(file, out, dry_run, check).await,
        },
        Commands::Create(args) => match args.what {
            CreateWhat::Config { print_versions } => {
                if print_versions {
                    for (name, v) in kurin::version::component_versions() {
                        println!("{name}: {v}");
                    }
                    return Ok(());
                }
                print!("{}", EXAMPLE_CONFIG);
                Ok(())
            }
            CreateWhat::Cluster {
                file,
                dry_run,
                check,
                resume,
                offline,
            } => {
                let mut cluster = load_config(&file)?;
                if let Some(dir) = offline {
                    cluster.offline_dir = Some(dir);
                }
                let cp = cluster.control_plane().count();
                if cp % 2 == 0 && cp > 1 && !out.json_enabled() {
                    eprintln!("warn: even control-plane count ({cp}); etcd quorum prefers odd");
                }
                if cp > 1 && cluster.vip.is_none() && !out.json_enabled() {
                    eprintln!("warn: {cp} control-plane nodes without ha.vip: the first master becomes the API single point of failure");
                }
                let op_id = events::new_op_id();
                // `--dry-run` (with or without `--check`) never mutates.
                let _lock = if dry_run {
                    None
                } else {
                    Some(lock_cluster(&cluster, &op_id)?)
                };
                let pool: Arc<dyn kurin::SessionPool> =
                    Arc::new(kurin::ssh::SshSessionPool::from_cluster(&cluster)?);
                let run = new_run_with_op_id(&cluster, resume, op_id)?;
                let op = OpSession::start(&run, out)?;
                op.register_cluster(&cluster);
                let result: Result<()> = async {
                    // Facts before the plan: per-node artifact selection is
                    // facts-driven. `--dry-run` stays offline (zero network):
                    // synthetic facts come from `nodes.<n>.arch` or the
                    // control machine's arch.
                    let facts = if dry_run && !check {
                        let facts = kurin::facts::FactsTable::synthetic(&cluster)?;
                        let missing = cluster
                            .nodes
                            .iter()
                            .filter(|n| n.arch.is_none())
                            .map(|n| n.name.as_str())
                            .collect::<Vec<_>>();
                        if !missing.is_empty() && !out.json_enabled() {
                            eprintln!(
                                "warn: arch not probed for {} (dry-run without --check): \
                                 using nodes.<n>.arch or the control machine's arch",
                                missing.join(", ")
                            );
                        }
                        facts
                    } else {
                        kurin::discover::run(&cluster, &pool).await?
                    };
                    run.set_facts(facts);
                    let mut plan =
                        kurin::plan::build_with_pool(&cluster, Some(pool.clone()), &run)?;
                    let fingerprint = plan.fingerprint();
                    op.begin("create-cluster", &cluster.name, &fingerprint);
                    if check {
                        kurin::executor::probe(&mut plan, &pool).await?;
                    }
                    if dry_run || !check {
                        let tree = kurin::render::render_tree(&plan);
                        if out.json_enabled() && !out.v2() {
                            let plan = serde_json::json!({ "plan": tree });
                            println!("{}", serde_json::to_string_pretty(&plan).into_diagnostic()?);
                        } else if !out.v2() {
                            print!("{tree}");
                        }
                    }
                    if dry_run {
                        return Ok(());
                    }

                    let mut opts =
                        kurin::executor::RunOptions::default().with_default_state(&cluster.name);
                    opts.resume = resume;
                    opts.plan_fingerprint = fingerprint.clone();
                    let mut emit = |event: kurin::Event| {
                        if out.v2() {
                            return;
                        }
                        if out.json_enabled() {
                            println!("{}", kurin::render::event_json_line(&event));
                        } else {
                            use kurin::Event::*;
                            match &event {
                                TaskSkipped { id, reason, .. } => match reason {
                                    Some(reason) => eprintln!("skip  {id} ({reason})"),
                                    None => eprintln!("skip  {id}"),
                                },
                                TaskDone { id, ms, .. } => eprintln!("done  {id} ({ms}ms)"),
                                TaskReconciled { id, .. } => {
                                    eprintln!("ok    {id} (reconciled)")
                                }
                                TaskFailed { id, reason, .. } => {
                                    eprintln!("FAIL  {id}: {reason}")
                                }
                                StageDone { stage, .. } => {
                                    eprintln!("-- stage {stage} complete")
                                }
                                _ => {}
                            }
                        }
                    };
                    kurin::executor::run_with_bus(
                        &mut plan,
                        &pool,
                        &run,
                        &run.events,
                        &mut emit,
                        opts,
                    )
                    .await?;
                    if !out.json_enabled() {
                        eprintln!("cluster {} ready", cluster.name);
                    }
                    Ok(())
                }
                .await;
                op.finish(&result);
                result
            }
        },
    }
}

/// `kurin logs`: read JSONL envelopes with cluster/op/node/level filters.
async fn logs_cmd(args: LogsArgs, out: OutputOpts) -> Result<()> {
    let min_level = match args.level.as_deref() {
        Some(level) => Level::parse(level).ok_or_else(|| {
            miette::miette!("invalid --level {level:?} (trace|debug|info|warn|error)")
        })?,
        None if out.verbose >= 2 => Level::Debug,
        None => Level::Info,
    };
    let query = LogQuery {
        min_level,
        node: args.node.clone(),
    };
    let cluster = resolve_cluster(args.cluster)?;
    // `--op` wins; otherwise (including `--last`) the newest log is read.
    let _ = args.last;
    let path = match &args.op {
        Some(op) => events::find_log(&cluster, op).ok_or_else(|| {
            miette::miette!("no operation log matching {op:?} for cluster {cluster}")
        })?,
        None => events::latest_log(&cluster)
            .ok_or_else(|| miette::miette!("no operation logs for cluster {cluster}"))?,
    };

    let mut offset = 0u64;
    loop {
        let envelopes = events::read_log_from(&path, &mut offset)
            .map_err(|e| miette::miette!("read {}: {e}", path.display()))?;
        for envelope in envelopes {
            if !query.matches(&envelope) {
                continue;
            }
            if out.json_enabled() {
                println!("{}", serde_json::to_string(&envelope).into_diagnostic()?);
            } else {
                println!("{}", kurin::render::format_envelope(&envelope));
            }
        }
        if !args.follow {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    Ok(())
}

fn resolve_cluster(explicit: Option<String>) -> Result<String> {
    if let Some(cluster) = explicit {
        return Ok(cluster);
    }
    let mut clusters = events::list_clusters().into_diagnostic()?;
    match clusters.len() {
        1 => Ok(clusters.remove(0)),
        0 => Err(miette::miette!(
            "no operation logs yet; run a kurin operation first"
        )),
        _ => Err(miette::miette!(
            "logs exist for multiple clusters ({}); pass --cluster",
            clusters.join(", ")
        )),
    }
}

/// `kurin capabilities --json`: the aggregated component capability
/// table. Machine-facing input for `just docs-matrix` and the Web UI.
async fn capabilities_cmd(_out: OutputOpts) -> Result<()> {
    let json = serde_json::to_string_pretty(&kurin::components::capabilities_report())
        .map_err(|e| miette::miette!("serialize capability table: {e}"))?;
    println!("{json}");
    Ok(())
}

async fn build_artifacts(
    out: PathBuf,
    arch: Option<String>,
    with_kubernetes: Option<String>,
) -> Result<()> {
    let zone = resolve_zone(None)?;
    // `--arch amd64,arm64` builds every listed arch (mixed-architecture
    // clusters need one bundle covering all of them); the default is
    // the control machine's arch.
    let arches: Vec<kurin::arch::Arch> = match arch.as_deref() {
        Some(list) => list
            .split(',')
            .map(|a| a.trim())
            .filter(|a| !a.is_empty())
            .map(|a| match a {
                "amd64" | "x86_64" => Ok(kurin::arch::Arch::Amd64),
                "arm64" | "aarch64" => Ok(kurin::arch::Arch::Arm64),
                other => Err(miette::miette!("unknown arch {other:?} (amd64|arm64)")),
            })
            .collect::<miette::Result<Vec<_>>>()?,
        None => vec![kurin::arch::Arch::host()?],
    };
    // De-duplicate while preserving order.
    let mut arches = arches;
    arches.dedup();
    for arch in arches {
        let manifest = match &with_kubernetes {
            Some(v) => {
                let target: semver::Version = v
                    .trim_start_matches('v')
                    .parse()
                    .map_err(|e| miette::miette!("--with-kubernetes {v:?}: {e}"))?;
                kurin::dynamic_manifest::k8s_manifest_for(&target, arch, zone).await?
            }
            None => kurin::artifact::ArtifactManifest::embedded()?,
        };
        let store = kurin::artifact::ArtifactStore::open_default()?;
        std::fs::create_dir_all(&out).into_diagnostic()?;
        // The bundle content is the component registry's aggregation (the
        // offline image set is no longer hand-maintained); the manifest
        // itself still resolves versions and pins.
        for name in kurin::components::bundle_artifacts(arch)
            .into_iter()
            .map(|r| r.name)
        {
            let spec = manifest.spec(&name, arch)?;
            let dir = store.artifact_dir(&spec);
            std::fs::create_dir_all(&dir).into_diagnostic()?;
            let dest = store.fetch(&spec, zone).await?;
            // Copy the artifact directory into the bundle layout.
            let target = out.join(format!("{}-{}-{}", spec.name, spec.version, arch.as_str()));
            std::fs::create_dir_all(&target).into_diagnostic()?;
            for f in std::fs::read_dir(&dir).into_diagnostic()? {
                let f = f.into_diagnostic()?;
                let name = f.file_name();
                if name.to_string_lossy().ends_with(".part") {
                    continue;
                }
                std::fs::copy(f.path(), target.join(&name)).into_diagnostic()?;
            }
            eprintln!("bundled {} {} ({})", spec.name, spec.version, arch.as_str());
            let _ = &dest;
        }
        // Sidecar checksums are included by the copy above; the dir name
        // layout is exactly what ArtifactManifest::from_dir expects.
        kurin::artifact_bundle::write_bundle_manifest(&out, &manifest)
            .map_err(|e| miette::miette!("bundle manifest: {e}"))?;
    }
    eprintln!("offline bundle written to {}", out.display());
    Ok(())
}

async fn add_nodes(file: PathBuf, out: OutputOpts, offline: Option<PathBuf>) -> Result<()> {
    let mut cluster = load_config(&file)?;
    if let Some(dir) = offline {
        cluster.offline_dir = Some(dir);
    }
    let op_id = events::new_op_id();
    let _lock = lock_cluster(&cluster, &op_id)?;
    let pool: Arc<dyn kurin::SessionPool> =
        Arc::new(kurin::ssh::SshSessionPool::from_cluster(&cluster)?);
    let zone = resolve_zone(cluster.zone.as_deref())?;

    // The dynamic manifest (when the cluster runs a version newer than the
    // embedded table) must be known before the run context is built: the
    // plan resolves artifacts from the run's sources, not a global.
    let mut manifest_json = None;
    let mut first_session = None;
    if let Some(first) = cluster.control_plane().next() {
        let s = pool.get(&first.name).await?;
        // If the cluster was initialized by this kurin (admin.conf present),
        // refresh the token + certificate key.
        let init = s.exec("test -f /etc/kubernetes/admin.conf").await?;
        if init.ok() {
            // Version threading: a cluster upgraded past the embedded table
            // gets its running version installed on the new nodes via a
            // dynamic manifest (dl.k8s.io checksums + registry digests).
            let version_out = s
                .exec("kubectl --kubeconfig /etc/kubernetes/admin.conf get --raw /version 2>/dev/null")
                .await?;
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&version_out.stdout) {
                if let Some(git) = v.get("gitVersion").and_then(|g| g.as_str()) {
                    let server = git.trim_start_matches('v').to_string();
                    let embedded = kurin::version::KUBERNETES_VERSION;
                    if server != embedded {
                        let target: semver::Version = server
                            .parse()
                            .map_err(|e| miette::miette!("server version {server:?}: {e}"))?;
                        // The dynamic manifest is resolved on the
                        // control machine (its kubeadm probe runs
                        // locally); node binaries pick their own arch.
                        let arch = kurin::arch::Arch::host()?;
                        let manifest = kurin::dynamic_manifest::k8s_manifest_for_from(
                            &target, &target, arch, zone,
                        )
                        .await?;
                        manifest_json = Some(
                            serde_json::to_string(&manifest)
                                .into_diagnostic()
                                .map_err(|e| miette::miette!("encode dynamic manifest: {e}"))?,
                        );
                        tracing::info!(
                            "[add-nodes] new nodes install v{server} (dynamic manifest; embedded v{embedded})"
                        );
                    }
                }
            }
            first_session = Some(s);
        }
    }

    let sources = ArtifactSources {
        zone,
        manifest_json,
        offline_dir: cluster.offline_dir.clone(),
    };
    let bb = match kurin::blackboard::Blackboard::default_path(&cluster.name) {
        Some(path) => {
            kurin::blackboard::Blackboard::open(path, false).map_err(|e| miette::miette!("{e}"))?
        }
        None => kurin::blackboard::Blackboard::in_memory(),
    };
    let run = Arc::new(RunContext::new(op_id, &cluster.name, sources).with_blackboard(bb));
    let op = OpSession::start(&run, out)?;
    op.register_cluster(&cluster);

    let result: Result<()> = async {
        if let Some(s) = first_session {
            let first = cluster
                .control_plane()
                .next()
                .map(|n| n.name.clone())
                .unwrap_or_default();
            kurin::tasks::bootstrap::refresh_join_material(s.as_ref(), &run)
                .await
                .map_err(|e| miette::miette!("refresh join material on {first}: {e}"))?;
        }

        // Same plan/executor as create: converged nodes skip, new nodes
        // install. Facts first — per-node artifact selection.
        let facts = kurin::discover::run(&cluster, &pool).await?;
        run.set_facts(facts);
        let mut plan = kurin::plan::build_with_pool(&cluster, Some(pool.clone()), &run)?;
        let fingerprint = plan.fingerprint();
        op.begin("add-nodes", &cluster.name, &fingerprint);
        let mut opts = kurin::executor::RunOptions::default().with_default_state(&cluster.name);
        opts.resume = false;
        opts.plan_fingerprint = fingerprint;
        let mut emit = |event: kurin::Event| {
            if out.v2() {
                return;
            }
            if out.json_enabled() {
                println!("{}", kurin::render::event_json_line(&event));
            } else {
                use kurin::Event::*;
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
            }
        };
        kurin::executor::run_with_bus(&mut plan, &pool, &run, &run.events, &mut emit, opts).await?;
        if !out.json_enabled() {
            eprintln!("nodes reconciled for cluster {}", cluster.name);
        }
        Ok(())
    }
    .await;
    op.finish(&result);
    result
}

/// Run a teardown plan: `--dry-run` prints the tree (after probing live
/// nodes with `--check`), otherwise the executor runs it and the best-effort
/// failures are summarized per node.
///
/// The tasks already print the historical `[node] ok step` lines through
/// `tracing`, so the human renderer prints nothing per task: the executor's
/// `done`/`skip` lines would duplicate the step report, and every teardown
/// step is best-effort, so a `FAIL` line would claim the operation failed
/// when it did not (the `warn:` line and the closing summary cover it).
async fn run_teardown_plan(
    plan: &mut kurin::Plan,
    pool: &Arc<dyn kurin::SessionPool>,
    run: &Arc<RunContext>,
    out: OutputOpts,
    dry_run: bool,
    check: bool,
) -> Result<()> {
    if check {
        kurin::executor::probe(plan, pool).await?;
    }
    if dry_run {
        let tree = kurin::render::render_tree(plan);
        if out.json_enabled() && !out.v2() {
            let plan = serde_json::json!({ "plan": tree });
            println!("{}", serde_json::to_string_pretty(&plan).into_diagnostic()?);
        } else if !out.v2() {
            print!("{tree}");
        }
        return Ok(());
    }

    let warnings = kurin::teardown::WarningSummary::new();
    run.events.add_sink(warnings.clone());
    let opts = kurin::executor::RunOptions {
        plan_fingerprint: plan.fingerprint(),
        ..Default::default()
    };
    let mut emit = |event: kurin::Event| {
        if out.v2() {
            return;
        }
        if out.json_enabled() {
            println!("{}", kurin::render::event_json_line(&event));
        }
    };
    kurin::executor::run_with_bus(plan, pool, run, &run.events, &mut emit, opts).await?;
    if let Some(summary) = warnings.render() {
        eprintln!("{summary}");
    }
    Ok(())
}

async fn delete_node_cmd(
    name: String,
    file: PathBuf,
    out: OutputOpts,
    dry_run: bool,
    check: bool,
) -> Result<()> {
    let cluster = load_config(&file)?;
    let node = cluster
        .nodes
        .iter()
        .find(|n| n.name == name)
        .cloned()
        .ok_or_else(|| miette::miette!("node {name:?} is not declared in {}", file.display()))?;
    let op_id = events::new_op_id();
    // `--dry-run` only reads; everything else mutates the cluster.
    let _lock = if dry_run {
        None
    } else {
        Some(lock_cluster(&cluster, &op_id)?)
    };
    let pool: Arc<dyn kurin::SessionPool> =
        Arc::new(kurin::ssh::SshSessionPool::from_cluster(&cluster)?);
    let run = new_ephemeral_run_with_op_id(&cluster, op_id)?;
    let op = OpSession::start(&run, out)?;
    op.register_cluster(&cluster);
    let result: Result<()> = async {
        let mut plan = kurin::plan::build_delete(&cluster, &run, Some(&node))?;
        op.begin("delete-node", &cluster.name, &plan.fingerprint());
        run_teardown_plan(&mut plan, &pool, &run, out, dry_run, check).await
    }
    .await;
    op.finish(&result);
    result
}

async fn delete_cluster_cmd(
    file: PathBuf,
    out: OutputOpts,
    dry_run: bool,
    check: bool,
) -> Result<()> {
    let cluster = load_config(&file)?;
    let op_id = events::new_op_id();
    let _lock = if dry_run {
        None
    } else {
        Some(lock_cluster(&cluster, &op_id)?)
    };
    // The plan ends with a control-machine step; the wrapper answers the
    // `local` node without an SSH connection.
    let pool: Arc<dyn kurin::SessionPool> = kurin::teardown::with_local_node(Arc::new(
        kurin::ssh::SshSessionPool::from_cluster(&cluster)?,
    ));
    let run = new_ephemeral_run_with_op_id(&cluster, op_id)?;
    let op = OpSession::start(&run, out)?;
    op.register_cluster(&cluster);
    let result: Result<()> = async {
        let mut plan = kurin::plan::build_delete(&cluster, &run, None)?;
        op.begin("delete-cluster", &cluster.name, &plan.fingerprint());
        run_teardown_plan(&mut plan, &pool, &run, out, dry_run, check).await
    }
    .await;
    op.finish(&result);
    result
}

const EXAMPLE_CONFIG: &str = include_str!("../examples/cluster.example.toml");

#[cfg(test)]
mod tests {
    use super::*;

    /// A cancelled run is reported as `OpFinished { result: cancelled }`
    /// with no error text, not as a failure.
    #[test]
    fn cancelled_run_finishes_as_cancelled() {
        let bus = Arc::new(EventBus::new("op-test"));
        let mut rx = bus.subscribe();
        let session = OpSession {
            bus: Arc::clone(&bus),
            started: Instant::now(),
        };
        let result: Result<()> = Err(miette::Report::new(kurin::executor::Cancelled));
        session.finish(&result);

        let envelope = rx.try_recv().expect("op-finished was published");
        match envelope.body {
            Body::OpFinished { result, error, .. } => {
                assert_eq!(result, OpResult::Cancelled);
                assert!(error.is_none());
            }
            other => panic!("expected op-finished, got {other:?}"),
        }
    }
}
