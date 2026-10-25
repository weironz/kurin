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


const EXAMPLE_CONFIG: &str = include_str!("../examples/cluster.example.toml");