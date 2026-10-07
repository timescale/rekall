//! Concurrent-client measurement of a query series: which sweep points to
//! measure under load, and the runner that drives several clients at once.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use futures_util::future::BoxFuture;
use tokio::sync::Barrier;
use tokio::task::JoinSet;

use crate::config::ClientLevel;
use crate::error::{Error, Result};
use crate::metrics::{operating_point, LatencyCollector};

/// A sweep point chosen for concurrent measurement.
#[derive(Debug, PartialEq)]
pub struct SelectedPoint {
    /// Position of the point in the candidate slice.
    pub candidate: usize,
    /// Recall targets this point is the operating point for, ascending.
    pub recall_targets: Vec<f64>,
}

#[derive(Debug, Default, PartialEq)]
pub struct Selection {
    pub points: Vec<SelectedPoint>,
    /// Targets no candidate reaches.
    pub unreached: Vec<f64>,
}

/// Pick the operating point for each recall target among a series' sweep
/// points: the highest-QPS point whose recall reaches the target. Targets
/// that land on the same point share one entry.
pub fn select_operating_points<T>(
    candidates: &[T],
    targets: &[f64],
    recall: impl Fn(&T) -> f64,
    qps: impl Fn(&T) -> f64,
) -> Selection {
    let mut targets = targets.to_vec();
    targets.sort_by(f64::total_cmp);
    targets.dedup();

    let mut selection = Selection::default();
    for target in targets {
        let best = operating_point(
            candidates.iter().enumerate(),
            target,
            |(_, c)| recall(c),
            |(_, c)| qps(c),
        );
        match best {
            None => selection.unreached.push(target),
            Some((idx, _)) => match selection.points.iter_mut().find(|p| p.candidate == idx) {
                Some(point) => point.recall_targets.push(target),
                None => selection.points.push(SelectedPoint {
                    candidate: idx,
                    recall_targets: vec![target],
                }),
            },
        }
    }
    selection
}

/// Untimed queries each client runs before a timed run. Covers the
/// session's first-query catalog loads and the five individually planned
/// executions PostgreSQL makes before it may switch a prepared statement
/// to a generic plan.
pub const SESSION_WARMUP_QUERIES: usize = 10;

/// Connections rekall holds open next to the concurrent clients: the
/// sweep's query connection, the series' management connection, the I/O
/// monitor and a cache-eviction connection.
pub const OWN_CONNECTIONS: usize = 4;

/// Split client levels into those whose clients, plus rekall's own
/// connections, fit in `available` free connection slots, and those that
/// don't.
pub fn split_levels_by_fit(
    levels: &[ClientLevel],
    available: usize,
) -> (Vec<ClientLevel>, Vec<ClientLevel>) {
    levels
        .iter()
        .partition(|(n, _)| n.saturating_add(OWN_CONNECTIONS) <= available)
}

/// Free connection slots: `max_connections` less the slots reserved for
/// roles this one is not, less the connections already open.
pub fn free_connection_slots(
    max_connections: i64,
    reserved: i64,
    is_superuser: bool,
    open: i64,
) -> usize {
    let usable = if is_superuser {
        max_connections
    } else {
        max_connections - reserved
    };
    (usable - open).max(0) as usize
}

/// CPU time (user + system) this process has used so far, in seconds.
pub fn process_cpu_secs() -> f64 {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    // SAFETY: getrusage only writes into the struct it is given, and
    // RUSAGE_SELF is valid on every platform rekall runs on.
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) } != 0 {
        return 0.0;
    }
    // SAFETY: getrusage returned 0, so it filled the struct.
    let usage = unsafe { usage.assume_init() };
    let secs = |tv: libc::timeval| tv.tv_sec as f64 + tv.tv_usec as f64 / 1e6;
    secs(usage.ru_utime) + secs(usage.ru_stime)
}

/// Fewest queries each client runs in a timed run. With fewer, ramp-up and
/// drain make up too much of the measurement.
pub const MIN_QUERIES_PER_CLIENT: u64 = 10;

/// Queries in one timed run: the whole query set, cycled until every client
/// gets at least `MIN_QUERIES_PER_CLIENT`.
pub fn queries_per_run(num_queries: usize, clients: usize) -> u64 {
    (num_queries as u64).max(MIN_QUERIES_PER_CLIENT * clients as u64)
}

/// Runs query-set entry `qi` on a client and returns the neighbour IDs.
pub type QueryFn<C> =
    Arc<dyn for<'a> Fn(&'a C, usize) -> BoxFuture<'a, Result<Vec<i64>>> + Send + Sync>;

/// Recall of the neighbour IDs returned for query-set entry `qi`.
pub type RecallFn = Arc<dyn Fn(usize, &[i64]) -> f64 + Send + Sync>;

/// A client query that failed, with the query-set entry it was running.
#[derive(Debug)]
pub struct ClientFailure {
    pub query_index: usize,
    pub error: Error,
}

/// One client's share of a timed run. Times are measured from the start
/// of the run and are `None` when the client completed fewer queries.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClientTiming {
    pub completed: u64,
    pub last_completion: Option<Duration>,
    pub previous_completion: Option<Duration>,
}

/// Throughput figures of one timed run. See `ConcurrencyResult` for what
/// each one means.
#[derive(Debug, Clone, PartialEq)]
pub struct Throughput {
    pub throughput_qps: f64,
    pub qps_wall: f64,
    pub wall_time_till_first_empty_secs: f64,
    pub wall_time_total_secs: f64,
    pub queries_finished_till_first_empty: u64,
    pub queries_finished_total: u64,
}

/// Throughput of a timed run from each client's timing and the moment the
/// first client found no query left.
///
/// `throughput_qps` adds up each client's rate over the queries it finished
/// by that moment: their count divided by the time of the last of them.
/// Until then every client is busy, and each client's window ends on one of
/// its own completions, so the rate includes neither the drain nor a query
/// cut in half. Only a client's last query can end after that moment, since
/// every query is handed out before it.
pub fn throughput(clients: &[ClientTiming], first_empty: Duration) -> Throughput {
    let rate = |queries: u64, secs: f64| {
        if secs > 0.0 {
            queries as f64 / secs
        } else {
            0.0
        }
    };
    let mut throughput_qps = 0.0;
    let mut finished_by_first_empty = 0;
    for c in clients {
        let (finished, until) = match (c.last_completion, c.previous_completion) {
            (Some(last), _) if last <= first_empty => (c.completed, last),
            (Some(_), Some(previous)) => (c.completed - 1, previous),
            _ => continue,
        };
        throughput_qps += rate(finished, until.as_secs_f64());
        finished_by_first_empty += finished;
    }
    let total: u64 = clients.iter().map(|c| c.completed).sum();
    let wall_total = clients
        .iter()
        .filter_map(|c| c.last_completion)
        .max()
        .unwrap_or_default()
        .as_secs_f64();
    Throughput {
        throughput_qps,
        qps_wall: rate(total, wall_total),
        wall_time_till_first_empty_secs: first_empty.as_secs_f64(),
        wall_time_total_secs: wall_total,
        queries_finished_till_first_empty: finished_by_first_empty,
        queries_finished_total: total,
    }
}

/// Outcome of one timed run.
pub struct TimedRun<C> {
    /// The clients, handed back for the next run.
    pub clients: Vec<C>,
    pub throughput: Throughput,
    /// Every query's latency, all clients merged.
    pub latencies: LatencyCollector,
    /// Mean recall over the first pass through the query set.
    pub avg_recall: f64,
    /// CPU time this process used from starting the clients until the last
    /// one finished, leaving out the merging and recall that follow.
    pub cpu_secs: f64,
}

/// What one client task did during a timed run.
struct ClientRun<C> {
    client: C,
    started: Instant,
    completed: u64,
    last_completion: Option<Instant>,
    previous_completion: Option<Instant>,
    latencies: LatencyCollector,
    /// IDs returned on the first pass through the query set, by entry, for
    /// recall once the run is over.
    first_pass: Vec<(usize, Vec<i64>)>,
}

/// Run `total_queries` queries from all clients at once. Clients start
/// together behind a barrier and take query-set entries from a shared
/// counter, cycling through the `num_queries` entries. Recall is computed
/// on the first pass only, after the run, from the IDs each client kept.
///
/// On a failed query the other clients are aborted and their connections
/// dropped.
pub async fn run_timed<C: Send + 'static>(
    clients: Vec<C>,
    num_queries: usize,
    total_queries: u64,
    query: QueryFn<C>,
    recall: RecallFn,
) -> std::result::Result<TimedRun<C>, ClientFailure> {
    if clients.is_empty() || num_queries == 0 {
        return Ok(TimedRun {
            clients,
            throughput: throughput(&[], Duration::ZERO),
            latencies: LatencyCollector::new(),
            avg_recall: 0.0,
            cpu_secs: 0.0,
        });
    }

    let cpu_start = process_cpu_secs();
    let barrier = Arc::new(Barrier::new(clients.len()));
    let next = Arc::new(AtomicU64::new(0));
    let first_empty: Arc<OnceLock<Instant>> = Arc::new(OnceLock::new());

    let mut tasks = JoinSet::new();
    for client in clients {
        let (barrier, next, first_empty) = (barrier.clone(), next.clone(), first_empty.clone());
        let query = query.clone();
        tasks.spawn(async move {
            barrier.wait().await;
            let started = Instant::now();
            let mut run = ClientRun {
                client,
                started,
                completed: 0,
                last_completion: None,
                previous_completion: None,
                latencies: LatencyCollector::new(),
                first_pass: Vec::new(),
            };
            loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= total_queries {
                    first_empty.get_or_init(Instant::now);
                    return Ok(run);
                }
                let qi = (i % num_queries as u64) as usize;
                let pending = query(&run.client, qi);
                let start = Instant::now();
                let ids = pending.await.map_err(|error| ClientFailure {
                    query_index: qi,
                    error,
                })?;
                let done = Instant::now();
                run.latencies
                    .record(done.duration_since(start).as_micros() as u64);
                run.completed += 1;
                run.previous_completion = run.last_completion.replace(done);
                if i < num_queries as u64 {
                    run.first_pass.push((qi, ids));
                }
            }
        });
    }
    let runs = join_clients(tasks).await?;
    let cpu_secs = process_cpu_secs() - cpu_start;

    let t0 = runs
        .iter()
        .map(|r| r.started)
        .min()
        .expect("at least one client ran");
    let timings: Vec<ClientTiming> = runs
        .iter()
        .map(|r| ClientTiming {
            completed: r.completed,
            last_completion: r.last_completion.map(|t| t.duration_since(t0)),
            previous_completion: r.previous_completion.map(|t| t.duration_since(t0)),
        })
        .collect();
    let empty_at = *first_empty
        .get()
        .expect("every client stops only after finding no query left");
    let mut latencies = LatencyCollector::new();
    let (mut recall_sum, mut recall_count) = (0.0, 0u64);
    for r in &runs {
        latencies.merge(&r.latencies);
        for (qi, ids) in &r.first_pass {
            recall_sum += recall(*qi, ids);
            recall_count += 1;
        }
    }
    Ok(TimedRun {
        throughput: throughput(&timings, empty_at.duration_since(t0)),
        latencies,
        avg_recall: if recall_count > 0 {
            recall_sum / recall_count as f64
        } else {
            0.0
        },
        cpu_secs,
        clients: runs.into_iter().map(|r| r.client).collect(),
    })
}

/// Run `per_client` untimed queries on every client at once, so each
/// session builds its caches and settles on its cached plan before a timed
/// run. Client `c` starts at query-set entry `c * per_client`, so the
/// clients warm different queries.
pub async fn warm_up<C: Send + 'static>(
    clients: Vec<C>,
    num_queries: usize,
    per_client: usize,
    query: QueryFn<C>,
) -> std::result::Result<Vec<C>, ClientFailure> {
    if num_queries == 0 {
        return Ok(clients);
    }
    let mut tasks = JoinSet::new();
    for (c, client) in clients.into_iter().enumerate() {
        let query = query.clone();
        tasks.spawn(async move {
            for j in 0..per_client {
                let qi = (c * per_client + j) % num_queries;
                query(&client, qi).await.map_err(|error| ClientFailure {
                    query_index: qi,
                    error,
                })?;
            }
            Ok((c, client))
        });
    }
    let mut warmed = join_clients(tasks).await?;
    warmed.sort_by_key(|(c, _)| *c);
    Ok(warmed.into_iter().map(|(_, client)| client).collect())
}

/// Wait for every client task. The first failure aborts the rest.
async fn join_clients<T: Send + 'static>(
    mut tasks: JoinSet<std::result::Result<T, ClientFailure>>,
) -> std::result::Result<Vec<T>, ClientFailure> {
    let mut out = Vec::with_capacity(tasks.len());
    while let Some(joined) = tasks.join_next().await {
        match joined {
            Ok(Ok(value)) => out.push(value),
            Ok(Err(failure)) => {
                tasks.abort_all();
                return Err(failure);
            }
            Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
            Err(e) => unreachable!("client tasks are only aborted after a failure: {e}"),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ClientCount;

    /// (recall, qps) pairs standing in for sweep points.
    fn select(points: &[(f64, f64)], targets: &[f64]) -> Selection {
        select_operating_points(points, targets, |p| p.0, |p| p.1)
    }

    fn point(candidate: usize, recall_targets: &[f64]) -> SelectedPoint {
        SelectedPoint {
            candidate,
            recall_targets: recall_targets.to_vec(),
        }
    }

    #[test]
    fn single_param_sweep_picks_the_first_point_past_each_target() {
        // ef_search ascending: recall rises as QPS falls.
        let sweep = [
            (0.80, 5000.0),
            (0.94, 3000.0),
            (0.96, 2000.0),
            (0.985, 1200.0),
            (0.995, 600.0),
        ];
        assert_eq!(
            select(&sweep, &[0.95, 0.99]),
            Selection {
                points: vec![point(2, &[0.95]), point(4, &[0.99])],
                unreached: vec![],
            }
        );
    }

    #[test]
    fn grid_sweep_prefers_higher_qps_over_closest_recall() {
        // Point 0 is closest above 0.95, but point 1 reaches it faster.
        let sweep = [(0.951, 1000.0), (0.97, 1500.0), (0.90, 4000.0)];
        assert_eq!(select(&sweep, &[0.95]).points, vec![point(1, &[0.95])]);
    }

    #[test]
    fn targets_sharing_a_point_are_merged() {
        let sweep = [(0.90, 3000.0), (0.995, 800.0)];
        assert_eq!(
            select(&sweep, &[0.95, 0.99]).points,
            vec![point(1, &[0.95, 0.99])]
        );
    }

    #[test]
    fn unreached_targets_are_reported_not_selected() {
        let sweep = [(0.90, 3000.0), (0.98, 900.0)];
        assert_eq!(
            select(&sweep, &[0.95, 0.99]),
            Selection {
                points: vec![point(1, &[0.95])],
                unreached: vec![0.99],
            }
        );
    }

    #[test]
    fn targets_are_processed_in_ascending_order_without_duplicates() {
        let sweep = [(0.96, 2000.0), (0.995, 600.0)];
        assert_eq!(
            select(&sweep, &[0.99, 0.95, 0.99]).points,
            vec![point(0, &[0.95]), point(1, &[0.99])]
        );
    }

    #[test]
    fn no_candidates_leaves_every_target_unreached() {
        assert_eq!(
            select(&[], &[0.95, 0.99]),
            Selection {
                points: vec![],
                unreached: vec![0.95, 0.99],
            }
        );
    }
    #[test]
    fn queries_per_run_cycles_small_query_sets() {
        assert_eq!(queries_per_run(1000, 16), 1000);
        assert_eq!(queries_per_run(100, 32), 320);
        assert_eq!(queries_per_run(100, 10), 100);
    }

    fn timing(completed: u64, previous_secs: f64, last_secs: f64) -> ClientTiming {
        ClientTiming {
            completed,
            last_completion: Some(Duration::from_secs_f64(last_secs)),
            previous_completion: Some(Duration::from_secs_f64(previous_secs)),
        }
    }

    #[test]
    fn throughput_sums_each_clients_rate_up_to_the_first_empty() {
        // Four clients, 40 queries, and the first client finds the counter
        // empty at 1.00 s. Three clients finish their 10th query exactly
        // then, so all 10 count, over 1.00 s. The fourth gets a slow last
        // query that ends at 1.10 s, so only its first 9 count, over the
        // 0.95 s it took to finish them.
        let clients = [
            timing(10, 0.90, 1.00),
            timing(10, 0.90, 1.00),
            timing(10, 0.90, 1.00),
            timing(10, 0.95, 1.10),
        ];
        let t = throughput(&clients, Duration::from_secs_f64(1.00));
        // 3 × 10/1.00 + 9/0.95
        assert!((t.throughput_qps - 39.473_684).abs() < 1e-5, "{t:?}");
        // 40 / 1.10, drain included.
        assert!((t.qps_wall - 36.363_636).abs() < 1e-5, "{t:?}");
        assert_eq!(t.wall_time_till_first_empty_secs, 1.00);
        assert_eq!(t.wall_time_total_secs, 1.10);
        assert_eq!(t.queries_finished_till_first_empty, 39);
        assert_eq!(t.queries_finished_total, 40);
    }

    #[test]
    fn throughput_skips_clients_with_nothing_finished_by_the_first_empty() {
        let idle = ClientTiming {
            completed: 0,
            last_completion: None,
            previous_completion: None,
        };
        // Its only query is still running at the first empty.
        let one_slow_query = ClientTiming {
            completed: 1,
            last_completion: Some(Duration::from_secs_f64(2.5)),
            previous_completion: None,
        };
        let t = throughput(
            &[idle, one_slow_query, timing(10, 1.8, 2.0)],
            Duration::from_secs(2),
        );
        assert_eq!(t.throughput_qps, 5.0); // 10 / 2.0
        assert_eq!(t.queries_finished_till_first_empty, 10);
        assert_eq!(t.queries_finished_total, 11);
        assert_eq!(t.qps_wall, 4.4); // 11 / 2.5
    }

    #[test]
    fn throughput_of_no_clients_is_zero_not_nan() {
        let t = throughput(&[], Duration::ZERO);
        assert_eq!(t.throughput_qps, 0.0);
        assert_eq!(t.qps_wall, 0.0);
        assert_eq!(t.queries_finished_total, 0);
    }

    type Log = Arc<std::sync::Mutex<Vec<(usize, usize)>>>;

    /// A fake client query that logs (client, entry), yields so clients
    /// interleave, and returns the entry's index as the only neighbour.
    /// Entries in `fail_on` return an error instead.
    fn fake_query(log: Log, fail_on: Option<usize>) -> QueryFn<usize> {
        Arc::new(move |client: &usize, qi: usize| {
            let (log, client) = (log.clone(), *client);
            Box::pin(async move {
                tokio::task::yield_now().await;
                if Some(qi) == fail_on {
                    return Err(Error::Config(format!("query {qi} failed")));
                }
                log.lock().unwrap().push((client, qi));
                Ok(vec![qi as i64])
            })
        })
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn run_timed_runs_each_query_the_planned_number_of_times() {
        let log: Log = Default::default();
        let recalled: Log = Default::default();
        let recall_log = recalled.clone();
        // Recall of entry qi is qi/10, so a first pass over 7 entries
        // averages (0 + 1 + ... + 6) / 7 / 10 = 0.3.
        let recall: RecallFn = Arc::new(move |qi, ids: &[i64]| {
            recall_log.lock().unwrap().push((ids[0] as usize, qi));
            qi as f64 / 10.0
        });

        let run = run_timed(
            vec![0, 1, 2, 3],
            7,
            30,
            fake_query(log.clone(), None),
            recall,
        )
        .await
        .unwrap();

        // 30 queries over 7 entries: entries 0 and 1 run 5 times, the rest 4.
        let mut per_entry = [0usize; 7];
        for (_, qi) in log.lock().unwrap().iter() {
            per_entry[*qi] += 1;
        }
        assert_eq!(per_entry, [5, 5, 4, 4, 4, 4, 4]);
        assert_eq!(run.throughput.queries_finished_total, 30);
        assert!(run.throughput.queries_finished_till_first_empty <= 30);
        assert_eq!(run.latencies.count(), 30);
        assert_eq!(run.clients.len(), 4);

        // Recall saw each entry exactly once, with that entry's own result.
        let mut recalled = recalled.lock().unwrap().clone();
        recalled.sort();
        assert_eq!(recalled, (0..7).map(|qi| (qi, qi)).collect::<Vec<_>>());
        assert!((run.avg_recall - 0.3).abs() < 1e-12);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn run_timed_returns_the_failing_query_and_stops_the_others() {
        // Counts the queries started and dropped. A stuck query is only
        // dropped when its client task is aborted.
        struct DropCounter(Arc<AtomicU64>);
        impl Drop for DropCounter {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let started = Arc::new(AtomicU64::new(0));
        let dropped = Arc::new(AtomicU64::new(0));
        let (started_by, dropped_by) = (started.clone(), dropped.clone());
        // Entry 0 fails; every other query never completes on its own, so
        // the run only ends, and they are only dropped, if the other clients
        // are aborted.
        let query: QueryFn<usize> = Arc::new(move |_: &usize, qi: usize| {
            started_by.fetch_add(1, Ordering::SeqCst);
            let guard = DropCounter(dropped_by.clone());
            Box::pin(async move {
                if qi == 0 {
                    return Err(Error::Config("query 0 failed".to_string()));
                }
                let _guard = guard;
                futures_util::future::pending::<()>().await;
                unreachable!()
            })
        });
        let recall: RecallFn = Arc::new(|_, _| 1.0);
        let run = run_timed(vec![0, 1, 2, 3], 7, 1000, query, recall);
        let failure = match tokio::time::timeout(Duration::from_secs(30), run)
            .await
            .expect("the other clients must be aborted, not awaited")
        {
            Err(failure) => failure,
            Ok(_) => panic!("a failing query must fail the run"),
        };
        assert_eq!(failure.query_index, 0);
        assert!(failure.error.to_string().contains("query 0 failed"));

        // Every query that started, stuck ones included, is dropped. A
        // client aborted before it started one has nothing to drop.
        tokio::time::timeout(Duration::from_secs(30), async {
            while dropped.load(Ordering::SeqCst) < started.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("a stuck client was never aborted");
    }

    #[tokio::test]
    async fn run_timed_with_no_queries_hands_clients_back() {
        let recall: RecallFn = Arc::new(|_, _| 1.0);
        let run = run_timed(
            vec![0, 1],
            0,
            0,
            fake_query(Default::default(), None),
            recall,
        )
        .await
        .unwrap();
        assert_eq!(run.clients, vec![0, 1]);
        assert_eq!(run.throughput.queries_finished_total, 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn warm_up_gives_each_client_its_own_queries() {
        let log: Log = Default::default();
        let clients = warm_up(vec![0, 1, 2], 10, 4, fake_query(log.clone(), None))
            .await
            .unwrap();
        assert_eq!(clients, vec![0, 1, 2]);

        let mut ran = log.lock().unwrap().clone();
        ran.sort();
        // Client c runs entries 4c .. 4c+3, wrapping past the 10th.
        let expected = vec![
            (0, 0),
            (0, 1),
            (0, 2),
            (0, 3),
            (1, 4),
            (1, 5),
            (1, 6),
            (1, 7),
            (2, 0),
            (2, 1),
            (2, 8),
            (2, 9),
        ];
        assert_eq!(ran, expected);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn warm_up_reports_a_failing_query() {
        let failure =
            match warm_up(vec![0, 1], 10, 4, fake_query(Default::default(), Some(6))).await {
                Err(failure) => failure,
                Ok(_) => panic!("a failing warmup query must fail"),
            };
        assert_eq!(failure.query_index, 6);
    }
    #[test]
    fn levels_fit_only_with_room_for_rekalls_own_connections() {
        let levels = [
            (8, ClientCount::CpuMultiple(1.0)),
            (16, ClientCount::CpuMultiple(2.0)),
        ];
        // 8 clients + 4 own = 12 fits in 12; 16 + 4 = 20 does not.
        let (fit, skipped) = split_levels_by_fit(&levels, 12);
        assert_eq!(fit, vec![levels[0]]);
        assert_eq!(skipped, vec![levels[1]]);
        let (fit, skipped) = split_levels_by_fit(&levels, 11);
        assert!(fit.is_empty());
        assert_eq!(skipped.len(), 2);
        // A huge CPU multiple resolves to usize::MAX clients; adding
        // rekall's own connections must not wrap it into fitting.
        let huge = [(usize::MAX, ClientCount::CpuMultiple(1e300))];
        let (fit, skipped) = split_levels_by_fit(&huge, 100);
        assert!(fit.is_empty());
        assert_eq!(skipped.len(), 1);
    }

    #[test]
    fn free_connection_slots_honors_reserved_slots() {
        // Default PG: 100 slots, 3 superuser-reserved, 5 already open.
        assert_eq!(free_connection_slots(100, 3, false, 5), 92);
        assert_eq!(free_connection_slots(100, 3, true, 5), 95);
        assert_eq!(free_connection_slots(10, 3, false, 20), 0);
    }

    #[test]
    fn process_cpu_secs_counts_cpu_burned_by_this_process() {
        let before = process_cpu_secs();
        let mut x = 0u64;
        let rose = (0..1000).any(|_| {
            for i in 0..1_000_000u64 {
                x = x.wrapping_add(i * i);
            }
            std::hint::black_box(x);
            process_cpu_secs() > before
        });
        assert!(rose, "CPU time did not rise after a busy loop");
    }
}
