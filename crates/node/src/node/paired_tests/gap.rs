//! A start rejects a cache whose paired history head the log's commands do not bind to its checkpoint, and
//! falls back to an earlier cache; a signed pair naming a head it cannot hold, after its checkpoint or, in
//! format 1, before it, refuses the start, as any signed pair that does not verify does. A format 1 pair at
//! its checkpoint still restarts. Each altered cache is signed again by the test key, as a faulty signer
//! could sign it.

use std::fs;
use std::path::Path;
use std::sync::Arc;

use crate::clock::test_clock::TestClock;
use crate::clock::{Clock, SystemClock};
use crate::config::NodeConfig;
use crate::node::paired_tests::anchor::seq_of;
use crate::node::paired_tests::harness::{Node, configuration};
use crate::node::paired_tests::lagging::{lagging_cache, paired_head};
use crate::snapshot::{self, Snapshot};
use crate::tee::EnclaveKey;
use crate::test_support::deposit;

/// A cache whose certified prefix the commands after its history head do not reach is refused.
#[tokio::test]
async fn a_cache_whose_history_head_does_not_bind_to_its_checkpoint_is_refused() {
    let clock = Arc::new(TestClock::at(SystemClock.now_ms()));
    let (cfg, checkpoint, before) = lagging_cache("paired-gap-prefix", clock.clone()).await;
    let newest = cache_at(&cfg, checkpoint);
    // Certified consistently by the cache and its pair, so only the commands after its head refuse it.
    resign(&cfg, &newest, |cache| {
        cache.context.command_prefix[0] ^= 1;
        cache.pair.as_mut().unwrap().prefix.command_prefix[0] ^= 1;
    });
    let node = Node::start_at(cfg, clock).await.unwrap();
    assert_eq!(node.snapshot_seq, Some(seq_of(&before[0])), "the start falls back to the earlier cache");
    node.stop_after_signing("after-unbound-head").await;
}

/// A signed cache whose pair names a history head it cannot hold refuses the start, as any signed pair that
/// does not verify does: a head after its checkpoint, or a format 1 head that trails it, which only format 2
/// allows.
#[tokio::test]
async fn a_cache_paired_with_a_history_head_it_cannot_hold_refuses_the_start() {
    let node = Node::start(configuration("paired-gap-later-head")).await.unwrap();
    for n in 0..9 {
        node.submit(deposit(1, &format!("later-head-{n}"))).await;
        node.history_caught_up().await;
    }
    let (cfg, reader) = node.stop().await;
    drop(reader);
    let caches = snapshot::candidates_readonly(cfg.snapshots.as_ref().unwrap()).unwrap();
    assert!(paired_head(&cfg, &caches[0]) > seq_of(&caches[1]), "the newest cache's head follows the next one's");
    let later = read(&cfg, &caches[0]).pair.expect("a paired cache").history;
    resign(&cfg, &caches[1], |cache| cache.pair.as_mut().unwrap().history = later);
    let refused = Node::start(cfg).await.err().expect("a head after its checkpoint refuses the start");
    assert!(format!("{refused:#}").contains("history head follows its checkpoint"), "{refused:#}");

    let clock = Arc::new(TestClock::at(SystemClock.now_ms()));
    let (cfg, checkpoint, _) = lagging_cache("paired-gap-format-1-trailing", clock.clone()).await;
    resign(&cfg, &cache_at(&cfg, checkpoint), |cache| cache.pair.as_mut().unwrap().format = 1);
    let refused = Node::start_at(cfg, clock).await.err().expect("a format 1 head before its checkpoint refuses");
    assert!(format!("{refused:#}").contains("history head cursor mismatch"), "{refused:#}");
}

/// A format 1 pair, which this release still reads, restarts at its checkpoint.
#[tokio::test]
async fn a_format_1_cache_at_its_checkpoint_still_restarts() {
    let clock = Arc::new(TestClock::at(SystemClock.now_ms()));
    let (cfg, checkpoint, before) = lagging_cache("paired-gap-format-1", clock.clone()).await;
    assert_eq!(paired_head(&cfg, &before[0]), seq_of(&before[0]), "the earlier cache pairs at its checkpoint");
    resign(&cfg, &before[0], |cache| cache.pair.as_mut().unwrap().format = 1);
    // The newer cache is unusable, so the start reaches the format 1 one.
    fs::write(cache_at(&cfg, checkpoint), b"{").unwrap();
    let node = Node::start_at(cfg, clock).await.unwrap();
    assert_eq!(node.snapshot_seq, Some(seq_of(&before[0])), "the format 1 cache at its checkpoint restarts");
    node.stop_after_signing("after-format-1").await;
}

// ── Helpers ──

/// Removes a cache file with no generation journal; the stopped node holds no reference to it.
struct RemoveFile;

impl snapshot::Prune for RemoveFile {
    fn remove(&mut self, path: &Path) -> anyhow::Result<()> {
        Ok(fs::remove_file(path)?)
    }
}

/// The cache at `path`, verified and decoded.
fn read(cfg: &NodeConfig, path: &Path) -> Snapshot {
    let section = cfg.snapshots.as_ref().unwrap();
    snapshot::read(section, path, cfg.custody.chain_id, cfg.custody.vault.parse().unwrap()).unwrap().0
}

/// The path of the cache at the checkpoint at `seq`.
fn cache_at(cfg: &NodeConfig, seq: u64) -> std::path::PathBuf {
    let caches = snapshot::candidates_readonly(cfg.snapshots.as_ref().unwrap()).unwrap();
    caches.into_iter().find(|path| seq_of(path) == seq).expect("a cache at the checkpoint")
}

/// Replaces the cache at `path` with one `edit` alters, signed by the test key.
fn resign(cfg: &NodeConfig, path: &Path, edit: impl FnOnce(&mut Snapshot)) {
    let mut cache = read(cfg, path);
    edit(&mut cache);
    let section = cfg.snapshots.as_ref().unwrap();
    let staged = snapshot::stage(section, &cache, &EnclaveKey::test_key()).unwrap();
    staged.publish_then(&mut RemoveFile, |_| {}).unwrap();
}
