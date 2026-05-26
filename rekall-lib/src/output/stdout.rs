use tabled::settings::Style;
use tabled::{Table, Tabled};

use crate::metrics::{IndexBuildResult, ParetoPoint, PgRuntimeInfo, RunSummary, SearchResult};
use crate::progress::ProgressEvent;

use super::OutputSink;

pub struct StdoutSink {
    progress_bar: Option<indicatif::ProgressBar>,
    current_phase: String,
}

impl Default for StdoutSink {
    fn default() -> Self {
        Self::new()
    }
}

impl StdoutSink {
    pub fn new() -> Self {
        Self {
            progress_bar: None,
            current_phase: String::new(),
        }
    }

    fn finish_progress(&mut self) {
        if let Some(pb) = self.progress_bar.take() {
            pb.finish_and_clear();
        }
    }

    fn strip_phase_prefix(phase: &str) -> &str {
        phase.strip_prefix("building index: ").unwrap_or(phase)
    }
}

impl OutputSink for StdoutSink {
    fn on_phase_start(&mut self, phase: &str, detail: &str) {
        if detail.is_empty() {
            println!("\n--- {phase} ---");
        } else {
            println!("\n--- {phase}: {detail} ---");
        }
    }

    fn on_phase_end(&mut self, phase: &str, elapsed_secs: f64) {
        self.finish_progress();
        println!("  {phase} completed in {:.1}s", elapsed_secs);
    }

    fn on_pg_runtime(&mut self, info: &PgRuntimeInfo) {
        println!("\n--- PostgreSQL Runtime ---");
        println!("  {}", info.version);
        println!("  Settings:");
        for (name, value) in &info.settings {
            println!("    {name:40} = {value}");
        }
        if !info.extensions.is_empty() {
            println!("  Extensions:");
            for (name, ext) in &info.extensions {
                let mut detail = ext.version.clone();
                if let Some(ref lib_ver) = ext.library_version {
                    detail = format!("{detail} (lib: {lib_ver})");
                }
                if let Some(ref commit) = ext.git_commit {
                    detail = format!("{detail} (commit: {commit})");
                }
                println!("    {name:40} {detail}");
            }
        }
    }

    fn on_progress(&mut self, event: &ProgressEvent) {
        match event {
            ProgressEvent::LoadProgress {
                loaded,
                total,
                rows_per_sec: _,
            } => {
                let pb = self.progress_bar.get_or_insert_with(|| {
                    let pb = indicatif::ProgressBar::new(*total);
                    pb.set_style(
                        indicatif::ProgressStyle::default_bar()
                            .template(
                                "  Loading [{bar:40}] {pos}/{len} \
                                 ({per_sec}, ETA {eta})",
                            )
                            .unwrap()
                            .progress_chars("=> "),
                    );
                    pb
                });
                pb.set_position(*loaded);
            }
            ProgressEvent::IndexBuildStart { index_name } => {
                self.finish_progress();
                self.current_phase.clear();
                println!("  Building index '{index_name}'...");
            }
            ProgressEvent::IndexBuildProgress {
                phase,
                tuples_done,
                tuples_total,
                workers,
                rss_bytes,
            } => {
                let phase = Self::strip_phase_prefix(phase);
                let rss_mb = *rss_bytes as f64 / (1024.0 * 1024.0);

                if phase != self.current_phase {
                    self.finish_progress();
                    self.current_phase = phase.to_string();
                }

                let status = if rss_mb > 0.0 {
                    format!("{phase} [{workers}w, {rss_mb:.0}MB]")
                } else {
                    format!("{phase} [{workers}w]")
                };

                if *tuples_total > 0 && *tuples_done <= *tuples_total {
                    let pb = self.progress_bar.get_or_insert_with(|| {
                        let pb = indicatif::ProgressBar::new(*tuples_total as u64);
                        pb.set_style(
                            indicatif::ProgressStyle::default_bar()
                                .template(
                                    "    {msg} [{bar:30}] {pos}/{len} \
                                     ({per_sec}, ETA {eta})",
                                )
                                .unwrap()
                                .progress_chars("=> "),
                        );
                        pb
                    });
                    pb.set_message(status);
                    pb.set_length(*tuples_total as u64);
                    pb.set_position(*tuples_done as u64);
                    pb.tick();
                } else {
                    use std::io::Write;
                    let mut stderr = std::io::stderr().lock();
                    let _ = write!(stderr, "\r\x1b[K    {status}");
                    let _ = stderr.flush();
                }
            }
            ProgressEvent::IndexBuildEnd {
                index_name,
                elapsed_secs,
            } => {
                if let Some(pb) = self.progress_bar.take() {
                    pb.finish_and_clear();
                }
                println!("  Index '{index_name}' built in {:.1}s", elapsed_secs);
            }
            ProgressEvent::SearchStart { index_name, params } => {
                println!("  Searching '{index_name}' with {params}...");
            }
            ProgressEvent::SearchEnd {
                index_name,
                recall,
                qps,
            } => {
                println!("  '{index_name}': recall={:.4} QPS={:.0}", recall, qps);
            }
            ProgressEvent::Message { text } => {
                println!("  {text}");
            }
            _ => {}
        }
    }

    fn on_index_built(&mut self, result: &IndexBuildResult) {
        let size_mb = result.index_size_bytes as f64 / (1024.0 * 1024.0);
        println!(
            "  Index '{}' ({}): {:.1}s, {:.1} MB",
            result.index_name,
            result.index_type,
            result.build_time.as_secs_f64(),
            size_mb,
        );
    }

    fn on_search_result(&mut self, _result: &SearchResult) {
        // Individual results printed via on_progress; table printed at end
    }

    fn on_pareto(&mut self, frontier: &[ParetoPoint]) {
        let pareto_count = frontier.iter().filter(|p| p.is_pareto).count();
        println!(
            "\n  Pareto frontier: {pareto_count} points out of {}",
            frontier.len()
        );
    }

    fn on_complete(&mut self, summary: &RunSummary) {
        self.finish_progress();

        if summary.search_results.is_empty() {
            return;
        }

        println!("\n=== Results: {} ===\n", summary.dataset);

        #[derive(Tabled)]
        struct Row {
            #[tabled(rename = "Index")]
            index: String,
            #[tabled(rename = "Params")]
            params: String,
            #[tabled(rename = "Recall@k")]
            recall: String,
            #[tabled(rename = "QPS")]
            qps: String,
            #[tabled(rename = "QPS(wall)")]
            qps_wall: String,
            #[tabled(rename = "Avg (us)")]
            avg_us: String,
            #[tabled(rename = "p50 (us)")]
            p50_us: String,
            #[tabled(rename = "p99 (us)")]
            p99_us: String,
        }

        let rows: Vec<Row> = summary
            .search_results
            .iter()
            .map(|r| Row {
                index: r.index_name.clone(),
                params: r.search_params.to_string(),
                recall: format!("{:.4}", r.avg_recall),
                qps: format!("{:.0}", r.qps),
                qps_wall: format!("{:.0}", r.qps_wall),
                avg_us: format!("{:.0}", r.avg_latency_us),
                p50_us: format!("{:.0}", r.p50_latency_us),
                p99_us: format!("{:.0}", r.p99_latency_us),
            })
            .collect();

        let mut table = Table::new(&rows);
        table.with(Style::rounded());
        println!("{table}");
    }
}
