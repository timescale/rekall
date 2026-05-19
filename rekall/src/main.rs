use std::path::PathBuf;

use clap::{Parser, Subcommand};

use rekall_lib::config::BenchmarkConfig;
use rekall_lib::dataset::hdf5::Hdf5Reader;
use rekall_lib::output::json::JsonSink;
use rekall_lib::output::stdout::StdoutSink;
use rekall_lib::output::OutputSink;
use rekall_lib::phase::Phase;
use rekall_lib::{install_signal_handler, new_cancel_registry, Agent};

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
    ///
    /// Accepts one or more paths: files, directories, or globs.
    /// Directories load base.toml + all index configs inside.
    /// Individual files are grouped by parent directory.
    /// Example: rekall run dir1/ dir2/mkt-*.toml
    Run {
        /// Config paths (files, directories, or shell globs)
        #[arg(required = true)]
        configs: Vec<PathBuf>,

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

        /// Force rebuild indexes even if they already exist
        #[arg(long)]
        rebuild: bool,

        /// Force reload dataset even if table already has data
        #[arg(long)]
        reload: bool,
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

    /// Generate recall-vs-QPS plot from result JSON files
    Plot {
        /// Result JSON files to plot (combine multiple for comparison)
        #[arg(required = true)]
        results: Vec<PathBuf>,

        /// Output file (svg or png)
        #[arg(short, long, default_value = "plot.svg")]
        output: PathBuf,

        /// Plot title
        #[arg(short, long)]
        title: Option<String>,

        /// Also export CSV data file
        #[arg(long)]
        csv: Option<PathBuf>,
    },

    /// Show configs. Without arguments, lists all configs found.
    /// With a path, shows the effective merged config.
    Config {
        /// Path to a specific config file (optional)
        config: Option<PathBuf>,
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

    let cancel_registry = new_cancel_registry();
    install_signal_handler(cancel_registry.clone());

    match cli.command {
        Command::Run {
            configs,
            phase,
            index,
            dry_run,
            rebuild,
            reload,
            ..
        } => {
            let groups = rekall_lib::config::resolve_config_groups(&configs)?;

            let phases = if let Some(ref p) = phase {
                vec![p.parse().map_err(|e: String| anyhow::anyhow!(e))?]
            } else {
                Phase::all().to_vec()
            };

            for group in &groups {
                if groups.len() > 1 {
                    println!("\n{}", "=".repeat(60));
                    println!("=== Group: {} ===", group.name);
                    println!("{}\n", "=".repeat(60));
                }

                if dry_run {
                    print_dry_run(&group.config, phase.as_deref(), index.as_deref());
                    continue;
                }

                let outputs = build_outputs(&group.config);
                let mut agent = Agent::new(group.config.clone(), outputs, cancel_registry.clone());
                agent
                    .run(&phases, index.as_deref(), rebuild, reload)
                    .await?;
            }
        }

        Command::Init { config } => {
            let cfg = BenchmarkConfig::from_path(&config)?;
            let outputs = build_outputs(&cfg);
            let mut agent = Agent::new(cfg, outputs, cancel_registry.clone());
            agent.run(&[Phase::Init], None, false, false).await?;
        }

        Command::Load {
            config,
            parallelism,
            ..
        } => {
            let mut cfg = BenchmarkConfig::from_path(&config)?;
            if let Some(p) = parallelism {
                cfg.load.parallelism = p;
            }
            let outputs = build_outputs(&cfg);
            let mut agent = Agent::new(cfg, outputs, cancel_registry.clone());
            agent.run(&[Phase::Load], None, false, false).await?;
        }

        Command::Index { config, name } => {
            let cfg = BenchmarkConfig::from_path(&config)?;
            let outputs = build_outputs(&cfg);
            let mut agent = Agent::new(cfg, outputs, cancel_registry.clone());
            agent
                .run(&[Phase::Index], name.as_deref(), false, false)
                .await?;
        }

        Command::Search { config, index } => {
            let cfg = BenchmarkConfig::from_path(&config)?;
            let outputs = build_outputs(&cfg);
            let mut agent = Agent::new(cfg, outputs, cancel_registry.clone());
            agent
                .run(
                    &[Phase::Search, Phase::Report],
                    index.as_deref(),
                    false,
                    false,
                )
                .await?;
        }

        Command::Report { config } => {
            let cfg = BenchmarkConfig::from_path(&config)?;
            let outputs = build_outputs(&cfg);
            let mut agent = Agent::new(cfg, outputs, cancel_registry.clone());
            agent.run(&[Phase::Report], None, false, false).await?;
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

        Command::Plot {
            results,
            output,
            title,
            csv,
        } => {
            let mut summaries = Vec::new();
            for path in &results {
                let content = std::fs::read_to_string(path)
                    .map_err(|e| anyhow::anyhow!("Cannot read '{}': {e}", path.display()))?;
                let summary: rekall_lib::metrics::RunSummary = serde_json::from_str(&content)
                    .map_err(|e| anyhow::anyhow!("Invalid JSON '{}': {e}", path.display()))?;
                summaries.push(summary);
            }

            rekall_lib::output::plot::generate_plot(&summaries, &output, title.as_deref())
                .map_err(|e| anyhow::anyhow!("Plot failed: {e}"))?;
            println!("Plot written to {}", output.display());

            if let Some(csv_path) = csv {
                rekall_lib::output::plot::export_csv(&summaries, &csv_path)
                    .map_err(|e| anyhow::anyhow!("CSV export failed: {e}"))?;
                println!("CSV written to {}", csv_path.display());
            }
        }

        Command::Config { config } => {
            if let Some(path) = config {
                let cfg = BenchmarkConfig::from_path(&path)?;
                let toml_str = toml::to_string_pretty(&cfg).unwrap();
                println!("# Effective config: {}", path.display());
                let local = path.with_file_name(format!(
                    "{}.local.toml",
                    path.file_stem().unwrap_or_default().to_string_lossy()
                ));
                if local.exists() {
                    println!("# (merged with {})", local.display());
                }
                println!();
                println!("{toml_str}");
            } else {
                show_all_configs();
            }
        }
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

fn show_all_configs() {
    let search_dirs = [".", "examples"];
    let mut found = Vec::new();

    for dir in &search_dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "toml")
                && !path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .contains(".local.")
                && !path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .starts_with("Cargo")
                && !path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .contains("rust-analyzer")
            {
                let content = std::fs::read_to_string(&path).unwrap_or_default();
                if !content.contains("[dataset]") {
                    continue;
                }
                found.push(path);
            }
        }
    }

    found.sort();

    if found.is_empty() {
        println!("No config files found in . or examples/");
        return;
    }

    for path in &found {
        let local = path.with_file_name(format!(
            "{}.local.toml",
            path.file_stem().unwrap_or_default().to_string_lossy()
        ));
        let has_local = local.exists();

        match BenchmarkConfig::from_path(path) {
            Ok(cfg) => {
                let local_marker = if has_local { " (+local)" } else { "" };
                println!("{}{local_marker}", path.display());
                println!(
                    "  dataset: {} ({}d, {})",
                    cfg.dataset.name, cfg.dataset.dimension, cfg.dataset.metric
                );
                println!("  table: {}", cfg.table_name());
                println!(
                    "  indexes: {}",
                    cfg.index
                        .iter()
                        .map(|i| format!("{} ({})", i.name, i.index_type))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                let sweep_points: usize = cfg
                    .search
                    .sweep
                    .iter()
                    .map(|s| s.params.values().map(|v| v.len()).product::<usize>().max(1))
                    .sum();
                println!(
                    "  search: k={}, {} sweep points",
                    cfg.search.k, sweep_points
                );
                println!();
            }
            Err(e) => {
                println!("{} (ERROR: {e})", path.display());
                println!();
            }
        }
    }
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
