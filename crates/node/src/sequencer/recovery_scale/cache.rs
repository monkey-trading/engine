//! The cache the log's node captures at a checkpoint, and a restart from it after a hard stop, to the
//! first command, on a copy of the log and its caches: as a node with `[snapshots]` and `[log_heads]`
//! restarts, from its newest cache, with the head posted one posting interval before the log's end,
//! through the node's own recovery (`Sequencer::read_paired_log`) and start checks. Beside it, timed
//! apart on the same copy: the cache's read and the posted head's check alone, the pass over the log's
//! whole prefix that check replaces, and the takeover cut's read of the log's last checkpoint from its
//! newest segments, which a production start makes twice. A start from a cache reads no signer
//! history, which only a full replay checks the log's checkpoints against.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use engine_core::types::{AccountId, Millis};

use crate::config::NodeConfig;
use crate::log::{self, LogEntry};
use crate::log_heads::{Head, prefix};
use crate::sequencer::recovery_scale::handover::copy_log;
use crate::sequencer::recovery_scale::paired::{Paired, node_config};
use crate::sequencer::recovery_scale::restart::{DOMAIN, file_bytes};
use crate::snapshot;
use crate::tee::EnclaveKey;

/// The template's `[log_heads] interval`: the newest head on chain is at most about this much flow
/// behind the log's end.
pub(super) const POSTING_INTERVAL: Duration = Duration::from_secs(3_600);

/// A cache the node captured: its checkpoint's seq and its bytes; how long the capture held the
/// sequencer's thread, against how long encoding the engine there takes, which a capture held it for
/// before; and how long the history owner and the stats worker then took to publish it. Before it, the
/// commands the history owner had yet to index when the flow stopped, and how long it took to.
pub(super) struct Cached {
    pub seq: u64,
    pub bytes: u64,
    pub held: Duration,
    pub encode_on_sequencer: Duration,
    pub published: Duration,
    pub history_behind: u64,
    pub history_caught_up: Duration,
}

/// A restart from the newest cache to its first command: `read_paired_log`, which reads the cache,
/// checks the posted head and replays the commands past the cache, and the start after it; with the
/// parts timed apart and the takeover cut's read, which is all a start reads outside the cache's bound.
pub(super) struct Restarted {
    pub cache_seq: u64,
    pub read: Duration,
    pub start: Duration,
    /// The commands past the cache the restart replayed.
    pub suffix_commands: u64,
    /// The last command of the log the restart recovered, before its own first.
    pub recovered_seq: Option<u64>,
    pub cache_read: Duration,
    pub posted: Head,
    pub posted_check: Duration,
    pub full_prefix_pass: Duration,
    /// The takeover cut's read of the log's last checkpoint, and that checkpoint's seq.
    pub takeover_cut_read: Duration,
    pub last_checkpoint: u64,
}

impl Restarted {
    /// The restart to its first command.
    pub(super) fn took(&self) -> Duration {
        self.read + self.start
    }
}

/// Captures a cache at the node's next checkpoint, once no capture is in flight and the history owner
/// has indexed the log, waits for it to be published, and defers any capture after it, so the flow
/// past it ages the cache as a hard stop finds it.
pub(super) async fn capture(paired: &mut Paired, cfg: &NodeConfig) -> Cached {
    paired.sealing.idle().await;
    let started = Instant::now();
    let history_behind = caught_up(paired).await;
    let history_caught_up = started.elapsed();
    // What a capture held the sequencer for before: encoding the engine on its thread.
    let started = Instant::now();
    serde_json::to_string(&paired.node.seq.engine).expect("the engine encodes");
    let encode_on_sequencer = started.elapsed();
    let section = cfg.snapshots.as_ref().expect("[snapshots]");
    let newest_seq = || snapshot::candidates_readonly(section).expect("the caches").first().map(|path| seq_of(path));
    let before = newest_seq();
    paired.node.seq.capture_next_checkpoint();
    let captured = paired.node.seq.captures.held.len();
    let checkpoint = paired.node.checkpoint_now();
    let held = *paired.node.seq.captures.held.get(captured).expect("the next checkpoint is captured");
    let started = Instant::now();
    paired.sealing.idle().await;
    let published = started.elapsed();
    paired.node.seq.defer_captures();
    // The capture just taken is the newest cache, at most at the checkpoint written for it.
    let taken = newest_seq();
    assert!(taken > before && taken <= Some(checkpoint.seq), "the capture is published: {before:?} then {taken:?}");
    let newest = snapshot::candidates_readonly(section).expect("the caches").into_iter().next().expect("a cache");
    let (cache, _) = snapshot::read(section, &newest, DOMAIN.chain_id, DOMAIN.vault).expect("the cache reads");
    let bytes = file_bytes(&newest);
    Cached { seq: cache.checkpoint.seq, bytes, held, encode_on_sequencer, published, history_behind, history_caught_up }
}

/// The seq of the cache at `path`, which its file name begins with.
fn seq_of(path: &Path) -> u64 {
    path.file_name().and_then(|name| name.to_str()?.get(..20)?.parse().ok()).expect("a cache file name")
}

/// Waits until the history owner has taken every entry the node has flushed, and returns how many
/// commands it had yet to take. Each inspection ends one of the owner's turns, and a turn commits what
/// it indexed, so the owner is asked once a second and indexes in whole batches meanwhile.
async fn caught_up(paired: &Paired) -> u64 {
    let flushed = paired.node.manifest.borrow().flushed;
    let mut last_entry = paired.sealing.inspect().await.expect("the history owner runs").last_entry;
    let behind = flushed.map_or(0, |at| at.seq).saturating_sub(last_entry.map_or(0, |at| at.seq));
    while last_entry < flushed {
        tokio::time::sleep(Duration::from_secs(1)).await;
        last_entry = paired.sealing.inspect().await.expect("the history owner runs").last_entry;
    }
    behind
}

/// A restart of the stopped node's log and caches under `cfg`, copied into `run`, from its newest cache
/// to its first command, signing with `key`, with the head posted `POSTING_INTERVAL` of flow before the
/// log's end, its last stamp `end_ts`, or at its first command on a shorter log.
/// Error cases:
/// - the log or caches cannot be copied or read, the posted head's check fails, or the restart fails
pub(super) async fn restart(
    cfg: &NodeConfig,
    key: Arc<EnclaveKey>,
    (interval, end_ts): (Duration, Millis),
    run: &Path,
) -> anyhow::Result<Restarted> {
    let signer = AccountId(key.address());
    let copy = node_config(run, interval, (signer, cfg.node.replay_workers));
    copy_log(&cfg.node.log_dir, &copy.node.log_dir)?;
    copy_dir(
        &cfg.snapshots.as_ref().context("[snapshots]")?.dir,
        &copy.snapshots.as_ref().context("[snapshots]")?.dir,
    )?;
    let (dir, section) = (&copy.node.log_dir, copy.snapshots.as_ref().context("[snapshots]")?);
    let posted = posted_head(dir, end_ts - Millis::try_from(POSTING_INTERVAL.as_millis())?)?;
    let newest = snapshot::candidates_readonly(section)?.into_iter().next().context("a cache")?;
    let started = Instant::now();
    let (cache, engine) = snapshot::read(section, &newest, DOMAIN.chain_id, DOMAIN.vault)?;
    let cache_read = started.elapsed();
    let (cache_seq, certified, base_ts) = (cache.checkpoint.seq, cache.context.command_prefix, engine.now());
    drop((cache, engine));
    // A head past the cache is the suffix replay's to check, as it captures the link there.
    let started = Instant::now();
    if posted.seq <= cache_seq {
        prefix::since(dir, posted, cache_seq, base_ts, certified)?;
    }
    let posted_check = started.elapsed();
    let started = Instant::now();
    prefix::verify(dir, DOMAIN, signer, cache_seq, cache_seq, certified, copy.node.replay_workers)?;
    let full_prefix_pass = started.elapsed();
    let started = Instant::now();
    let last_checkpoint = log::last_checkpoint(dir)?.context("the log holds a checkpoint")?.seq;
    let takeover_cut_read = started.elapsed();
    let (paired, recovered_seq, (read, start)) = Paired::restart(&copy, key, posted)?;
    let suffix_commands = recovered_seq.map_or(0, |seq| seq - cache_seq);
    paired.stop().await?;
    Ok(Restarted {
        cache_seq,
        read,
        start,
        suffix_commands,
        recovered_seq,
        cache_read,
        posted,
        posted_check,
        full_prefix_pass,
        takeover_cut_read,
        last_checkpoint,
    })
}

/// The head on chain when a poster's last post was at `before`: the last command of the log in `dir`
/// stamped at or before it, with its cumulative command prefix, or the log's first command when none is.
/// Error cases:
/// - the log cannot be read, or holds no command
fn posted_head(dir: &Path, before: Millis) -> anyhow::Result<Head> {
    let (mut posted, mut command_prefix) = (None, [0u8; 32]);
    log::read_from(dir, 0, |entry| {
        if let LogEntry::Command(command) = &entry {
            command_prefix = log::extend_command_prefix(command_prefix, log::command_digest(command)?);
            if posted.is_none() || command.sequenced.ts <= before {
                posted = Some(Head { seq: command.sequenced.seq, link: command_prefix.into() });
            }
        }
        Ok(())
    })?;
    posted.context("the log holds a command")
}

/// Copies every file under `from` to `to`, as the node left its cache directory.
/// Error cases:
/// - a directory or file cannot be read or written
fn copy_dir(from: &Path, to: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(to)?;
    for item in std::fs::read_dir(from)? {
        let item = item?;
        let target = to.join(item.file_name());
        match item.file_type()?.is_dir() {
            true => copy_dir(&item.path(), &target)?,
            false => _ = std::fs::copy(item.path(), target)?,
        }
    }
    Ok(())
}
