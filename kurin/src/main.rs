//! `kurin` binary: argv parsing only. Every operation lives in `kurin-ops`.

use std::io::IsTerminal;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{ArgAction, Parser, Subcommand, ValueEnum};
use kurin_config::{HostKeyMode, SecretRef, SshAuth};
use miette::{IntoDiagnostic, Result};

use kurin::cli;
use kurin::cli::addon;
use kurin::cli::quick::{
    self, KnownHostsArg, NodeNameStrategy, QuickArgs, QuickConfig, SaveDefault, SaveOutcome,
};
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
    /// Cap concurrent payload uploads per operation (default 4). Uploads
    /// share the control host's uplink, so they are gated independently
    /// of task concurrency.
    #[arg(long = "max-upload-parallel", global = true, value_name = "N")]
    max_upload_parallel: Option<usize>,
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

// The quick-create block is one large struct shared by two subcommands;
// argv is parsed once per process, so the enum's size buys nothing.
#[allow(clippy::large_enum_variant)]
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
    /// Manage etcd backups: snapshot, restore, schedule.
    Etcd {
        #[command(subcommand)]
        what: EtcdWhat,
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
        /// Offline source: a legacy bundle directory, or a package
        /// (`file://X.tar.zst`, `oci-layout://DIR`) — equivalent to
        /// `--package` (WP-34).
        #[arg(long, value_name = "DIR|URI")]
        offline: Option<String>,
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
    /// Manage declarative addons: list, info, install, upgrade, remove.
    Addon {
        #[command(subcommand)]
        what: AddonWhat,
    },
    /// Delete a node or the whole cluster.
    Delete(DeleteArgs),
    /// Read the JSONL operation logs written by every operation.
    Logs(LogsArgs),
    /// Reconstruct a cluster config from a running cluster's state
    /// record (WP-73).
    Import(ImportArgs),
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
    /// Install a systemd timer on every control plane that renews the
    /// certificates before they expire.
    Schedule {
        /// Config file (.toml or .yaml) of the running cluster.
        #[arg(short, long)]
        file: PathBuf,
        /// Renew when a certificate has less than this many days left.
        #[arg(long, default_value_t = 30)]
        warn_days: i64,
        /// Check interval, e.g. 1h, 12h, 1d.
        #[arg(long, default_value = "24h")]
        every: String,
    },
}

#[derive(Subcommand)]
enum EtcdWhat {
    /// Take an etcd snapshot on every control-plane node and pull a
    /// copy back to the control machine.
    Backup {
        /// Config file (.toml or .yaml) of the running cluster.
        #[arg(short, long)]
        file: PathBuf,
        /// Output file (default: ./etcd-<cluster>-<timestamp>.db).
        #[arg(short, long, value_name = "FILE")]
        output: Option<PathBuf>,
    },
    /// Restore every control plane's etcd from a snapshot. Destructive:
    /// the running etcd data is parked (never deleted) on each node and
    /// the cluster's state is rewound to the snapshot.
    Restore {
        /// Config file (.toml or .yaml) of the running cluster.
        #[arg(short, long)]
        file: PathBuf,
        /// Snapshot file written by `kurin etcd backup`.
        #[arg(long, value_name = "FILE")]
        from: PathBuf,
    },
    /// Install a systemd timer taking (and pruning) node-local etcd
    /// snapshots on every control plane.
    Schedule {
        /// Config file (.toml or .yaml) of the running cluster.
        #[arg(short, long)]
        file: PathBuf,
        /// Snapshot interval, e.g. 30m, 6h, 1d.
        #[arg(long)]
        every: String,
        /// Node-local snapshots to keep (default 7).
        #[arg(long, default_value_t = 7)]
        keep: usize,
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
        /// Package source: `oci://REG/NS/NAME[:TAG]`, `oci-layout://DIR`,
        /// `file://X.tar` or `file://X.tar.zst`.
        source: String,
        /// Print the full `kurin-package.json` document instead of the
        /// summary. (Named `--manifest`, not `--json`: the global
        /// `--json` owns the event-stream flag and clap requires long
        /// names to be unique per command.)
        #[arg(long)]
        manifest: bool,
        /// Print the package's SPDX SBOM document instead of the summary.
        #[arg(long)]
        sbom: bool,
    },
    /// Re-hash every blob in the package's digest closure (04 §6), and
    /// with `--key` also require a cosign signature that one of the keys
    /// verifies.
    Verify {
        /// Package source: any of the three transport forms.
        source: String,
        /// cosign public key to require a signature from (repeatable).
        /// Without a key only the digest closure is verified.
        #[arg(long = "key", value_name = "PUB")]
        keys: Vec<PathBuf>,
        /// Registry user (registry sources only).
        #[arg(long)]
        user: Option<String>,
        /// Environment variable holding the registry password.
        #[arg(long)]
        password_env: Option<String>,
    },
    /// Copy a package to an `oci://` registry, sending only the blobs
    /// the registry is missing (04 §3.3).
    Push {
        /// Source: `oci-layout://DIR`, `file://X.tar[.zst]` or `oci://…`.
        source: String,
        /// Destination: `oci://REG/NS/NAME:TAG`.
        destination: String,
        #[arg(long)]
        user: Option<String>,
        #[arg(long)]
        password_env: Option<String>,
    },
    /// Copy a package from any source to a local destination
    /// (`oci-layout://` or `file://`); the same transfer as `push`.
    Pull {
        /// Source: any of the three transport forms.
        source: String,
        /// Destination: `oci-layout://DIR`, `file://X.tar` or
        /// `file://X.tar.zst`.
        destination: String,
        #[arg(long)]
        user: Option<String>,
        #[arg(long)]
        password_env: Option<String>,
    },
    /// List the local content-addressed cache (`~/.cache/kurin/blobs`).
    List {
        /// Print a JSON array of `{digest, size}` objects instead of
        /// the summary. (`--format`, not `--json`: the global `--json`
        /// owns the event-stream flag and clap requires long names to
        /// be unique per command.)
        #[arg(long, value_enum, default_value_t = ListFormat::Text)]
        format: ListFormat,
    },
    /// Delete cache blobs no artifact currently references.
    Prune {
        /// Print what would be removed without deleting anything.
        #[arg(long)]
        dry_run: bool,
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
        /// Addons to embed (`--addons ingress-nginx,metrics-server`):
        /// each entry is a built-in catalog name, a directory, an
        /// `addon.yaml` path, or an addon package (`oci-layout://` /
        /// `file://`). Their files land under `addons/<name>/` and their
        /// images join the package, so the selected addons install
        /// offline from this one package.
        #[arg(long, value_name = "NAMES")]
        addons: Option<String>,
        /// Output target: `oci-layout://DIR`, `file://X.tar` or
        /// `file://X.tar.zst`.
        #[arg(short, long)]
        output: String,
    },
    /// One addon (addon.yaml + templates + chart + images), installable
    /// offline (04 §3.4).
    Addon {
        /// The addon directory or its `addon.yaml`
        /// (`-f addons/<name>/addon.yaml`).
        #[arg(short, long, value_name = "ADDON")]
        file: PathBuf,
        /// Architecture(s) whose image sub-manifests to carry,
        /// comma-separated (default: the addon's `spec.compat.arch`, else
        /// this machine's).
        #[arg(long)]
        arch: Option<String>,
        /// Output target: `oci-layout://DIR`, `file://X.tar` or
        /// `file://X.tar.zst`.
        #[arg(short, long)]
        output: String,
    },
    /// One target OS's dependency-package repository (04 §3.4, WP-38):
    /// downloaded inside a container of that distro (docker/podman)
    /// with its repository metadata, for air-gapped system packages.
    Os {
        /// Target OS: one of ubuntu-24.04, ubuntu-22.04, debian-12,
        /// rocky-9, rhel-9, almalinux-9, openeuler-24.03, kylin-10
        /// (kylin-10 needs a locally pinned container image).
        #[arg(long)]
        target: String,
        /// Architecture(s) to build for, comma-separated (default: this
        /// machine's). arm64 downloads run under qemu.
        #[arg(long)]
        arch: Option<String>,
        /// Package version (default: the current month, e.g. 2026.10).
        #[arg(long)]
        version: Option<String>,
        /// Output target: `oci-layout://DIR`, `file://X.tar` or
        /// `file://X.tar.zst`.
        #[arg(short, long)]
        output: String,
    },
}

/// Output format of `kurin package list`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ListFormat {
    /// One `sha256:<digest> <size>` line per blob, with a summary head.
    Text,
    /// A JSON array of `{digest, size}` objects with a `total`.
    Json,
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
        /// Offline source: a legacy bundle directory, or a package
        /// (`file://X.tar.zst`, `oci-layout://DIR`) — equivalent to
        /// `--package` (WP-34).
        #[arg(long, value_name = "DIR|URI")]
        offline: Option<String>,
        /// Nodes to append to the config before joining them (WP-27).
        #[command(flatten)]
        quick: quick::AddNodeArgs,
    },
}

#[derive(clap::Args)]
struct DeleteArgs {
    #[command(subcommand)]
    what: DeleteWhat,
}

#[derive(Subcommand)]
enum AddonWhat {
    /// List catalog addons and the cluster's installed inventory.
    List(addon::ListArgs),
    /// Show one addon's metadata, or its values JSON Schema (`--schema`).
    Info(addon::InfoArgs),
    /// Install (or re-converge) one addon and enable it in the config.
    Install(addon::InstallArgs),
    /// Upgrade one addon to a version the catalog provides.
    Upgrade(addon::UpgradeArgs),
    /// Uninstall one addon and disable it in the config.
    Remove(addon::RemoveArgs),
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
        /// Config file (.toml or .yaml). Optional when quick-create
        /// arguments are given; then it is the base being overridden.
        #[arg(short, long)]
        file: Option<PathBuf>,
        /// Print the plan without connecting or changing anything.
        #[arg(long)]
        dry_run: bool,
        /// Additionally probe live nodes, annotating would-change/unchanged.
        #[arg(long)]
        check: bool,
        /// Resume from the run-state file (skips already-done tasks).
        #[arg(long)]
        resume: bool,
        /// Quick-create parameters: build the config from the command line.
        #[command(flatten)]
        quick: QuickArgs,
    },
    /// Print an annotated example config, or the config the quick-create
    /// arguments describe.
    Config {
        /// Config file (.toml or .yaml) the quick arguments override.
        #[arg(short, long)]
        file: Option<PathBuf>,
        /// Schema version of the printed example (default: v1alpha2).
        #[arg(long, value_enum, default_value_t = SchemaVersion::V1Alpha2)]
        schema_version: SchemaVersion,
        /// Print the pinned component versions this build would install.
        #[arg(long)]
        print_versions: bool,
        /// Quick-create parameters: print the merged v1alpha2 config.
        #[command(flatten)]
        quick: QuickArgs,
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

/// `kurin import --from-cluster` options (WP-73).
#[derive(clap::Args)]
struct ImportArgs {
    /// Reconstruct from a running cluster's kurin-state record.
    #[arg(long, required = true)]
    from_cluster: bool,
    /// Control-plane node to read the state through.
    #[arg(long)]
    host: IpAddr,
    /// SSH login user on the host.
    #[arg(long, default_value = "root")]
    user: String,
    /// SSH port on the host.
    #[arg(long, default_value_t = 22)]
    port: u16,
    /// Authenticate with this private key; also written into [ssh.auth].
    #[arg(long, value_name = "PATH", conflicts_with = "password_env")]
    key: Option<PathBuf>,
    /// Authenticate with this environment variable's value; written into
    /// [ssh.auth] (and every other redacted field) as a reference.
    #[arg(long, value_name = "NAME")]
    password_env: Option<String>,
    /// Host-key policy for the import connection: tofu (default),
    /// strict or accept-new.
    #[arg(long, value_name = "MODE", value_enum)]
    known_hosts: Option<KnownHostsArg>,
    /// Write the reconstructed config here (mode 0600) instead of
    /// stdout.
    #[arg(short, long, value_name = "FILE")]
    output: Option<PathBuf>,
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
    /// Global cap on concurrently executing operations (default: 2).
    #[arg(long, value_name = "N", default_value_t = 2)]
    max_concurrent_ops: usize,
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
    let settings = kurin::run::RunSettings {
        max_upload_parallel: cli.max_upload_parallel,
        ..Default::default()
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
                    settings: settings.clone(),
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
                    addons,
                    output,
                } => package_create_base(kubernetes, distribution, arch, cni, addons, output).await,
                PackageCreateWhat::Os {
                    target,
                    arch,
                    version,
                    output,
                } => package_create_os(target, arch, version, output).await,
                PackageCreateWhat::Addon { file, arch, output } => {
                    package_create_addon(file, arch, output).await
                }
            },
            PackageWhat::Inspect {
                source,
                manifest,
                sbom,
            } => package_inspect(&source, manifest, sbom).await,
            PackageWhat::Verify {
                source,
                keys,
                user,
                password_env,
            } => {
                let source = kurin::transfer::PackageRef::parse(&source)?;
                let policy = match kurin::transfer::VerifyPolicy::signature(&keys)? {
                    Some(policy) => policy,
                    None => kurin::transfer::VerifyPolicy::Digest,
                };
                let opts = transfer_options(user, password_env);
                let report = kurin::transfer::verify_package(&source, &opts, &policy)
                    .await
                    .map_err(|e| miette::miette!("{e:?}"))?;
                let signed = if report.signatures > 0 {
                    format!("{} signature(s) verified", report.signatures)
                } else {
                    "no signature required (digest only)".to_string()
                };
                println!(
                    "verified {} ({})",
                    report.index,
                    report.tag.as_deref().unwrap_or("no tag")
                );
                println!(
                    "  {} manifest(s), {} blob(s), {} bytes re-hashed; {signed}",
                    report.manifests, report.blobs, report.bytes
                );
                Ok(())
            }
            PackageWhat::Push {
                source,
                destination,
                user,
                password_env,
            } => package_transfer(&source, &destination, user, password_env, "push").await,
            PackageWhat::Pull {
                source,
                destination,
                user,
                password_env,
            } => package_transfer(&source, &destination, user, password_env, "pull").await,
            PackageWhat::List { format } => package_list(format),
            PackageWhat::Prune { dry_run } => package_prune(dry_run),
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
                        settings: settings.clone(),
                        kind: kurin::plan::OpKind::CertsRenew,
                        spec,
                        params: Params::CertsRenew { force },
                    },
                    out,
                )
                .await
            }
            CertsWhat::Schedule {
                file,
                warn_days,
                every,
            } => {
                let spec = ops::load_config(&file)?;
                ops::certs_schedule(&spec, warn_days, &every, out).await?;
                Ok(())
            }
        },
        Commands::Etcd { what } => match what {
            EtcdWhat::Backup { file, output } => {
                let spec = ops::load_config(&file)?;
                ops::etcd_backup(&spec, output.as_deref(), out).await?;
                Ok(())
            }
            EtcdWhat::Restore { file, from } => {
                let spec = ops::load_config(&file)?;
                ops::etcd_restore(&spec, &from, out).await?;
                Ok(())
            }
            EtcdWhat::Schedule { file, every, keep } => {
                let spec = ops::load_config(&file)?;
                ops::etcd_schedule(&spec, &every, keep, out).await?;
                Ok(())
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
            AddWhat::Nodes {
                file,
                offline,
                quick: node_args,
            } => {
                // With quick node arguments the config file is the desired
                // state: append the nodes (comments preserved) and run the
                // same add-nodes operation. `--no-save` merges in memory.
                let spec = if node_args.is_empty() {
                    ops::load_config(&file)?
                } else {
                    quick::append_nodes(&file, &node_args)?.spec
                };
                run_op(
                    OpRequest {
                        settings: settings.clone(),
                        kind: kurin::plan::OpKind::AddNodes,
                        spec,
                        params: Params::AddNodes { offline },
                    },
                    out,
                )
                .await
            }
        },
        Commands::Addon { what } => match what {
            AddonWhat::List(args) => addon::list(&args).await,
            AddonWhat::Info(args) => addon::info(&args),
            AddonWhat::Install(args) => {
                let change = addon::install(&args)?;
                run_op(
                    OpRequest {
                        settings: settings.clone(),
                        kind: kurin::plan::OpKind::AddonInstall,
                        spec: change.scoped.clone(),
                        params: Params::AddonInstall {
                            dry_run: args.dry_run,
                        },
                    },
                    out,
                )
                .await?;
                change.commit(&args.file)
            }
            AddonWhat::Upgrade(args) => {
                let change = addon::upgrade(&args)?;
                run_op(
                    OpRequest {
                        settings: settings.clone(),
                        kind: kurin::plan::OpKind::AddonInstall,
                        spec: change.scoped.clone(),
                        params: Params::AddonInstall { dry_run: false },
                    },
                    out,
                )
                .await?;
                change.commit(&args.file)
            }
            AddonWhat::Remove(args) => {
                let change = addon::remove(&args)?;
                run_op(
                    OpRequest {
                        settings: settings.clone(),
                        kind: kurin::plan::OpKind::AddonRemove,
                        spec: change.scoped.clone(),
                        params: Params::AddonRemove {
                            dry_run: false,
                            keep_crds: args.keep_crds,
                        },
                    },
                    out,
                )
                .await?;
                change.commit(&args.file)
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
                        settings: settings.clone(),
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
                        settings: settings.clone(),
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
                file,
                schema_version,
                print_versions,
                quick: quick_args,
            } => {
                if print_versions {
                    for (name, v) in kurin::version::component_versions() {
                        println!("{name}: {v}");
                    }
                    return Ok(());
                }
                if quick_args.is_empty() && file.is_none() {
                    let example = match schema_version {
                        SchemaVersion::V1Alpha1 => EXAMPLE_CONFIG_V1ALPHA1,
                        SchemaVersion::V1Alpha2 => EXAMPLE_CONFIG_V1ALPHA2,
                    };
                    print!("{example}");
                    return Ok(());
                }
                // `create config` prints to stdout (the documented usage is
                // `... > prod.kurin.toml`); `--save` additionally writes it.
                let config =
                    quick::build(file.as_deref(), &quick_args, SaveDefault::Stdout, false)?;
                for warning in &config.warnings {
                    eprintln!("warn: {warning}");
                }
                print!("{}", config.document);
                if let Some(path) = &config.save_path {
                    quick::save_config(path, &config.document, config.force_save)?;
                    eprintln!("[config] wrote {}", path.display());
                }
                Ok(())
            }
            CreateWhat::Cluster {
                file,
                dry_run,
                check,
                resume,
                quick: quick_args,
            } => {
                if quick_args.is_empty() {
                    let Some(file) = file else {
                        return Err(miette::miette!(
                            "`create cluster` needs -f <config> or quick-create arguments \
                             (--name, --control-plane, --worker, …)"
                        ));
                    };
                    let spec = ops::load_config(&file)?;
                    return run_op(
                        OpRequest {
                            settings: settings.clone(),
                            kind: kurin::plan::OpKind::Create,
                            spec,
                            params: Params::Create {
                                dry_run,
                                check,
                                resume,
                                // `--offline DIR|URI`: payload resolution
                                // classifies it (WP-34) — a URI behaves
                                // exactly like `--package`.
                                offline: quick_args.offline.clone(),
                            },
                        },
                        out,
                    )
                    .await;
                }
                let config =
                    quick::build(file.as_deref(), &quick_args, SaveDefault::File, !dry_run)?;
                for warning in &config.warnings {
                    eprintln!("warn: {warning}");
                }
                run_quick(
                    OpRequest {
                        settings: settings.clone(),
                        kind: kurin::plan::OpKind::Create,
                        spec: config.spec.clone(),
                        params: Params::Create {
                            dry_run,
                            check,
                            resume,
                            offline: None,
                        },
                    },
                    out,
                    config,
                )
                .await
            }
        },
        Commands::Import(args) => run_import(args, out).await,
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
            max_concurrent_ops: args.max_concurrent_ops,
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

/// Quick-create pipeline: prepare (discover) -> summary -> confirm -> save
/// -> plan -> execute (`05-cli-quick-create.md` §4).
///
/// The plan and execution are exactly `create cluster -f`'s; only the
/// summary, the confirmation and the config write are added.
async fn run_quick(req: OpRequest, out: OutputOpts, mut config: QuickConfig) -> Result<()> {
    let mut req = req;

    // `--setup-ssh-key` (05 §4) runs before discover: the password session
    // pushes the key, the spec and the saved document switch to key auth,
    // and the private key's login is verified before anything else
    // connects.
    if let Some(public) = config.setup_ssh_key.clone() {
        let passphrase = config
            .setup_passphrase_env
            .as_deref()
            .and_then(|var| std::env::var(var).ok());
        let key = cli::ssh_key::resolve_control_key(Some(&public), passphrase.as_deref())?;
        let report = cli::ssh_key::setup(
            &mut req.spec,
            &mut config.document,
            &key,
            config.setup_passphrase_env.as_deref(),
            setup_pool,
        )
        .await?;
        for node in &report {
            eprintln!(
                "[ssh-key] {}: {}",
                node.node,
                match node.outcome {
                    cli::ssh_key::PushOutcome::Appended => "authorized key appended",
                    cli::ssh_key::PushOutcome::AlreadyPresent => {
                        "authorized key already present"
                    }
                }
            );
        }
        eprintln!(
            "[ssh-key] private key {} verified on {} node(s); the saved config uses \
             private-key auth",
            key.key_path.display(),
            report.len()
        );
    }

    let mut prepared = ops::prepare(req, out).await?;

    // `--node-name hostname` can only be honoured once discover reported
    // the hostnames; the provisional role-index names are what the plan
    // would otherwise use.
    if config.node_name == NodeNameStrategy::Hostname {
        let (renames, warnings) = quick::hostname_renames(&prepared.spec, &prepared.facts);
        for warning in warnings {
            eprintln!("warn: {warning}");
        }
        if !renames.is_empty() {
            quick::apply_renames(
                &mut prepared.spec,
                &mut prepared.facts,
                &mut config.document,
                &renames,
            );
            prepared.run.set_facts(prepared.facts.clone());
        }
    }

    if !out.json_enabled() {
        print!(
            "{}",
            quick::render_summary(
                &prepared.spec,
                &prepared.facts,
                config.save_path.as_deref(),
                prepared.params.dry_run(),
            )
        );
    }

    if !prepared.params.dry_run() {
        if !config.yes {
            if !out.json_enabled() {
                print!("proceed? [y/N] ");
                std::io::Write::flush(&mut std::io::stdout()).into_diagnostic()?;
            }
            let is_tty = std::io::stdin().is_terminal();
            let answer = if is_tty {
                let mut line = String::new();
                std::io::BufRead::read_line(&mut std::io::stdin().lock(), &mut line)
                    .into_diagnostic()?;
                Some(line)
            } else {
                None
            };
            if !quick::confirm_proceed(false, is_tty, answer.as_deref())? {
                return Err(miette::miette!("aborted by user"));
            }
        }
        if let Some(path) = &config.save_path {
            match quick::save_config(path, &config.document, config.force_save)? {
                SaveOutcome::Written => eprintln!(
                    "[config] wrote {} (secrets referenced, not stored)",
                    path.display()
                ),
                SaveOutcome::Unchanged => {
                    eprintln!("[config] {} is already up to date", path.display())
                }
            }
        }
    }

    let plan = ops::plan(&prepared).await?;
    ops::execute(prepared, plan).await?;
    Ok(())
}

/// Session pool for the `--setup-ssh-key` phases: resolve the spec's
/// secrets and connect per node, exactly like `ops::prepare` does.
fn setup_pool(
    spec: &kurin_config::Cluster,
) -> Result<std::sync::Arc<dyn kurin::executor::SessionPool>> {
    let secrets = spec.resolve_secrets()?;
    Ok(std::sync::Arc::new(
        kurin::ssh::SshSessionPool::from_cluster(spec, &secrets)?,
    ))
}

/// Prepare -> plan -> execute, the whole operation pipeline.
async fn run_op(req: OpRequest, out: OutputOpts) -> Result<()> {
    let prepared = ops::prepare(req, out).await?;
    let plan = ops::plan(&prepared).await?;
    ops::execute(prepared, plan).await?;
    Ok(())
}

/// `kurin import --from-cluster` (WP-73): one SSH session to a
/// control-plane node, the state record, and a re-credentialed config.
async fn run_import(args: ImportArgs, out: OutputOpts) -> Result<()> {
    let auth = match (&args.key, &args.password_env) {
        (Some(path), None) => SshAuth::PrivateKey {
            key_path: path.clone(),
            passphrase: None,
        },
        (None, Some(env)) => SshAuth::Password {
            password: SecretRef::Env { env: env.clone() },
        },
        _ => {
            return Err(miette::miette!(
                "`kurin import` needs exactly one of --key PATH or --password-env NAME"
            ))
        }
    };
    let known_hosts = match args.known_hosts {
        Some(KnownHostsArg::Strict) => HostKeyMode::Strict,
        Some(KnownHostsArg::AcceptNew) => HostKeyMode::AcceptNew,
        Some(KnownHostsArg::Tofu) | None => HostKeyMode::Tofu,
    };
    ops::import(
        &ops::ImportOptions {
            host: args.host,
            user: args.user,
            port: args.port,
            auth,
            known_hosts,
            output: args.output,
        },
        out,
    )
    .await?;
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

/// Parse `--arch amd64,arm64`. `None` returns an empty list; the caller
/// decides the default (host arch for a base package, the addon's own
/// compatibility list for an addon package).
fn parse_arch_list(arch: Option<&str>) -> Result<Vec<kurin::arch::Arch>> {
    let Some(list) = arch else {
        return Ok(Vec::new());
    };
    let mut arches = list
        .split(',')
        .map(|a| a.trim())
        .filter(|a| !a.is_empty())
        .map(|a| match a {
            "amd64" | "x86_64" => Ok(kurin::arch::Arch::Amd64),
            "arm64" | "aarch64" => Ok(kurin::arch::Arch::Arm64),
            other => Err(miette::miette!("unknown arch {other:?} (amd64|arm64)")),
        })
        .collect::<miette::Result<Vec<_>>>()?;
    arches.dedup();
    Ok(arches)
}

async fn package_create_base(
    kubernetes: Option<String>,
    distribution: Option<String>,
    arch: Option<String>,
    cni: Option<String>,
    addons: Option<String>,
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
    let mut arches = parse_arch_list(arch.as_deref())?;
    if arches.is_empty() {
        arches.push(kurin::arch::Arch::host()?);
    }

    let out = kurin::package::PackageOutput::parse(&output)?;
    let store = kurin::artifact::ArtifactStore::open_default()?;

    // `--addons a,b` embeds the selected addons (WP-45): their files
    // under `addons/<name>/`, their chart payloads and their images.
    let extras = match addons.as_deref() {
        Some(list) => addon_extras_for(list, &store, zone, &arches).await?,
        None => Vec::new(),
    };

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
        extras,
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

/// `kurin package create os --target … --arch …` (WP-38, 04 §3.4).
async fn package_create_os(
    target: String,
    arch: Option<String>,
    version: Option<String>,
    output: String,
) -> Result<()> {
    use kurin::os as os_pkg;

    let target = os_pkg::OsTarget::parse(&target).map_err(|e| miette::miette!("{e:?}"))?;
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
    let opts = os_pkg::OsPackage {
        target,
        arches,
        version: version.unwrap_or_else(os_pkg::calver),
    };
    let engine = os_pkg::detect_container_engine()
        .await
        .map_err(|e| miette::miette!("{e:?}"))?;
    eprintln!("[package] downloading closure via {engine}");
    let report = os_pkg::create_os(&store, &opts, &out)
        .await
        .map_err(|e| miette::miette!("{e:?}"))?;
    eprintln!(
        "package os {} [{}] written to {}",
        report.tag,
        opts.target.label(),
        report.output.display(),
    );
    eprintln!("index: {}", report.index);
    eprintln!("sbom:  {}", report.sbom.display());
    Ok(())
}

/// Resolve `--addons` entries and build their package payloads.
async fn addon_extras_for(
    list: &str,
    store: &kurin::artifact::ArtifactStore,
    zone: kurin::zone::Zone,
    arches: &[kurin::arch::Arch],
) -> Result<Vec<kurin::package::PackageExtras>> {
    let mut catalog = kurin::addon::Catalog::embedded()?;
    let mut extras = Vec::new();
    for entry in list.split(',').map(str::trim).filter(|e| !e.is_empty()) {
        let addon = load_addon_entry(&mut catalog, entry)?;
        let diagnostics = addon.validate();
        if !diagnostics.is_empty() {
            let lines = diagnostics
                .iter()
                .map(|d| format!("  - {}", d.summary()))
                .collect::<Vec<_>>()
                .join("\n");
            return Err(miette::miette!(
                "addon `{}` is not packageable:\n{lines}",
                addon.name()
            ));
        }
        let prefix = format!("addons/{}/", addon.name());
        extras.push(kurin::addon::addon_extras(store, &addon, arches, zone, &prefix).await?);
    }
    Ok(extras)
}

/// One `--addons` entry: a built-in catalog name, a directory, an
/// `addon.yaml` path, or an addon package source.
fn load_addon_entry(
    catalog: &mut kurin::addon::Catalog,
    entry: &str,
) -> Result<kurin::addon::Addon> {
    if entry.contains("://") || Path::new(entry).exists() {
        let path = Path::new(entry);
        if path.is_file()
            && path
                .file_name()
                .is_some_and(|name| name == kurin::addon::ADDON_FILE)
        {
            return Ok(kurin::addon::Addon::from_dir(
                path.parent().unwrap_or_else(|| Path::new(".")),
            )?);
        }
        return Ok(kurin::addon::Addon::from_source(entry)?);
    }
    catalog.take(entry).ok_or_else(|| {
        miette::miette!(
            "addon `{entry}` is not in the built-in catalog; pass a directory, an addon.yaml \
             or an addon package source"
        )
    })
}

/// `kurin package create addon -f addons/<name>/addon.yaml` (04 §3.4,
/// WP-45): one addon as an offline-installable OCI package.
async fn package_create_addon(file: PathBuf, arch: Option<String>, output: String) -> Result<()> {
    let zone = resolve_zone(None)?;
    let out = kurin::package::PackageOutput::parse(&output)?;
    let store = kurin::artifact::ArtifactStore::open_default()?;
    let opts = kurin::addon::AddonPackageOptions {
        arches: parse_arch_list(arch.as_deref())?,
    };
    let source = file.display().to_string();
    let report = kurin::addon::create_addon(&store, &source, &opts, zone, &out)
        .await
        .map_err(|e| miette::miette!("{e:?}"))?;
    eprintln!(
        "package addon {} written to {} ({} file(s), {} image(s))",
        report.tag,
        report.output.display(),
        report.files,
        report.images
    );
    eprintln!("index: {}", report.index);
    eprintln!("sbom:  {}", report.sbom.display());
    Ok(())
}

/// Shared credentials for `push` / `pull` / `verify` / registry
/// `inspect` (04 §4: `--user` + `--password-env`, else the Docker
/// config, else anonymous).
fn transfer_options(
    user: Option<String>,
    password_env: Option<String>,
) -> kurin::transfer::TransferOptions {
    kurin::transfer::TransferOptions { user, password_env }
}

/// `kurin package push/pull src dst` — one transfer, one report
/// (04 §3.3). The two verbs are spelling: any source may copy to any
/// destination the CLI accepts.
async fn package_transfer(
    source: &str,
    destination: &str,
    user: Option<String>,
    password_env: Option<String>,
    verb: &str,
) -> Result<()> {
    let source = kurin::transfer::PackageRef::parse(source)?;
    let destination = kurin::transfer::PackageRef::parse(destination)?;
    if matches!(destination, kurin::transfer::PackageRef::Registry(_)) && verb == "pull" {
        return Err(miette::miette!(
            "`pull` writes to a local destination (oci-layout:// or file://); use `push` to upload"
        ));
    }
    let opts = transfer_options(user, password_env);
    let report = kurin::transfer::transfer(&source, &destination, &opts)
        .await
        .map_err(|e| miette::miette!("{e:?}"))?;
    let tag = report.tag.as_deref().unwrap_or("-");
    match &report.output {
        Some(output) => println!(
            "package {verb} [{tag}] -> {} ({} of {} blob(s), {} sent; {} of {} manifest(s) sent)",
            output.display(),
            report.counts.blobs_moved,
            report.counts.blobs_total(),
            humansize(report.counts.bytes_moved),
            report.counts.manifests_moved,
            report.counts.manifests_total(),
        ),
        None => println!(
            "package {verb} to {destination} [{tag}]: {} of {} blob(s) already present, \
             {} sent ({}); {} of {} manifest(s) sent",
            report.counts.blobs_present,
            report.counts.blobs_total(),
            report.counts.blobs_moved,
            humansize(report.counts.bytes_moved),
            report.counts.manifests_moved,
            report.counts.manifests_total(),
        ),
    }
    println!("index: {}", report.index);
    Ok(())
}

/// `kurin package inspect [--manifest] [--sbom]`. Registry sources are
/// pulled (and thereby verified) into a scratch layout first.
async fn package_inspect(source: &str, manifest: bool, sbom: bool) -> Result<()> {
    let parsed = kurin::transfer::PackageRef::parse(source)?;
    let (tag, handle) = match &parsed {
        kurin::transfer::PackageRef::Registry(reg) => {
            let auth = kurin::oci::resolve_registry_auth(None, None, reg.registry())
                .map_err(|e| miette::miette!("{e:?}"))?;
            let temp = tempfile::TempDir::new().into_diagnostic()?;
            let layout = kurin_artifact::oci::Layout::create(temp.path())
                .map_err(|e| miette::miette!("{e:?}"))?;
            let outcome = kurin_artifact::oci::remote::pull_package(reg, &layout, &auth)
                .await
                .map_err(|e| miette::miette!("{e:?}"))?;
            let handle = kurin::package::PackageSource::from_layout(layout, Some(temp));
            (outcome.tag, handle)
        }
        _ => (None, kurin::package::PackageSource::open(source)?),
    };
    if let Some(tag) = &tag {
        println!("tag: {tag}");
    }
    if sbom {
        // The package's SPDX document (04 §6): the referrer attached to
        // the package index (WP-39), rendered from the verified
        // metadata for packages built before referrers existed.
        if let Some(bytes) = handle.sbom().map_err(|e| miette::miette!("{e:?}"))? {
            println!("{}", String::from_utf8_lossy(&bytes));
        } else {
            let meta = handle.manifest()?;
            let index = handle.index_digest()?;
            let document = kurin::package::spdx::Document::from_package(
                &meta,
                &index,
                &format!("kurin-{}", env!("CARGO_PKG_VERSION")),
            );
            println!(
                "{}",
                serde_json::to_string_pretty(&document).into_diagnostic()?
            );
        }
    } else if manifest {
        let meta = handle.manifest()?;
        println!("{}", serde_json::to_string_pretty(&meta).into_diagnostic()?);
    } else {
        print!("{}", kurin::package::inspect_summary(&handle)?);
    }
    Ok(())
}

/// `kurin package list [--format text|json]`.
fn package_list(format: ListFormat) -> Result<()> {
    let store = kurin::artifact::ArtifactStore::open_default()?;
    let entries = kurin::transfer::list_cache(&store).map_err(|e| miette::miette!("{e:?}"))?;
    let bytes: u64 = entries.iter().map(|e| e.size).sum();
    if format == ListFormat::Json {
        let value = serde_json::json!({
            "blobs": entries
                .iter()
                .map(|e| serde_json::json!({ "digest": e.digest, "size": e.size }))
                .collect::<Vec<_>>(),
            "total": bytes,
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&value).into_diagnostic()?
        );
    } else {
        println!(
            "{} blob(s), {} in {}",
            entries.len(),
            humansize(bytes),
            store.blob_store().root().display()
        );
        for entry in &entries {
            println!("{} {}", entry.digest, humansize(entry.size));
        }
    }
    Ok(())
}

/// `kurin package prune [--dry-run]`.
fn package_prune(dry_run: bool) -> Result<()> {
    let store = kurin::artifact::ArtifactStore::open_default()?;
    let report = kurin::transfer::prune_cache(&store).map_err(|e| miette::miette!("{e:?}"))?;
    let head = if dry_run {
        "prune --dry-run: would "
    } else {
        "prune: "
    };
    println!(
        "{head}kept {} blob(s), removed {} blob(s) ({})",
        report.kept,
        report.removed,
        humansize(report.bytes_removed)
    );
    Ok(())
}

/// Human-readable byte counts for CLI reports.
fn humansize(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

const EXAMPLE_CONFIG_V1ALPHA1: &str = include_str!("../examples/cluster.example.toml");
const EXAMPLE_CONFIG_V1ALPHA2: &str = include_str!("../examples/cluster.example.v1alpha2.toml");
