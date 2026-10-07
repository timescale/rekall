use std::io::IsTerminal;
use std::time::Instant;

use tabled::settings::Style;
use tabled::{Table, Tabled};

use crate::metrics::{
    ConcurrencyResult, IndexBuildResult, ParetoPoint, PgRuntimeInfo, RecallBasis, RunSummary,
    SearchResult,
};
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

        if let Some(table) = concurrency_table(&summary.concurrency_results) {
            println!("\n=== Concurrency: {} ===\n", summary.dataset);
            println!("{table}");
        }
    }
}

/// One row of the concurrency table.
#[derive(Tabled)]
struct ConcurrencyRow {
    #[tabled(rename = "Index")]
    index: String,
    #[tabled(rename = "Params")]
    params: String,
    #[tabled(rename = "Recall targets")]
    targets: String,
    #[tabled(rename = "Clients")]
    clients: String,
    #[tabled(rename = "Recall")]
    recall: String,
    #[tabled(rename = "QPS(wall)")]
    qps_wall: String,
    #[tabled(rename = "QPS(busy)")]
    qps_busy: String,
    #[tabled(rename = "vs 1 client")]
    scaling: String,
    #[tabled(rename = "p50 (us)")]
    p50_us: String,
    #[tabled(rename = "p99 (us)")]
    p99_us: String,
    #[tabled(rename = "rekall cores")]
    rekall_cores: String,
}

/// Rows for the concurrency table, by series and recall target, with client
/// counts ascending. QPS(wall) covers the whole run and QPS(busy) the time
/// every client was busy; scaling is wall-clock QPS over the same point's
/// single-client baseline; rekall cores is the CPU rekall itself used,
/// averaged over the run. `mark_id` tags ID-based recall values, for a
/// table that mixes bases.
fn concurrency_rows(results: &[ConcurrencyResult], mark_id: bool) -> Vec<ConcurrencyRow> {
    let mut sorted: Vec<&ConcurrencyResult> = results.iter().collect();
    let lowest_target = |r: &ConcurrencyResult| {
        r.recall_targets
            .iter()
            .copied()
            .fold(f64::INFINITY, f64::min)
    };
    sorted.sort_by(|a, b| {
        a.index_name
            .cmp(&b.index_name)
            .then(lowest_target(a).total_cmp(&lowest_target(b)))
            .then(a.clients.cmp(&b.clients))
    });
    sorted
        .into_iter()
        .map(|r| {
            let baseline = results
                .iter()
                .find(|b| b.measures(&r.index_name, &r.search_params, 1, r.k, r.num_queries))
                .map(|b| b.qps_wall);
            let targets: Vec<String> = r.recall_targets.iter().map(|t| t.to_string()).collect();
            ConcurrencyRow {
                index: r.index_name.clone(),
                params: r.search_params.to_string(),
                targets: targets.join(", "),
                clients: match &r.clients_spec {
                    Some(spec) => format!("{} ({spec})", r.clients),
                    None => r.clients.to_string(),
                },
                recall: if mark_id && r.recall_basis == RecallBasis::Id {
                    format!("{:.4} (ID)", r.avg_recall)
                } else {
                    format!("{:.4}", r.avg_recall)
                },
                qps_wall: format!("{:.0}", r.qps_wall),
                qps_busy: format!("{:.0}", r.throughput_qps),
                scaling: match baseline {
                    Some(base) if base > 0.0 => format!("x{:.2}", r.qps_wall / base),
                    _ => "-".to_string(),
                },
                p50_us: format!("{:.0}", r.p50_latency_us),
                p99_us: format!("{:.0}", r.p99_latency_us),
                rekall_cores: if r.wall_time_total_secs > 0.0 {
                    format!("{:.2}", r.client_cpu_secs / r.wall_time_total_secs)
                } else {
                    "-".to_string()
                },
            }
        })
        .collect()
}

/// The concurrency results as a table with a note on its recall basis, or
/// `None` when there are none.
pub fn concurrency_table(results: &[ConcurrencyResult]) -> Option<String> {
    let first = results.first()?.recall_basis;
    let mixed = results.iter().any(|r| r.recall_basis != first);
    let mut table = Table::new(concurrency_rows(results, mixed));
    table.with(Style::rounded());
    let note = if mixed {
        format!(
            "Recall is {}, as in published reports, except where marked (ID).",
            RecallBasis::Distance.describe()
        )
    } else {
        format!("Recall is {}.", first.describe())
    };
    Some(format!("{table}\n{note}"))
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

    #[test]
    fn concurrency_rows_group_by_target_with_scaling_over_the_baseline() {
        use crate::metrics::test_concurrency_result as result;
        let mut baseline = result("a", 10, 1, 1000.0);
        baseline.clients_spec = None;
        let mut sixteen = result("a", 10, 16, 8000.0);
        // 0.5 s of rekall CPU over a 0.25 s run is two cores.
        sixteen.client_cpu_secs = 0.5;
        sixteen.wall_time_total_secs = 0.25;
        // A lower target sorts first, though its params string sorts later.
        let mut low_target = result("a", 200, 16, 300.0);
        low_target.recall_targets = vec![0.90];
        let rows = concurrency_rows(
            &[result("b", 10, 16, 500.0), sixteen, low_target, baseline],
            false,
        );

        let order: Vec<(&str, &str, &str)> = rows
            .iter()
            .map(|r| (r.index.as_str(), r.targets.as_str(), r.clients.as_str()))
            .collect();
        assert_eq!(
            order,
            [
                ("a", "0.9", "16 (2x)"),
                ("a", "0.95", "1"),
                ("a", "0.95", "16 (2x)"),
                ("b", "0.95", "16 (2x)")
            ]
        );
        // nprobe 200 and series b have no baseline of their own.
        assert_eq!(rows[0].scaling, "-");
        assert_eq!(rows[1].scaling, "x1.00");
        assert_eq!(rows[2].scaling, "x8.00");
        assert_eq!(rows[2].rekall_cores, "2.00");
        assert_eq!(rows[3].scaling, "-");
    }

    #[test]
    fn concurrency_table_is_none_without_results() {
        assert!(concurrency_table(&[]).is_none());
    }

    #[test]
    fn concurrency_table_states_one_basis_and_marks_id_rows_only_when_mixed() {
        use crate::metrics::test_concurrency_result as result;
        let distance = concurrency_table(&[result("a", 10, 16, 1.0)]).unwrap();
        assert!(distance.ends_with("Recall is distance-based (ann-benchmarks, tie-aware)."));
        assert!(!distance.contains("(ID)"));

        let mut id_based = result("b", 10, 16, 1.0);
        id_based.recall_basis = RecallBasis::Id;
        let only_id = concurrency_table(std::slice::from_ref(&id_based)).unwrap();
        assert!(only_id.ends_with("Recall is ID-based."));
        assert!(!only_id.contains("(ID)"));

        let mixed = concurrency_table(&[result("a", 10, 16, 1.0), id_based]).unwrap();
        assert!(mixed.contains("0.9600 (ID)"));
        assert!(mixed.ends_with("except where marked (ID)."));
    }
}
