use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use miette::{IntoDiagnostic, Result};

#[derive(Parser)]
#[command(
    name = "kurin",
    about = "Declarative Kubernetes cluster installer",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
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
    /// Probe cluster health (nodes, pods, etcd, certificates).
    Status {
        /// Config file (.toml or .yaml).
        #[arg(short, long)]
        file: PathBuf,
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
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
    },
    /// Build an offline artifact bundle for air-gapped installs.
    Artifact {
        /// Output directory (created if missing).
        #[arg(short, long)]
        out: PathBuf,
        /// Architecture to build for (default: this machine's).
        #[arg(long)]
        arch: Option<String>,
        /// Kubernetes version to pin binaries and control-plane images for
        /// (writes kurin-manifest.json; default: the compiled-in table).
        #[arg(long)]
        with_kubernetes: Option<String>,
    },
    /// Add nodes from an updated config to an existing cluster.
    Add(AddArgs),
    /// Delete a node or the whole cluster.
    Delete(DeleteArgs),
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
        /// Emit machine-readable JSON events.
        #[arg(long)]
        json: bool,
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
    },
    /// Remove every node and destroy the cluster.
    Cluster {
        #[arg(short, long)]
        file: PathBuf,
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
        /// Emit machine-readable JSON events.
        #[arg(long)]
        json: bool,
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

/// Resolve the download zone: `KURIN_ZONE` env wins over config's
/// `[artifacts] zone`. Must be called before any artifact fetch.
fn init_zone(config_zone: Option<&str>) -> Result<()> {
    let zone_str = std::env::var("KURIN_ZONE")
        .ok()
        .filter(|z| !z.is_empty())
        .or_else(|| config_zone.map(str::to_string))
        .unwrap_or_else(|| "global".to_string());
    let zone = kurin::zone::Zone::parse(&zone_str)
        .ok_or_else(|| miette::miette!(r#"invalid zone {zone_str:?} (expected "cn")"#))?;
    kurin::artifact::set_zone(zone);
    if zone == kurin::zone::Zone::Cn {
        eprintln!("[zone] artifact downloads via domestic mirrors");
    }
    Ok(())
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
    match cli.command {
        Commands::Upgrade {
            file,
            with_kubernetes,
            dry_run,
            batch,
            offline,
        } => {
            let cluster = kurin_config::load(&file)?;
            let target = with_kubernetes
                .as_deref()
                .map(|v| v.trim_start_matches('v'))
                .map(semver::Version::parse)
                .transpose()
                .map_err(|e| miette::miette!("invalid --with-kubernetes: {e}"))?
                .unwrap_or_else(|| cluster.kubernetes_version.clone());
            let pool: std::sync::Arc<dyn kurin::SessionPool> =
                std::sync::Arc::new(kurin::ssh::SshSessionPool::from_cluster(&cluster)?);
            if dry_run {
                kurin::upgrade::plan(&cluster, &target, &pool).await
            } else if let Some(offline) = offline {
                kurin::upgrade::run_offline(&cluster, &target, &offline, &pool, batch).await
            } else {
                kurin::upgrade::run(&cluster, &target, &pool, batch).await
            }
        }
        Commands::Artifact {
            out,
            arch,
            with_kubernetes,
        } => build_artifacts(out, arch, with_kubernetes).await,
        Commands::Certs { what } => match what {
            CertsWhat::Check { file, warn_days } => {
                let cluster = kurin_config::load(&file)?;
                let pool: std::sync::Arc<dyn kurin::SessionPool> =
                    std::sync::Arc::new(kurin::ssh::SshSessionPool::from_cluster(&cluster)?);
                let reports = kurin::certs::check(&cluster, &pool, warn_days).await?;
                kurin::certs::render(&reports, warn_days);
                Ok(())
            }
            CertsWhat::Renew { file, force } => {
                let cluster = kurin_config::load(&file)?;
                let pool: std::sync::Arc<dyn kurin::SessionPool> =
                    std::sync::Arc::new(kurin::ssh::SshSessionPool::from_cluster(&cluster)?);
                kurin::certs::renew(&cluster, &pool, force).await
            }
        },
        Commands::Status { file, json } => {
            let cluster = kurin_config::load(&file)?;
            let pool: std::sync::Arc<dyn kurin::SessionPool> =
                std::sync::Arc::new(kurin::ssh::SshSessionPool::from_cluster(&cluster)?);
            let report = kurin::status::collect(&cluster, &pool).await?;
            if json {
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
        Commands::Add(args) => match args.what {
            AddWhat::Nodes {
                file,
                json,
                offline,
            } => add_nodes(file, json, offline).await,
        },
        Commands::Delete(args) => match args.what {
            DeleteWhat::Node { name, file } => delete_node_cmd(name, file).await,
            DeleteWhat::Cluster { file } => delete_cluster_cmd(file).await,
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
                json,
                resume,
                offline,
            } => {
                let mut cluster = kurin_config::load(&file)?;
                init_zone(cluster.zone.as_deref())?;
                if let Some(dir) = offline {
                    cluster.offline_dir = Some(dir);
                }
                let cp = cluster.control_plane().count();
                if cp % 2 == 0 && cp > 1 && !json {
                    eprintln!("warn: even control-plane count ({cp}); etcd quorum prefers odd");
                }
                if cp > 1 && cluster.vip.is_none() && !json {
                    eprintln!("warn: {cp} control-plane nodes without ha.vip: the first master becomes the API single point of failure");
                }
                let pool: std::sync::Arc<dyn kurin::SessionPool> =
                    std::sync::Arc::new(kurin::ssh::SshSessionPool::from_cluster(&cluster)?);
                let mut plan = kurin::Plan::build_with_pool(&cluster, Some(pool.clone()))?;
                if check {
                    kurin::executor::probe(&mut plan, &pool).await?;
                }
                if dry_run || !check {
                    let tree = kurin::render::render_tree(&plan);
                    if json {
                        let plan = serde_json::json!({ "plan": tree });
                        println!("{}", serde_json::to_string_pretty(&plan).into_diagnostic()?);
                    } else {
                        print!("{tree}");
                    }
                }
                if dry_run {
                    return Ok(());
                }

                let mut opts =
                    kurin::executor::RunOptions::default().with_default_state(&cluster.name);
                opts.resume = resume;
                let mut emit = |event: kurin::Event| {
                    if json {
                        println!("{}", kurin::render::event_json_line(&event));
                    } else {
                        use kurin::Event::*;
                        match &event {
                            TaskSkipped { id, .. } => eprintln!("skip  {id}"),
                            TaskDone { id, ms, .. } => eprintln!("done  {id} ({ms}ms)"),
                            TaskReconciled { id, .. } => eprintln!("ok    {id} (reconciled)"),
                            TaskFailed { id, reason, .. } => eprintln!("FAIL  {id}: {reason}"),
                            StageDone { stage } => eprintln!("-- stage {stage} complete"),
                            _ => {}
                        }
                    }
                };
                kurin::executor::run_with(&mut plan, &pool, &mut emit, opts).await?;
                if !json {
                    eprintln!("cluster {} ready", cluster.name);
                }
                Ok(())
            }
        },
    }
}

async fn build_artifacts(
    out: PathBuf,
    arch: Option<String>,
    with_kubernetes: Option<String>,
) -> Result<()> {
    init_zone(None)?;
    let arch = match arch.as_deref() {
        Some("amd64" | "x86_64") => kurin::artifact::Arch::Amd64,
        Some("arm64" | "aarch64") => kurin::artifact::Arch::Arm64,
        Some(other) => return Err(miette::miette!("unknown arch {other:?} (amd64|arm64)")),
        None => match std::env::consts::ARCH {
            "x86_64" => kurin::artifact::Arch::Amd64,
            "aarch64" => kurin::artifact::Arch::Arm64,
            other => return Err(miette::miette!("unsupported host arch {other}")),
        },
    };
    let manifest = match &with_kubernetes {
        Some(v) => {
            let target: semver::Version = v
                .trim_start_matches('v')
                .parse()
                .map_err(|e| miette::miette!("--with-kubernetes {v:?}: {e}"))?;
            kurin::dynamic_manifest::k8s_manifest_for(&target, arch).await?
        }
        None => kurin::artifact::ArtifactManifest::embedded()?,
    };
    let store = kurin::artifact::ArtifactStore::open_default()?;
    std::fs::create_dir_all(&out).into_diagnostic()?;
    for entry in manifest.artifacts() {
        let spec = manifest.spec(&entry.name, arch)?;
        let dir = store.artifact_dir(&spec);
        std::fs::create_dir_all(&dir).into_diagnostic()?;
        let dest = store.fetch(&spec).await?;
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
    // Sidecar checksums are included by the copy above; the dir name layout
    // is exactly what ArtifactManifest::from_dir expects.
    kurin::artifact_bundle::write_bundle_manifest(&out, &manifest)
        .map_err(|e| miette::miette!("bundle manifest: {e}"))?;
    eprintln!("offline bundle written to {}", out.display());
    Ok(())
}

async fn add_nodes(file: PathBuf, json: bool, offline: Option<PathBuf>) -> Result<()> {
    let mut cluster = kurin_config::load(&file)?;
    init_zone(cluster.zone.as_deref())?;
    if let Some(dir) = offline {
        cluster.offline_dir = Some(dir);
    }
    let pool: std::sync::Arc<dyn kurin::SessionPool> =
        std::sync::Arc::new(kurin::ssh::SshSessionPool::from_cluster(&cluster)?);

    if let Some(first) = cluster.control_plane().next() {
        let s = pool.get(&first.name).await?;
        // If the cluster was initialized by this kurin (admin.conf present),
        // refresh the token + certificate key.
        let init = s.exec("test -f /etc/kubernetes/admin.conf").await?;
        if init.ok() {
            // Version threading: a cluster upgraded past the embedded table
            // gets its running version installed on the new nodes via a
            // dynamic manifest (dl.k8s.io checksums + registry digests).
            let out = s
                .exec("kubectl --kubeconfig /etc/kubernetes/admin.conf get --raw /version 2>/dev/null")
                .await?;
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&out.stdout) {
                if let Some(git) = v.get("gitVersion").and_then(|g| g.as_str()) {
                    let server = git.trim_start_matches('v').to_string();
                    let embedded = kurin::version::KUBERNETES_VERSION;
                    if server != embedded {
                        let target: semver::Version = server
                            .parse()
                            .map_err(|e| miette::miette!("server version {server:?}: {e}"))?;
                        let arch = match std::env::consts::ARCH {
                            "x86_64" => kurin::artifact::Arch::Amd64,
                            "aarch64" => kurin::artifact::Arch::Arm64,
                            other => return Err(miette::miette!("unsupported arch {other}")),
                        };
                        let manifest =
                            kurin::dynamic_manifest::k8s_manifest_for_from(&target, &target, arch)
                                .await?;
                        kurin::artifact::set_manifest_override(manifest);
                        eprintln!(
                            "[add-nodes] new nodes install v{server} (dynamic manifest; embedded v{embedded})"
                        );
                    }
                }
            }
            kurin::tasks::bootstrap::refresh_join_material(s.as_ref(), &cluster.name)
                .await
                .map_err(|e| miette::miette!("refresh join material on {}: {e}", first.name))?;
        }
    }

    // Same plan/executor as create: converged nodes skip, new nodes install.
    let mut plan = kurin::Plan::build_with_pool(
        &cluster,
        Some(std::sync::Arc::new(
            kurin::ssh::SshSessionPool::from_cluster(&cluster)?,
        )),
    )?;
    let mut opts = kurin::executor::RunOptions::default().with_default_state(&cluster.name);
    opts.resume = false;
    let mut emit = |event: kurin::Event| {
        if json {
            println!("{}", kurin::render::event_json_line(&event));
        } else {
            use kurin::Event::*;
            match &event {
                TaskSkipped { id, .. } => eprintln!("skip  {id}"),
                TaskDone { id, ms, .. } => eprintln!("done  {id} ({ms}ms)"),
                TaskReconciled { id, .. } => eprintln!("ok    {id} (reconciled)"),
                TaskFailed { id, reason, .. } => eprintln!("FAIL  {id}: {reason}"),
                StageDone { stage } => eprintln!("-- stage {stage} complete"),
                _ => {}
            }
        }
    };
    kurin::executor::run_with(&mut plan, &pool, &mut emit, opts).await?;
    if !json {
        eprintln!("nodes reconciled for cluster {}", cluster.name);
    }
    Ok(())
}

async fn delete_node_cmd(name: String, file: PathBuf) -> Result<()> {
    let cluster = kurin_config::load(&file)?;
    let node = cluster
        .nodes
        .iter()
        .find(|n| n.name == name)
        .ok_or_else(|| miette::miette!("node {name:?} is not declared in {}", file.display()))?;
    let pool: std::sync::Arc<dyn kurin::SessionPool> =
        std::sync::Arc::new(kurin::ssh::SshSessionPool::from_cluster(&cluster)?);
    // API session: first control-plane node that is not the target itself.
    let api = cluster
        .control_plane()
        .find(|n| n.name != name)
        .map(|n| n.name.clone());
    let api_session = match api {
        Some(cp) => match pool.get(&cp).await {
            Ok(s) => Some(s),
            Err(e) => {
                eprintln!("warn: control-plane {cp} unreachable ({e}); deleting without drain");
                None
            }
        },
        None => None,
    };
    kurin::teardown::delete_node(&pool, &cluster, node, api_session.as_ref()).await
}

async fn delete_cluster_cmd(file: PathBuf) -> Result<()> {
    let cluster = kurin_config::load(&file)?;
    let pool: std::sync::Arc<dyn kurin::SessionPool> =
        std::sync::Arc::new(kurin::ssh::SshSessionPool::from_cluster(&cluster)?);
    kurin::teardown::delete_cluster(&pool, &cluster).await
}

const EXAMPLE_CONFIG: &str = include_str!("../examples/cluster.example.toml");
