use std::path::PathBuf;

use clap::{Parser, Subcommand};

use rekall_lib::config::{self, BenchmarkConfig};
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
        /// Suite config file, or legacy config paths (files/directories)
        configs: Vec<PathBuf>,

        /// Dataset config files (builds an anonymous suite with --indexes)
        #[arg(long, num_args = 1..)]
        datasets: Option<Vec<PathBuf>>,

        /// Index config files or directories (used with --datasets)
        #[arg(long, num_args = 1..)]
        indexes: Option<Vec<PathBuf>>,

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

        /// Output file (svg, png, or html)
        #[arg(short, long, default_value = "plot.svg")]
        output: PathBuf,

        /// Plot title
        #[arg(short, long)]
        title: Option<String>,

        /// Use linear X-axis instead of log scale
        #[arg(long)]
        linear_x: bool,

        /// Use linear Y-axis instead of log scale
        #[arg(long)]
        linear_y: bool,

        /// Show only Pareto frontier lines (hide non-optimal points)
        #[arg(long)]
        pareto_only: bool,

        /// Show one line per config instead of one line per algorithm
        #[arg(long)]
        per_config: bool,

        /// Minimum recall to show on X-axis (e.g., 0.9)
        #[arg(long)]
        min_recall: Option<f64>,

        /// QPS method: "mean" (1M/avg_latency, default) or "wall" (queries/wall_time, matches ann-benchmarks)
        #[arg(long, default_value = "mean")]
        qps_method: String,

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

    /// Generate global config at ~/.config/rekall/config.toml.
    /// Auto-detects PostgreSQL and sets default paths.
    Setup {
        /// Overwrite existing config file
        #[arg(long)]
        force: bool,

        /// Dataset storage directory
        #[arg(long)]
        data_dir: Option<String>,

        /// Results output directory
        #[arg(long)]
        results_dir: Option<String>,

        /// PostgreSQL connection string
        #[arg(long)]
        dsn: Option<String>,
    },

    /// Manage cloud infrastructure for benchmarks
    Infra {
        #[command(subcommand)]
        command: InfraCommand,
    },
}

#[derive(Subcommand)]
enum InfraCommand {
    /// Initialize local resources and print setup instructions.
    /// Generates deploy keys, IAM templates, and a local config template.
    /// Run this once before your first `infra up`.
    #[command(alias = "setup")]
    Init {
        /// Config file with [infra] section
        #[arg(short, long)]
        config: Option<PathBuf>,

        /// Output IAM template as Terraform HCL instead of CloudFormation
        #[arg(long)]
        terraform: bool,
    },

    /// Provision an AWS EC2 instance with PostgreSQL
    Up {
        /// Config file with [infra] section
        #[arg(short, long)]
        config: Option<PathBuf>,

        /// AWS region override
        #[arg(long)]
        region: Option<String>,

        /// Instance type override
        #[arg(long)]
        instance_type: Option<String>,

        /// Memorable Name tag for this instance (e.g. "perf-test").
        /// Defaults to `<name_prefix>-<YYYYMMDD-HHMMSS>` if not given.
        /// Use it later with attach/ssh/dsn via --name.
        #[arg(short = 'n', long = "instance-name", visible_alias = "name")]
        instance_name: Option<String>,
    },

    /// Destroy cloud infrastructure.
    ///
    /// With -c/--suite (the default), tears down the latest-launched
    /// instance from local state AND deletes the suite's security
    /// group and key pair. With --instance-id or --instance-name,
    /// terminates only that single instance — shared SG and key pair
    /// are left in place since other instances under the same suite
    /// may still be using them.
    Down {
        /// Config file with [infra] section
        #[arg(short, long)]
        config: Option<PathBuf>,

        /// Suite name (name_prefix), alternative to --config. Selects
        /// the latest-launched instance under that suite via local state.
        #[arg(long)]
        suite: Option<String>,

        /// AWS instance-id (e.g. i-0abc123). Targets a single instance
        /// — leaves the suite's security group and key pair intact.
        #[arg(short = 'i', long)]
        instance_id: Option<String>,

        /// Look up the instance by its Name tag. Errors if zero or
        /// multiple instances match.
        #[arg(short = 'n', long = "instance-name", visible_alias = "name")]
        instance_name: Option<String>,

        /// AWS region for --instance-id/--instance-name lookup (default: us-east-1).
        /// AWS region. Defaults to the config's region (when -c is
        /// given) or us-east-1.
        #[arg(short, long)]
        region: Option<String>,
    },

    /// Show infrastructure status
    Status {
        /// Config file with [infra] section
        #[arg(short, long)]
        config: Option<PathBuf>,

        /// Suite name (name_prefix), alternative to --config. Selects
        /// the latest-launched instance under that suite via local state.
        #[arg(long)]
        suite: Option<String>,

        /// AWS instance-id (e.g. i-0abc123). Bypasses local state —
        /// useful when multiple instances share the same name_prefix
        /// or you're on a different machine than the launcher.
        #[arg(short = 'i', long)]
        instance_id: Option<String>,

        /// Look up the instance by its Name tag (set via `infra up
        /// --name`). Errors if zero or multiple instances match.
        #[arg(short = 'n', long = "instance-name", visible_alias = "name")]
        instance_name: Option<String>,

        /// AWS region for --instance-id/--instance-name lookup (default: us-east-1).
        /// AWS region. Defaults to the config's region (when -c is
        /// given) or us-east-1.
        #[arg(short, long)]
        region: Option<String>,
    },

    /// List all provisioned instances.
    /// Shows both locally tracked and AWS-discovered instances.
    #[command(alias = "ls")]
    List {
        /// AWS region to scan (default: us-east-1)
        /// AWS region. Defaults to the config's region (when -c is
        /// given) or us-east-1.
        #[arg(short, long)]
        region: Option<String>,
    },

    /// Run a benchmark on a remote instance.
    /// Uploads configs, ensures dataset is available, and starts
    /// rekall in a tmux session.
    #[command(name = "run")]
    RemoteRun {
        /// Infra config file
        #[arg(short, long)]
        config: Option<PathBuf>,

        /// Suite name (name_prefix), alternative to --config. Selects
        /// the latest-launched instance under that suite via local state.
        #[arg(long)]
        suite: Option<String>,

        /// AWS instance-id (e.g. i-0abc123). Bypasses local state.
        #[arg(short = 'i', long)]
        instance_id: Option<String>,

        /// Look up the instance by its Name tag (set via `infra up
        /// --name`). Errors if zero or multiple instances match.
        #[arg(short = 'n', long = "instance-name", visible_alias = "name")]
        instance_name: Option<String>,

        /// AWS region for --instance-id/--instance-name lookup (default: us-east-1).
        /// AWS region. Defaults to the config's region (when -c is
        /// given) or us-east-1.
        #[arg(short, long)]
        region: Option<String>,

        /// Benchmark config paths (files or directories) to upload and run
        #[arg(required = true)]
        benchmarks: Vec<PathBuf>,

        /// tmux session name (default: rekall)
        #[arg(short, long, default_value = "rekall")]
        session: String,

        /// Additional flags to pass to rekall run (e.g., --rebuild)
        #[arg(long)]
        rebuild: bool,

        /// S3 path for the dataset (downloaded on instance if not present)
        #[arg(long)]
        s3_dataset: Option<String>,
    },

    /// Attach to the tmux session on a benchmark instance.
    /// Creates a new session if one doesn't exist.
    Attach {
        /// Config file with [infra] section
        #[arg(short, long)]
        config: Option<PathBuf>,

        /// Suite name (name_prefix), alternative to --config. Selects
        /// the latest-launched instance under that suite via local state.
        #[arg(long)]
        suite: Option<String>,

        /// AWS instance-id (e.g. i-0abc123). Bypasses local state —
        /// useful when multiple instances share the same name_prefix
        /// or you're on a different machine than the launcher.
        #[arg(short = 'i', long)]
        instance_id: Option<String>,

        /// Look up the instance by its Name tag (set via `infra up
        /// --name`). Errors if zero or multiple instances match.
        #[arg(short = 'n', long = "instance-name", visible_alias = "name")]
        instance_name: Option<String>,

        /// AWS region for --instance-id/--instance-name lookup (default: us-east-1).
        /// AWS region. Defaults to the config's region (when -c is
        /// given) or us-east-1.
        #[arg(short, long)]
        region: Option<String>,

        /// tmux session name (default: rekall)
        #[arg(short, long, default_value = "rekall")]
        session: String,
    },

    /// SSH into a benchmark instance
    Ssh {
        /// Config file with [infra] section
        #[arg(short, long)]
        config: Option<PathBuf>,

        /// Suite name (name_prefix), alternative to --config. Selects
        /// the latest-launched instance under that suite via local state.
        #[arg(long)]
        suite: Option<String>,

        /// AWS instance-id (e.g. i-0abc123). Bypasses local state.
        #[arg(short = 'i', long)]
        instance_id: Option<String>,

        /// Look up the instance by its Name tag (set via `infra up
        /// --name`). Errors if zero or multiple instances match.
        #[arg(short = 'n', long = "instance-name", visible_alias = "name")]
        instance_name: Option<String>,

        /// AWS region for --instance-id/--instance-name lookup (default: us-east-1).
        /// AWS region. Defaults to the config's region (when -c is
        /// given) or us-east-1.
        #[arg(short, long)]
        region: Option<String>,
    },

    /// Print the PostgreSQL DSN for a cloud instance
    Dsn {
        /// Config file with [infra] section
        #[arg(short, long)]
        config: Option<PathBuf>,

        /// Suite name (name_prefix), alternative to --config. Selects
        /// the latest-launched instance under that suite via local state.
        #[arg(long)]
        suite: Option<String>,

        /// AWS instance-id (e.g. i-0abc123). Bypasses local state.
        #[arg(short = 'i', long)]
        instance_id: Option<String>,

        /// Look up the instance by its Name tag (set via `infra up
        /// --name`). Errors if zero or multiple instances match.
        #[arg(short = 'n', long = "instance-name", visible_alias = "name")]
        instance_name: Option<String>,

        /// AWS region for --instance-id/--instance-name lookup (default: us-east-1).
        /// AWS region. Defaults to the config's region (when -c is
        /// given) or us-east-1.
        #[arg(short, long)]
        region: Option<String>,
    },

    /// Re-run provisioning (deploy keys + builds) on an existing instance.
    /// Use this after fixing a failed build or changing git refs.
    Provision {
        /// Config file with [infra] section
        #[arg(short, long)]
        config: Option<PathBuf>,

        /// Suite name (name_prefix), alternative to --config. Selects
        /// the latest-launched instance under that suite via local state.
        #[arg(long)]
        suite: Option<String>,
    },

    /// Print IAM policy and role templates for AWS setup.
    /// Review and apply these in your AWS account before running `infra up`.
    IamPolicy {
        /// Config file with [infra] section (for s3_bucket scoping)
        #[arg(short, long)]
        config: Option<PathBuf>,

        /// Output as CloudFormation YAML instead of Terraform HCL
        #[arg(long)]
        cloudformation: bool,
    },

    /// Generate a deploy key for GitHub access.
    /// Add the printed public key to your GitHub repos as a deploy key
    /// before running `infra up`.
    DeployKey {
        /// Config file with [infra] section
        #[arg(short, long)]
        config: Option<PathBuf>,

        /// Force regenerate even if a key already exists
        #[arg(long)]
        regenerate: bool,
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
        .with_timer(tracing_subscriber::fmt::time::LocalTime::rfc_3339())
        .init();

    let cancel_registry = new_cancel_registry();
    install_signal_handler(cancel_registry.clone());

    match cli.command {
        Command::Run {
            configs,
            datasets,
            indexes,
            phase,
            index,
            dry_run,
            rebuild,
            reload,
            ..
        } => {
            let groups = resolve_run_groups(&configs, datasets.as_deref(), indexes.as_deref())?;

            let phases = if let Some(ref p) = phase {
                vec![p.parse().map_err(|e: String| anyhow::anyhow!(e))?]
            } else {
                Phase::all().to_vec()
            };

            let num_groups = groups.len();
            for group in &groups {
                if num_groups > 1 {
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
            linear_x,
            linear_y,
            pareto_only,
            per_config,
            min_recall,
            qps_method,
            csv,
        } => {
            let use_wall = qps_method == "wall";
            let mut summaries = Vec::new();
            for path in &results {
                let content = std::fs::read_to_string(path)
                    .map_err(|e| anyhow::anyhow!("Cannot read '{}': {e}", path.display()))?;
                let summary: rekall_lib::metrics::RunSummary = serde_json::from_str(&content)
                    .map_err(|e| anyhow::anyhow!("Invalid JSON '{}': {e}", path.display()))?;
                summaries.push(summary);
            }

            let opts = rekall_lib::output::plot::PlotOptions {
                log_x: !linear_x,
                log_y: !linear_y,
                pareto_only,
                per_config,
                min_recall: min_recall.unwrap_or(0.0),
                use_wall_qps: use_wall,
            };
            rekall_lib::output::plot::generate_plot(&summaries, &output, title.as_deref(), &opts)
                .map_err(|e| anyhow::anyhow!("Plot failed: {e}"))?;
            println!("Plot written to {}", output.display());

            if let Some(csv_path) = csv {
                rekall_lib::output::plot::export_csv(&summaries, &csv_path)
                    .map_err(|e| anyhow::anyhow!("CSV export failed: {e}"))?;
                println!("CSV written to {}", csv_path.display());
            }
        }

        Command::Infra { command } => {
            handle_infra(command)?;
        }

        Command::Setup {
            force,
            data_dir,
            results_dir,
            dsn,
        } => {
            handle_setup(force, data_dir, results_dir, dsn)?;
        }

        Command::Config { config } => {
            if let Some(path) = config {
                let cfg = BenchmarkConfig::from_path(&path)?;
                let toml_str = toml::to_string_pretty(&cfg).unwrap();
                println!("# Effective config: {}", path.display());
                let global = config::global_config_path();
                if global.exists() {
                    println!("# (merged with {})", global.display());
                }
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

fn handle_setup(
    force: bool,
    data_dir_override: Option<String>,
    results_dir_override: Option<String>,
    dsn_override: Option<String>,
) -> anyhow::Result<()> {
    let config_path = config::global_config_path();

    if config_path.exists() && !force {
        println!("Global config already exists: {}", config_path.display());
        println!();
        let content = std::fs::read_to_string(&config_path)?;
        print!("{content}");
        println!();
        println!("Run `rekall setup --force` to regenerate.");
        return Ok(());
    }

    let dsn = dsn_override.unwrap_or_else(detect_postgres);

    let data_dir = data_dir_override.unwrap_or_else(|| {
        dirs::data_dir()
            .map(|d| d.join("rekall/datasets").to_string_lossy().to_string())
            .unwrap_or_else(|| "~/.local/share/rekall/datasets".to_string())
    });

    let results_dir = results_dir_override;

    let mut config_content = format!(
        r#"# Rekall global configuration
# Generated by `rekall setup`. Edit freely.
#
# Merge order: this file → benchmark config → .local.toml override
# Values here act as defaults for all benchmark runs on this machine.

[dataset]
data_dir = "{data_dir}"

# Override the download source for all datasets:
# uri = "https://my-mirror.example.com/datasets/{{{{filename}}}}"

[connection]
dsn = "{dsn}"

# [load]
# parallelism = 4
# chunk_size = 50000

# [search]
# k = 10
# runs = 3
# warmup = 500
"#
    );

    if let Some(ref dir) = results_dir {
        config_content.push_str(&format!(
            r#"
[output]
results_dir = "{dir}"
"#
        ));
    } else {
        config_content.push_str(
            r#"
# [output]
# results_dir = "results"
"#,
        );
    }

    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&config_path, &config_content)?;

    println!("Wrote {}", config_path.display());
    println!();
    println!("  data_dir:    {data_dir}");
    println!("  connection:  {dsn}");
    if let Some(ref dir) = results_dir {
        println!("  results_dir: {dir}");
    }
    println!();
    println!("Edit this file to change defaults for all benchmark runs.");
    println!("Per-benchmark overrides go in .local.toml files.");

    Ok(())
}

/// Try common PostgreSQL socket paths and ports.
fn detect_postgres() -> String {
    let socket_dirs = ["/var/run/postgresql", "/tmp", "/run/postgresql"];

    for dir in &socket_dirs {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if let Some(port) = name.strip_prefix(".s.PGSQL.") {
                    return format!("host={dir} port={port} dbname=postgres");
                }
            }
        }
    }

    for port in [5432, 5433, 5434] {
        if std::net::TcpStream::connect(format!("127.0.0.1:{port}")).is_ok() {
            return format!("host=localhost port={port} dbname=postgres");
        }
    }

    "host=localhost port=5432 dbname=postgres".to_string()
}

fn resolve_run_groups(
    configs: &[PathBuf],
    datasets: Option<&[PathBuf]>,
    indexes: Option<&[PathBuf]>,
) -> anyhow::Result<Vec<config::ConfigGroup>> {
    match (datasets, indexes) {
        (Some(ds), Some(ix)) => {
            if !configs.is_empty() {
                anyhow::bail!("Cannot use positional configs with --datasets/--indexes");
            }
            Ok(config::resolve_anonymous_suite(ds, ix)?)
        }
        (Some(_), None) => anyhow::bail!("--datasets requires --indexes"),
        (None, Some(_)) => anyhow::bail!("--indexes requires --datasets"),
        (None, None) => {
            if configs.is_empty() {
                anyhow::bail!("Provide a suite file, config paths, or --datasets/--indexes");
            }
            if configs.len() == 1 && config::is_suite_file(&configs[0]) {
                Ok(config::resolve_suite(&configs[0])?)
            } else {
                Ok(config::resolve_config_groups(configs)?)
            }
        }
    }
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
    let search_dirs = [".", "benchmarks", "benchmarks/datasets"];
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
        println!("No config files found in . or benchmarks/");
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

fn load_infra_config(
    config: Option<&PathBuf>,
) -> anyhow::Result<rekall_lib::infra::config::InfraConfig> {
    if let Some(path) = config {
        rekall_lib::infra::config::InfraConfig::from_file(path).map_err(|e| anyhow::anyhow!("{e}"))
    } else {
        Ok(rekall_lib::infra::config::InfraConfig::default())
    }
}

/// Resolve infra config from --config or --name. --name creates a
/// minimal config targeting an existing instance by name_prefix.
fn resolve_infra_config(
    config: Option<&PathBuf>,
    name: Option<&String>,
) -> anyhow::Result<rekall_lib::infra::config::InfraConfig> {
    if let Some(n) = name {
        Ok(rekall_lib::infra::config::InfraConfig {
            name_prefix: n.clone(),
            ssh_key: detect_ssh_key(),
            ..Default::default()
        })
    } else {
        load_infra_config(config)
    }
}

/// Resolve the AWS region from CLI args, preferring explicit `--region`
/// then the config's region. Falls back to us-east-1 when neither is
/// available, but most commands have a config so this rarely kicks in.
fn effective_region(explicit: Option<&str>, config: Option<&PathBuf>) -> String {
    if let Some(r) = explicit {
        return r.to_string();
    }
    if let Some(p) = config {
        if let Ok(cfg) = rekall_lib::infra::config::InfraConfig::from_file(p) {
            return cfg.region;
        }
    }
    "us-east-1".to_string()
}

/// Build the `ssh ...` command string for an instance.
///
/// Resolution priority:
///   1. `--instance-id`: AWS-side lookup by exact id.
///   2. `--instance-name`: AWS-side lookup by Name tag (errors if no
///      match or multiple matches).
///   3. `-c CONFIG` / `--suite`: local state lookup (latest-launched
///      instance under that name_prefix).
///
/// For (1) and (2) the SSH key falls back to `--config`'s ssh_key if
/// a config is also given, otherwise to ~/.ssh/id_ed25519.
fn resolve_ssh_command(
    config: Option<&PathBuf>,
    suite: Option<&String>,
    instance_id: Option<&str>,
    instance_name: Option<&str>,
    region: &str,
) -> anyhow::Result<String> {
    if let Some(ip) = aws_side_lookup_ip(region, instance_id, instance_name)? {
        let infra_cfg = if config.is_some() || suite.is_some() {
            resolve_infra_config(config, suite).unwrap_or_default()
        } else {
            rekall_lib::infra::config::InfraConfig::default()
        };
        let ssh_priv = infra_cfg.ssh_private_key_path();
        return Ok(format!(
            "ssh -i {ssh_priv} -o StrictHostKeyChecking=no ubuntu@{ip}"
        ));
    }
    let infra_cfg = resolve_infra_config(config, suite)?;
    let mgr = rekall_lib::infra::InfraManager::new(&infra_cfg)?;
    mgr.ssh_command().map_err(|e| anyhow::anyhow!("{e}"))
}

/// Returns the instance's public IP via AWS-side lookup if either
/// --instance-id or --instance-name is set. None if neither is set.
fn aws_side_lookup_ip(
    region: &str,
    instance_id: Option<&str>,
    instance_name: Option<&str>,
) -> anyhow::Result<Option<String>> {
    let inst = if let Some(id) = instance_id {
        rekall_lib::infra::find_instance(region, id)?
    } else if let Some(n) = instance_name {
        rekall_lib::infra::find_instance_by_name(region, n)?
    } else {
        return Ok(None);
    };
    let ip = inst
        .status
        .public_ip
        .ok_or_else(|| anyhow::anyhow!("Instance has no public IP yet"))?;
    Ok(Some(ip))
}

fn detect_ssh_key() -> Option<String> {
    let ssh_dir = dirs::home_dir()?.join(".ssh");
    for name in ["id_ed25519", "id_rsa", "id_ecdsa"] {
        let priv_path = ssh_dir.join(name);
        let pub_path = ssh_dir.join(format!("{name}.pub"));
        if priv_path.exists() && pub_path.exists() {
            return Some(format!("~/.ssh/{name}"));
        }
    }
    None
}

fn handle_infra(command: InfraCommand) -> anyhow::Result<()> {
    match command {
        InfraCommand::Init { config, terraform } => {
            let infra_cfg = load_infra_config(config.as_ref()).unwrap_or_default();
            let mgr = rekall_lib::infra::InfraManager::new(&infra_cfg)?;

            // Generate deploy keys for rekall and meerkat repos
            let repo_names = vec!["rekall", "meerkat"];
            let deploy_keys = mgr.ensure_deploy_keys(&repo_names, false)?;
            for key in &deploy_keys {
                if key.generated {
                    println!("Generated deploy key for '{}'", key.repo_name);
                } else {
                    println!("Deploy key for '{}' already exists", key.repo_name);
                }
            }

            // Step 2: Write IAM template (CloudFormation by default)
            let iam_filename = if terraform {
                "rekall-iam.tf"
            } else {
                "rekall-iam.yaml"
            };
            let iam_content = if terraform {
                rekall_lib::infra::iam_terraform_template(&infra_cfg)
            } else {
                rekall_lib::infra::iam_cloudformation_template(&infra_cfg)
            };
            std::fs::write(iam_filename, &iam_content)?;
            println!("Wrote IAM template to {iam_filename}");

            // Step 3: Write local config template if it doesn't exist
            if let Some(ref cfg_path) = config {
                let local_path = cfg_path.with_file_name(format!(
                    "{}.local.toml",
                    cfg_path.file_stem().unwrap_or_default().to_string_lossy()
                ));
                if !local_path.exists() {
                    let local_content = r#"# Local overrides (gitignored, never committed)

[infra]
ssh_key = "~/.ssh/id_ed25519.pub"
iam_instance_profile = "rekall-profile"

# Private repo URLs (uses the deploy key generated by `rekall infra init`)
# [infra.rekall]
# git_url = "git@github.com:org/rekall.git"
# git_ref = "main"

# [infra.meerkat]
# git_url = "git@github.com:org/meerkat.git"
# git_ref = "main"
"#;
                    std::fs::write(&local_path, local_content)?;
                    println!("Wrote local config template to {}", local_path.display());
                }
            }

            // Print instructions
            println!();
            println!("=== Setup instructions ===");
            println!();

            let mut step = 1;

            println!("{step}. Apply the IAM template in your AWS account:");
            if terraform {
                println!("   cp {iam_filename} <your-infra-repo>/ && cd <your-infra-repo> && terraform apply");
            } else {
                println!("   Option A: Upload {iam_filename} in AWS Console → CloudFormation → Create stack");
                println!("   Option B: aws cloudformation deploy \\");
                println!("               --template-file {iam_filename} \\");
                println!("               --stack-name rekall-iam \\");
                println!("               --capabilities CAPABILITY_NAMED_IAM");
            }
            println!("   This creates the IAM role, instance profile, and operator user.");
            println!("   Note the operator access key and secret from the stack outputs.");
            println!();
            step += 1;

            println!("{step}. Configure the operator credentials on your machine:");
            println!("   aws configure --profile rekall");
            println!("   (enter the access key ID and secret from the stack outputs)");
            println!("   Then export AWS_PROFILE=rekall before running rekall infra commands,");
            println!("   or set it in your shell profile.");
            println!();
            step += 1;

            println!("{step}. Add deploy keys to your private GitHub repos");
            println!("   (each repo → Settings → Deploy keys → Add deploy key, read-only):");
            println!();
            for key in &deploy_keys {
                println!("   {}:", key.repo_name);
                println!("   {}", key.public_key.trim());
                println!();
            }
            println!("   Skip this step if you only use public repos.");
            println!();
            step += 1;

            if let Some(ref cfg_path) = config {
                let local_path = cfg_path.with_file_name(format!(
                    "{}.local.toml",
                    cfg_path.file_stem().unwrap_or_default().to_string_lossy()
                ));
                println!("{step}. Edit {}:", local_path.display());
                println!("   - Set ssh_key to your SSH private key path");
                println!("   - Set iam_instance_profile to the profile name from step 1");
                println!("   - Uncomment and set git URLs for private repos");
                println!();
                println!("   Verify the merged config with:");
                println!("   rekall config {}", cfg_path.display());
                println!();
                step += 1;
            } else {
                println!("{step}. Create a .local.toml override next to your config file:");
                println!("   cp infra/instances/local.toml.example \\");
                println!("      infra/instances/<instance>.local.toml");
                println!("   Then set ssh_key, iam_instance_profile, and git URLs.");
                println!();
                step += 1;
            }

            println!("{step}. Provision the instance:");
            if let Some(ref cfg_path) = config {
                println!("   rekall infra up -c {}", cfg_path.display());
            } else {
                println!("   rekall infra up -c infra/instances/r7g.2xlarge.toml");
            }
        }

        InfraCommand::Up {
            config,
            region,
            instance_type,
            instance_name,
        } => {
            let mut infra_cfg = load_infra_config(config.as_ref())?;
            if let Some(r) = region {
                infra_cfg.region = r;
            }
            if let Some(it) = instance_type {
                infra_cfg.instance_type = it;
            }

            println!(
                "Provisioning {} instance in {}...",
                infra_cfg.instance_type, infra_cfg.region
            );
            println!("  PG version: {}", infra_cfg.pg_version);
            println!("  Disk: {} GB (gp3)", infra_cfg.disk_size_gb);
            println!(
                "  Extensions: {}",
                if infra_cfg.extensions.is_empty() {
                    "none".to_string()
                } else {
                    infra_cfg.extensions.join(", ")
                }
            );
            println!();

            let mgr = rekall_lib::infra::InfraManager::new(&infra_cfg)?;
            let output = mgr.up_with_name(instance_name.as_deref())?;
            let ssh_cmd = mgr.ssh_command()?;

            let summary = format!(
                "Instance ready:\n\
                 \n\
                 IP:            {}\n\
                 Instance:      {} ({})\n\
                 DSN:           {}\n\
                 IAM profile:   {}\n\
                 \n\
                 SSH:  {ssh_cmd}\n\
                 DSN:  rekall infra dsn -n {}\n",
                output.public_ip,
                output.instance_id,
                output.instance_type,
                output.dsn,
                output.iam_profile,
                infra_cfg.name_prefix,
            );

            println!();
            print!("{summary}");

            let info_path = format!(".rekall/infra/{}/instance.txt", infra_cfg.name_prefix);
            if let Err(e) = std::fs::write(&info_path, &summary) {
                tracing::warn!("Could not save instance info: {e}");
            } else {
                println!();
                println!("Instance info saved to {info_path}");
            }
        }

        InfraCommand::Down {
            config,
            suite,
            instance_id,
            instance_name,
            region,
        } => {
            let region = effective_region(region.as_deref(), config.as_ref());
            // Targeted termination: resolve the specific instance via
            // AWS, terminate only that one, leave SG/key in place.
            if instance_id.is_some() || instance_name.is_some() {
                let id = if let Some(id) = instance_id {
                    id
                } else {
                    let n = instance_name.unwrap();
                    let inst = rekall_lib::infra::find_instance_by_name(&region, &n)?;
                    inst.status
                        .instance_id
                        .ok_or_else(|| anyhow::anyhow!("Instance '{n}' has no instance-id"))?
                };
                rekall_lib::infra::terminate_instance(&region, &id)?;
                println!(
                    "Terminated {id}. Shared security group and key pair left in \
                     place — use `rekall infra down -c <config>` or `--suite` to \
                     also clean those up once the last instance under the suite is gone."
                );
            } else {
                let infra_cfg = resolve_infra_config(config.as_ref(), suite.as_ref())?;
                let mgr = rekall_lib::infra::InfraManager::new(&infra_cfg)?;
                mgr.down()?;
                println!("Infrastructure '{}' destroyed.", infra_cfg.name_prefix);
            }
        }

        InfraCommand::Status {
            config,
            suite,
            instance_id,
            instance_name,
            region,
        } => {
            let region = effective_region(region.as_deref(), config.as_ref());
            let (header, status) = if let Some(id) = instance_id.as_deref() {
                let inst = rekall_lib::infra::find_instance(&region, id)?;
                let h = inst
                    .status
                    .instance_name
                    .clone()
                    .unwrap_or_else(|| id.to_string());
                (h, inst.status)
            } else if let Some(n) = instance_name.as_deref() {
                let inst = rekall_lib::infra::find_instance_by_name(&region, n)?;
                (n.to_string(), inst.status)
            } else {
                let infra_cfg = resolve_infra_config(config.as_ref(), suite.as_ref())?;
                let mgr = rekall_lib::infra::InfraManager::new(&infra_cfg)?;
                (infra_cfg.name_prefix.clone(), mgr.status()?)
            };

            println!("Instance: {header}");
            if let Some(ref name) = status.instance_name {
                if name != &header {
                    println!("  Name:       {name}");
                }
            }
            if !status.running {
                println!("  Status:     not running");
                return Ok(());
            }

            if let Some(ref ip) = status.public_ip {
                println!("  IP:         {ip}");
            }
            if let Some(ref id) = status.instance_id {
                println!("  Instance:   {id}");
            }
            if let Some(ref it) = status.instance_type {
                println!("  Type:       {it}");
            }
            println!(
                "  PG ready:   {}",
                if status.pg_ready { "yes" } else { "no" }
            );
            if let Some(ref dsn) = status.dsn {
                println!("  DSN:        {dsn}");
            }
            if let Some(ref arn) = status.iam_profile {
                println!("  IAM role:   {arn}");
            }
        }

        InfraCommand::List { region } => {
            let region = effective_region(region.as_deref(), None);
            // Discover from AWS tags (works across machines)
            let instances = rekall_lib::infra::list_instances_aws(&region)?;

            if instances.is_empty() {
                println!("No rekall instances found in {region}.");
                println!(
                    "Run `rekall infra init -c infra/instances/r7g.2xlarge.toml` to get started."
                );
                return Ok(());
            }

            println!(
                "{:<28} {:<10} {:<16} {:<14} {:<22} {:<20}",
                "NAME", "STATE", "IP", "TYPE", "INSTANCE", "GROUP"
            );
            for inst in &instances {
                let state = if inst.status.running {
                    "running"
                } else {
                    "stopped"
                };
                let ip = inst.status.public_ip.as_deref().unwrap_or("-");
                let itype = inst.status.instance_type.as_deref().unwrap_or("-");
                let id = inst.status.instance_id.as_deref().unwrap_or("-");
                let name = inst.status.instance_name.as_deref().unwrap_or("-");
                println!(
                    "{:<28} {:<10} {:<16} {:<14} {:<22} {:<20}",
                    name, state, ip, itype, id, inst.name
                );
            }
        }

        InfraCommand::RemoteRun {
            config,
            suite,
            instance_id,
            instance_name,
            region,
            benchmarks,
            session,
            rebuild,
            s3_dataset,
        } => {
            let region = effective_region(region.as_deref(), config.as_ref());
            let (ip, ssh_priv): (String, String) = if let Some(ip) =
                aws_side_lookup_ip(&region, instance_id.as_deref(), instance_name.as_deref())?
            {
                // AWS-side lookup. SSH key from --config if provided, else default.
                let cfg = if config.is_some() || suite.is_some() {
                    resolve_infra_config(config.as_ref(), suite.as_ref()).unwrap_or_default()
                } else {
                    rekall_lib::infra::config::InfraConfig::default()
                };
                (ip, cfg.ssh_private_key_path())
            } else {
                let infra_cfg = resolve_infra_config(config.as_ref(), suite.as_ref())?;
                let mgr = rekall_lib::infra::InfraManager::new(&infra_cfg)?;
                let status = mgr.status()?;
                if !status.running {
                    return Err(anyhow::anyhow!(
                        "Instance not running. Run `rekall infra up` first."
                    ));
                }
                let ip = status
                    .public_ip
                    .ok_or_else(|| anyhow::anyhow!("No public IP for instance"))?;
                let (_, sp) = infra_cfg
                    .require_ssh_key()
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                (ip, sp)
            };
            let ip = ip.as_str();
            let ssh_priv = ssh_priv.as_str();

            let ssh_target = format!("ubuntu@{ip}");
            let ssh_args = |extra: &[&str]| -> std::process::Command {
                let mut cmd = std::process::Command::new("ssh");
                cmd.args([
                    "-o",
                    "StrictHostKeyChecking=no",
                    "-i",
                    ssh_priv,
                    &ssh_target,
                ]);
                cmd.args(extra);
                cmd
            };

            // Create remote benchmark directory
            let remote_dir = "~/benchmarks";
            let run_status = ssh_args(&[&format!("mkdir -p {remote_dir}")]).status()?;
            if !run_status.success() {
                return Err(anyhow::anyhow!(
                    "Failed to create remote benchmark directory"
                ));
            }

            // Upload benchmark configs
            println!("Uploading benchmark configs...");
            for bench_path in &benchmarks {
                let scp_status = if bench_path.is_dir() {
                    std::process::Command::new("scp")
                        .args([
                            "-o",
                            "StrictHostKeyChecking=no",
                            "-i",
                            ssh_priv,
                            "-r",
                            bench_path.to_str().unwrap(),
                            &format!("{ssh_target}:{remote_dir}/"),
                        ])
                        .status()?
                } else {
                    std::process::Command::new("scp")
                        .args([
                            "-o",
                            "StrictHostKeyChecking=no",
                            "-i",
                            ssh_priv,
                            bench_path.to_str().unwrap(),
                            &format!("{ssh_target}:{remote_dir}/"),
                        ])
                        .status()?
                };
                if !scp_status.success() {
                    return Err(anyhow::anyhow!("Failed to upload {}", bench_path.display()));
                }

                // Upload .local.toml if exists
                if bench_path.is_dir() {
                    // For directories, local overrides are inside already
                } else {
                    let stem = bench_path.file_stem().unwrap_or_default().to_string_lossy();
                    let parent = bench_path.parent().unwrap_or(std::path::Path::new("."));
                    let local_path = parent.join(format!("{stem}.local.toml"));
                    if local_path.exists() {
                        let _ = std::process::Command::new("scp")
                            .args([
                                "-o",
                                "StrictHostKeyChecking=no",
                                "-i",
                                ssh_priv,
                                local_path.to_str().unwrap(),
                                &format!("{ssh_target}:{remote_dir}/"),
                            ])
                            .status();
                    }
                }

                // If this is a suite file, also upload every index +
                // dataset it references, preserving their relative paths
                // so the suite's references resolve on the remote.
                if !bench_path.is_dir() {
                    if let Some(deps) = rekall_lib::config::suite_dependencies(bench_path)
                        .map_err(|e| anyhow::anyhow!("{e}"))?
                    {
                        let suite_parent = bench_path.parent().unwrap_or(std::path::Path::new("."));
                        // mkdir for each subdirectory referenced, dedup.
                        let mut subdirs: std::collections::BTreeSet<String> =
                            std::collections::BTreeSet::new();
                        for dep in &deps {
                            if let Some(d) = dep.parent() {
                                let s = d.to_string_lossy().to_string();
                                if !s.is_empty() {
                                    subdirs.insert(s);
                                }
                            }
                        }
                        for sub in &subdirs {
                            let _ = ssh_args(&[&format!("mkdir -p {remote_dir}/{sub}")]).status();
                        }
                        for dep in &deps {
                            let local_dep = suite_parent.join(dep);
                            if !local_dep.exists() {
                                return Err(anyhow::anyhow!(
                                    "Suite '{}' references missing file: {}",
                                    bench_path.display(),
                                    local_dep.display()
                                ));
                            }
                            // Remote path mirrors the relative path from the
                            // suite's parent dir.
                            let remote_target = format!("{remote_dir}/{}", dep.display());
                            let scp_status = std::process::Command::new("scp")
                                .args([
                                    "-o",
                                    "StrictHostKeyChecking=no",
                                    "-i",
                                    ssh_priv,
                                    local_dep.to_str().unwrap(),
                                    &format!("{ssh_target}:{remote_target}"),
                                ])
                                .status()?;
                            if !scp_status.success() {
                                return Err(anyhow::anyhow!(
                                    "Failed to upload suite dep {}",
                                    local_dep.display()
                                ));
                            }
                            // Also push the dep's .local.toml if present.
                            let stem = local_dep.file_stem().unwrap_or_default().to_string_lossy();
                            let local_overlay = local_dep
                                .parent()
                                .unwrap_or(std::path::Path::new("."))
                                .join(format!("{stem}.local.toml"));
                            if local_overlay.exists() {
                                let remote_overlay_dir = dep
                                    .parent()
                                    .map(|p| format!("{remote_dir}/{}", p.display()))
                                    .unwrap_or_else(|| remote_dir.to_string());
                                let _ = std::process::Command::new("scp")
                                    .args([
                                        "-o",
                                        "StrictHostKeyChecking=no",
                                        "-i",
                                        ssh_priv,
                                        local_overlay.to_str().unwrap(),
                                        &format!("{ssh_target}:{remote_overlay_dir}/"),
                                    ])
                                    .status();
                            }
                        }
                        println!(
                            "Uploaded {} suite dependency file(s) for {}",
                            deps.len(),
                            bench_path.display()
                        );
                    }
                }
            }

            // Generate a .local.toml on the remote with the correct DSN
            let dsn = "host=localhost port=5432 dbname=rekall user=rekall password=rekall";
            let remote_local_setup = format!(
                r#"for base in {remote_dir}/*/base.toml {remote_dir}/*.toml; do
  [ -f "$base" ] || continue
  dir=$(dirname "$base")
  stem=$(basename "$base" .toml)
  local="$dir/$stem.local.toml"
  if [ ! -f "$local" ]; then
    echo '[connection]' > "$local"
    echo 'dsn = "{dsn}"' >> "$local"
  fi
done"#
            );
            let _ = ssh_args(&[&remote_local_setup]).status();

            // Download dataset from S3 if specified
            if let Some(ref s3_path) = s3_dataset {
                let filename = s3_path.rsplit('/').next().unwrap_or("dataset.hdf5");
                println!("Ensuring dataset is on instance...");
                let download_cmd = format!(
                    "mkdir -p ~/data && [ -f ~/data/{filename} ] && echo 'Dataset already present' || aws s3 cp {s3_path} ~/data/{filename}"
                );
                let dl_status = ssh_args(&[&download_cmd]).status()?;
                if !dl_status.success() {
                    return Err(anyhow::anyhow!("Failed to download dataset from {s3_path}"));
                }

                // Update dataset path in uploaded configs
                let update_cmd = format!(
                    r#"find {remote_dir} -name '*.toml' -exec grep -l 'path.*=.*data/' {{}} \; | while read f; do
  sed -i 's|path = ".*data/|path = "~/data/|' "$f"
done"#
                );
                let _ = ssh_args(&[&update_cmd]).status();
            }

            // Build the remote rekall run command
            let mut remote_configs: Vec<String> = Vec::new();
            for bench_path in &benchmarks {
                let name = bench_path.file_name().unwrap_or_default().to_string_lossy();
                remote_configs.push(format!("{remote_dir}/{name}"));
            }
            let configs_str = remote_configs.join(" ");
            let rebuild_flag = if rebuild { " --rebuild" } else { "" };

            // Pipe both stdout and stderr to a log file. If the tmux
            // session dies (because rekall exited fast on an error),
            // the log persists and the user can SSH in and read it
            // instead of being stuck guessing what went wrong.
            //
            // `source ~/.cargo/env` ensures ~/.cargo/bin is on PATH —
            // tmux inherits the non-interactive SSH shell's PATH, which
            // doesn't include cargo bins by default. Without this, a
            // user who runs `cargo install` to update their rekall
            // would have their update silently ignored in favour of
            // the older /usr/local/bin/rekall from provision.sh.
            let log_path = format!("{remote_dir}/last-run.log");
            let run_cmd = format!(
                "cd ~ && source ~/.cargo/env 2>/dev/null; \
                 rekall run {configs_str}{rebuild_flag} 2>&1 | tee {log_path}"
            );

            // Launch in tmux
            println!("Launching benchmark in tmux session '{session}'...");
            println!("(Output also tee'd to {log_path} on the remote.)");
            let tmux_cmd = format!(
                "TERM=xterm-256color tmux new -d -s {session} '{run_cmd}' 2>/dev/null || \
                 TERM=xterm-256color tmux send-keys -t {session} '{run_cmd}' Enter"
            );
            let tmux_status = ssh_args(&["-t", &tmux_cmd]).status()?;
            if !tmux_status.success() {
                return Err(anyhow::anyhow!("Failed to start tmux session"));
            }

            // Build a selector hint for the follow-up commands, matching
            // whichever flag the user actually used to find the instance.
            let selector_hint = if let Some(ref id) = instance_id {
                format!("--instance-id {id}")
            } else if let Some(ref n) = instance_name {
                format!("--name {n}")
            } else if let Some(ref p) = config {
                format!("-c {}", p.display())
            } else if let Some(ref s) = suite {
                format!("--suite {s}")
            } else {
                String::new()
            };

            println!();
            println!("Benchmark started on {} in tmux session '{session}'", ip);
            println!();
            println!("To check progress:");
            println!("  rekall infra attach {selector_hint}");
            println!();
            println!("To view results later:");
            println!("  rekall infra ssh {selector_hint} -- cat ~/benchmarks/results/*.json");
        }

        InfraCommand::Attach {
            config,
            suite,
            instance_id,
            instance_name,
            region,
            session,
        } => {
            let region = effective_region(region.as_deref(), config.as_ref());
            let cmd = resolve_ssh_command(
                config.as_ref(),
                suite.as_ref(),
                instance_id.as_deref(),
                instance_name.as_deref(),
                &region,
            )?;
            let parts: Vec<&str> = cmd.split_whitespace().collect();
            if parts.len() < 2 {
                return Err(anyhow::anyhow!("Invalid SSH command: {cmd}"));
            }
            // SSH with -t for tty, set TERM for compatibility
            let status = std::process::Command::new(parts[0])
                .args(&parts[1..])
                .arg("-t")
                .arg(format!(
                    "TERM=xterm-256color tmux attach -t {session} 2>/dev/null || TERM=xterm-256color tmux new -s {session}"
                ))
                .status()?;

            std::process::exit(status.code().unwrap_or(1));
        }

        InfraCommand::Ssh {
            config,
            suite,
            instance_id,
            instance_name,
            region,
        } => {
            let region = effective_region(region.as_deref(), config.as_ref());
            let cmd = resolve_ssh_command(
                config.as_ref(),
                suite.as_ref(),
                instance_id.as_deref(),
                instance_name.as_deref(),
                &region,
            )?;
            let parts: Vec<&str> = cmd.split_whitespace().collect();
            if parts.len() < 2 {
                return Err(anyhow::anyhow!("Invalid SSH command: {cmd}"));
            }
            let status = std::process::Command::new(parts[0])
                .args(&parts[1..])
                .status()?;

            std::process::exit(status.code().unwrap_or(1));
        }

        InfraCommand::Dsn {
            config,
            suite,
            instance_id,
            instance_name,
            region,
        } => {
            let region = effective_region(region.as_deref(), config.as_ref());
            if let Some(ip) =
                aws_side_lookup_ip(&region, instance_id.as_deref(), instance_name.as_deref())?
            {
                // Default pg_port — we don't have config context with --instance-id/-name.
                let dsn = format!(
                    "host={ip} port=5432 dbname=rekall user=rekall password=rekall sslmode=disable"
                );
                println!("{dsn}");
            } else {
                let infra_cfg = resolve_infra_config(config.as_ref(), suite.as_ref())?;
                let mgr = rekall_lib::infra::InfraManager::new(&infra_cfg)?;
                let dsn = mgr.dsn()?;
                println!("{dsn}");
            }
        }

        InfraCommand::Provision { config, suite } => {
            let infra_cfg = resolve_infra_config(config.as_ref(), suite.as_ref())?;
            let mgr = rekall_lib::infra::InfraManager::new(&infra_cfg)?;
            mgr.provision()?;
            println!("Provisioning complete.");
        }

        InfraCommand::IamPolicy {
            config,
            cloudformation,
        } => {
            let infra_cfg = load_infra_config(config.as_ref()).unwrap_or_default();
            if cloudformation {
                print!(
                    "{}",
                    rekall_lib::infra::iam_cloudformation_template(&infra_cfg)
                );
            } else {
                print!("{}", rekall_lib::infra::iam_terraform_template(&infra_cfg));
            }
        }

        InfraCommand::DeployKey { config, regenerate } => {
            let infra_cfg = load_infra_config(config.as_ref())?;
            let mgr = rekall_lib::infra::InfraManager::new(&infra_cfg)?;
            let repo_names = vec!["rekall", "meerkat"];
            let keys = mgr.ensure_deploy_keys(&repo_names, regenerate)?;

            println!("Add each key to the corresponding GitHub repo");
            println!("(Settings → Deploy keys → read-only):");
            println!();
            for key in &keys {
                let status = if key.generated { "generated" } else { "exists" };
                println!("{} ({status}):", key.repo_name);
                println!("{}", key.public_key.trim());
                println!();
            }
            if regenerate {
                println!("Keys regenerated. Update the deploy keys on GitHub.");
            }
        }
    }

    Ok(())
}

fn print_dry_run(cfg: &BenchmarkConfig, phase: Option<&str>, index: Option<&str>) {
    println!("Dry run for dataset '{}'", cfg.dataset.name);
    println!("  Table: {}", cfg.table_name());
    println!("  Connection: {}", cfg.connection.dsn);
    println!(
        "  Dataset: {} (dim={}, metric={})",
        cfg.dataset.display_path(),
        cfg.dataset.dimension,
        cfg.dataset.metric,
    );
    if let Some(ref uri) = cfg.dataset.uri {
        let exists = cfg.dataset.resolved_path().exists();
        if exists {
            println!("  Source: {uri} (cached)");
        } else {
            println!("  Source: {uri} (will download)");
        }
    }

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
