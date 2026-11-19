//! A member's replica of the generated log, and the tail handovers it takes from the node holding the
//! log while that node sequences launch flow. The member's copy is the log as it followed it, its
//! completed segments linked rather than copied, and its replica is built by one replay of the copy
//! from the book (`Follower::open_seeded`), writing each rollover's rows as a member's does. Each tail
//! grows on the holder to its size under launch flow, which the holder then stops behind a final
//! checkpoint; the holder reads the tail past the member's tip from its log (`read_tail`), and the
//! member takes it through the real successor path (`take_tail`) from a stand-in predecessor serving
//! those bytes with the keys, applying it to its replica, sealing the keys and confirming. Its node
//! then starts from the replica, with no replay, and stamps its first command.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use engine_core::types::AccountId;
use tokio::net::TcpListener;

use crate::config::{HistorySection, NodeConfig};
use crate::handover::assigned::Bound;
use crate::handover::stand_in::{Answer, serve_tail};
use crate::handover::tail::{MAX_TAIL_BYTES, Prepared, read_tail, take_tail};
use crate::handover::{REPLY_TIMEOUT, tail_over, tail_room};
use crate::history::HistoryPlan;
use crate::history::derived::RowsStore;
use crate::history::marks::Retentions;
use crate::history::marks::fold::MarkFold;
use crate::log::{LogEntry, MANIFEST, Manifest, PrefixRing};
use crate::log_check::answer::CheckedHeads;
use crate::sequencer::recovery_scale::Base;
use crate::sequencer::recovery_scale::flow::{Flow, Node, SEGMENT_COMMANDS};
use crate::sequencer::recovery_scale::restart::DOMAIN;
use crate::standby::Follower;
use crate::tee::keys::{Head, KeySet};
use crate::tee::sealing::MockSealer;
use crate::tee::{EnclaveKey, MockAttestor};

/// The member's stand-in launch measurement.
const DIGEST: [u8; 48] = [0x5e; 48];
/// How far under `MAX_TAIL_BYTES` the largest tail is grown to, for the growth's last step and the
/// holder's final checkpoint: well over either.
const TAIL_HEADROOM: u64 = 1 << 20;

/// One tail handover: the tail's bytes and entries, the holder's stop (its final checkpoint and the
/// segment it seals) and read of it, the member's take of it (the transfer, its apply to the replica
/// and the copy, the keys sealed and the confirmation), whether the take confirmed within the
/// predecessor's wait for it, and what a release build's predecessor would refuse it for before its
/// stop, if anything.
pub(super) struct Switch {
    pub bytes: u64,
    pub entries: u64,
    pub read: Duration,
    pub take: Duration,
    pub confirmed: bool,
    pub refused: Option<&'static str>,
}

impl Switch {
    /// The switch's window with `start`, the member's start from its replica to its first command, but
    /// for the chain reads and stops a handover makes whatever its tail.
    pub(super) fn window(&self, start: Duration) -> Duration {
        self.read + self.take + start
    }
}

/// Copies the log in `from` to `to` as a member's copy of it: each completed segment linked, since
/// neither writer changes it, and the manifest and the open segment copied.
/// Error cases:
/// - a file cannot be read, linked or written
pub(super) fn copy_log(from: &Path, to: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(to)?;
    let listed: BTreeSet<String> = Manifest::load(from)?.segments.iter().map(|info| info.file.clone()).collect();
    for file in std::fs::read_dir(from)? {
        let file = file?;
        let name = file.file_name().to_string_lossy().into_owned();
        match listed.contains(&name) {
            true => std::fs::hard_link(file.path(), to.join(&name))?,
            false => _ = std::fs::copy(file.path(), to.join(&name))?,
        }
    }
    anyhow::ensure!(to.join(MANIFEST).exists(), "the copy holds no manifest");
    Ok(())
}

/// The member's replica of its copy in `dir`, built by one replay from the book as a member builds
/// it, with the time it took: recording every command into a dashboard record, folding every mark,
/// recording the checkpoints it verifies, writing each rollover's rows to `rows`, and checking the
/// history index in `history` against every entry.
pub(super) fn build_replica(
    base: &Base,
    dir: &Path,
    signer: AccountId,
    (rows, history): (Arc<RowsStore>, &HistorySection),
) -> (Follower, Duration) {
    // Decoded before the clock starts, as the other replays' books are: a member's own build starts
    // from an empty engine. A retried build decodes it again.
    let decoded = std::cell::Cell::new(Some(base.engine()));
    let fresh = || decoded.take().unwrap_or_else(|| base.engine());
    let started = Instant::now();
    let marks = Rc::new(RefCell::new(MarkFold::new(Retentions::defaults())));
    let records = (Some(crate::test_support::stats()), Some(marks), Some(CheckedHeads::default()), Some(rows));
    let mut plan = HistoryPlan::for_member(history).expect("the member's history plan");
    let observe = |entry: &LogEntry| {
        plan.observe(entry);
        Ok(())
    };
    let follower =
        Follower::open_seeded(dir, SEGMENT_COMMANDS, (DOMAIN, signer, [signer].into()), records, fresh, observe)
            .expect("the copy replays from the book");
    (follower, started.elapsed())
}

/// The member's node config: the sealing directory under `run`, in the log's deployment.
pub(super) fn member_config(run: &Path) -> NodeConfig {
    let mut cfg = crate::test_support::dev_config();
    cfg.custody.chain_id = DOMAIN.chain_id;
    cfg.custody.vault = DOMAIN.vault.to_string();
    cfg.tee.use_published_mock_key = false;
    cfg.tee.sealing = Some(crate::config::SealingSection { dir: run.join("sealed"), handover: None, secrets: None });
    cfg
}

/// Grows the tail past `follower`'s tip on `holder` under launch flow to about `bytes`, or
/// `TAIL_HEADROOM` under the handover's bound if that is less, stops the holder behind a final
/// checkpoint, and hands the tail over to the member, whose replica takes it and which seals the keys
/// in `generation`, its turn's. `peak_bytes_per_sec` is the log a second of launch flow writes, every
/// market open.
pub(super) async fn hand_over(
    (holder, flow, key): (&mut Node, &mut Flow, &Arc<EnclaveKey>),
    (follower, cfg, generation): (&mut Follower, &NodeConfig, u64),
    (bytes, peak_bytes_per_sec): (u64, f64),
    dir: &Path,
) -> Switch {
    let (tip, _) = follower.tip().expect("the copy holds a command");
    // Each step writes about half of what is left at the fastest rate seen, and the growth stops a tenth
    // of a second short of the size, so a step that writes more than its share passes it by a few
    // commands at most.
    let size = bytes.min(MAX_TAIL_BYTES - TAIL_HEADROOM);
    let (from, mut rate) = (log_bytes(dir), peak_bytes_per_sec);
    loop {
        let grown = log_bytes(dir) - from;
        let left = size.saturating_sub(grown) as f64;
        if left < rate / 10.0 {
            break;
        }
        let step = Duration::from_secs_f64(left / rate / 2.0).max(Duration::from_millis(1));
        flow.run(holder, step, 1.0, |_, _, _| {});
        rate = rate.max((log_bytes(dir) - from - grown) as f64 / step.as_secs_f64());
    }
    // What the predecessor checks before its stop: the tip among the prefixes its sequencer retains,
    // as many as a release build's, and the tail within the room it leaves under the bound.
    let behind = holder.seq.last_seq().unwrap_or(0) - tip;
    let refused = match () {
        _ if behind >= PrefixRing::RELEASE_CAPACITY as u64 => Some("prefix_ring"),
        _ if tail_over(dir, tip, tail_room()) => Some("tail_room"),
        _ => None,
    };
    // The holder's stop: its final checkpoint, and that checkpoint's segment sealed, as an admitted
    // handover seals it; then the tail read from its log.
    let started = Instant::now();
    let checkpoint = holder.checkpoint_now();
    let head = Head { seq: checkpoint.seq, root: checkpoint.root.clone() };
    holder.seq.log.seal_open_segment().expect("the holder seals its log's last segment");
    let (listing, tail) = read_tail(dir, tip, &head).expect("the holder reads the tail past the member's tip");
    let read = started.elapsed();
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("a loopback listener");
    let address = listener.local_addr().expect("its address").to_string();
    let mut keys = KeySet::for_key(key).expect("the holder's keys");
    keys.predecessor_head = Some(head);
    let (served, _) = serve_tail(listener, DOMAIN, keys, vec![Answer::Tail(tail)]);
    let signer = AccountId(key.address());
    let prepared = Prepared { generation, signer, posted: None, bound: Bound::none() };
    let sealer = MockSealer { launch_digest: DIGEST };
    let started = Instant::now();
    let taken = take_tail(cfg, &address, &sealer, &MockAttestor, &prepared, (follower, None)).await;
    let take = started.elapsed();
    served.abort();
    let confirmed = match taken {
        Ok(taken) => {
            assert_eq!((taken.after, taken.bytes), (tip, listing.bytes), "the member took the tail the holder read");
            true
        }
        // A predecessor waits `REPLY_TIMEOUT` for the confirmation, which a take applying a tail for
        // longer than that misses: a measurement, as the take's own time shows.
        Err(error) if take >= REPLY_TIMEOUT => {
            println!("RECOVERY tail bytes={} unconfirmed after {:.0} s: {error:#}", listing.bytes, take.as_secs_f64());
            false
        }
        Err(error) => panic!("the member takes the tail: {error:#}"),
    };
    let switch = Switch { bytes: listing.bytes, entries: listing.entries, read, take, confirmed, refused };
    println!(
        "RECOVERY tail bytes={} entries={} read_s={:.3} take_s={:.3} confirmed={confirmed} admitted={}",
        switch.bytes,
        switch.entries,
        read.as_secs_f64(),
        take.as_secs_f64(),
        refused.map_or("yes".to_owned(), |why| format!("no ({why})"))
    );
    switch
}

/// The member's node started from `follower`'s replica, with no replay, on its copy in `dir`, signing
/// with `key`, and the time to its first command.
pub(super) fn start_from_replica(follower: Follower, dir: &Path, key: Arc<EnclaveKey>) -> (Node, Duration) {
    let started = Instant::now();
    let log = follower.into_log(None, AccountId(key.address()), None).expect("the replica finishes as a log");
    let mut node = Node::recovered(log, dir, key).expect("the member's node starts from its replica");
    node.first_command().expect("the member's first command");
    (node, started.elapsed())
}

/// The bytes the log in `dir` holds.
pub(super) fn log_bytes(dir: &Path) -> u64 {
    std::fs::read_dir(dir)
        .expect("the log's directory")
        .filter_map(|file| file.ok()?.metadata().ok())
        .map(|found| found.len())
        .sum()
}
