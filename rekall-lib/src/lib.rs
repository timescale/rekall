#![allow(clippy::collapsible_if)]

pub mod backend;
pub mod config;
pub mod dataset;
pub mod diagnostics;
pub mod environment;
pub mod error;
pub mod infra;
pub mod metrics;
pub mod oscache;
pub mod output;
pub mod phase;
pub mod progress;
pub mod publish;
pub mod run_dir;

use std::sync::Arc;
use std::time::Instant;

use backend::DataLoader;
use config::{BenchmarkConfig, CacheMode};
use dataset::DatasetReader;
use error::Result;
use metrics::{IndexBuildResult, PgRuntimeInfo, RunEnvironment, SearchResult};
use output::OutputSink;
use phase::Phase;
use progress::{progress_channel, ProgressEvent};
use tokio_postgres::CancelToken;

fn save_run_info(
    config: &BenchmarkConfig,
    env: &RunEnvironment,
    pg_runtime: Option<&PgRuntimeInfo>,
) {
    #[derive(serde::Serialize)]
    struct RunInfo<'a> {
        environment: &'a RunEnvironment,
        pg_runtime: Option<&'a PgRuntimeInfo>,
    }

    let info = RunInfo {
        environment: env,
        pg_runtime,
    };

    let dir = &config.output.results_dir;
    let ts = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let path = format!("{dir}/run-info-{ts}.json");
    if let Some(parent) = std::path::Path::new(&path).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(&info) {
        let _ = std::fs::write(&path, json);
        tracing::info!("Run info saved to {path}");
    }
}

/// Shared registry of active PG cancel tokens. On Ctrl+C, all
/// registered tokens are cancelled to stop in-flight PG queries.
pub type CancelRegistry = Arc<tokio::sync::Mutex<Vec<CancelToken>>>;

pub fn new_cancel_registry() -> CancelRegistry {
    Arc::new(tokio::sync::Mutex::new(Vec::new()))
}

/// Spawn a Ctrl+C handler that cancels all registered PG backends.
pub fn install_signal_handler(registry: CancelRegistry) {
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            eprintln!("\nInterrupted — cancelling PostgreSQL queries...");
            let tokens = registry.lock().await;
            for token in tokens.iter() {
                let _ = token.cancel_query(tokio_postgres::NoTls).await;
            }
            // Give PG a moment to process cancellations
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            std::process::exit(130);
        }
    });
}

pub struct Agent {
    config: BenchmarkConfig,
    outputs: Vec<Box<dyn OutputSink>>,
    registry: backend::BackendRegistry,
    cancel_registry: CancelRegistry,
    /// When true, the search phase re-measures every sweep point instead of
    /// skipping ones already saved in the results dir (`search --rerun`).
    rerun: bool,
    /// Free-form description of what this run is for (`run --comment`),
    /// recorded in the run environment metadata.
    comment: Option<String>,
}

impl Agent {
    pub fn new(
        config: BenchmarkConfig,
        outputs: Vec<Box<dyn OutputSink>>,
        cancel_registry: CancelRegistry,
    ) -> Self {
        Self {
            config,
            outputs,
            registry: backend::BackendRegistry::new(),
            cancel_registry,
            rerun: false,
            comment: None,
        }
    }

    /// Re-measure every search sweep point, ignoring saved results
    /// (`search --rerun`). Default is to skip already-completed points.
    pub fn set_rerun(&mut self, rerun: bool) {
        self.rerun = rerun;
    }

    /// Attach a free-form comment describing the purpose of this run
    /// (`run --comment`). Recorded in the run metadata and shown on
    /// reports and published pages.
    pub fn set_comment(&mut self, comment: Option<String>) {
        self.comment = comment;
    }

    async fn capture_pg_runtime(&self) -> Option<PgRuntimeInfo> {
        let mut pg = backend::postgres::PgConnection::new(
            &self.config.connection,
            &self.config.table_name(),
            self.config.dataset.dimension,
            self.config.dataset.metric,
        );
        if pg.connect().await.is_err() {
            return None;
        }
        pg.runtime_info().await.ok()
    }

    /// Observe the storage PG is actually running on, using the server's own
    /// `data_directory` to find the device. Deliberately separate from
    /// `collect_host_info()`, which has no DB connection: resolving the real
    /// device needs to ask PG where its data lives rather than assume the
    /// root volume.
    async fn capture_storage(&self) -> Option<crate::metrics::StorageInfo> {
        let mut pg = backend::postgres::PgConnection::new(
            &self.config.connection,
            &self.config.table_name(),
            self.config.dataset.dimension,
            self.config.dataset.metric,
        );
        if pg.connect().await.is_err() {
            // Still worth recording the root volume: knowing the media and
            // provisioned IOPS beats knowing nothing.
            return environment::collect_storage_info(None);
        }
        // data_directory is superuser / pg_read_all_settings only; fall back
        // to the root volume when the benchmark user cannot read it.
        let data_dir = pg
            .client()
            .query_one("SHOW data_directory", &[])
            .await
            .ok()
            .map(|r| r.get::<_, String>(0));
        environment::collect_storage_info(data_dir.as_deref())
    }

    /// If the dataset file doesn't exist locally and a URI is configured,
    /// download it before running phases that need it.
    async fn ensure_dataset(&mut self, phases: &[Phase]) -> Result<()> {
        // Init is in here too: the column type it creates now comes from the
        // source file's precision, so it cannot run before the file is there.
        let needs_dataset = phases
            .iter()
            .any(|p| matches!(p, Phase::Init | Phase::Load | Phase::Search));
        if !needs_dataset {
            return Ok(());
        }

        let uri = match self.config.dataset.uri {
            Some(ref u) => u.clone(),
            None => return Ok(()),
        };

        let dest = self.config.dataset.resolved_path();
        if dest.exists() {
            return Ok(());
        }

        for output in self.outputs.iter_mut() {
            output.on_progress(&ProgressEvent::Message {
                text: format!(
                    "Dataset not found at {}. Downloading from {uri}",
                    dest.display()
                ),
            });
        }

        dataset::download::ensure_available(&uri, &dest).await?;

        for output in self.outputs.iter_mut() {
            output.on_progress(&ProgressEvent::Message {
                text: format!("Dataset ready at {}", dest.display()),
            });
        }

        Ok(())
    }

    pub async fn run(
        &mut self,
        phases: &[Phase],
        filter_index: Option<&str>,
        rebuild_indexes: bool,
        reload_data: bool,
    ) -> Result<()> {
        self.ensure_dataset(phases).await?;

        let mut run_env = environment::collect(self.comment.clone());

        let mut index_builds: Vec<IndexBuildResult> = Vec::new();
        let mut search_results: Vec<SearchResult> = Vec::new();
        let mut pg_runtime: Option<PgRuntimeInfo> = None;

        if phases
            .iter()
            .any(|p| matches!(p, Phase::Init | Phase::Load | Phase::Index | Phase::Search))
        {
            pg_runtime = self.capture_pg_runtime().await;
            if let Some(ref info) = pg_runtime {
                for output in self.outputs.iter_mut() {
                    output.on_pg_runtime(info);
                }
            }
            // Observe the storage the benchmark is about to run on. Recorded
            // whatever the config claimed: a config is a request, the device
            // is the fact.
            run_env.host.storage = self.capture_storage().await;
            if let Some(ref st) = run_env.host.storage {
                let iops = st
                    .ebs_provisioned_iops
                    .map(|i| i.to_string())
                    .unwrap_or_else(|| st.ebs_source.clone());
                for output in self.outputs.iter_mut() {
                    output.on_progress(&ProgressEvent::Message {
                        text: format!(
                            "Storage: {} on {} ({}), provisioned IOPS {}",
                            st.data_directory.as_deref().unwrap_or("(root volume)"),
                            st.device,
                            st.media,
                            iops,
                        ),
                    });
                }
            }
        }

        // Write run-info.json early so metadata survives crashes
        save_run_info(&self.config, &run_env, pg_runtime.as_ref());

        // When both Index and Search are requested, interleave them
        // per-index: build → search → drop → next. This allows exploring
        // multiple index configs on the same column in one run.
        let has_index = phases.contains(&Phase::Index);
        let has_search = phases.contains(&Phase::Search);
        let interleave = has_index && has_search;

        // Check if table already has the expected data — skip init/load unless --reload
        let skip_data_phases =
            if !reload_data && (phases.contains(&Phase::Init) || phases.contains(&Phase::Load)) {
                let pg = backend::postgres::PgConnection::new(
                    &self.config.connection,
                    &self.config.table_name(),
                    self.config.dataset.dimension,
                    self.config.dataset.metric,
                );
                if let Ok(true) = pg.table_exists().await {
                    let row_count = pg.row_count().await.unwrap_or(0);
                    if row_count > 0 {
                        // Verify against dataset size
                        let dataset_path = self.config.dataset.resolved_path();
                        if let Ok(reader) = dataset::hdf5::Hdf5Reader::open(
                            &dataset_path,
                            self.config.dataset.metric,
                            self.config.dataset.dimension,
                        ) {
                            let expected = reader.meta().num_vectors;
                            if row_count == expected {
                                true
                            } else {
                                for output in self.outputs.iter_mut() {
                                    output.on_progress(&ProgressEvent::Message {
                                        text: format!(
                                            "Table '{}' has {row_count} rows but dataset has \
                                         {expected} vectors — recreating and reloading it.",
                                            self.config.table_name(),
                                        ),
                                    });
                                }
                                false
                            }
                        } else {
                            true // can't check dataset, assume table is fine
                        }
                    } else {
                        false
                    }
                } else {
                    false
                }
            } else {
                false
            };

        // Init is DROP TABLE ... CASCADE then CREATE, so running it against
        // a table that is already there destroys it -- including a table the
        // user created themselves, or one whose data is mid-load. Init is
        // therefore "ensure the table exists", not "recreate it": skip it
        // whenever the table is present. --reload is the explicit opt-in to
        // recreate, and skip_data_phases above still short-circuits both
        // phases when the data is already fully loaded.
        //
        // A pre-existing table with the wrong shape now fails at COPY rather
        // than being silently replaced, which is the better failure: it is
        // the user's table.
        let skip_init_only =
            !reload_data && !skip_data_phases && phases.contains(&Phase::Init) && {
                let pg = backend::postgres::PgConnection::new(
                    &self.config.connection,
                    &self.config.table_name(),
                    self.config.dataset.dimension,
                    self.config.dataset.metric,
                );
                matches!(pg.table_exists().await, Ok(true))
            };

        // An existing table is reused, not recreated, and the reuse check
        // above compares only row counts -- so a table whose `v` column is a
        // different type than this run resolves to would sail through every
        // phase and have its search silently attributed to the wrong heap.
        // Tolerant of a missing dataset file (an index-only run does not need
        // one); the mismatch itself is fatal.
        if phases
            .iter()
            .any(|p| matches!(p, Phase::Init | Phase::Load | Phase::Index | Phase::Search))
        {
            if let Ok(kind) = self.config.resolve_storage() {
                let mut pg = backend::postgres::PgConnection::new(
                    &self.config.connection,
                    &self.config.table_name(),
                    self.config.dataset.dimension,
                    self.config.dataset.metric,
                );
                pg.set_storage(kind);
                if let Ok(true) = pg.table_exists().await {
                    pg.verify_column_type().await?;
                }
            }
        }

        for &phase in phases {
            // With interleave, Index and Search run together after this loop
            // (run_index_search_interleaved) and the report runs after them.
            // Skip all three here so Report doesn't fire prematurely — before
            // any search results exist — and print an empty summary.
            if interleave
                && (phase == Phase::Index || phase == Phase::Search || phase == Phase::Report)
            {
                continue;
            }

            if skip_init_only && phase == Phase::Init {
                for output in self.outputs.iter_mut() {
                    output.on_progress(&ProgressEvent::Message {
                        text: format!(
                            "Table '{}' already exists, skipping init",
                            self.config.table_name(),
                        ),
                    });
                }
                continue;
            }

            if skip_data_phases && (phase == Phase::Init || phase == Phase::Load) {
                let pg = backend::postgres::PgConnection::new(
                    &self.config.connection,
                    &self.config.table_name(),
                    self.config.dataset.dimension,
                    self.config.dataset.metric,
                );
                let count = pg.row_count().await.unwrap_or(0);
                for output in self.outputs.iter_mut() {
                    output.on_progress(&ProgressEvent::Message {
                        text: format!(
                            "Table '{}' already has {count} rows, skipping {phase}",
                            self.config.table_name(),
                        ),
                    });
                }
                continue;
            }

            self.run_phase(
                phase,
                filter_index,
                &mut index_builds,
                &mut search_results,
                &run_env,
                &pg_runtime,
            )
            .await?;
        }

        if interleave {
            self.run_index_search_interleaved(
                filter_index,
                rebuild_indexes,
                &mut index_builds,
                &mut search_results,
                &run_env,
                &pg_runtime,
            )
            .await?;
        }

        // Always run report at the end if it was requested
        if phases.contains(&Phase::Report) && interleave {
            let config_value = serde_json::to_value(&self.config).unwrap_or_default();
            phase::report::run(
                &self.config.dataset.name,
                Some(run_env),
                config_value,
                pg_runtime,
                index_builds,
                search_results,
                &mut self.outputs,
            );
        }

        Ok(())
    }

    /// Apply one phase's `[phase_pg_config]` overrides: ALTER SYSTEM each
    /// GUC, then either pg_reload_conf() or -- when any changed GUC is
    /// restart-context (e.g. shared_buffers) -- run the configured
    /// restart_command and wait for the server to come back.
    /// Apply a phase's `[phase_pg_config]` GUC overrides via `ALTER SYSTEM`,
    /// returning a restore plan that puts every touched GUC back to its
    /// pre-phase state. The suite declares only what the phase needs (e.g.
    /// a small `shared_buffers` for builds); the box's own configuration is
    /// snapshotted here and reinstated by [`Self::restore_phase_pg_config`],
    /// so a run leaves `postgresql.auto.conf` exactly as it found it.
    async fn apply_phase_pg_config(
        &mut self,
        phase_label: &str,
        gucs: &std::collections::HashMap<String, String>,
    ) -> Result<PgRestorePlan> {
        if gucs.is_empty() {
            return Ok(Vec::new());
        }
        let valid = |s: &str| {
            !s.is_empty()
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
        };
        let pg = self.admin_pg();
        let client = pg.open_connection().await?;
        let names: Vec<String> = gucs.keys().cloned().collect();
        for k in &names {
            if !valid(k) {
                return Err(crate::error::Error::Config(format!(
                    "phase_pg_config: invalid GUC name '{k}'"
                )));
            }
        }
        // Snapshot the current postgresql.auto.conf entries for the GUCs
        // we are about to override (raw file values, so units and quoting
        // survive the round trip; last entry wins as in PostgreSQL).
        let mut prior: std::collections::HashMap<String, String> = Default::default();
        for row in client
            .query(
                "SELECT DISTINCT ON (name) name, setting FROM pg_file_settings \
                 WHERE sourcefile LIKE '%postgresql.auto.conf' AND name = ANY($1) \
                 ORDER BY name, seqno DESC",
                &[&names],
            )
            .await?
        {
            prior.insert(row.get(0), row.get(1));
        }
        let plan: PgRestorePlan = names
            .iter()
            .map(|k| (k.clone(), prior.get(k).cloned()))
            .collect();

        for (k, v) in gucs {
            let quoted = v.replace('\'', "''");
            client
                .execute(&format!("ALTER SYSTEM SET {k} = '{quoted}'"), &[])
                .await?;
        }
        self.sync_pg_config(
            &pg,
            client,
            &names,
            &format!("Applying {phase_label}-phase pg config"),
        )
        .await?;
        Ok(plan)
    }

    /// Reinstate the pre-phase PostgreSQL config recorded by
    /// [`Self::apply_phase_pg_config`]: GUCs the box had overridden in
    /// `postgresql.auto.conf` get their old value back verbatim; GUCs it
    /// hadn't are `ALTER SYSTEM RESET`, so nothing of the phase override
    /// outlives the phase.
    async fn restore_phase_pg_config(
        &mut self,
        phase_label: &str,
        plan: PgRestorePlan,
    ) -> Result<()> {
        if plan.is_empty() {
            return Ok(());
        }
        let pg = self.admin_pg();
        let client = pg.open_connection().await?;
        let names: Vec<String> = plan.iter().map(|(k, _)| k.clone()).collect();
        for (k, prior) in &plan {
            client
                .execute(&restore_guc_sql(k, prior.as_deref()), &[])
                .await?;
        }
        self.sync_pg_config(
            &pg,
            client,
            &names,
            &format!("Restoring pre-{phase_label} pg config"),
        )
        .await
    }

    fn admin_pg(&self) -> backend::postgres::PgConnection {
        backend::postgres::PgConnection::new(
            &self.config.connection,
            &self.config.table_name(),
            self.config.dataset.dimension,
            self.config.dataset.metric,
        )
    }

    /// Make pending `ALTER SYSTEM` changes to `names` effective: reload if
    /// every touched GUC is reloadable, otherwise restart PostgreSQL and
    /// wait for it to accept connections again. The restart mechanism is
    /// `phase_pg_config.restart_command` if configured (an escape hatch for
    /// unusual environments) and auto-detected from the server otherwise.
    async fn sync_pg_config(
        &mut self,
        pg: &backend::postgres::PgConnection,
        client: tokio_postgres::Client,
        names: &[String],
        action: &str,
    ) -> Result<()> {
        let needs_restart = client
            .query(
                "SELECT count(*) FROM pg_settings WHERE name = ANY($1) \
                 AND context = 'postmaster'",
                &[&names],
            )
            .await?
            .first()
            .map(|r| r.get::<_, i64>(0) > 0)
            .unwrap_or(false);

        if needs_restart {
            let cmd = match self.config.phase_pg_config.restart_command.clone() {
                Some(cmd) => cmd,
                None => detect_restart_command(&client).await?,
            };
            drop(client);
            for output in self.outputs.iter_mut() {
                output.on_progress(&ProgressEvent::Message {
                    text: format!(
                        "{action} ({}) and restarting PostgreSQL via '{cmd}'",
                        names.join(", ")
                    ),
                });
            }
            let status = tokio::process::Command::new("sh")
                .arg("-c")
                .arg(&cmd)
                .status()
                .await
                .map_err(|e| {
                    crate::error::Error::Config(format!("restart command failed to spawn: {e}"))
                })?;
            if !status.success() {
                return Err(crate::error::Error::Config(format!(
                    "restart command '{cmd}' exited with {status}"
                )));
            }
            // Wait for the server to accept connections again.
            let deadline = Instant::now() + std::time::Duration::from_secs(120);
            loop {
                match pg.open_connection().await {
                    Ok(_) => break,
                    Err(e) if Instant::now() >= deadline => {
                        return Err(crate::error::Error::Config(format!(
                            "PostgreSQL did not come back after restart: {e}"
                        )));
                    }
                    Err(_) => tokio::time::sleep(std::time::Duration::from_secs(2)).await,
                }
            }
        } else {
            client.execute("SELECT pg_reload_conf()", &[]).await?;
            for output in self.outputs.iter_mut() {
                output.on_progress(&ProgressEvent::Message {
                    text: format!("{action} ({}) via reload", names.join(", ")),
                });
            }
        }
        Ok(())
    }

    async fn run_phase(
        &mut self,
        phase: Phase,
        filter_index: Option<&str>,
        index_builds: &mut Vec<IndexBuildResult>,
        search_results: &mut Vec<SearchResult>,
        run_env: &RunEnvironment,
        pg_runtime: &Option<PgRuntimeInfo>,
    ) -> Result<()> {
        let start = Instant::now();

        for output in self.outputs.iter_mut() {
            output.on_phase_start(&phase.to_string(), "");
        }

        match phase {
            Phase::Init => {
                phase::init::run(&self.config).await?;
            }
            Phase::Load => {
                let (tx, mut rx) = progress_channel();
                let config = self.config.clone();
                let load_handle = tokio::spawn(async move { phase::load::run(&config, &tx).await });

                let outputs = &mut self.outputs;
                while let Some(event) = rx.recv().await {
                    for output in outputs.iter_mut() {
                        output.on_progress(&event);
                    }
                }
                load_handle.await.unwrap()?;
            }
            Phase::Index => {
                let gucs = self.config.phase_pg_config.index.clone();
                let plan = self.apply_phase_pg_config("index", &gucs).await?;
                let (tx, mut rx) = progress_channel();
                let config = self.config.clone();
                let registry = backend::BackendRegistry::new();
                let filter = filter_index.map(|s| s.to_string());
                let build_handle = tokio::spawn(async move {
                    phase::index::run(&config, &registry, filter.as_deref(), Some(&tx)).await
                });

                let outputs = &mut self.outputs;
                while let Some(event) = rx.recv().await {
                    for output in outputs.iter_mut() {
                        output.on_progress(&event);
                    }
                }
                // Restore the box's config whether or not the build
                // succeeded; a failed build must not leave the phase
                // override behind. The build error takes precedence.
                let build_result = build_handle.await.unwrap();
                let restore_result = self.restore_phase_pg_config("index", plan).await;
                let results = build_result?;
                restore_result?;
                for result in &results {
                    for output in self.outputs.iter_mut() {
                        output.on_index_built(result);
                    }
                }
                index_builds.extend(results);
            }
            Phase::Search => {
                let gucs = self.config.phase_pg_config.search.clone();
                let plan = self.apply_phase_pg_config("search", &gucs).await?;
                let search_result = self
                    .run_search(
                        filter_index,
                        Some(run_env.clone()),
                        pg_runtime.clone(),
                        index_builds.clone(),
                    )
                    .await;
                let restore_result = self.restore_phase_pg_config("search", plan).await;
                let results = search_result?;
                restore_result?;
                for result in &results {
                    for output in self.outputs.iter_mut() {
                        output.on_search_result(result);
                    }
                }
                search_results.extend(results);
            }
            Phase::Report => {
                let pg_runtime = self.capture_pg_runtime().await;
                let config_value = serde_json::to_value(&self.config).unwrap_or_default();
                phase::report::run(
                    &self.config.dataset.name,
                    Some(run_env.clone()),
                    config_value,
                    pg_runtime,
                    index_builds.clone(),
                    search_results.clone(),
                    &mut self.outputs,
                );
            }
        }

        let elapsed = start.elapsed().as_secs_f64();
        for output in self.outputs.iter_mut() {
            output.on_phase_end(&phase.to_string(), elapsed);
        }

        Ok(())
    }

    /// Build indexes that don't exist yet, then for each index: disable
    /// all others, run search sweeps, re-enable. Avoids expensive rebuilds
    /// when indexes already exist.
    async fn run_index_search_interleaved(
        &mut self,
        filter_index: Option<&str>,
        rebuild: bool,
        index_builds: &mut Vec<IndexBuildResult>,
        search_results: &mut Vec<SearchResult>,
        run_env: &RunEnvironment,
        pg_runtime: &Option<PgRuntimeInfo>,
    ) -> Result<()> {
        let index_configs: Vec<_> = self
            .config
            .index
            .iter()
            .filter(|idx| {
                if let Some(name) = filter_index {
                    idx.name == name
                } else {
                    true
                }
            })
            .cloned()
            .collect();

        // Admin connection to normalize the table's parallel_workers reloption
        // before each build, mirroring phase::index::run. The interleaved path
        // previously skipped this, so table_parallel_workers was ignored in a
        // full `run` and meerkat fell back to the planner's heap-size heuristic.
        let admin_pg = backend::postgres::PgConnection::new(
            &self.config.connection,
            &self.config.table_name(),
            self.config.dataset.dimension,
            self.config.dataset.metric,
        );
        let admin = admin_pg.open_connection().await?;

        {
            let gucs = self.config.phase_pg_config.index.clone();
            let plan = self.apply_phase_pg_config("index", &gucs).await?;
            let build_result = self
                .interleaved_build(&index_configs, &admin, rebuild, index_builds)
                .await;
            // Restore the box's config whether or not the phase succeeded;
            // the phase error takes precedence.
            let restore_result = self.restore_phase_pg_config("index", plan).await;
            build_result?;
            restore_result?;
        }

        {
            let gucs = self.config.phase_pg_config.search.clone();
            let plan = self.apply_phase_pg_config("search", &gucs).await?;
            let search_result = self
                .interleaved_search(
                    &index_configs,
                    index_builds,
                    search_results,
                    run_env,
                    pg_runtime,
                )
                .await;
            let restore_result = self.restore_phase_pg_config("search", plan).await;
            search_result?;
            restore_result?;
        }
        Ok(())
    }

    /// Phase 1 of the interleaved runner: build any indexes that don't
    /// exist yet.
    async fn interleaved_build(
        &mut self,
        index_configs: &[config::IndexConfig],
        admin: &tokio_postgres::Client,
        rebuild: bool,
        index_builds: &mut Vec<IndexBuildResult>,
    ) -> Result<()> {
        for idx_config in index_configs {
            let (progress_tx, mut progress_rx) = progress_channel();
            let backend = self
                .registry
                .create(
                    &self.config.connection,
                    &self.config.table_name(),
                    self.config.dataset.dimension,
                    self.config.dataset.metric,
                    idx_config,
                    Some(&self.cancel_registry),
                    crate::config::QueryMode::Default,
                    Some(progress_tx),
                )
                .await?;

            let exists = backend.index_exists().await?;
            if exists && rebuild {
                backend.drop_index().await?;
                for output in self.outputs.iter_mut() {
                    output.on_progress(&ProgressEvent::Message {
                        text: format!(
                            "Dropping index '{}' for rebuild (--rebuild)",
                            idx_config.name,
                        ),
                    });
                }
            }
            if exists && !rebuild {
                let size = backend.index_size().await.unwrap_or(0);
                let size_mb = size as f64 / (1024.0 * 1024.0);
                for output in self.outputs.iter_mut() {
                    output.on_progress(&ProgressEvent::Message {
                        text: format!(
                            "Index '{}' already exists ({:.1} MB), skipping build",
                            idx_config.name, size_mb,
                        ),
                    });
                }
                // Recover the persisted build result so reused indexes still
                // report build time/size/workers (e.g. in the build-time
                // graph) instead of silently dropping out.
                if let Some(prior) =
                    crate::phase::index::load_build_result(&self.config, &idx_config.name)
                {
                    index_builds.push(prior);
                } else {
                    for output in self.outputs.iter_mut() {
                        output.on_progress(&ProgressEvent::Message {
                            text: format!(
                                "No saved build log for '{}'; build time unavailable \
                                 (rebuild with --rebuild to record it)",
                                idx_config.name,
                            ),
                        });
                    }
                }
            } else {
                let idx_start = Instant::now();
                for output in self.outputs.iter_mut() {
                    output.on_phase_start("index", &idx_config.name);
                }

                crate::phase::index::normalize_table_parallel_workers(
                    admin,
                    &self.config.table_name(),
                    idx_config.table_parallel_workers,
                )
                .await?;

                // Same cache conditions for every arm (see
                // apply_build_cache_policy): otherwise build order decides
                // how warm the heap is for each build.
                let cache_label =
                    crate::phase::index::apply_build_cache_policy(&self.config).await?;

                let build_handle = tokio::spawn(async move { backend.build_index().await });

                let outputs = &mut self.outputs;
                while let Some(event) = progress_rx.recv().await {
                    for output in outputs.iter_mut() {
                        output.on_progress(&event);
                    }
                }
                let mut build_result = match build_handle.await.unwrap() {
                    Ok(r) => r,
                    Err(e) => {
                        if let Some(path) = diagnostics::report_if_crash(
                            &self.config.output.results_dir,
                            &self.config.connection.dsn,
                            &format!("build index '{}'", idx_config.name),
                            &e,
                        ) {
                            for output in self.outputs.iter_mut() {
                                output.on_progress(&ProgressEvent::Message {
                                    text: format!(
                                        "crash/disconnect building '{}' — diagnostics written to {}",
                                        idx_config.name,
                                        path.display()
                                    ),
                                });
                            }
                        }
                        return Err(e);
                    }
                };

                build_result.cache = Some(cache_label.to_string());

                // Persist the build result immediately so it survives the run
                // and is recoverable on later reuse (symmetric with the
                // load_build_result read-back above).
                crate::phase::index::save_build_result(&self.config, &build_result);

                for output in self.outputs.iter_mut() {
                    output.on_index_built(&build_result);
                }
                index_builds.push(build_result);

                let idx_elapsed = idx_start.elapsed().as_secs_f64();
                for output in self.outputs.iter_mut() {
                    output.on_phase_end("index", idx_elapsed);
                }
            }
        }
        Ok(())
    }

    /// Phase 2 of the interleaved runner: for each index, disable all
    /// other indexes, run its searches, then re-enable everything.
    async fn interleaved_search(
        &mut self,
        index_configs: &[config::IndexConfig],
        index_builds: &[IndexBuildResult],
        search_results: &mut Vec<SearchResult>,
        run_env: &RunEnvironment,
        pg_runtime: &Option<PgRuntimeInfo>,
    ) -> Result<()> {
        let pg = backend::postgres::PgConnection::new(
            &self.config.connection,
            &self.config.table_name(),
            self.config.dataset.dimension,
            self.config.dataset.metric,
        );
        // Connect to manage index enable/disable
        let mut pg_mgmt = backend::postgres::PgConnection::new(
            &self.config.connection,
            &self.config.table_name(),
            self.config.dataset.dimension,
            self.config.dataset.metric,
        );
        pg_mgmt.connect().await?;

        // Get ALL rekall indexes on the table, not just the ones in this config.
        // Other indexes from previous runs must also be disabled.
        let all_index_names = pg_mgmt.list_rekall_indexes().await?;

        for idx_config in index_configs {
            let has_searches = self
                .config
                .search
                .sweep
                .iter()
                .any(|s| s.index == idx_config.name)
                || self.config.query.iter().any(|q| q.index == idx_config.name);
            if !has_searches {
                continue;
            }

            // Resolve through the arm's physical-index redirection: this
            // management connection is built outside create_pg_backend.
            let active_idx = pg.index_sql_name(
                idx_config
                    .physical_index
                    .as_deref()
                    .unwrap_or(&idx_config.name),
            );

            // Disable all other indexes, enable the active one
            for idx_sql in &all_index_names {
                if idx_sql == &active_idx {
                    pg_mgmt.enable_index(idx_sql).await?;
                } else {
                    pg_mgmt.disable_index(idx_sql).await?;
                }
            }

            // Prewarm in warm mode, evict in cold mode. Order matters and
            // is measured: the heap goes first (buffer-warm prefix up to the
            // budget + blocking OS-cache read of the remainder — a shared-
            // buffers-resident heap slice serves rerank fetches buffer-fast,
            // worth ~20% QPS at low nprobe on 100M), the index last so LRU
            // keeps it fully resident on top of whatever heap slice fits.
            if self.config.search.cache == CacheMode::Warm {
                pg_mgmt.prewarm(&self.config.table_name()).await?;
                pg_mgmt.prewarm(&active_idx).await?;
            }

            let search_start = Instant::now();
            for output in self.outputs.iter_mut() {
                output.on_phase_start("search", &idx_config.name);
            }

            let (tx, mut rx) = progress_channel();
            let config = self.config.clone();
            let registry = backend::BackendRegistry::new();
            let idx_name = idx_config.name.clone();

            let env_clone = run_env.clone();
            let pg_clone = pg_runtime.clone();
            let builds_clone = index_builds.to_vec();
            let rerun = self.rerun;
            let search_handle = tokio::spawn(async move {
                phase::search::run(
                    &config,
                    &registry,
                    &tx,
                    Some(&idx_name),
                    Some(&env_clone),
                    pg_clone.as_ref(),
                    &builds_clone,
                    rerun,
                )
                .await
            });

            let outputs = &mut self.outputs;
            while let Some(event) = rx.recv().await {
                for output in outputs.iter_mut() {
                    output.on_progress(&event);
                }
            }

            let results = search_handle.await.unwrap()?;
            for result in &results {
                for output in self.outputs.iter_mut() {
                    output.on_search_result(result);
                }
            }
            search_results.extend(results);

            let search_elapsed = search_start.elapsed().as_secs_f64();
            for output in self.outputs.iter_mut() {
                output.on_phase_end("search", search_elapsed);
            }
        }

        // Re-enable all indexes when done
        for idx_sql in &all_index_names {
            let _ = pg_mgmt.enable_index(idx_sql).await;
        }

        Ok(())
    }

    async fn run_search(
        &mut self,
        filter_index: Option<&str>,
        environment: Option<RunEnvironment>,
        pg_runtime: Option<PgRuntimeInfo>,
        index_builds: Vec<IndexBuildResult>,
    ) -> Result<Vec<SearchResult>> {
        let (tx, mut rx) = progress_channel();
        let config = self.config.clone();
        let registry = backend::BackendRegistry::new();
        let filter = filter_index.map(|s| s.to_string());
        let rerun = self.rerun;

        let search_handle = tokio::spawn(async move {
            phase::search::run(
                &config,
                &registry,
                &tx,
                filter.as_deref(),
                environment.as_ref(),
                pg_runtime.as_ref(),
                &index_builds,
                rerun,
            )
            .await
        });

        let outputs = &mut self.outputs;
        while let Some(event) = rx.recv().await {
            for output in outputs.iter_mut() {
                output.on_progress(&event);
            }
        }

        search_handle.await.unwrap()
    }
}

/// Per-GUC pre-phase state captured by `apply_phase_pg_config`: the raw
/// `postgresql.auto.conf` value if the box had one, or `None` if the GUC
/// wasn't overridden there (restored with `ALTER SYSTEM RESET`).
type PgRestorePlan = Vec<(String, Option<String>)>;

/// SQL that puts one GUC back to its pre-phase `postgresql.auto.conf`
/// state. GUC names are validated at apply time; values are quote-escaped.
fn restore_guc_sql(name: &str, prior: Option<&str>) -> String {
    match prior {
        Some(v) => format!("ALTER SYSTEM SET {name} = '{}'", v.replace('\'', "''")),
        None => format!("ALTER SYSTEM RESET {name}"),
    }
}

/// Derive the systemd unit for a Debian/Ubuntu-packaged cluster from
/// `cluster_name` (e.g. `18/main` -> `postgresql@18-main`).
fn pg_unit_from_cluster_name(cluster_name: &str) -> Option<String> {
    let (version, cluster) = cluster_name.trim().split_once('/')?;
    if version.is_empty() || cluster.is_empty() {
        return None;
    }
    Some(format!("postgresql@{version}-{cluster}"))
}

/// Derive the systemd unit from a Debian/Ubuntu-layout data directory
/// (e.g. `/var/lib/postgresql/18/main` -> `postgresql@18-main`).
fn pg_unit_from_data_directory(datadir: &str) -> Option<String> {
    let parts: Vec<&str> = datadir.trim().trim_end_matches('/').split('/').collect();
    match parts.as_slice() {
        [.., "postgresql", version, cluster] if !version.is_empty() && !cluster.is_empty() => {
            Some(format!("postgresql@{version}-{cluster}"))
        }
        _ => None,
    }
}

/// Work out how to restart the PostgreSQL the DSN points at, by asking the
/// server where it lives: `cluster_name`/`data_directory` name the systemd
/// unit under Debian/Ubuntu packaging, with a generic `postgresql` unit as
/// fallback. Each candidate is verified to exist (`systemctl cat`) before
/// being chosen. `sudo -n` fails fast rather than prompting if the rekall
/// user can't restart services; `phase_pg_config.restart_command` remains
/// as the manual escape hatch for non-systemd environments.
async fn detect_restart_command(client: &tokio_postgres::Client) -> Result<String> {
    let show = |name: &'static str| async move {
        client
            .query_one(&format!("SHOW {name}"), &[])
            .await
            .map(|r| r.get::<_, String>(0))
    };
    let mut candidates: Vec<String> = Vec::new();
    if let Ok(cn) = show("cluster_name").await {
        candidates.extend(pg_unit_from_cluster_name(&cn));
    }
    if let Ok(dd) = show("data_directory").await {
        candidates.extend(pg_unit_from_data_directory(&dd));
    }
    candidates.push("postgresql".to_string());
    candidates.dedup();

    for unit in &candidates {
        let exists = tokio::process::Command::new("systemctl")
            .args(["cat", "--", unit])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await
            .map(|s| s.success())
            .unwrap_or(false);
        if exists {
            return Ok(format!("sudo -n systemctl restart {unit}"));
        }
    }
    Err(crate::error::Error::Config(format!(
        "cannot auto-detect how to restart PostgreSQL (no systemd unit found; \
         tried {}); set phase_pg_config.restart_command",
        candidates.join(", ")
    )))
}

#[cfg(test)]
mod phase_pg_tests {
    use super::*;

    #[test]
    fn restore_sql_resets_when_no_prior() {
        assert_eq!(
            restore_guc_sql("shared_buffers", None),
            "ALTER SYSTEM RESET shared_buffers"
        );
    }

    #[test]
    fn restore_sql_reinstates_prior_verbatim() {
        assert_eq!(
            restore_guc_sql("shared_buffers", Some("24GB")),
            "ALTER SYSTEM SET shared_buffers = '24GB'"
        );
        // Values from the file are quote-escaped on the way back.
        assert_eq!(
            restore_guc_sql("search_path", Some("it''s")),
            "ALTER SYSTEM SET search_path = 'it''''s'"
        );
    }

    #[test]
    fn unit_from_cluster_name() {
        assert_eq!(
            pg_unit_from_cluster_name("18/main").as_deref(),
            Some("postgresql@18-main")
        );
        assert_eq!(pg_unit_from_cluster_name("").as_deref(), None);
        assert_eq!(pg_unit_from_cluster_name("nodash").as_deref(), None);
        assert_eq!(pg_unit_from_cluster_name("18/").as_deref(), None);
    }

    #[test]
    fn unit_from_data_directory() {
        assert_eq!(
            pg_unit_from_data_directory("/var/lib/postgresql/18/main").as_deref(),
            Some("postgresql@18-main")
        );
        assert_eq!(
            pg_unit_from_data_directory("/var/lib/postgresql/18/main/").as_deref(),
            Some("postgresql@18-main")
        );
        assert_eq!(pg_unit_from_data_directory("/pgdata").as_deref(), None);
        assert_eq!(pg_unit_from_data_directory("").as_deref(), None);
    }

    fn test_agent(dsn: &str) -> Agent {
        let cfg: BenchmarkConfig = toml::from_str(&format!(
            r#"
[dataset]
name = "phase-pg-test"
metric = "angular"
dimension = 3

[connection]
dsn = "{dsn}"
"#
        ))
        .unwrap();
        Agent::new(cfg, Vec::new(), new_cancel_registry())
    }

    async fn show(pg: &backend::postgres::PgConnection, guc: &str) -> String {
        pg.open_connection()
            .await
            .unwrap()
            .query_one(&format!("SHOW {guc}"), &[])
            .await
            .unwrap()
            .get(0)
    }

    /// Live roundtrip on the reload path (needs REKALL_TEST_DSN pointing
    /// at a superuser connection): apply overrides the value for new
    /// sessions, restore reinstates the pre-apply state — both when the
    /// box had no auto.conf entry (RESET) and when it had one (verbatim).
    #[tokio::test]
    async fn phase_pg_apply_restore_roundtrip_live() {
        let Ok(dsn) = std::env::var("REKALL_TEST_DSN") else {
            eprintln!("Skipping PG test: REKALL_TEST_DSN not set");
            return;
        };
        let mut agent = test_agent(&dsn);
        let pg = agent.admin_pg();
        let settle = || tokio::time::sleep(std::time::Duration::from_millis(500));
        let original = show(&pg, "work_mem").await;

        // Case 1: no prior auto.conf entry -> restore RESETs.
        let gucs: std::collections::HashMap<String, String> =
            [("work_mem".to_string(), "77MB".to_string())].into();
        let plan = agent.apply_phase_pg_config("test", &gucs).await.unwrap();
        settle().await;
        assert_eq!(show(&pg, "work_mem").await, "77MB");
        agent.restore_phase_pg_config("test", plan).await.unwrap();
        settle().await;
        assert_eq!(show(&pg, "work_mem").await, original);

        // Case 2: the box had its own auto.conf override -> restore
        // reinstates it verbatim.
        let client = pg.open_connection().await.unwrap();
        client
            .execute("ALTER SYSTEM SET work_mem = '55MB'", &[])
            .await
            .unwrap();
        client
            .execute("SELECT pg_reload_conf()", &[])
            .await
            .unwrap();
        settle().await;
        let plan = agent.apply_phase_pg_config("test", &gucs).await.unwrap();
        settle().await;
        assert_eq!(show(&pg, "work_mem").await, "77MB");
        agent.restore_phase_pg_config("test", plan).await.unwrap();
        settle().await;
        assert_eq!(show(&pg, "work_mem").await, "55MB");

        // Leave the server as we found it.
        let client = pg.open_connection().await.unwrap();
        client
            .execute("ALTER SYSTEM RESET work_mem", &[])
            .await
            .unwrap();
        client
            .execute("SELECT pg_reload_conf()", &[])
            .await
            .unwrap();
    }

    /// Live roundtrip on the restart path, including restart-command
    /// auto-detection. Restarts the server twice — run explicitly:
    ///   REKALL_TEST_DSN=... cargo test -p rekall-lib \
    ///     phase_pg_restart_roundtrip_live -- --ignored
    #[tokio::test]
    #[ignore]
    async fn phase_pg_restart_roundtrip_live() {
        let Ok(dsn) = std::env::var("REKALL_TEST_DSN") else {
            eprintln!("Skipping PG test: REKALL_TEST_DSN not set");
            return;
        };
        let mut agent = test_agent(&dsn);
        let pg = agent.admin_pg();
        let original = show(&pg, "shared_buffers").await;
        assert_ne!(original, "1536MB", "pick a different test value");

        let gucs: std::collections::HashMap<String, String> =
            [("shared_buffers".to_string(), "1536MB".to_string())].into();
        let plan = agent.apply_phase_pg_config("test", &gucs).await.unwrap();
        assert_eq!(show(&pg, "shared_buffers").await, "1536MB");
        agent.restore_phase_pg_config("test", plan).await.unwrap();
        assert_eq!(show(&pg, "shared_buffers").await, original);
    }
}
