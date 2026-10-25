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
            CreateWhat::Cluster { file, dry_run, check, json } => {
                let cluster = kurin_config::load(&file)?;
                let warn = cluster.control_plane().count();
                if warn % 2 == 0 && warn > 1 && !json {
                    eprintln!("warn: even control-plane count ({warn}); etcd quorum prefers odd");
                }
                let mut plan = kurin::Plan::build(&cluster);
                if check {
                    // Live probe requires SSH sessions; W1 wires russh.
                    // Dry-run --check stays offline until then.
                }
                if dry_run || !check {
                    let tree = kurin::render::render_tree(&plan);
                    if json {
                        println!("{}", serde_json::to_string_pretty(&serde_json::json!({ "plan": tree })).into_diagnostic()?);
                    } else {
                        print!("{tree}");
                    }
                }
                if !dry_run {
                    return Err(miette::miette!(
                        "execution requires SSH sessions; land --dry-run first (W1 wires russh executor)"
                    ));
                }
                let _ = &mut plan;
                Ok(())
            }
        },
    }
}


const EXAMPLE_CONFIG: &str = include_str!("../examples/cluster.example.toml");