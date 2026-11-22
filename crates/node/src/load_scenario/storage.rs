//! The runner's storage, timed apart from the node, and the windows a stall of it excuses. The load
//! generator, a process of its own, runs a thread that writes a page over the start of a file it keeps
//! on the node's filesystem every `PERIOD`, directly and synchronously (`O_DIRECT | O_DSYNC`), so no page
//! cache, journal or other file's lock lies in its path, and keeps each write's block: its time less what
//! the thread spent on a CPU or waiting for one, as the kernel's scheduler counts it
//! (`/proc/thread-self/schedstat`), which leaves the device's own latency, with what the disks under
//! that filesystem did over the same write (`host::backing_disks`). On a healthy runner a write holds
//! the thread a few milliseconds; when the storage itself stalls, it holds it as long as every order
//! and stream frame released only after the log's sync.
//!
//! The generator reads the probe as it closes each window, a block counting in the window it ended in:
//! one that ended `MARGIN` or more before the generator began taking the window's samples counts in it,
//! one that ended after it had taken them counts in the next, and one that ended in between counts in
//! neither, since the orders and frames it held may land on either side.
//!
//! A window is excused only by what the host shows over the probe's longest block, never by the node's
//! figures: the probe blocked at least `STALLED`, and the disks under its file completed no flush, which
//! may destage writes the node made earlier, and less than half the I/O and bytes a second they
//! completed over the window, reads and discards included, so they stalled rather than worked through a
//! load, the node's included. Only an order-to-fill or stream-lag tail over its bar by no more than the
//! block is excused, and only when every breach of the window is one. A stall in the node's own code,
//! or cores it keeps busy, leaves the probe unblocked, and a median, a price's stamp, which does not
//! wait on the sync, and the busy share are never excused. A host whose scheduler counts no thread's
//! time, that names no disk under the probe's file or whose filesystem takes no direct writes runs no
//! probe, since it could excuse nothing.

use std::collections::BTreeSet;
use std::fs::File;
use std::io::ErrorKind;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::load_scenario::host::{Waited, Waits, backing_disks};
use crate::load_scenario::report::Breach;

/// How often the probe writes.
const PERIOD: Duration = Duration::from_millis(50);
/// What each write writes: one page, at the alignment a direct write needs.
const PAGE: usize = 4096;
/// The shortest block that shows the storage stalled: twice the order-to-fill p99 bar, and some twice
/// the longest a healthy Azure runner held a probe that appended and synced a page over 1,536 seconds,
/// 29 ms.
const STALLED: Duration = Duration::from_millis(50);
/// How long before the generator begins taking a window's samples a block must have ended to count in
/// it: the orders and frames a stall held leave once it ends, and reach the generator within a few
/// milliseconds.
pub(super) const MARGIN: Duration = Duration::from_millis(100);
/// The budgets a stall of the storage holds: an order's fill and a stream frame leave only once the
/// log's sync is done.
const HELD_BY_THE_SYNC: [&str; 2] = ["order_to_fill_us", "stream_lag_ms"];

/// The probe's thread, which stops when this is dropped.
pub(super) struct Probe {
    shared: Arc<Shared>,
}

struct Shared {
    /// The disks under the probe's file.
    disks: BTreeSet<String>,
    blocks: Mutex<Blocks>,
    stop: AtomicBool,
}

/// The blocks of the writes that ended since the last cut, at `since`, with when each ended, and the
/// disks' totals at the cut.
struct Blocks {
    since: Instant,
    host: Waits,
    ended: Vec<(Instant, Block)>,
}

/// One write of the probe: how long it held the thread blocked, and what the disks under the probe's
/// file did over it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(super) struct Block {
    pub blocked: Duration,
    pub host: Waited,
}

/// What the probe saw over a window: its longest block, what the disks under its file did over the
/// window, and the window's length in seconds.
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
    /// Starts writing a page over the start of `dir/storage-probe` every `PERIOD`, or None on a host
    /// whose scheduler counts no thread's time, that names no disk under the file or whose filesystem
    /// takes no direct writes.
    /// Error cases:
    /// - the file cannot be created or first written but for want of direct writes, or the thread
    ///   cannot start
    pub(super) fn start(dir: &Path) -> std::io::Result<Option<Self>> {
        let path = dir.join("storage-probe");
        // A page at the alignment a direct write needs, the heap block staying put as the buffer moves,
        // and of bytes no layer below could take for a hole.
        let mut buffer = vec![0xa5u8; 2 * PAGE];
        let at = buffer.as_ptr().align_offset(PAGE);
        // The first write gives the page its place on the disk, so each later one only overwrites it.
        let file = match open_direct(&path).and_then(|file| file.write_all_at(&buffer[at..at + PAGE], 0).map(|()| file))
        {
            Ok(file) => file,
            Err(error) if matches!(error.kind(), ErrorKind::InvalidInput | ErrorKind::Unsupported) => return Ok(None),
            Err(error) => return Err(error),
        };
        let Some(disks) = backing_disks(&path).filter(|_| scheduled_ns().is_some()) else { return Ok(None) };
        let shared = Arc::new(Shared::new(disks, Instant::now()));
        let probe = shared.clone();
        std::thread::Builder::new().name("storage-probe".into()).spawn(move || {
            let page = &mut buffer[at..at + PAGE];
            while !probe.stop.load(Ordering::Relaxed) {
                // The scheduler's counts enclose the write's time, and the disks' readings enclose both.
                let host = Waits::now_on(&probe.disks);
                let scheduled = scheduled_ns();
                let began = Instant::now();
                let written = file.write_all_at(page, 0);
                let (took, ended) = (began.elapsed(), Instant::now());
                let scheduled = scheduled.zip(scheduled_ns()).map(|(before, after)| after.saturating_sub(before));
                let host = Waits::now_on(&probe.disks).since(&host);
                if let Err(error) = written {
                    // A probe that cannot write shows no stall, so it excuses nothing.
                    eprintln!("LOAD storage probe stopped: {error}");
                    return;
                }
                probe.record(Block { blocked: blocked(took, scheduled), host }, ended);
                // Each write carries bytes the last did not, so no layer below can pass it by.
                page[0] = page[0].wrapping_add(1);
                std::thread::sleep(PERIOD);
            }
        })?;
        Ok(Some(Probe { shared }))
    }

    /// What the probe saw over the window whose samples the generator began taking at `begun` and had
    /// taken by `cut`.
    pub(super) fn take(&self, begun: Instant, cut: Instant) -> Storage {
        self.shared.take(begun, cut, Waits::now_on(&self.shared.disks))
    }
}

impl Shared {
    /// The probe's state from `since`, its first window's start.
    fn new(disks: BTreeSet<String>, since: Instant) -> Self {
        let blocks = Blocks { since, host: Waits::now_on(&disks), ended: Vec::new() };
        Shared { disks, blocks: Mutex::new(blocks), stop: AtomicBool::new(false) }
    }

    /// Keeps `block`, of a write that `ended`, unless it ended before the last cut, whose window is read.
    fn record(&self, block: Block, ended: Instant) {
        let mut blocks = self.blocks.lock().expect("the probe's blocks");
        if ended >= blocks.since {
            blocks.ended.push((ended, block));
        }
    }

    /// The window whose samples were begun at `begun` and taken by `cut`, the disks' totals then being
    /// `host`: its longest block of those that ended `MARGIN` or more before `begun`, and what the disks
    /// did since the last cut. A block that ended after the cut is kept for the next window, and one that
    /// ended between counts in neither.
    fn take(&self, begun: Instant, cut: Instant, host: Waits) -> Storage {
        let mut blocks = self.blocks.lock().expect("the probe's blocks");
        let (window, secs) = (host.since(&blocks.host), cut.saturating_duration_since(blocks.since).as_secs_f64());
        let counted = |ended: &Instant| ended.checked_add(MARGIN).is_some_and(|counted| counted <= begun);
        let longest = blocks.ended.iter().filter(|(ended, _)| counted(ended)).map(|(_, block)| *block);
        let block = longest.max_by_key(|block| block.blocked).unwrap_or_default();
        blocks.ended.retain(|(ended, _)| *ended > cut);
        (blocks.since, blocks.host) = (cut, host);
        Storage { block, window, secs }
    }
}

/// `path` opened for direct, synchronous writes, each done once the device has it; refused as invalid
/// where the filesystem takes none.
#[cfg(target_os = "linux")]
fn open_direct(path: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .custom_flags(nix::libc::O_DIRECT | nix::libc::O_DSYNC)
        .open(path)
}

/// Direct writes are Linux's.
#[cfg(not(target_os = "linux"))]
fn open_direct(_path: &Path) -> std::io::Result<File> {
    Err(std::io::Error::new(ErrorKind::Unsupported, "direct writes are Linux's"))
}

#[cfg(test)]
impl Probe {
    /// A probe with no thread whose first window starts at `since`, and whose blocks a test records.
    pub(super) fn idle(since: Instant) -> Self {
        Probe { shared: Arc::new(Shared::new(BTreeSet::new(), since)) }
    }

    /// Records `block`, of a write that `ended`, as the probe's thread does.
    pub(super) fn record(&self, block: Block, ended: Instant) {
        self.shared.record(block, ended);
    }
}

/// How long a write of `took` held the thread blocked: less its `scheduled` nanoseconds on a CPU or
/// waiting for one, a request its plug held while it waited for a CPU included, and nothing where the
/// scheduler does not count them, since then a wait for a CPU cannot be told from a block.
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
    // A flush may destage writes the node made earlier, so a write held behind one shows the node's load
    // as much as the storage's.
    let on_storage = block.blocked >= STALLED && during.flushes == 0;
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
            "the probe's write blocked {blocked_ms:.0} ms, its disks completing {} I/Os and no flush meanwhile, \
             against {} in the window",
            during.ops, window.ops
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::load_scenario::host::{DiskIo, Disks};

    /// A window of 24 s whose disks completed 300 I/Os and 3 MB a second, as a runner's do at launch flow.
    fn window(block: Block) -> Storage {
        let io = DiskIo { ops: 7_200, bytes: 72_000_000, flushes: 0 };
        let window = Waited { io_ms: Some(400.0), cpu_ms: Some(1_000.0), disk_io: Some(io) };
        Storage { block, window, secs: 24.0 }
    }

    /// A write blocked `blocked_ms`, over which the disks under the probe's file completed `ops` I/Os
    /// moving `bytes`, and no flush.
    fn block(blocked_ms: u64, ops: u64, bytes: u64) -> Block {
        let disk_io = Some(DiskIo { ops, bytes, flushes: 0 });
        let host = Waited { io_ms: Some(60.0), cpu_ms: Some(0.0), disk_io };
        Block { blocked: Duration::from_millis(blocked_ms), host }
    }

    /// A write held 240 ms while the disks under it completed its page and two others. The host's storage
    /// wait over it, 60 ms, is averaged over the busy CPUs, as on a loaded runner.
    fn stall() -> Block {
        block(240, 3, 12_288)
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
        assert!(why.is_some_and(|why| why.contains("blocked 240 ms") && why.contains("completing 3 I/Os")));
        let unreported = Block { host: Waited { io_ms: None, ..stall().host }, ..stall() };
        assert!(excuse(&stalled_window(), &window(unreported)).is_some(), "the pressure files play no part");
    }

    /// Over a 240 ms block, half the window's rate is 36 I/Os and 360,000 bytes.
    #[test]
    fn a_window_is_judged_unless_the_host_shows_the_storage_stalled_on_its_own_over_the_block() {
        let flushed = |flushes| Block {
            host: Waited { disk_io: Some(DiskIo { flushes, ..stall().host.disk_io.expect("I/O") }), ..stall().host },
            ..stall()
        };
        let cases = [
            ("a flush, which may destage the node's earlier writes, as a write carrying a preflush does", flushed(1)),
            ("the disks completing half their window's I/O", block(240, 36, 12_288)),
            ("the disks moving half their window's bytes", block(240, 3, 360_000)),
            ("no disk I/O reported", Block { host: Waited { disk_io: None, ..stall().host }, ..stall() }),
        ];
        for (case, held) in cases {
            assert_eq!(excuse(&stalled_window(), &window(held)), None, "{case}");
        }
        let just_inside = block(240, 35, 359_999);
        assert!(excuse(&stalled_window(), &window(just_inside)).is_some(), "just under half");
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
        assert!(excuse(&slow, &window(block(50, 1, 4_096))).is_some(), "a block of STALLED");
        assert_eq!(excuse(&slow, &window(block(49, 1, 4_096))), None, "a block under STALLED");
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
    fn a_block_is_the_writes_time_less_the_schedulers() {
        let took = Duration::from_millis(300);
        // A request held in the thread's plug while it waited for a CPU counts as that wait.
        assert_eq!(blocked(took, Some(40_000_000)), Duration::from_millis(260), "the time on and for a CPU");
        assert_eq!(blocked(took, Some(300_000_000)), Duration::ZERO, "a write spent waiting for a CPU");
        assert_eq!(blocked(took, Some(400_000_000)), Duration::ZERO, "counts past the write's time");
        assert_eq!(blocked(took, None), Duration::ZERO, "no counts, no block");
    }

    /// A window whose samples were begun at `begun` and taken by `cut` takes the longest block of the
    /// writes that ended `MARGIN` or more before `begun`, keeps one that ended after the cut for the next
    /// window, and drops one that ended in between, whose orders and frames may have landed on either side.
    #[test]
    fn a_window_counts_each_block_in_the_window_its_write_ended_in() {
        let shared = Shared::new(BTreeSet::new(), Instant::now());
        let epoch = shared.blocks.lock().expect("the blocks").since;
        let at = |ms: u64| epoch + Duration::from_millis(ms);
        let (early, late, taking, next) = (block(240, 3, 0), block(300, 3, 0), block(500, 3, 0), block(60, 1, 0));
        // The samples are begun at 24 s and taken 300 ms later.
        let (begun, cut) = (at(24_000), at(24_300));
        shared.record(block(30, 1, 0), at(1_000));
        shared.record(early, at(10_000));
        shared.record(late, at(23_950));
        shared.record(taking, at(24_150));
        shared.record(next, at(24_310));
        let first = shared.take(begun, cut, Waits::default()).block;
        assert_eq!(first, early, "the blocks within MARGIN of the samples and while they were taken count in neither");
        shared.record(block(400, 1, 0), at(24_200));
        assert_eq!(
            shared.take(at(48_300), at(48_400), Waits::default()).block,
            next,
            "a block recorded after the cut it ended before counts in none"
        );
    }

    #[test]
    fn a_cut_takes_what_the_disks_did_since_the_last_cut() {
        let shared = Shared::new(BTreeSet::new(), Instant::now());
        let host =
            |ops, bytes, flushes| Waits { io_us: None, cpu_us: None, disks: Some(Disks { ops, bytes, flushes }) };
        let start = shared.blocks.lock().expect("the blocks").since;
        let first = shared.take(start, start + Duration::from_secs(24), host(1_000, 10_000, 5));
        assert_eq!(first.secs, 24.0);
        let second = shared.take(start, start + Duration::from_secs(48), host(8_200, 82_000, 7));
        let io = DiskIo { ops: 7_200, bytes: 72_000, flushes: 2 };
        assert_eq!((second.window.disk_io, second.secs), (Some(io), 24.0), "the delta since the first cut");
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
