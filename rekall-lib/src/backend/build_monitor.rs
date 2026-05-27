/*
 * build_monitor.rs - Monitor index build progress via a second PG connection
 *
 * Polls pg_stat_progress_create_index and pg_stat_activity while
 * a CREATE INDEX runs on the main connection. Records:
 * - Build phase transitions and durations
 * - Actual parallel worker count (peak)
 * - Tuples/blocks progress for reporting
 */

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio_postgres::Client;

use crate::progress::{ProgressEvent, ProgressTx};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildPhase {
    pub phase: String,
    pub duration_ms: u64,
    pub workers: u32,
    pub tuples_done: i64,
    pub tuples_total: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemorySample {
    pub elapsed_ms: u64,
    pub rss_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildMonitorResult {
    pub peak_workers: u32,
    pub peak_rss_bytes: u64,
    pub phases: Vec<BuildPhase>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub memory_samples: Vec<MemorySample>,
}

fn read_proc_rss(pid: i32) -> u64 {
    let path = format!("/proc/{pid}/status");
    let Ok(content) = std::fs::read_to_string(&path) else {
        return 0;
    };
    for line in content.lines() {
        if let Some(val) = line.strip_prefix("VmRSS:") {
            let kb: u64 = val
                .split_whitespace()
                .next()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            return kb * 1024;
        }
    }
    0
}

fn read_total_rss(leader_pid: i32, worker_pids: &[i32]) -> u64 {
    let mut total = read_proc_rss(leader_pid);
    for &pid in worker_pids {
        total += read_proc_rss(pid);
    }
    total
}

pub struct BuildMonitor {
    client: Client,
    poll_interval: Duration,
}

impl BuildMonitor {
    pub async fn new(dsn: &str) -> Option<Self> {
        let (client, connection) = tokio_postgres::connect(dsn, tokio_postgres::NoTls)
            .await
            .ok()?;
        tokio::spawn(async move {
            let _ = connection.await;
        });
        Some(Self {
            client,
            poll_interval: Duration::from_millis(500),
        })
    }

    pub async fn monitor_until_done(&self, progress_tx: Option<&ProgressTx>) -> BuildMonitorResult {
        let mut phases: Vec<BuildPhase> = Vec::new();
        let mut memory_samples: Vec<MemorySample> = Vec::new();
        let mut peak_workers: u32 = 0;
        let mut peak_rss: u64 = 0;
        let mut current_phase = String::new();
        let mut phase_start = Instant::now();
        let mut monitor_start = Instant::now();
        let mut last_workers: u32 = 0;
        let mut last_tuples_done: i64 = 0;
        let mut last_tuples_total: i64 = 0;
        let mut started = false;
        let mut estimated_total: Option<i64> = None;

        loop {
            tokio::time::sleep(self.poll_interval).await;

            let progress = self
                .client
                .query_opt(
                    "SELECT phase, tuples_done, tuples_total, \
                     blocks_done, blocks_total, pid \
                     FROM pg_stat_progress_create_index \
                     LIMIT 1",
                    &[],
                )
                .await;

            let row = match progress {
                Ok(Some(row)) => row,
                Ok(None) => {
                    if started {
                        break; // build finished
                    }
                    continue; // not started yet
                }
                Err(_) => break,
            };

            if !started {
                started = true;
                monitor_start = Instant::now();
            }

            let phase: String = row.get(0);
            let raw_tuples_done: i64 = row.get(1);
            let raw_tuples_total: i64 = row.get(2);
            let blocks_done: i64 = row.get(3);
            let blocks_total: i64 = row.get(4);
            let pid: i32 = row.get(5);

            let (tuples_done, tuples_total) = if raw_tuples_total > 0 {
                (raw_tuples_done, raw_tuples_total)
            } else if raw_tuples_done > 0 {
                // Extension reports tuples_done but not tuples_total
                // (e.g. pgvector HNSW). Estimate from pg_class.reltuples.
                if estimated_total.is_none() {
                    estimated_total = self
                        .client
                        .query_opt(
                            "SELECT reltuples::bigint FROM pg_class \
                             WHERE oid = (SELECT relid \
                                          FROM pg_stat_progress_create_index \
                                          LIMIT 1)",
                            &[],
                        )
                        .await
                        .ok()
                        .flatten()
                        .and_then(|r| {
                            let n: i64 = r.get(0);
                            if n > 0 {
                                Some(n)
                            } else {
                                None
                            }
                        });
                }
                (raw_tuples_done, estimated_total.unwrap_or(0))
            } else if blocks_total > 0 {
                (blocks_done, blocks_total)
            } else {
                (0, 0)
            };

            // Count parallel workers and collect PIDs
            let worker_rows = self
                .client
                .query(
                    "SELECT pid::int FROM pg_stat_activity \
                     WHERE leader_pid = $1",
                    &[&pid],
                )
                .await
                .unwrap_or_default();
            let worker_pids: Vec<i32> = worker_rows.iter().map(|r| r.get(0)).collect();
            let workers = worker_pids.len() as u32;

            if workers > peak_workers {
                peak_workers = workers;
            }

            // Sample memory (leader + workers)
            let rss = read_total_rss(pid, &worker_pids);
            if rss > 0 {
                if rss > peak_rss {
                    peak_rss = rss;
                }
                memory_samples.push(MemorySample {
                    elapsed_ms: monitor_start.elapsed().as_millis() as u64,
                    rss_bytes: rss,
                });
            }

            // Track phase transitions
            if phase != current_phase {
                if !current_phase.is_empty() {
                    phases.push(BuildPhase {
                        phase: current_phase.clone(),
                        duration_ms: phase_start.elapsed().as_millis() as u64,
                        workers: last_workers,
                        tuples_done: last_tuples_done,
                        tuples_total: last_tuples_total,
                    });
                }
                current_phase = phase.clone();
                phase_start = Instant::now();
            }

            last_workers = workers;
            last_tuples_done = tuples_done;
            last_tuples_total = tuples_total;

            if let Some(tx) = progress_tx {
                let _ = tx.send(ProgressEvent::IndexBuildProgress {
                    phase: phase.clone(),
                    tuples_done,
                    tuples_total,
                    workers,
                    rss_bytes: rss,
                });
            }
        }

        if !current_phase.is_empty() {
            phases.push(BuildPhase {
                phase: current_phase,
                duration_ms: phase_start.elapsed().as_millis() as u64,
                workers: last_workers,
                tuples_done: last_tuples_done,
                tuples_total: last_tuples_total,
            });
        }

        BuildMonitorResult {
            peak_workers,
            peak_rss_bytes: peak_rss,
            phases,
            memory_samples,
        }
    }
}
