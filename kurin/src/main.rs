use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use miette::{IntoDiagnostic, Result};

#[derive(Parser)]
#[command(name = "kurin", about = "Declarative Kubernetes cluster installer", version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Create resources.
    Create(CreateArgs),
    /// Add nodes from an updated config to an existing cluster.
    Add(AddArgs),
    /// Delete a node or the whole cluster.
    Delete(DeleteArgs),
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
    },
    /// Print an annotated example config.
    Config {
        /// Print the pinned component versions this build would install.
        #[arg(long)]
        print_versions: bool,
    },
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
        // `load`'s ConfigError is Diagnostic, so `?` keeps the fancy render.
    }
}

async fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Commands::Add(args) => match args.what {
            AddWhat::Nodes { file, json } => add_nodes(file, json).await,
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
            CreateWhat::Cluster { file, dry_run, check, json, resume } => {
                let cluster = kurin_config::load(&file)?;
                let cp = cluster.control_plane().count();
                if cp % 2 == 0 && cp > 1 && !json {
                    eprintln!("warn: even control-plane count ({cp}); etcd quorum prefers odd");
                }
                let mut plan = kurin::Plan::build(&cluster);

                let pool: std::sync::Arc<dyn kurin::SessionPool> =
                    std::sync::Arc::new(kurin::ssh::SshSessionPool::from_cluster(&cluster)?);
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

                let mut opts = kurin::executor::RunOptions::default()
                    .with_default_state(&cluster.name);
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


async fn add_nodes(file: PathBuf, json: bool) -> Result<()> {
    let cluster = kurin_config::load(&file)?;
    let pool: std::sync::Arc<dyn kurin::SessionPool> =
        std::sync::Arc::new(kurin::ssh::SshSessionPool::from_cluster(&cluster)?);

    // Fresh join material: the certificate key from the original install is
    // only valid for 2h, and a new CP member needs a usable one.
    if let Some(first) = cluster.control_plane().next() {
        let s = pool.get(&first.name).await?;
        // If the cluster was initialized by this kurin (admin.conf present),
        // refresh the token + certificate key.
        let init = s.exec("test -f /etc/kubernetes/admin.conf").await?;
        if init.ok() {
            kurin::tasks::bootstrap::refresh_join_material(s.as_ref(), &cluster.name)
                .await
                .map_err(|e| miette::miette!("refresh join material on {}: {e}", first.name))?;
        }
    }

    // Same plan/executor as create: converged nodes skip, new nodes install.
    let mut plan = kurin::Plan::build(&cluster);
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
    kurin::teardown::delete_node(&pool, node, api_session.as_ref()).await
}

async fn delete_cluster_cmd(file: PathBuf) -> Result<()> {
    let cluster = kurin_config::load(&file)?;
    let pool: std::sync::Arc<dyn kurin::SessionPool> =
        std::sync::Arc::new(kurin::ssh::SshSessionPool::from_cluster(&cluster)?);
    kurin::teardown::delete_cluster(&pool, &cluster).await
}

const EXAMPLE_CONFIG: &str = include_str!("../examples/cluster.example.toml");