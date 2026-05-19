use std::path::PathBuf;

use clap::{Parser, Subcommand};

use rekall_lib::config::BenchmarkConfig;
use rekall_lib::dataset::hdf5::Hdf5Reader;
use rekall_lib::output::json::JsonSink;
use rekall_lib::output::stdout::StdoutSink;
use rekall_lib::output::OutputSink;
use rekall_lib::phase::Phase;
use rekall_lib::Agent;

#[derive(Parser)]
#[command(name = "rekall", about = "Vector search benchmark tool")]
#[command(version)]
struct Cli {
    #[arg(short, long, global = true)]
    verbose: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run a full benchmark (all phases)
    Run {
        /// Path to benchmark config file
        config: PathBuf,

        /// Run only a specific phase
        #[arg(long)]
        phase: Option<String>,

        /// Only process this index
        #[arg(long)]
        index: Option<String>,

        /// Resume an interrupted load
        #[arg(long)]
        resume: bool,

        /// Show what would be executed without running
        #[arg(long)]
        dry_run: bool,
    },

    /// Initialize database and tables
    Init { config: PathBuf },

    /// Load dataset into database
    Load {
        config: PathBuf,

        /// Number of parallel COPY streams
        #[arg(long)]
        parallelism: Option<usize>,

        /// Resume an interrupted load
        #[arg(long)]
        resume: bool,
    },

    /// Build indexes
    Index {
        config: PathBuf,

        /// Build only this index
        #[arg(long)]
        name: Option<String>,
    },

    /// Run search benchmarks
    Search {
        config: PathBuf,

        /// Benchmark only this index
        #[arg(long)]
        index: Option<String>,
    },

    /// Generate report from results
    Report { config: PathBuf },

    /// Dataset utilities
    Dataset {
        #[command(subcommand)]
        command: DatasetCommand,
    },

    /// Configuration management
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
}

#[derive(Subcommand)]
enum DatasetCommand {
    /// Show HDF5 dataset metadata
    Info {
        /// Path to HDF5 file
        path: PathBuf,
    },

    /// Validate HDF5 dataset structure
    Validate {
        /// Path to HDF5 file
        path: PathBuf,
    },
}

#[derive(Subcommand)]
enum ConfigCommand {
    /// Show global configuration
    Show,

    /// Create default configuration
    Init,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    let filter = if cli.verbose {
        "rekall_lib=debug,rekall=debug"
    } else {
        "rekall_lib=info,rekall=info"
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| filter.into()),
        )
        .with_target(false)
        .init();

    match cli.command {
        Command::Run {
            config,
            phase,
            index,
            dry_run,
            ..
        } => {
            let cfg = BenchmarkConfig::from_file(&config)?;

            if dry_run {
                print_dry_run(&cfg, phase.as_deref(), index.as_deref());
                return Ok(());
            }

            let phases = if let Some(ref p) = phase {
                vec![p.parse().map_err(|e: String| anyhow::anyhow!(e))?]
            } else {
                Phase::all().to_vec()
            };

            let outputs = build_outputs(&cfg);
            let mut agent = Agent::new(cfg, outputs);
            agent.run(&phases, index.as_deref()).await?;
        }

        Command::Init { config } => {
            let cfg = BenchmarkConfig::from_file(&config)?;
            let outputs = build_outputs(&cfg);
            let mut agent = Agent::new(cfg, outputs);
            agent.run(&[Phase::Init], None).await?;
        }

        Command::Load {
            config,
            parallelism,
            ..
        } => {
            let mut cfg = BenchmarkConfig::from_file(&config)?;
            if let Some(p) = parallelism {
                cfg.load.parallelism = p;
            }
            let outputs = build_outputs(&cfg);
            let mut agent = Agent::new(cfg, outputs);
            agent.run(&[Phase::Load], None).await?;
        }

        Command::Index { config, name } => {
            let cfg = BenchmarkConfig::from_file(&config)?;
            let outputs = build_outputs(&cfg);
            let mut agent = Agent::new(cfg, outputs);
            agent.run(&[Phase::Index], name.as_deref()).await?;
        }

        Command::Search { config, index } => {
            let cfg = BenchmarkConfig::from_file(&config)?;
            let outputs = build_outputs(&cfg);
            let mut agent = Agent::new(cfg, outputs);
            agent
                .run(&[Phase::Search, Phase::Report], index.as_deref())
                .await?;
        }

        Command::Report { config } => {
            let cfg = BenchmarkConfig::from_file(&config)?;
            let outputs = build_outputs(&cfg);
            let mut agent = Agent::new(cfg, outputs);
            agent.run(&[Phase::Report], None).await?;
        }

        Command::Dataset { command } => match command {
            DatasetCommand::Info { path } => {
                let info = Hdf5Reader::info(&path)?;
                println!("Dataset: {}", path.display());
                println!("{info}");
            }
            DatasetCommand::Validate { path } => match Hdf5Reader::info(&path) {
                Ok(info) => {
                    println!("Dataset: {} (valid)", path.display());
                    println!("{info}");
                }
                Err(e) => {
                    eprintln!("Validation failed: {e}");
                    std::process::exit(1);
                }
            },
        },

        Command::Config { command } => match command {
            ConfigCommand::Show => {
                println!("Global config: ~/.config/rekall/config.toml");
                println!("(not yet implemented)");
            }
            ConfigCommand::Init => {
                println!("(not yet implemented)");
            }
        },
    }

    Ok(())
}

fn build_outputs(cfg: &BenchmarkConfig) -> Vec<Box<dyn OutputSink>> {
    let mut outputs: Vec<Box<dyn OutputSink>> = Vec::new();

    if cfg.output.format.contains(&"table".to_string()) {
        outputs.push(Box::new(StdoutSink::new()));
    }

    if cfg.output.format.contains(&"json".to_string()) {
        let path = cfg.output.json_path.as_deref().unwrap_or("results.json");
        outputs.push(Box::new(JsonSink::new(PathBuf::from(path))));
    }

    if outputs.is_empty() {
        outputs.push(Box::new(StdoutSink::new()));
    }

    outputs
}

fn print_dry_run(cfg: &BenchmarkConfig, phase: Option<&str>, index: Option<&str>) {
    println!("Dry run for dataset '{}'", cfg.dataset.name);
    println!("  Table: {}", cfg.table_name());
    println!("  Connection: {}", cfg.connection.dsn);
    println!(
        "  Dataset: {} (dim={}, metric={})",
        cfg.dataset.path, cfg.dataset.dimension, cfg.dataset.metric,
    );

    let phases = if let Some(p) = phase {
        vec![p.to_string()]
    } else {
        vec![
            "init".into(),
            "load".into(),
            "index".into(),
            "search".into(),
            "report".into(),
        ]
    };
    println!("  Phases: {}", phases.join(" -> "));

    println!("\n  Indexes:");
    for idx in &cfg.index {
        if index.is_some_and(|n| n != idx.name) {
            continue;
        }
        println!("    - {} (type: {})", idx.name, idx.index_type);
    }

    println!("\n  Search sweeps:");
    for sweep in &cfg.search.sweep {
        if index.is_some_and(|n| n != sweep.index) {
            continue;
        }
        let n_combos: usize = sweep
            .params
            .values()
            .map(|v| v.len())
            .product::<usize>()
            .max(1);
        println!(
            "    - {} ({} parameter combinations)",
            sweep.index, n_combos
        );
    }
}
