//! A replay from a recovery cache that carries the replay state at its checkpoint folds what a whole replay of the
//! log folds, while it goes on from the payout state the cache owes; one that carries none, or a state that is not
//! its own, folds none and recovers as before.

use std::num::NonZeroUsize;

use engine_core::types::AccountId;

use crate::config::SnapshotSection;
use crate::log::{CheckpointDomain, LogEntry};
use crate::sequencer::replay::{Carried, Replay, Replayed};
use crate::snapshot::{self, Owed, Payouts, Progress, RecoveryBase, RecoveryContext, Snapshot};
use crate::tee::EnclaveKey;
use crate::test_support::{log_entries, scratch_dir};

/// The deployment the test sequencer seals and signs for.
const DOMAIN: CheckpointDomain = CheckpointDomain { chain_id: 31_337, vault: AccountId([0; 20]) };

#[tokio::test]
async fn a_cache_that_carries_the_replay_state_folds_what_a_whole_replay_folds() {
    let dir = scratch_dir("cache-fold");
    crate::sequencer::fixture_tests::write_funded_log(&dir, 4);
    let entries = log_entries(&dir);
    let mut whole = through(&entries, entries.len() - 1).finish();
    let expected = whole.fold.as_ref().map(|fold| fold.digest()).expect("a whole replay folds");
    let at = delivered_checkpoint(&entries);
    assert!(
        entries[at + 1..].iter().any(|entry| matches!(entry, LogEntry::Withdrawal(_))),
        "the log signs approvals after the cache"
    );
    let carried = through(&entries, at).carried().unwrap();

    let mut restored = from_cache(&entries, at, Some(carried.clone()), "cache-fold-carried");
    assert!(restored.delivered > 0, "the cache owes fewer approvals than the engine holds");
    assert_eq!(restored.fold.as_ref().map(|fold| fold.digest()), Some(expected), "the whole replay's fold");
    assert_eq!(restored.engine.state_root(), whole.engine.state_root());
    let bare = from_cache(&entries, at, None, "cache-fold-bare");
    assert!(bare.fold.is_none(), "a cache that carries no replay state gives no fold");
    let other = Carried { commands: carried.commands + 1, ..carried };
    let mut refused = from_cache(&entries, at, Some(other), "cache-fold-other");
    assert!(refused.fold.is_none(), "a carried state of another prefix gives no fold");
    assert_eq!(refused.engine.state_root(), whole.engine.state_root(), "and the cache recovers as before");
}

// ── Helpers ──

/// The approvals `replay`'s engine holds that no withdrawal entry has signed, in payout order: those a cache owes.
fn unsigned(replay: &Replay) -> Vec<Owed> {
    let unsigned = replay.payout_order.iter().filter(|(key, _)| !replay.signed.contains(key));
    unsigned
        .map(|((account, nonce), event_index)| Owed {
            account: *account,
            nonce: *nonce,
            event_index: *event_index,
            latest: None,
        })
        .collect()
}

/// The test sequencer's enclave, which seals and signs its log.
fn signer() -> AccountId {
    AccountId(EnclaveKey::test_key().address())
}

/// A replay under the test enclave's trust of `entries` through `entries[last]`.
fn through(entries: &[LogEntry], last: usize) -> Replay {
    let mut replay = Replay::with_trust(Some(DOMAIN), Some(signer()));
    entries[..=last].iter().for_each(|entry| replay.apply(entry).unwrap());
    replay
}

/// The first checkpoint among `entries` at which the engine has custody finality and holds an approval a withdrawal
/// entry has signed.
fn delivered_checkpoint(entries: &[LogEntry]) -> usize {
    let mut replay = Replay::with_trust(Some(DOMAIN), Some(signer()));
    for (index, entry) in entries.iter().enumerate() {
        replay.apply(entry).unwrap();
        if matches!(entry, LogEntry::Checkpoint(_))
            && replay.engine.deposit_finality_enabled()
            && replay.payout_order.iter().any(|(key, _)| replay.signed.contains(key))
        {
            return index;
        }
    }
    panic!("no checkpoint at which the engine holds a signed approval")
}

/// What a replay from a cache of the checkpoint at `entries[at]` that owes only the approvals no withdrawal entry has
/// signed, as mock custody delivers an approval once it signs it, and carries `carried`, written to and read from a directory named for `name`, finishes with once it has applied the entries
/// after it.
fn from_cache(entries: &[LogEntry], at: usize, carried: Option<Carried>, name: &str) -> Replayed {
    let replay = through(entries, at);
    let LogEntry::Checkpoint(checkpoint) = entries[at].clone() else { panic!("no checkpoint at entry {at}") };
    let cache = Snapshot {
        schema: 2,
        chain_id: DOMAIN.chain_id,
        vault: DOMAIN.vault,
        checkpoint,
        offset: replay.last_offset.unwrap(),
        engine: serde_json::to_string(&replay.engine).unwrap(),
        progress: Progress {
            genesis: replay.genesis.clone(),
            activations: replay.activations.clone(),
            checkpoints: replay.checkpoints,
            params_seq: replay.params_seq,
        },
        context: RecoveryContext {
            commands: replay.commands,
            command_prefix: replay.command_prefix,
            accounting: replay.accounting,
            logs_events: true,
            unsigned: Vec::new(),
            deferred: replay.deferred.clone(),
            checkpoint_signers: [signer()].into(),
            base: RecoveryBase::Ungated,
            base_work: None,
            work: Vec::new(),
            payouts: Some(Payouts { owed: unsigned(&replay), roots: Default::default() }),
        },
        pair: None,
        carried,
    };
    let cfg = SnapshotSection {
        dir: scratch_dir(name),
        retain: NonZeroUsize::new(3).unwrap(),
        interval: std::time::Duration::ZERO,
        max_bytes: NonZeroUsize::new(64 * 1024 * 1024).unwrap(),
        trusted_signers: vec![signer()],
    };
    snapshot::write(&cfg, &cache, &EnclaveKey::test_key()).unwrap();
    let path = snapshot::candidates_readonly(&cfg).unwrap().remove(0);
    let (cache, engine) = snapshot::read(&cfg, &path, DOMAIN.chain_id, DOMAIN.vault).unwrap();
    let mut replay = Replay::from_snapshot(engine, cache, DOMAIN, Some(signer()), None).unwrap();
    entries[at + 1..].iter().for_each(|entry| replay.apply(entry).unwrap());
    replay.finish()
}
