//! The runner's storage, timed apart from the node, and the windows a stall of it excuses. The load
//! generator, a process of its own, runs a thread that appends a page to a file on the node's filesystem
//! and syncs it every `PERIOD`, as the log's sync thread syncs each batch, and keeps its longest block
//! since it was last read: the sync's time less what the thread spent on a CPU or waiting for one, as
//! the kernel's scheduler counts it (`/proc/thread-self/schedstat`), with what the host did over the
//! same sync. The generator reads it as it closes each window's samples, with what the host did over the
//! window, so a block counts in the window whose orders and frames it held. On a healthy runner a sync
//! holds the thread a few milliseconds; when the storage itself stalls, it holds it as long as every
//! order and stream frame released only after the log's sync.
//!
//! A window is excused only by what the host shows over the probe's block, never by the node's figures:
//! the probe blocked at least `STALLED`, a disk with I/O in flight for at least half the block, and the
//! disks completing less than half the I/O and bytes a second they completed over the window, reads,
//! discards and flushes included, so they stalled rather than worked through a load, the node's
//! included. Only an order-to-fill or stream-lag tail over its bar by no more than the block is excused,
//! and only when every breach of the window is one. A stall in the node's own code, or cores it keeps
//! busy, leaves the probe unblocked, and a median, a price's stamp, which does not wait on the sync, and
//! the busy share are never excused. A host whose scheduler counts no thread's time runs no probe, since
//! it could excuse nothing.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

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

/// The longest block of the syncs that ended since the last `take`, at `since`, and the host's totals
/// then.
struct Longest {
    since: Instant,
    host: Waits,
    block: Block,
}

/// One sync of the probe: how long it held the thread blocked, and what the host did over it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(super) struct Block {
    pub blocked: Duration,
    pub host: Waited,
}

/// What the probe saw over a window: its longest block, what the host did over the window, and the
/// window's length in seconds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(super) struct Storage {
    pub block: Block,
    pub window: Waited,
    pub secs: f64,
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
                    eprintln!("LOAD storage probe stopped: {error}");
                    return;
                }
                probe.record(Block { blocked: blocked(took, scheduled), host }, ended);
                std::thread::sleep(PERIOD);
            }
        })?;
        Ok(Some(Probe { shared }))
    }

    /// The longest block of the syncs that ended since the last call, and what the host did since it.
    pub(super) fn take(&self) -> Storage {
        let (now, host) = (Instant::now(), Waits::now());
        let mut longest = self.shared.longest.lock().expect("the probe's block");
        let window = host.since(&longest.host);
        let secs = now.duration_since(longest.since).as_secs_f64();
        (longest.since, longest.host) = (now, host);
        Storage { block: std::mem::take(&mut longest.block), window, secs }
    }
}

impl Shared {
    fn new() -> Self {
        let longest = Longest { since: Instant::now(), host: Waits::now(), block: Block::default() };
        Shared { longest: Mutex::new(longest), stop: AtomicBool::new(false) }
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

/// Why the runner's storage excuses a window with `breaches`, if it does: the probe's longest block over
/// it, in `storage`, shows the storage stalled on its own, and every breach is an order-to-fill or
/// stream-lag tail over its bar by no more than that block.
pub(super) fn excuse(breaches: &[Breach], storage: &Storage) -> Option<String> {
    let (block, during) = (&storage.block, storage.block.host.disk_io?);
    let window = storage.window.disk_io?;
    let (blocked_ms, blocked_us) = (block.blocked.as_secs_f64() * 1e3, block.blocked.as_micros());
    // A disk had I/O in flight for at least half the block: the probe waited on storage, not on memory
    // or a lock, by a figure the kernel keeps per disk rather than averaged over the busy CPUs.
    let on_storage = block.blocked >= STALLED && u128::from(during.busiest_ms) * 2_000 >= blocked_us;
    // Under half the window's rate: `during / blocked < over_window / window / 2`, in whole microseconds.
    let window_us = (storage.secs * 1e6) as u128;
    let slower =
        |during: u64, over_window: u64| u128::from(during) * window_us * 2 < u128::from(over_window) * blocked_us;
    let stalled = on_storage && slower(during.ops, window.ops) && slower(during.bytes, window.bytes);
    let explained = |breach: &Breach| {
        let per_ms = if breach.budget.ends_with("_ms") { 1.0 } else { 1e3 };
        HELD_BY_THE_SYNC.contains(&breach.budget)
            && matches!(breach.quantile, "p99" | "p999")
            && breach.figure <= breach.bar + blocked_ms * per_ms
    };
    (stalled && !breaches.is_empty() && breaches.iter().all(explained)).then(|| {
        format!(
            "the probe blocked {blocked_ms:.0} ms on storage, a disk busy {} ms of it and the disks completing {} \
             I/Os meanwhile, against {} in the window",
            during.busiest_ms, during.ops, window.ops
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::load_scenario::host::DiskIo;

    /// A window of 24 s whose disks completed 300 I/Os and 3 MB a second, as a runner's do at launch flow.
    fn window(block: Block) -> Storage {
        let io = DiskIo { ops: 7_200, bytes: 72_000_000, busiest_ms: 2_000 };
        let window = Waited { io_ms: Some(400.0), cpu_ms: Some(1_000.0), disk_io: Some(io) };
        Storage { block, window, secs: 24.0 }
    }

    fn block(blocked_ms: u64, busiest_ms: u64, ops: u64, bytes: u64) -> Block {
        let disk_io = Some(DiskIo { ops, bytes, busiest_ms });
        Block {
            blocked: Duration::from_millis(blocked_ms),
            host: Waited { io_ms: Some(60.0), cpu_ms: Some(0.0), disk_io },
        }
    }

    /// A block of 240 ms through which a disk held I/O in flight and completed almost nothing. The
    /// host's storage wait over it, 60 ms, is averaged over the busy CPUs, as on a loaded runner.
    fn stall() -> Block {
        block(240, 240, 3, 12_288)
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
    fn a_window_the_storage_stalled_on_its_own_is_excused_whatever_the_averaged_storage_wait() {
        let why = excuse(&stalled_window(), &window(stall()));
        assert!(why.is_some_and(|why| why.contains("blocked 240 ms") && why.contains("busy 240 ms")));
        let unreported = Block { host: Waited { io_ms: None, ..stall().host }, ..stall() };
        assert!(excuse(&stalled_window(), &window(unreported)).is_some(), "the pressure files play no part");
    }

    /// Over a 240 ms block, half the window's rate is 36 I/Os and 360,000 bytes, and half the block 120 ms.
    #[test]
    fn a_window_is_judged_unless_the_host_shows_the_storage_stalled_on_its_own_over_the_block() {
        let cases = [
            ("no disk busy half the block, as for a wait on memory or a lock", block(240, 119, 3, 12_288)),
            ("the disks completing half their window's I/O", block(240, 240, 36, 12_288)),
            ("the disks moving half their window's bytes", block(240, 240, 3, 360_000)),
            ("no disk I/O reported", Block { host: Waited { disk_io: None, ..stall().host }, ..stall() }),
        ];
        for (case, held) in cases {
            assert_eq!(excuse(&stalled_window(), &window(held)), None, "{case}");
        }
        let just_inside = block(240, 120, 35, 359_999);
        assert!(excuse(&stalled_window(), &window(just_inside)).is_some(), "a disk busy half the block, under half");
        let quiet = [
            Storage { window: Waited { disk_io: None, ..window(stall()).window }, ..window(stall()) },
            Storage {
                window: Waited { disk_io: Some(DiskIo::default()), ..window(stall()).window },
                ..window(stall())
            },
        ];
        for storage in quiet {
            assert_eq!(excuse(&stalled_window(), &storage), None, "no window I/O: {storage:?}");
        }
        assert_eq!(excuse(&[], &window(stall())), None, "a window that held its bar needs no excuse");
        let slow = [breach("order_to_fill_us", "p99", 70_000.0, 35_975.0)];
        assert!(excuse(&slow, &window(block(50, 50, 0, 0))).is_some(), "a block of STALLED");
        assert_eq!(excuse(&slow, &window(block(49, 49, 0, 0))), None, "a block under STALLED");
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
            assert_eq!(excuse(&breaches, &window(stall())), None, "{case}");
        }
        let within =
            [breach("order_to_fill_us", "p99", 275_900.0, 35_975.0), breach("stream_lag_ms", "p99", 383.0, 144.0)];
        assert!(excuse(&within, &window(stall())).is_some(), "tails over their bars by just under the stall");
    }

    #[test]
    fn a_block_is_the_syncs_time_less_the_schedulers() {
        let took = Duration::from_millis(300);
        assert_eq!(blocked(took, Some(40_000_000)), Duration::from_millis(260), "the time on and for a CPU");
        assert_eq!(blocked(took, Some(300_000_000)), Duration::ZERO, "a sync spent waiting for a CPU");
        assert_eq!(blocked(took, Some(400_000_000)), Duration::ZERO, "counts past the sync's time");
        assert_eq!(blocked(took, None), Duration::ZERO, "no counts, no block");
    }

    /// The generator reads the probe as it closes each window: a block whose sync ended after the read
    /// counts in the next window, whose orders and frames it held, and one that ended before it in none.
    #[test]
    fn a_read_takes_the_longest_block_of_the_syncs_that_ended_since_the_last_with_the_window_it_spans() {
        let probe = Probe { shared: Arc::new(Shared::new()) };
        let (long, short) = (block(30, 30, 1, 4_096), block(12, 12, 1, 4_096));
        let before = Instant::now();
        for recorded in [short, long, short] {
            probe.shared.record(recorded, Instant::now());
        }
        let first = probe.take();
        assert_eq!(first.block, long);
        assert!(first.secs >= 0.0 && first.secs < 60.0, "the window from the probe's start: {first:?}");
        probe.shared.record(short, Instant::now());
        assert_eq!(probe.take().block, short, "a sync that ended after the read counts in the next window");
        assert_eq!(probe.take().block, Block::default(), "each block is read once");
        probe.shared.record(long, before);
        assert_eq!(
            probe.take().block,
            Block::default(),
            "a sync that ended before the last read belongs to its window"
        );
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
