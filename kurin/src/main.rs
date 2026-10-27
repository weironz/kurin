//! `kurin` binary: argv parsing only. Every operation lives in `kurin-ops`.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{ArgAction, Parser, Subcommand, ValueEnum};
use miette::Result;

use kurin::ops::{self, JsonMode, OpRequest, OutputOpts, Params, StatusOpts};

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
    json: Option<JsonArg>,
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

/// Version of the `--json` stream (clap surface).
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum JsonArg {
    /// Frozen v1 event objects, now carrying `ts`.
    V1,
    /// Full v2 envelopes (`v`, `seq`, `ts`, `op`, `body`).
    V2,
}

impl From<JsonArg> for JsonMode {
    fn from(value: JsonArg) -> Self {
        match value {
            JsonArg::V1 => JsonMode::V1,
            JsonArg::V2 => JsonMode::V2,
        }
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

#[tokio::main(flavor = "multi_thread")]
async fn main() -> ExitCode {
    miette::set_panic_hook();
    let cli = Cli::parse();
    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e:?}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<()> {
    let out = OutputOpts {
        json: cli.json.map(JsonMode::from),
        timestamps: cli.timestamps,
        verbose: cli.verbose,
    };
    match cli.command {
        Commands::Logs(args) => {
            ops::logs(
                ops::LogsRequest {
                    cluster: args.cluster,
                    op: args.op,
                    last: args.last,
                    follow: args.follow,
                    node: args.node,
                    level: args.level,
                },
                out,
            )
            .await
        }
        Commands::Upgrade {
            file,
            with_kubernetes,
            dry_run,
            batch,
            offline,
            force_drain_failure,
        } => {
            let spec = ops::load_config(&file)?;
            let target = with_kubernetes
                .as_deref()
                .map(|v| v.trim_start_matches('v'))
                .map(semver::Version::parse)
                .transpose()
                .map_err(|e| miette::miette!("invalid --with-kubernetes: {e}"))?;
            run_op(
                OpRequest {
                    kind: kurin::plan::OpKind::Upgrade,
                    spec,
                    params: Params::Upgrade {
                        target,
                        dry_run,
                        batch,
                        offline,
                        force_drain_failure,
                    },
                },
                out,
            )
            .await
        }
        Commands::Artifact {
            out: dir,
            arch,
            with_kubernetes,
        } => ops::build_artifacts(dir, arch, with_kubernetes).await,
        Commands::Capabilities => {
            println!("{}", ops::capabilities()?);
            Ok(())
        }
        Commands::Certs { what } => match what {
            CertsWhat::Check { file, warn_days } => {
                let spec = ops::load_config(&file)?;
                ops::certs_check(&spec, warn_days, out).await?;
                Ok(())
            }
            CertsWhat::Renew { file, force } => {
                let spec = ops::load_config(&file)?;
                run_op(
                    OpRequest {
                        kind: kurin::plan::OpKind::CertsRenew,
                        spec,
                        params: Params::CertsRenew { force },
                    },
                    out,
                )
                .await
            }
        },
        Commands::Precheck { file } => {
            let spec = ops::load_config(&file)?;
            let report = ops::precheck(&spec, out).await?;
            match report.ok() {
                true => Ok(()),
                false => Err(miette::miette!(
                    "precheck found {} failing check(s)",
                    report.failures()
                )),
            }
        }
        Commands::Status { file } => {
            let spec = ops::load_config(&file)?;
            let report = ops::status(&spec, StatusOpts { certs: true }, out).await?;
            if report.healthy {
                Ok(())
            } else {
                Err(miette::miette!("cluster is not healthy"))
            }
        }
        Commands::Add(args) => match args.what {
            AddWhat::Nodes { file, offline } => {
                let spec = ops::load_config(&file)?;
                run_op(
                    OpRequest {
                        kind: kurin::plan::OpKind::AddNodes,
                        spec,
                        params: Params::AddNodes { offline },
                    },
                    out,
                )
                .await
            }
        },
        Commands::Delete(args) => match args.what {
            DeleteWhat::Node {
                name,
                file,
                dry_run,
                check,
            } => {
                let spec = ops::load_config(&file)?;
                run_op(
                    OpRequest {
                        kind: kurin::plan::OpKind::DeleteNode,
                        spec,
                        params: Params::DeleteNode {
                            node: name,
                            dry_run,
                            check,
                        },
                    },
                    out,
                )
                .await
            }
            DeleteWhat::Cluster {
                file,
                dry_run,
                check,
            } => {
                let spec = ops::load_config(&file)?;
                run_op(
                    OpRequest {
                        kind: kurin::plan::OpKind::DeleteCluster,
                        spec,
                        params: Params::DeleteCluster { dry_run, check },
                    },
                    out,
                )
                .await
            }
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
                let spec = ops::load_config(&file)?;
                run_op(
                    OpRequest {
                        kind: kurin::plan::OpKind::Create,
                        spec,
                        params: Params::Create {
                            dry_run,
                            check,
                            resume,
                            offline,
                        },
                    },
                    out,
                )
                .await
            }
        },
    }
}

/// Prepare -> plan -> execute, the whole operation pipeline.
async fn run_op(req: OpRequest, out: OutputOpts) -> Result<()> {
    let prepared = ops::prepare(req, out).await?;
    let plan = ops::plan(&prepared).await?;
    ops::execute(prepared, plan).await?;
    Ok(())
}

const EXAMPLE_CONFIG: &str = include_str!("../examples/cluster.example.toml");
