/*
 * search_monitor.rs - I/O statistics monitor for the search phase
 *
 * Wraps each timed sweep point with before/after PostgreSQL I/O
 * snapshots and samples the block device backing the PG data directory
 * continuously while the queries run. The result explains *why* a
 * point is slow, layer by layer:
 *
 *  - pg_statio_user_{indexes,tables}: shared-buffer traffic split
 *    between the active index and the heap (all PG versions),
 *  - pg_stat_get_backend_io(pid): the query backend's own reads/hits/
 *    bytes, exact attribution (PG 18+),
 *  - /proc/diskstats: what actually hit the device after the OS page
 *    cache — the layer PG cannot see.
 *
 * All counters are maintained unconditionally by PG and the kernel, so
 * monitoring adds no work to the timed queries. The only caveat is
 * PG's cumulative-stats flush delay (up to ~1s): callers should force
 * a flush on the *query* connection (pg_stat_force_next_flush) before
 * begin() and finish() so warmup I/O doesn't leak into the deltas.
 */

use std::time::{Duration, Instant};

use tokio_postgres::Client;

use crate::metrics::{BackendIo, DiskIoSample, SearchIoStats};

/// Cumulative counters for one or more block devices, parsed from
/// /proc/diskstats. Sectors are always 512 bytes there regardless of
/// the device's logical block size.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct DiskCounters {
    reads: u64,
    sectors_read: u64,
    writes: u64,
    sectors_written: u64,
    io_time_ms: u64,
}

impl DiskCounters {
    fn delta(&self, before: &DiskCounters) -> DiskCounters {
        DiskCounters {
            reads: self.reads.saturating_sub(before.reads),
            sectors_read: self.sectors_read.saturating_sub(before.sectors_read),
            writes: self.writes.saturating_sub(before.writes),
            sectors_written: self.sectors_written.saturating_sub(before.sectors_written),
            io_time_ms: self.io_time_ms.saturating_sub(before.io_time_ms),
        }
    }
}

/// Parse one /proc/diskstats line into (device name, counters).
/// Layout: major minor name reads reads_merged sectors_read ms_reading
/// writes writes_merged sectors_written ms_writing in_flight ms_doing_io ...
fn parse_diskstats_line(line: &str) -> Option<(String, DiskCounters)> {
    let f: Vec<&str> = line.split_whitespace().collect();
    if f.len() < 13 {
        return None;
    }
    let n = |i: usize| f[i].parse::<u64>().ok();
    Some((
        f[2].to_string(),
        DiskCounters {
            reads: n(3)?,
            sectors_read: n(5)?,
            writes: n(7)?,
            sectors_written: n(9)?,
            io_time_ms: n(12)?,
        },
    ))
}

/// Sum the counters of `devices` found in diskstats content.
fn sum_devices(diskstats: &str, devices: &[String]) -> DiskCounters {
    let mut total = DiskCounters::default();
    for line in diskstats.lines() {
        if let Some((name, c)) = parse_diskstats_line(line) {
            if devices.contains(&name) {
                total.reads += c.reads;
                total.sectors_read += c.sectors_read;
                total.writes += c.writes;
                total.sectors_written += c.sectors_written;
                total.io_time_ms += c.io_time_ms;
            }
        }
    }
    total
}

/// Whole-disk device names (partitions excluded) — the fallback set
/// when the data directory's device can't be resolved. Covers NVMe
/// (nvme0n1), SCSI/SATA (sda), Xen (xvda), and virtio (vda) naming.
fn whole_disk_devices(diskstats: &str) -> Vec<String> {
    fn is_whole_disk(name: &str) -> bool {
        let strip = |prefix: &str| name.strip_prefix(prefix);
        if let Some(rest) = strip("nvme") {
            // nvme<ctrl>n<ns> is the disk; a trailing p<N> is a partition.
            return rest.contains('n') && !rest.contains('p');
        }
        for prefix in ["sd", "xvd", "vd"] {
            if let Some(rest) = strip(prefix) {
                return !rest.is_empty() && rest.chars().all(|c| c.is_ascii_lowercase());
            }
        }
        false
    }
    diskstats
        .lines()
        .filter_map(parse_diskstats_line)
        .map(|(name, _)| name)
        .filter(|n| is_whole_disk(n))
        .collect()
}

/// Resolve the device backing `path` by longest mount-point prefix
/// match over /proc/mounts content. Returns the device path (e.g.
/// "/dev/nvme0n1p1"); symlinks (LVM/mapper names) are not resolved
/// here — the caller canonicalizes.
fn device_for_path(path: &str, mounts: &str) -> Option<String> {
    let mut best: Option<(&str, &str)> = None; // (mount_point, device)
    for line in mounts.lines() {
        let mut f = line.split_whitespace();
        let (Some(dev), Some(mnt)) = (f.next(), f.next()) else {
            continue;
        };
        if !dev.starts_with("/dev/") {
            continue;
        }
        let matches = path == mnt
            || (mnt == "/" && path.starts_with('/'))
            || path.strip_prefix(mnt).is_some_and(|r| r.starts_with('/'));
        if matches && best.is_none_or(|(m, _)| mnt.len() > m.len()) {
            best = Some((mnt, dev));
        }
    }
    best.map(|(_, dev)| dev.to_string())
}

/// The device names to sample for a PG data directory: the resolved
/// backing device when possible, else all whole disks. The bool is
/// true when resolution succeeded (single named device).
fn devices_for_data_dir(
    data_dir: Option<&str>,
    mounts: &str,
    diskstats: &str,
) -> (Vec<String>, bool) {
    if let Some(dir) = data_dir {
        if let Some(dev_path) = device_for_path(dir, mounts) {
            // Canonicalize to resolve mapper/LVM symlinks to dm-N.
            let real = std::fs::canonicalize(&dev_path)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or(dev_path);
            if let Some(name) = real.strip_prefix("/dev/") {
                let name = name.to_string();
                if diskstats
                    .lines()
                    .filter_map(parse_diskstats_line)
                    .any(|(n, _)| n == name)
                {
                    return (vec![name], true);
                }
            }
        }
    }
    (whole_disk_devices(diskstats), false)
}

fn read_diskstats() -> Option<String> {
    std::fs::read_to_string("/proc/diskstats").ok()
}

/// Per-relation + per-backend PG counters at one point in time.
#[derive(Debug, Default, Clone)]
struct PgIoSnapshot {
    idx_read: i64,
    idx_hit: i64,
    heap_read: i64,
    heap_hit: i64,
    backend: Option<BackendIoRaw>,
}

#[derive(Debug, Default, Clone, Copy)]
struct BackendIoRaw {
    reads: i64,
    read_bytes: i64,
    hits: i64,
    writes: i64,
    write_bytes: i64,
    evictions: i64,
    read_time_ms: f64,
}

/// In-flight state for one sweep point, returned by [`SearchIoMonitor::begin`].
pub struct ActivePoint {
    started: Instant,
    backend_pids: Vec<i32>,
    pg_before: PgIoSnapshot,
    disk_before: Option<DiskCounters>,
    sampler: Option<(
        tokio::sync::watch::Sender<bool>,
        tokio::task::JoinHandle<Vec<DiskIoSample>>,
    )>,
}

/// Monitors I/O around the timed runs of each sweep point, on its own
/// PG connection (like `BuildMonitor`). Construct once per query
/// series; call `begin`/`finish` around each point.
pub struct SearchIoMonitor {
    client: Client,
    index_sql_name: String,
    table_name: String,
    devices: Vec<String>,
    device_resolved: bool,
    track_io_timing: bool,
    has_backend_io: bool,
    interval: Duration,
}

impl SearchIoMonitor {
    /// Returns `None` if the monitor connection can't be opened.
    pub async fn new(
        dsn: &str,
        index_sql_name: &str,
        table_name: &str,
        interval: Duration,
    ) -> Option<Self> {
        let (client, connection) = tokio_postgres::connect(dsn, tokio_postgres::NoTls)
            .await
            .ok()?;
        tokio::spawn(async move {
            let _ = connection.await;
        });

        let track_io_timing = client
            .query_one("SHOW track_io_timing", &[])
            .await
            .map(|r| r.get::<_, String>(0) == "on")
            .unwrap_or(false);

        let has_backend_io = client
            .query_one(
                "SELECT to_regproc('pg_stat_get_backend_io') IS NOT NULL",
                &[],
            )
            .await
            .map(|r| r.get::<_, bool>(0))
            .unwrap_or(false);

        // data_directory is superuser/pg_read_all_settings-only; on
        // failure fall back to sampling all whole disks.
        let data_dir = client
            .query_one("SHOW data_directory", &[])
            .await
            .map(|r| r.get::<_, String>(0))
            .ok();

        let (devices, device_resolved) = match (
            std::fs::read_to_string("/proc/mounts").ok(),
            read_diskstats(),
        ) {
            (Some(mounts), Some(diskstats)) => {
                devices_for_data_dir(data_dir.as_deref(), &mounts, &diskstats)
            }
            _ => (Vec::new(), false),
        };
        if devices.is_empty() {
            tracing::warn!("io monitor: no block devices found; disk stats will be zero");
        }

        Some(Self {
            client,
            index_sql_name: index_sql_name.to_string(),
            table_name: table_name.to_string(),
            devices,
            device_resolved,
            track_io_timing,
            has_backend_io,
            interval,
        })
    }

    async fn pg_snapshot(&self, backend_pids: &[i32]) -> PgIoSnapshot {
        let mut snap = PgIoSnapshot::default();

        if let Ok(Some(row)) = self
            .client
            .query_opt(
                "SELECT COALESCE(idx_blks_read, 0), COALESCE(idx_blks_hit, 0) \
                 FROM pg_statio_user_indexes WHERE indexrelname = $1",
                &[&self.index_sql_name],
            )
            .await
        {
            snap.idx_read = row.get(0);
            snap.idx_hit = row.get(1);
        }

        if let Ok(Some(row)) = self
            .client
            .query_opt(
                "SELECT COALESCE(heap_blks_read, 0), COALESCE(heap_blks_hit, 0) \
                 FROM pg_statio_user_tables WHERE relname = $1",
                &[&self.table_name],
            )
            .await
        {
            snap.heap_read = row.get(0);
            snap.heap_hit = row.get(1);
        }

        if self.has_backend_io && !backend_pids.is_empty() {
            // Byte columns are numeric (uint64 range) — cast for i64
            // decoding; realistic benchmark values fit comfortably.
            if let Ok(Some(row)) = self
                .client
                .query_opt(
                    "SELECT COALESCE(sum(io.reads), 0)::int8, \
                            COALESCE(sum(io.read_bytes), 0)::int8, \
                            COALESCE(sum(io.hits), 0)::int8, \
                            COALESCE(sum(io.writes), 0)::int8, \
                            COALESCE(sum(io.write_bytes), 0)::int8, \
                            COALESCE(sum(io.evictions), 0)::int8, \
                            COALESCE(sum(io.read_time), 0)::float8 \
                     FROM unnest($1::int4[]) AS b(pid), \
                          LATERAL pg_stat_get_backend_io(b.pid) AS io",
                    &[&backend_pids],
                )
                .await
            {
                snap.backend = Some(BackendIoRaw {
                    reads: row.get(0),
                    read_bytes: row.get(1),
                    hits: row.get(2),
                    writes: row.get(3),
                    write_bytes: row.get(4),
                    evictions: row.get(5),
                    read_time_ms: row.get(6),
                });
            }
        }

        snap
    }

    fn read_disk(&self) -> Option<DiskCounters> {
        if self.devices.is_empty() {
            return None;
        }
        read_diskstats().map(|c| sum_devices(&c, &self.devices))
    }

    /// Snapshot the PG counters, record the disk baseline, and start
    /// the continuous device sampler. Call after warmup/cache eviction
    /// (and after flushing the query backends' stats). `backend_pids` are
    /// the connections that will run the timed queries; their own I/O is
    /// summed. Keep them open until `finish`, since PG drops a backend's
    /// stats when it exits.
    pub async fn begin(&self, backend_pids: &[i32]) -> ActivePoint {
        let pg_before = self.pg_snapshot(backend_pids).await;
        let disk_before = self.read_disk();

        let sampler = disk_before.map(|baseline| {
            let (stop_tx, mut stop_rx) = tokio::sync::watch::channel(false);
            let devices = self.devices.clone();
            let interval = self.interval;
            let handle = tokio::spawn(async move {
                let start = Instant::now();
                let mut prev = baseline;
                let mut samples = Vec::new();
                loop {
                    tokio::select! {
                        _ = tokio::time::sleep(interval) => {}
                        _ = stop_rx.changed() => break,
                    }
                    let Some(cur) = read_diskstats().map(|c| sum_devices(&c, &devices)) else {
                        continue;
                    };
                    let d = cur.delta(&prev);
                    prev = cur;
                    samples.push(DiskIoSample {
                        elapsed_ms: start.elapsed().as_millis() as u64,
                        read_bytes: d.sectors_read * 512,
                        write_bytes: d.sectors_written * 512,
                        reads: d.reads,
                        writes: d.writes,
                        io_time_ms: d.io_time_ms,
                    });
                }
                samples
            });
            (stop_tx, handle)
        });

        ActivePoint {
            started: Instant::now(),
            backend_pids: backend_pids.to_vec(),
            pg_before,
            disk_before,
            sampler,
        }
    }

    /// Stop sampling, snapshot again, and produce the point's stats.
    /// `queries` is the number of timed queries the window covered
    /// (num_queries × runs) — the denominator for per-query rates.
    pub async fn finish(&self, point: ActivePoint, queries: u64) -> SearchIoStats {
        let duration_ms = point.started.elapsed().as_millis() as u64;
        let disk_after = self.read_disk();

        let disk_samples = match point.sampler {
            Some((stop_tx, handle)) => {
                let _ = stop_tx.send(true);
                handle.await.unwrap_or_default()
            }
            None => Vec::new(),
        };

        let pg_after = self.pg_snapshot(&point.backend_pids).await;
        let before = &point.pg_before;

        let backend = match (pg_after.backend, before.backend) {
            (Some(a), Some(b)) => Some(BackendIo {
                reads: a.reads - b.reads,
                read_bytes: a.read_bytes - b.read_bytes,
                hits: a.hits - b.hits,
                writes: a.writes - b.writes,
                write_bytes: a.write_bytes - b.write_bytes,
                evictions: a.evictions - b.evictions,
                read_time_ms: self
                    .track_io_timing
                    .then_some(a.read_time_ms - b.read_time_ms),
            }),
            _ => None,
        };

        let disk = match (disk_after, point.disk_before) {
            (Some(a), Some(b)) => a.delta(&b),
            _ => DiskCounters::default(),
        };

        SearchIoStats {
            queries,
            duration_ms,
            idx_blks_read: pg_after.idx_read - before.idx_read,
            idx_blks_hit: pg_after.idx_hit - before.idx_hit,
            heap_blks_read: pg_after.heap_read - before.heap_read,
            heap_blks_hit: pg_after.heap_hit - before.heap_hit,
            backend,
            disk_device: self.device_resolved.then(|| self.devices.join("+")),
            disk_read_bytes: disk.sectors_read * 512,
            disk_write_bytes: disk.sectors_written * 512,
            disk_reads: disk.reads,
            disk_writes: disk.writes,
            disk_io_time_ms: disk.io_time_ms,
            disk_samples,
        }
    }
}

/// Flush the query backend's pending cumulative stats so the monitor's
/// deltas have exact boundaries. Must run on the connection that
/// executed the queries (the flush is per-backend). Falls back to a
/// settle sleep covering PG's ~1s flush rate limit when the function
/// is unavailable.
pub async fn force_stats_flush(client: &Client) {
    if client
        .execute("SELECT pg_stat_force_next_flush()", &[])
        .await
        .is_err()
    {
        tokio::time::sleep(Duration::from_millis(1200)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DISKSTATS: &str = "\
 259       0 nvme0n1 1000 5 80000 300 2000 10 160000 500 0 700 900
 259       1 nvme0n1p1 900 5 72000 280 1900 10 152000 480 0 650 850
 259       4 nvme1n1 50 0 4000 10 60 0 4800 20 0 25 35
   8       0 sda 10 0 800 3 20 0 1600 6 0 8 12
   8       1 sda1 9 0 720 2 19 0 1520 5 0 7 11
   7       0 loop0 5 0 40 0 0 0 0 0 0 1 1
 252       0 dm-0 30 0 2400 8 40 0 3200 12 0 15 20";

    #[test]
    fn parse_line_extracts_counters() {
        let (name, c) = parse_diskstats_line(
            " 259       0 nvme0n1 1000 5 80000 300 2000 10 160000 500 0 700 900",
        )
        .unwrap();
        assert_eq!(name, "nvme0n1");
        assert_eq!(c.reads, 1000);
        assert_eq!(c.sectors_read, 80000);
        assert_eq!(c.writes, 2000);
        assert_eq!(c.sectors_written, 160000);
        assert_eq!(c.io_time_ms, 700);
    }

    #[test]
    fn parse_line_rejects_short_lines() {
        assert!(parse_diskstats_line("259 0 nvme0n1 1000").is_none());
        assert!(parse_diskstats_line("").is_none());
    }

    #[test]
    fn sum_devices_selects_named_devices() {
        let total = sum_devices(DISKSTATS, &["nvme0n1p1".to_string()]);
        assert_eq!(total.reads, 900);
        assert_eq!(total.sectors_read, 72000);

        let both = sum_devices(DISKSTATS, &["nvme0n1".to_string(), "nvme1n1".to_string()]);
        assert_eq!(both.reads, 1050);
    }

    #[test]
    fn whole_disks_exclude_partitions_and_loop() {
        let disks = whole_disk_devices(DISKSTATS);
        assert!(disks.contains(&"nvme0n1".to_string()));
        assert!(disks.contains(&"nvme1n1".to_string()));
        assert!(disks.contains(&"sda".to_string()));
        assert!(!disks.contains(&"nvme0n1p1".to_string()));
        assert!(!disks.contains(&"sda1".to_string()));
        assert!(!disks.contains(&"loop0".to_string()));
        assert!(!disks.contains(&"dm-0".to_string()));
    }

    const MOUNTS: &str = "\
/dev/nvme0n1p1 / ext4 rw 0 0
tmpfs /tmp tmpfs rw 0 0
/dev/nvme1n1 /var/lib/postgresql ext4 rw 0 0";

    #[test]
    fn device_for_path_longest_prefix_wins() {
        assert_eq!(
            device_for_path("/var/lib/postgresql/18/main", MOUNTS).as_deref(),
            Some("/dev/nvme1n1")
        );
        assert_eq!(
            device_for_path("/home/user/pgdata", MOUNTS).as_deref(),
            Some("/dev/nvme0n1p1")
        );
        // Prefix must match at a path boundary, not mid-component.
        assert_eq!(
            device_for_path("/var/lib/postgresql-old/data", MOUNTS).as_deref(),
            Some("/dev/nvme0n1p1")
        );
    }

    #[test]
    fn devices_for_data_dir_falls_back_to_whole_disks() {
        // Unresolvable data dir (no matching /dev mount) → whole disks.
        let (devs, resolved) = devices_for_data_dir(None, MOUNTS, DISKSTATS);
        assert!(!resolved);
        assert!(devs.contains(&"nvme0n1".to_string()));

        // Resolvable and present in diskstats → single device.
        let (devs, resolved) =
            devices_for_data_dir(Some("/var/lib/postgresql/18/main"), MOUNTS, DISKSTATS);
        assert!(resolved);
        assert_eq!(devs, vec!["nvme1n1".to_string()]);
    }

    #[test]
    fn delta_saturates_on_counter_reset() {
        let a = DiskCounters {
            reads: 5,
            ..Default::default()
        };
        let b = DiskCounters {
            reads: 10,
            ..Default::default()
        };
        assert_eq!(a.delta(&b).reads, 0);
        assert_eq!(b.delta(&a).reads, 5);
    }
}
