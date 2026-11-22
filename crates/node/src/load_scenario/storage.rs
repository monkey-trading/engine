//! The runner's storage, timed apart from the node, and the windows a stall of it excuses. A thread of
//! its own appends a page to a file on the node's filesystem and syncs it every `PERIOD`, as the log's
//! sync thread syncs each batch, and keeps its longest block since it was last read: the sync's time
//! less what the thread spent on a CPU or waiting for one, as the kernel's scheduler counts it
//! (`/proc/thread-self/schedstat`), with what the host did over the same sync. On a healthy runner a
//! sync holds the thread a few milliseconds; when the storage itself stalls, it holds it as long as
//! every order and stream frame released only after the log's sync.
//!
//! A window is excused only by what the host shows over the probe's block, never by the node's figures:
//! the probe blocked at least `STALLED`, some task on the host waiting on storage throughout, and the
//! disks completing less than half the I/O and bytes a second they completed over the window, reads,
//! discards and flushes included, so they stalled rather than worked through a load, the node's
//! included. A host whose scheduler counts no thread's time runs no probe, since it could excuse
//! nothing. Even then only an order-to-fill
//! or stream-lag tail over its bar by no more than the block is excused, and only when every breach of
//! the window is one. A stall in the node's own code, or cores it keeps busy, leaves the probe
//! unblocked, and a median, a price's stamp, which does not wait on the sync, and the busy share are
//! never excused.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::load_scenario::host::{Waited, Waits};
use crate::load_scenario::report::Breach;

/// How often the probe syncs.
const PERIOD: Duration = Duration::from_millis(50);
/// What each sync writes: one page.
const PAGE: [u8; 4096] = [0; 4096];
/// The shortest block that shows the storage stalled: twice the order-to-fill p99 bar, and some twice
/// the longest sync a healthy Azure runner's probe took in 1,536 seconds, 29 ms.
const STALLED: Duration = Duration::from_millis(50);
/// The budgets a stall of the storage holds: an order's fill and a stream frame leave only once the
/// log's sync is done.
const HELD_BY_THE_SYNC: [&str; 2] = ["order_to_fill_us", "stream_lag_ms"];

/// The probe's thread, which stops when this is dropped.
pub(super) struct Probe {
    shared: Arc<Shared>,
}

struct Shared {
    longest: Mutex<Longest>,
    stop: AtomicBool,
}

/// The longest block of the syncs that ended since the last `take`, at `since`.
struct Longest {
    since: Instant,
    block: Block,
}

/// One sync of the probe: how long it held the thread blocked, and what the host did over it.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) struct Block {
    pub blocked: Duration,
    pub host: Waited,
}

impl Drop for Probe {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
    }
}

impl Probe {
    /// Starts syncing a page to `dir/storage-probe` every `PERIOD`, or None on a host whose scheduler
    /// counts no thread's time.
    /// Error cases:
    /// - the file cannot be created, or the thread cannot start
    pub(super) fn start(dir: &Path) -> std::io::Result<Option<Self>> {
        if scheduled_ns().is_none() {
            return Ok(None);
        }
        let mut file = OpenOptions::new().create(true).append(true).open(dir.join("storage-probe"))?;
        let shared = Arc::new(Shared::new());
        let probe = shared.clone();
        std::thread::Builder::new().name("storage-probe".into()).spawn(move || {
            while !probe.stop.load(Ordering::Relaxed) {
                // The scheduler's counts enclose the sync's time, and the host's figures enclose both.
                let host = Waits::now();
                let scheduled = scheduled_ns();
                let began = Instant::now();
                let synced = file.write_all(&PAGE).and_then(|()| file.sync_data());
                let (took, ended) = (began.elapsed(), Instant::now());
                let scheduled = scheduled.zip(scheduled_ns()).map(|(before, after)| after.saturating_sub(before));
                let host = Waits::now().since(&host);
                if let Err(error) = synced {
                    // A probe that cannot sync shows no stall, so it excuses nothing.
                    println!("LOAD storage probe stopped: {error}");
                    return;
                }
                probe.record(Block { blocked: blocked(took, scheduled), host }, ended);
                std::thread::sleep(PERIOD);
            }
        })?;
        Ok(Some(Probe { shared }))
    }

    /// The longest block of the syncs that ended since the last call.
    pub(super) fn take(&self) -> Block {
        let mut longest = self.shared.longest.lock().expect("the probe's block");
        longest.since = Instant::now();
        std::mem::take(&mut longest.block)
    }
}

impl Shared {
    fn new() -> Self {
        Shared {
            longest: Mutex::new(Longest { since: Instant::now(), block: Block::default() }),
            stop: AtomicBool::new(false),
        }
    }

    /// Keeps `block`, of a sync that `ended`, if it is the longest of those that ended since the last
    /// `take`: one that ended before it belongs to the window before.
    fn record(&self, block: Block, ended: Instant) {
        let mut longest = self.longest.lock().expect("the probe's block");
        if ended >= longest.since && block.blocked > longest.block.blocked {
            longest.block = block;
        }
    }
}

/// How long a sync of `took` held the thread blocked: less its `scheduled` nanoseconds on a CPU or
/// waiting for one, and nothing where the scheduler does not count them, since then a wait for a CPU
/// cannot be told from a block.
fn blocked(took: Duration, scheduled: Option<u64>) -> Duration {
    scheduled.map_or(Duration::ZERO, |scheduled| took.saturating_sub(Duration::from_nanos(scheduled)))
}

/// The calling thread's time on a CPU and waiting for one, in nanoseconds; None where the host does
/// not report it.
fn scheduled_ns() -> Option<u64> {
    std::fs::read_to_string("/proc/thread-self/schedstat").ok().as_deref().and_then(on_cpu_and_queued_ns)
}

/// The sum of a `schedstat`'s first two fields: the time on a CPU and the time waiting for one.
fn on_cpu_and_queued_ns(schedstat: &str) -> Option<u64> {
    let mut fields = schedstat.split_whitespace().map(str::parse::<u64>);
    Some(fields.next()?.ok()? + fields.next()?.ok()?)
}

/// Why the runner's storage excuses a window of `secs`, with `breaches` and the host's `window` figures,
/// if it does: the probe's longest `block` shows the storage stalled on its own, and every breach is an
/// order-to-fill or stream-lag tail over its bar by no more than that block.
pub(super) fn excuse(breaches: &[Breach], block: &Block, window: &Waited, secs: f64) -> Option<String> {
    let (io_wait_ms, (ops, bytes)) = (block.host.io_ms?, block.host.disk_io?);
    let (window_ops, window_bytes) = window.disk_io?;
    let blocked_ms = block.blocked.as_secs_f64() * 1e3;
    let waited_on_storage = block.blocked >= STALLED && io_wait_ms >= blocked_ms;
    // Under half the window's rate: `during / blocked < over_window / window / 2`, in whole microseconds.
    let (blocked_us, window_us) = (block.blocked.as_micros(), (secs * 1e6) as u128);
    let slower =
        |during: u64, over_window: u64| u128::from(during) * window_us * 2 < u128::from(over_window) * blocked_us;
    let stalled = waited_on_storage && slower(ops, window_ops) && slower(bytes, window_bytes);
    let explained = |breach: &Breach| {
        let per_ms = if breach.budget.ends_with("_ms") { 1.0 } else { 1e3 };
        HELD_BY_THE_SYNC.contains(&breach.budget)
            && matches!(breach.quantile, "p99" | "p999")
            && breach.figure <= breach.bar + blocked_ms * per_ms
    };
    (stalled && !breaches.is_empty() && breaches.iter().all(explained)).then(|| {
        format!(
            "the probe blocked {blocked_ms:.0} ms on storage, the host waiting {io_wait_ms:.0} ms and its disks \
             completing {ops} I/Os meanwhile, against {window_ops} in the window"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECS: f64 = 24.0;
    /// A window's disks completing 300 I/Os and 3 MB a second, as a runner's do at launch flow.
    const WINDOW: Waited = Waited { io_ms: Some(400.0), cpu_ms: Some(1_000.0), disk_io: Some((7_200, 72_000_000)) };

    fn block(blocked_ms: u64, io_ms: f64, ops: u64, bytes: u64) -> Block {
        let host = Waited { io_ms: Some(io_ms), cpu_ms: Some(0.0), disk_io: Some((ops, bytes)) };
        Block { blocked: Duration::from_millis(blocked_ms), host }
    }

    /// A block of 240 ms in which the disks completed almost nothing, the host waiting on them throughout.
    fn stall() -> Block {
        block(240, 245.0, 3, 12_288)
    }

    fn breach(budget: &'static str, quantile: &'static str, figure: f64, bar: f64) -> Breach {
        Breach { budget, quantile, figure, bar }
    }

    /// A launch-flow window a 250 ms stall broke, as on the runners whose storage held the log's syncs.
    fn stalled_window() -> Vec<Breach> {
        vec![
            breach("order_to_fill_us", "p99", 249_531.0, 35_975.0),
            breach("order_to_fill_us", "p999", 249_531.0, 143_900.0),
            breach("stream_lag_ms", "p99", 198.0, 144.0),
        ]
    }

    #[test]
    fn a_window_the_storage_stalled_on_its_own_is_excused() {
        let why = excuse(&stalled_window(), &stall(), &WINDOW, SECS);
        assert!(why.is_some_and(|why| why.contains("240 ms")));
    }

    /// Over a 240 ms block, half the window's rate is 36 I/Os and 360,000 bytes.
    #[test]
    fn a_window_is_judged_unless_the_host_shows_the_storage_stalled_on_its_own_over_the_block() {
        let cases = [
            ("less storage wait over the block than the block", block(240, 200.0, 3, 12_288)),
            ("the disks completing half their window's I/O", block(240, 245.0, 36, 12_288)),
            ("the disks moving half their window's bytes", block(240, 245.0, 3, 360_000)),
            ("no storage wait reported", Block { host: Waited { io_ms: None, ..stall().host }, ..stall() }),
            ("no disk I/O reported", Block { host: Waited { disk_io: None, ..stall().host }, ..stall() }),
        ];
        for (case, held) in cases {
            assert_eq!(excuse(&stalled_window(), &held, &WINDOW, SECS), None, "{case}");
        }
        let just_under = block(240, 245.0, 35, 359_999);
        assert!(excuse(&stalled_window(), &just_under, &WINDOW, SECS).is_some(), "just under half the window's rate");
        for window in [Waited { disk_io: None, ..WINDOW }, Waited { disk_io: Some((0, 0)), ..WINDOW }] {
            assert_eq!(excuse(&stalled_window(), &stall(), &window, SECS), None, "no window I/O: {window:?}");
        }
        assert_eq!(excuse(&[], &stall(), &WINDOW, SECS), None, "a window that held its bar needs no excuse");
        let slow = [breach("order_to_fill_us", "p99", 70_000.0, 35_975.0)];
        assert!(excuse(&slow, &block(50, 50.0, 0, 0), &WINDOW, SECS).is_some(), "a block of STALLED");
        assert_eq!(excuse(&slow, &block(49, 49.0, 0, 0), &WINDOW, SECS), None, "a block under STALLED");
    }

    #[test]
    fn only_an_order_or_frame_tail_the_stall_explains_is_excused() {
        let unexplained = [
            ("a median", breach("order_to_fill_us", "p50", 9_000.0, 7_195.0)),
            ("the busy share", breach("sequencer_busy_share", "mean", 0.6, 0.5)),
            ("a price's stamp, which no sync holds", breach("price_to_stamp_us", "p999", 81_000.0, 50_000.0)),
            ("an order held past the bar and the stall", breach("order_to_fill_us", "p99", 276_000.0, 35_975.0)),
            ("a frame held past the bar and the stall", breach("stream_lag_ms", "p99", 385.0, 144.0)),
        ];
        for (case, extra) in unexplained {
            let mut breaches = stalled_window();
            breaches.push(extra);
            assert_eq!(excuse(&breaches, &stall(), &WINDOW, SECS), None, "{case}");
        }
        let within =
            [breach("order_to_fill_us", "p99", 275_900.0, 35_975.0), breach("stream_lag_ms", "p99", 383.0, 144.0)];
        assert!(excuse(&within, &stall(), &WINDOW, SECS).is_some(), "tails over their bars by just under the stall");
    }

    #[test]
    fn a_block_is_the_syncs_time_less_the_schedulers() {
        let took = Duration::from_millis(300);
        assert_eq!(blocked(took, Some(40_000_000)), Duration::from_millis(260), "the time on and for a CPU");
        assert_eq!(blocked(took, Some(300_000_000)), Duration::ZERO, "a sync spent waiting for a CPU");
        assert_eq!(blocked(took, Some(400_000_000)), Duration::ZERO, "counts past the sync's time");
        assert_eq!(blocked(took, None), Duration::ZERO, "no counts, no block");
    }

    #[test]
    fn a_read_takes_the_longest_block_of_the_syncs_that_ended_since_the_last() {
        let probe = Probe { shared: Arc::new(Shared::new()) };
        let (long, short) = (block(30, 30.0, 1, 4_096), block(12, 12.0, 1, 4_096));
        let before = Instant::now();
        for recorded in [short, long, short] {
            probe.shared.record(recorded, Instant::now());
        }
        assert_eq!(probe.take(), long);
        assert_eq!(probe.take(), Block::default(), "each block is read once");
        probe.shared.record(long, before);
        assert_eq!(probe.take(), Block::default(), "a sync that ended before the last read belongs to its window");
    }

    #[test]
    fn a_threads_scheduled_time_is_its_time_on_a_cpu_and_waiting_for_one() {
        assert_eq!(on_cpu_and_queued_ns("1500 250 7\n"), Some(1_750));
        assert_eq!(on_cpu_and_queued_ns("1500\n"), None);
        assert_eq!(on_cpu_and_queued_ns(""), None);
        // CI's Linux runners must count a thread's time, or the probe would never run and no stall of
        // their storage could be excused.
        if cfg!(target_os = "linux") {
            let (before, spinning) = (scheduled_ns().expect("Linux reports a thread's schedstat"), Instant::now());
            while scheduled_ns().expect("again") == before {
                assert!(spinning.elapsed() < Duration::from_secs(2), "the thread's time on a CPU never grew");
            }
        }
    }
}
