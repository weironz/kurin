//! `kurin` binary: argv parsing only. Every operation lives in `kurin-ops`.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{ArgAction, Parser, Subcommand, ValueEnum};
use miette::{IntoDiagnostic, Result};

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
    /// Configuration utilities.
    Config {
        #[command(subcommand)]
        what: ConfigWhat,
    },
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
    /// Build and inspect OCI packages for air-gapped installs (WP-32).
    Package {
        #[command(subcommand)]
        what: PackageWhat,
    },
    /// Deprecated alias of `kurin package create base -o oci-layout://<dir>`.
    ///
    /// Builds an offline artifact bundle for air-gapped installs.
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
        /// (default: the compiled-in table).
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
    /// Run the web UI server (built with `--features ui`).
    Serve(ServeArgs),
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

#[derive(Subcommand)]
enum PackageWhat {
    /// Build a package.
    Create {
        #[command(subcommand)]
        what: PackageCreateWhat,
    },
    /// Print a package's metadata (name, version, components, artifacts,
    /// images).
    Inspect {
        /// Package source: `oci-layout://DIR`, `file://X.tar` or
        /// `file://X.tar.zst`.
        source: String,
        /// Print the full `kurin-package.json` instead of the summary.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum PackageCreateWhat {
    /// One Kubernetes version + distribution's full binary and image set
    /// (04 §3.4).
    Base {
        /// Kubernetes version to pin (default: the compiled-in table).
        #[arg(long)]
        kubernetes: Option<String>,
        /// Kubernetes distribution: `kubeadm` (default) or `k3s`.
        #[arg(long)]
        distribution: Option<String>,
        /// Architecture(s) to build for, comma-separated (default: this
        /// machine's). A mixed-architecture cluster needs every arch its
        /// nodes use, e.g. `--arch amd64,arm64`.
        #[arg(long)]
        arch: Option<String>,
        /// CNI payloads to include: `all` (default), `none`, or a
        /// comma-separated list (`calico,flannel,cilium,kube-ovn`).
        #[arg(long)]
        cni: Option<String>,
        /// Output target: `oci-layout://DIR`, `file://X.tar` or
        /// `file://X.tar.zst`.
        #[arg(short, long)]
        output: String,
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

#[derive(Subcommand)]
enum ConfigWhat {
    /// Rewrite a v1alpha1 config as v1alpha2, preserving comments and order.
    /// Without `-o` the migrated document is printed to stdout.
    Migrate {
        /// Config file to migrate (.toml).
        #[arg(short, long)]
        file: PathBuf,
        /// Write the migrated document here instead of stdout.
        #[arg(short, long)]
        out: Option<PathBuf>,
    },
    /// Print the JSON Schema for a config version.
    Schema {
        /// Schema version to export (default: v1alpha2).
        #[arg(long, value_enum, default_value_t = SchemaVersion::V1Alpha2)]
        version: SchemaVersion,
    },
}

/// Schema version printed by `create config`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum SchemaVersion {
    #[value(name = "v1alpha1")]
    V1Alpha1,
    #[value(name = "v1alpha2")]
    V1Alpha2,
}

impl SchemaVersion {
    /// Config version string this CLI spelling selects.
    fn as_str(self) -> &'static str {
        match self {
            SchemaVersion::V1Alpha1 => kurin_config::CONFIG_VERSION,
            SchemaVersion::V1Alpha2 => kurin_config::v1alpha2::CONFIG_VERSION,
        }
    }
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
        /// Schema version of the printed example (default: v1alpha2).
        #[arg(long, value_enum, default_value_t = SchemaVersion::V1Alpha2)]
        schema_version: SchemaVersion,
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

/// `kurin serve` options (UI architecture §3.1 / §6.10).
#[derive(clap::Args)]
struct ServeArgs {
    /// Listen address (default: 127.0.0.1:7080; non-loopback requires TLS
    /// or --allow-insecure-http).
    #[arg(long, value_name = "ADDR", default_value = "127.0.0.1:7080")]
    listen: SocketAddr,
    /// Data directory holding the access token (and, later, the database);
    /// default: ~/.local/share/kurin.
    #[arg(long, value_name = "DIR")]
    data_dir: Option<PathBuf>,
    /// Extra allowed Host header (repeatable), e.g. kurin.example.com:7080.
    #[arg(long = "public-host", value_name = "HOST")]
    public_host: Vec<String>,
    /// TLS certificate chain (PEM); requires --tls-key.
    #[arg(long, requires = "tls_key", value_name = "FILE")]
    tls_cert: Option<PathBuf>,
    /// TLS private key (PEM); requires --tls-cert.
    #[arg(long, requires = "tls_cert", value_name = "FILE")]
    tls_key: Option<PathBuf>,
    /// Serve plain HTTP on a non-loopback address (insecure; prints a
    /// warning).
    #[arg(long)]
    allow_insecure_http: bool,
    /// Generate a fresh access token, invalidating every session.
    #[arg(long)]
    rotate_token: bool,
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
        } => {
            eprintln!(
                "warn: `kurin artifact` is deprecated: it now builds the same OCI package as \
                 `kurin package create base -o oci-layout://<dir>`; the alias will be removed \
                 in a future release"
            );
            package_create_base(
                with_kubernetes,
                None,
                arch,
                None,
                format!("oci-layout://{}", dir.display()),
            )
            .await
        }
        Commands::Package { what } => match what {
            PackageWhat::Create { what } => match what {
                PackageCreateWhat::Base {
                    kubernetes,
                    distribution,
                    arch,
                    cni,
                    output,
                } => package_create_base(kubernetes, distribution, arch, cni, output).await,
            },
            PackageWhat::Inspect { source, json } => {
                let handle = kurin::package::PackageSource::open(&source)?;
                if json {
                    let meta = handle.manifest()?;
                    println!("{}", serde_json::to_string_pretty(&meta).into_diagnostic()?);
                } else {
                    print!("{}", kurin::package::inspect_summary(&handle)?);
                }
                Ok(())
            }
        },
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
        Commands::Config { what } => match what {
            ConfigWhat::Migrate { file, out } => {
                let src = std::fs::read_to_string(&file)
                    .map_err(|e| miette::miette!("failed to read {}: {e}", file.display()))?;
                let migrated = kurin_config::migrate::migrate_str(&file, &src)?;
                match out {
                    Some(path) => {
                        std::fs::write(&path, &migrated).map_err(|e| {
                            miette::miette!("failed to write {}: {e}", path.display())
                        })?;
                        eprintln!("[config] migrated {} -> {}", file.display(), path.display());
                        Ok(())
                    }
                    None => {
                        print!("{migrated}");
                        Ok(())
                    }
                }
            }
            ConfigWhat::Schema { version } => {
                print!("{}", kurin_config::schema::schema_json(version.as_str())?);
                Ok(())
            }
        },
        Commands::Create(args) => match args.what {
            CreateWhat::Config {
                schema_version,
                print_versions,
            } => {
                if print_versions {
                    for (name, v) in kurin::version::component_versions() {
                        println!("{name}: {v}");
                    }
                    return Ok(());
                }
                let example = match schema_version {
                    SchemaVersion::V1Alpha1 => EXAMPLE_CONFIG_V1ALPHA1,
                    SchemaVersion::V1Alpha2 => EXAMPLE_CONFIG_V1ALPHA2,
                };
                print!("{example}");
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
        Commands::Serve(args) => serve(args).await,
    }
}

/// Wire the parsed `serve` arguments into `kurin_server` (UI §3.1). Built
/// without the `ui` feature the subcommand explains itself instead.
async fn serve(args: ServeArgs) -> Result<()> {
    #[cfg(feature = "ui")]
    {
        let options = kurin_server::ServeOptions {
            listen: args.listen,
            data_dir: args.data_dir.unwrap_or_else(kurin_server::default_data_dir),
            public_hosts: args.public_host,
            tls: match (args.tls_cert, args.tls_key) {
                (Some(cert), Some(key)) => Some(kurin_server::TlsPaths { cert, key }),
                (cert, key) => {
                    debug_assert!(
                        cert.is_none() && key.is_none(),
                        "clap `requires` pairs them"
                    );
                    None
                }
            },
            allow_insecure_http: args.allow_insecure_http,
            rotate_token: args.rotate_token,
        };
        kurin_server::serve(options)
            .await
            .map_err(|e| miette::miette!("{e}"))
    }
    #[cfg(not(feature = "ui"))]
    {
        let _ = args;
        Err(miette::miette!(
            "`serve` needs a build with --features ui (the release binaries include it)"
        ))
    }
}

/// Prepare -> plan -> execute, the whole operation pipeline.
async fn run_op(req: OpRequest, out: OutputOpts) -> Result<()> {
    let prepared = ops::prepare(req, out).await?;
    let plan = ops::plan(&prepared).await?;
    ops::execute(prepared, plan).await?;
    Ok(())
}

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

async fn package_create_base(
    kubernetes: Option<String>,
    distribution: Option<String>,
    arch: Option<String>,
    cni: Option<String>,
    output: String,
) -> Result<()> {
    let zone = resolve_zone(None)?;
    let distribution = match distribution.as_deref() {
        None | Some("kubeadm") => "kubeadm",
        Some("k3s") => "k3s",
        Some(other) => {
            return Err(miette::miette!(
                "unknown distribution {other:?} (kubeadm|k3s)"
            ))
        }
    };
    let config_distribution = match distribution {
        "k3s" => kurin_config::Distribution::K3s,
        _ => kurin_config::Distribution::K8s,
    };
    let cni_selection = match cni.as_deref() {
        None => kurin::components::CniSelection::All,
        Some(value) => kurin::components::CniSelection::parse(value).ok_or_else(|| {
            miette::miette!("invalid --cni {value:?} (all | none | calico,flannel,cilium,kube-ovn)")
        })?,
    };
    // `--arch amd64,arm64` builds every listed arch (mixed-architecture
    // clusters need one package covering all of them); the default is
    // the control machine's arch.
    let mut arches: Vec<kurin::arch::Arch> = match arch.as_deref() {
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
    arches.dedup();

    let out = kurin::package::PackageOutput::parse(&output)?;
    let store = kurin::artifact::ArtifactStore::open_default()?;
    let opts = kurin::package::BasePackage {
        kubernetes: kubernetes.clone(),
        distribution: distribution.to_string(),
        arches,
        components: kurin::components::base_components(config_distribution, &cni_selection)
            .into_iter()
            .map(|c| kurin::package::ComponentSpec {
                id: c.id,
                artifacts: c.artifacts,
            })
            .collect(),
    };
    let report = kurin::package::create_base(&store, &opts, zone, &out)
        .await
        .map_err(|e| miette::miette!("{e:?}"))?;
    eprintln!(
        "package base {} [{}] written to {} ({} file(s), {} image(s))",
        report.tag,
        opts.arches
            .iter()
            .map(|a| a.as_str())
            .collect::<Vec<_>>()
            .join(","),
        report.output.display(),
        report.files,
        report.images
    );
    eprintln!("index: {}", report.index);
    eprintln!("sbom:  {}", report.sbom.display());
    Ok(())
}

const EXAMPLE_CONFIG_V1ALPHA1: &str = include_str!("../examples/cluster.example.toml");
const EXAMPLE_CONFIG_V1ALPHA2: &str = include_str!("../examples/cluster.example.v1alpha2.toml");
