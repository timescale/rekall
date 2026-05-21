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
    },

    /// Destroy cloud infrastructure
    Down {
        /// Config file with [infra] section
        #[arg(short, long)]
        config: Option<PathBuf>,

        /// Instance name (name_prefix). Alternative to --config.
        #[arg(short, long)]
        name: Option<String>,
    },

    /// Show infrastructure status
    Status {
        /// Config file with [infra] section
        #[arg(short, long)]
        config: Option<PathBuf>,

        /// Instance name (name_prefix). Alternative to --config.
        #[arg(short, long)]
        name: Option<String>,
    },

    /// List all provisioned instances
    #[command(alias = "ls")]
    List,

    /// SSH into a benchmark instance
    Ssh {
        /// Config file with [infra] section
        #[arg(short, long)]
        config: Option<PathBuf>,

        /// Instance name (name_prefix). Alternative to --config.
        #[arg(short, long)]
        name: Option<String>,
    },

    /// Print the PostgreSQL DSN for a cloud instance
    Dsn {
        /// Config file with [infra] section
        #[arg(short, long)]
        config: Option<PathBuf>,

        /// Instance name (name_prefix). Alternative to --config.
        #[arg(short, long)]
        name: Option<String>,
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
            phase,
            index,
            dry_run,
            rebuild,
            reload,
            ..
        } => {
            let mut groups = rekall_lib::config::resolve_config_groups(&configs)?;

            let phases = if let Some(ref p) = phase {
                vec![p.parse().map_err(|e: String| anyhow::anyhow!(e))?]
            } else {
                Phase::all().to_vec()
            };

            let num_groups = groups.len();
            for group in &mut groups {
                resolve_infra_dsn(&mut group.config);
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
            let mut cfg = BenchmarkConfig::from_path(&config)?;
            resolve_infra_dsn(&mut cfg);
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
            resolve_infra_dsn(&mut cfg);
            if let Some(p) = parallelism {
                cfg.load.parallelism = p;
            }
            let outputs = build_outputs(&cfg);
            let mut agent = Agent::new(cfg, outputs, cancel_registry.clone());
            agent.run(&[Phase::Load], None, false, false).await?;
        }

        Command::Index { config, name } => {
            let mut cfg = BenchmarkConfig::from_path(&config)?;
            resolve_infra_dsn(&mut cfg);
            let outputs = build_outputs(&cfg);
            let mut agent = Agent::new(cfg, outputs, cancel_registry.clone());
            agent
                .run(&[Phase::Index], name.as_deref(), false, false)
                .await?;
        }

        Command::Search { config, index } => {
            let mut cfg = BenchmarkConfig::from_path(&config)?;
            resolve_infra_dsn(&mut cfg);
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
            let mut cfg = BenchmarkConfig::from_path(&config)?;
            resolve_infra_dsn(&mut cfg);
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

fn load_infra_config(
    config: Option<&PathBuf>,
) -> anyhow::Result<rekall_lib::infra::config::InfraConfig> {
    if let Some(path) = config {
        let cfg = BenchmarkConfig::from_path(path)?;
        cfg.infra.ok_or_else(|| {
            anyhow::anyhow!(
                "Config '{}' has no [infra] section. Add an [infra] section or use defaults.",
                path.display()
            )
        })
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
            ..Default::default()
        })
    } else {
        load_infra_config(config)
    }
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
                println!("   cp examples/cloud-benchmark.local.toml.example \\");
                println!("      examples/cloud-benchmark.local.toml");
                println!("   Then set ssh_key, iam_instance_profile, and git URLs.");
                println!();
                step += 1;
            }

            println!("{step}. Provision the instance:");
            if let Some(ref cfg_path) = config {
                println!("   rekall infra up -c {}", cfg_path.display());
            } else {
                println!("   rekall infra up -c examples/cloud-benchmark.toml");
            }
        }

        InfraCommand::Up {
            config,
            region,
            instance_type,
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
            let output = mgr.up()?;

            println!();
            println!("Instance ready:");
            println!("  IP:            {}", output.public_ip);
            println!(
                "  Instance:      {} ({})",
                output.instance_id, output.instance_type
            );
            println!("  DSN:           {}", output.dsn);
            println!("  IAM role:      {}", output.iam_profile);
            println!();
            println!("SSH:  {}", mgr.ssh_command()?);
            println!("DSN:  rekall infra dsn");
        }

        InfraCommand::Down { config, name } => {
            let infra_cfg = resolve_infra_config(config.as_ref(), name.as_ref())?;
            let mgr = rekall_lib::infra::InfraManager::new(&infra_cfg)?;
            mgr.down()?;
            println!("Infrastructure '{}' destroyed.", infra_cfg.name_prefix);
        }

        InfraCommand::Status { config, name } => {
            let infra_cfg = resolve_infra_config(config.as_ref(), name.as_ref())?;
            let mgr = rekall_lib::infra::InfraManager::new(&infra_cfg)?;
            let status = mgr.status()?;

            println!("Instance: {}", infra_cfg.name_prefix);
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

        InfraCommand::List => {
            let instances = rekall_lib::infra::list_instances()?;
            if instances.is_empty() {
                println!("No instances provisioned.");
                println!(
                    "Run `rekall infra init -c examples/cloud-benchmark.toml` to get started."
                );
                return Ok(());
            }

            for inst in &instances {
                let state = if inst.status.running {
                    "running"
                } else {
                    "stopped"
                };
                let ip = inst.status.public_ip.as_deref().unwrap_or("-");
                let itype = inst.status.instance_type.as_deref().unwrap_or("-");
                let pg = if inst.status.pg_ready { "ready" } else { "-" };
                println!(
                    "{:<20} {:<10} {:<16} {:<14} pg: {}",
                    inst.name, state, ip, itype, pg
                );
            }
        }

        InfraCommand::Ssh { config, name } => {
            let infra_cfg = resolve_infra_config(config.as_ref(), name.as_ref())?;
            let mgr = rekall_lib::infra::InfraManager::new(&infra_cfg)?;
            let cmd = mgr.ssh_command()?;

            let parts: Vec<&str> = cmd.split_whitespace().collect();
            if parts.len() < 2 {
                return Err(anyhow::anyhow!("Invalid SSH command: {cmd}"));
            }
            let status = std::process::Command::new(parts[0])
                .args(&parts[1..])
                .status()?;

            std::process::exit(status.code().unwrap_or(1));
        }

        InfraCommand::Dsn { config, name } => {
            let infra_cfg = resolve_infra_config(config.as_ref(), name.as_ref())?;
            let mgr = rekall_lib::infra::InfraManager::new(&infra_cfg)?;
            let dsn = mgr.dsn()?;
            println!("{dsn}");
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

/// If [infra] is configured and running, override the connection DSN.
fn resolve_infra_dsn(config: &mut BenchmarkConfig) {
    if let Some(ref infra) = config.infra {
        if let Some(dsn) = rekall_lib::infra::resolve_infra_dsn(infra) {
            tracing::info!("Using infrastructure DSN: {dsn}");
            config.connection.dsn = dsn;
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
