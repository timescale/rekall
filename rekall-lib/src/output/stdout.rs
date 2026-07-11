use std::io::IsTerminal;
use std::time::Instant;

use tabled::settings::Style;
use tabled::{Table, Tabled};

use crate::metrics::{IndexBuildResult, ParetoPoint, PgRuntimeInfo, RunSummary, SearchResult};
use crate::progress::ProgressEvent;

use super::OutputSink;

pub struct StdoutSink {
    progress_bar: Option<indicatif::ProgressBar>,
    current_phase: String,
    has_bar: bool,
    // indicatif hides bars/spinners when stderr isn't a TTY (e.g. a captured
    // `infra run` / tmux log), so in that case we emit throttled plain-text
    // progress lines instead.
    interactive: bool,
    last_progress_log: Option<Instant>,
    /// Progress fraction (0–1) at the last non-interactive log line, so we
    /// can print on a meaningful jump rather than every poll.
    last_logged_frac: f64,
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
            has_bar: false,
            interactive: std::io::stderr().is_terminal(),
            last_progress_log: None,
            last_logged_frac: 0.0,
        }
    }

    /// Non-interactive progress throttle. Logs a line only on a *meaningful*
    /// change so captured logs stay compact over long builds: on a phase
    /// change, when progress advanced ≥5% since the last line, or as a ≥30s
    /// heartbeat (covers slow/stalled or total-less phases). `frac` is the
    /// 0–1 completion of the current phase (0 when unknown). Always false
    /// when interactive — the indicatif bar handles live updates there.
    fn should_log_progress(&mut self, phase_changed: bool, frac: f64) -> bool {
        if self.interactive {
            return false;
        }
        let now = Instant::now();
        let heartbeat = self
            .last_progress_log
            .map(|t| now.duration_since(t).as_secs() >= 30)
            .unwrap_or(true);
        let advanced = frac - self.last_logged_frac >= 0.05;
        let due = phase_changed || heartbeat || advanced;
        if due {
            self.last_progress_log = Some(now);
            self.last_logged_frac = frac;
        }
        due
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
                rows_per_sec,
            } => {
                if !self.interactive {
                    let frac = if *total > 0 {
                        *loaded as f64 / *total as f64
                    } else {
                        0.0
                    };
                    if self.should_log_progress(false, frac) {
                        let pct = if *total > 0 { loaded * 100 / total } else { 0 };
                        println!("  Loading {loaded}/{total} ({pct}%, {rows_per_sec:.0} rows/s)");
                    }
                    return;
                }
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
                self.has_bar = false;
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

                if !self.interactive {
                    let phase_changed = phase != self.current_phase;
                    if phase_changed {
                        self.current_phase = phase.to_string();
                    }
                    let frac = if *tuples_total > 0 {
                        *tuples_done as f64 / *tuples_total as f64
                    } else {
                        0.0
                    };
                    if self.should_log_progress(phase_changed, frac) {
                        let rss_mb = *rss_bytes as f64 / (1024.0 * 1024.0);
                        if *tuples_total > 0 {
                            let pct = *tuples_done * 100 / (*tuples_total).max(1);
                            println!(
                                "    {phase} {tuples_done}/{tuples_total} ({pct}%) \
                                 [1+{workers}w, {rss_mb:.0}MB]"
                            );
                        } else {
                            println!("    {phase} [1+{workers}w, {rss_mb:.0}MB]");
                        }
                    }
                    return;
                }

                let rss_mb = *rss_bytes as f64 / (1024.0 * 1024.0);
                let want_bar = *tuples_total > 0;

                if phase != self.current_phase || want_bar != self.has_bar {
                    self.finish_progress();
                    self.current_phase = phase.to_string();
                    self.has_bar = want_bar;
                }

                let procs = format!("1+{workers}w");
                let status = if rss_mb > 0.0 {
                    format!("{phase} [{procs}, {rss_mb:.0}MB]")
                } else {
                    format!("{phase} [{procs}]")
                };

                let pb = self.progress_bar.get_or_insert_with(|| {
                    if want_bar {
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
                    } else {
                        let pb = indicatif::ProgressBar::new_spinner();
                        pb.set_style(
                            indicatif::ProgressStyle::default_spinner()
                                .template("    {spinner} {msg} ({elapsed})")
                                .unwrap(),
                        );
                        pb
                    }
                });
                pb.set_message(status);

                if want_bar {
                    let total = *tuples_total as u64;
                    let done = (*tuples_done as u64).min(total);
                    pb.set_length(total);
                    pb.set_position(done);
                }
                pb.tick();
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
            #[tabled(rename = "Disk/q")]
            disk_per_q: String,
            #[tabled(rename = "Hit%")]
            hit_pct: String,
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
                disk_per_q: r
                    .io
                    .as_ref()
                    .map(|io| crate::output::plot::fmt_bytes(io.disk_read_bytes_per_query()))
                    .unwrap_or_else(|| "-".to_string()),
                hit_pct: r
                    .io
                    .as_ref()
                    .and_then(|io| io.shared_hit_ratio())
                    .map(|h| format!("{:.1}", h * 100.0))
                    .unwrap_or_else(|| "-".to_string()),
            })
            .collect();

        let mut table = Table::new(&rows);
        table.with(Style::rounded());
        println!("{table}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn noninteractive() -> StdoutSink {
        let mut s = StdoutSink::new();
        s.interactive = false;
        s.last_progress_log = Some(Instant::now());
        s.last_logged_frac = 0.0;
        s
    }

    #[test]
    fn logs_on_phase_change() {
        let mut s = noninteractive();
        assert!(s.should_log_progress(true, 0.0));
    }

    #[test]
    fn logs_on_5pct_advance_but_not_smaller() {
        let mut s = noninteractive();
        assert!(s.should_log_progress(false, 0.05)); // +5% → log
        assert!(!s.should_log_progress(false, 0.08)); // +3% → skip
        assert!(s.should_log_progress(false, 0.13)); // +5% from 0.08 → log
    }

    #[test]
    fn heartbeat_after_30s_even_without_progress() {
        let mut s = noninteractive();
        s.last_progress_log = Instant::now().checked_sub(Duration::from_secs(31));
        assert!(s.should_log_progress(false, 0.0));
    }

    #[test]
    fn interactive_never_emits_text() {
        let mut s = noninteractive();
        s.interactive = true;
        assert!(!s.should_log_progress(true, 1.0));
    }
}
