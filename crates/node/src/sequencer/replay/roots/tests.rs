//! Roots computed on the replay's root thread (`Replay::check_roots_ahead`) leave the replay where
//! roots its engine computes leave it: a log that verifies reaches the same roots and hands on an
//! engine that computes them, and a root that differs from the log's fails the replay with the error
//! an engine's own root gives, ahead of any later entry's error.

use super::*;

use std::path::{Path, PathBuf};

use engine_core::event::Event;
use engine_core::types::AccountId;

use crate::log::command_seal::{checkpoint_link, command_link};
use crate::log::{Checkpoint, CheckpointDomain, CheckpointParent};
use crate::tee::EnclaveKey;
use crate::test_support::file_entries;

/// The deployment the reference logs record.
const REFERENCE: CheckpointDomain = CheckpointDomain { chain_id: 31337, vault: AccountId([0xaa; 20]) };

/// Every reference log replays, with its roots on the thread, to the roots, checkpoints and commands
/// of a replay whose engine computes them, through every protocol version's activation and the tree
/// events among them, and hands on an engine whose own root is the last command's. From `V9`'s log,
/// whose state tree a checkpoint after the first hands to the thread, the thread computes roots.
#[test]
fn every_reference_log_reaches_the_roots_its_engine_computes() {
    for reference in reference_logs() {
        let name = reference.display();
        let entries = resealed(&reference, |_| {});
        let inline = replay(&entries, false);
        let ahead = replay(&entries, true);
        assert!(inline.outcome.is_ok(), "{name}: {:?}", inline.outcome);
        assert_eq!(ahead.outcome, inline.outcome, "{name}");
        if version(&reference) >= 9 {
            assert!(ahead.on_thread > 0, "{name}: no root was computed on the thread");
        }
    }
}

/// A checkpoint whose root differs from the replay's fails the replay with the error a replay whose
/// engine computes its roots gives, where the checkpoint comes first, before the next, and as the log's
/// last: with its signature over the root it names, with a signature that fails, and with a later
/// command's events altered too.
#[test]
fn a_root_that_differs_fails_with_an_inline_roots_error_ahead_of_later_faults() {
    let reference = latest_reference();
    let checkpoints: Vec<u64> = resealed(&reference, |_| {})
        .iter()
        .filter_map(|entry| match entry {
            LogEntry::Checkpoint(checkpoint) => Some(checkpoint.seq),
            _ => None,
        })
        .collect();
    let at =
        [checkpoints[checkpoints.len() / 2], checkpoints[checkpoints.len() - 2], checkpoints[checkpoints.len() - 1]];
    let faults: [(&str, Fault); 3] = [
        ("signed over the root it names", |_, _| {}),
        ("with a signature that fails", |entries, at| {
            let LogEntry::Checkpoint(checkpoint) = &mut entries[at] else { unreachable!("a checkpoint") };
            checkpoint.signature = EnclaveKey::generate().sign_checkpoint(1, &[1; 20], 1, &[1; 32]);
        }),
        ("before a command whose events differ", |entries, at| {
            let Some(LogEntry::Command(command)) = entries.get_mut(at + 1) else { return };
            command.events.as_mut().expect("events").push(Event::AdminApplied);
        }),
    ];
    let mut later = 0;
    for seq in at {
        for (fault, change) in faults {
            let mut entries = resealed(&reference, |checkpoint| {
                if checkpoint.seq == seq {
                    let flipped = if checkpoint.root.ends_with('0') { '1' } else { '0' };
                    checkpoint.root.pop();
                    checkpoint.root.push(flipped);
                }
            });
            let index = entries
                .iter()
                .position(|entry| matches!(entry, LogEntry::Checkpoint(checkpoint) if checkpoint.seq == seq))
                .expect("the checkpoint");
            change(&mut entries, index);
            later += usize::from(matches!(entries.get(index + 1), Some(LogEntry::Command(_))));
            let (inline, ahead) = (replay(&entries, false), replay(&entries, true));
            let case = format!("checkpoint at seq {seq} {fault}");
            assert!(
                inline.outcome.as_ref().is_err_and(|e| e.contains(&format!("state root mismatch at seq {seq}"))),
                "{case}: {:?}",
                inline.outcome
            );
            assert_eq!(ahead.outcome, inline.outcome, "{case}");
        }
    }
    assert!(later >= 2, "a command followed the altered checkpoint");
}

/// A recovery anchored at a checkpoint the vault accepted but the log holds no entry for anchors at
/// the replay's own state there, as an engine computing its roots anchors, while its roots are on the
/// thread: the anchor takes the tree back for its root, and the next checkpoint hands it on again.
#[test]
fn a_recovery_anchors_where_the_log_lacks_the_vaults_checkpoint_with_its_roots_on_the_thread() {
    let reference = latest_reference();
    let unsealed = file_entries(&reference);
    let checkpoints: Vec<&Checkpoint> = unsealed
        .iter()
        .filter_map(|entry| match entry {
            LogEntry::Checkpoint(checkpoint) => Some(checkpoint),
            _ => None,
        })
        .collect();
    let dropped = checkpoints[checkpoints.len() / 2].clone();
    let parent = CheckpointParent { has_checkpoint: true, seq: dropped.seq, root: dropped.root.parse().unwrap() };
    let entries = sealed(
        unsealed.iter().filter(|entry| !matches!(entry, LogEntry::Checkpoint(cp) if cp.seq == dropped.seq)).cloned(),
        |_| {},
    );
    let recovered = |ahead: bool| {
        let mut replay =
            Replay::with_recovery(Some(REFERENCE), Some(AccountId(EnclaveKey::test_key().address())), parent);
        if ahead {
            replay.check_roots_ahead();
        }
        let mut on_thread_before = false;
        for entry in &entries {
            if matches!(entry, LogEntry::Command(c) if c.sequenced.seq == dropped.seq + 1) {
                on_thread_before = replay.roots.on_thread();
            }
            replay.apply(entry).unwrap();
        }
        replay.settle().unwrap();
        let mut replayed = replay.finish();
        let anchored = replayed.recovery_anchor_work.as_ref().map(|work| work.checkpoint.root.clone());
        (on_thread_before, anchored, hex::encode(replayed.engine.state_root()), replayed.checkpoints)
    };
    let (_, inline_anchor, inline_root, inline_checkpoints) = recovered(false);
    let (was_on_thread, anchor, root, checkpoints) = recovered(true);
    assert!(was_on_thread, "the tree was on the thread when the anchor came");
    assert_eq!(inline_anchor.as_deref(), Some(dropped.root.as_str()), "the replay anchored at the vault's checkpoint");
    assert_eq!((anchor, root, checkpoints), (inline_anchor, inline_root, inline_checkpoints));
}

// ── Helpers ──

/// A change to a log's entries around the entry at an index, a checkpoint.
type Fault = fn(&mut Vec<LogEntry>, usize);

/// What a replay reached: the root its engine holds after the last entry and the checkpoints it
/// passed, or the error that stopped it; and how many entries it applied with the tree on the thread.
struct Reached {
    outcome: Result<(String, u64), String>,
    on_thread: usize,
}

/// Replays `entries` under the test key in the reference deployment, with the roots on the thread when
/// `ahead`, settled after the last entry.
fn replay(entries: &[LogEntry], ahead: bool) -> Reached {
    let mut replay = Replay::with_trust(Some(REFERENCE), Some(AccountId(EnclaveKey::test_key().address())));
    if ahead {
        replay.check_roots_ahead();
    }
    let mut on_thread = 0;
    let applied = entries.iter().try_for_each(|entry| {
        replay.apply(entry)?;
        on_thread += usize::from(replay.roots.on_thread());
        anyhow::Ok(())
    });
    let outcome = applied.and_then(|()| replay.settle()).map_err(|e| format!("{e:#}")).map(|()| {
        let mut replayed = replay.finish();
        (hex::encode(replayed.engine.state_root()), replayed.checkpoints)
    });
    Reached { outcome, on_thread }
}

/// The reference log at `path`, written with placeholder signatures, sealed and signed (`sealed`).
fn resealed(path: &Path, change: impl FnMut(&mut Checkpoint)) -> Vec<LogEntry> {
    sealed(file_entries(path), change)
}

/// `entries`, with each checkpoint changed by `change`, then each command sealed and each checkpoint
/// and withdrawal signed by the test key in the deployment the reference logs record.
fn sealed(entries: impl IntoIterator<Item = LogEntry>, mut change: impl FnMut(&mut Checkpoint)) -> Vec<LogEntry> {
    let key = EnclaveKey::test_key();
    let mut link = B256::default();
    let mut sign = |mut entry: LogEntry| {
        match &mut entry {
            LogEntry::Command(command) => {
                link = command_link(link, command).unwrap();
                command.seal = Some(key.seal_command(REFERENCE, link));
            }
            LogEntry::Checkpoint(checkpoint) => {
                change(checkpoint);
                let root: B256 = checkpoint.root.parse().unwrap();
                checkpoint.signature =
                    key.sign_checkpoint(REFERENCE.chain_id, &REFERENCE.vault.0, checkpoint.seq, &root.0);
                checkpoint.signer = key.address_hex();
                link = checkpoint_link(checkpoint).unwrap();
            }
            LogEntry::Withdrawal(withdrawal) => {
                (withdrawal.chain_id, withdrawal.vault) = (REFERENCE.chain_id, REFERENCE.vault);
                withdrawal.signature = key.sign_withdrawal(
                    withdrawal.chain_id,
                    &withdrawal.vault.0,
                    withdrawal.account,
                    withdrawal.amount,
                    withdrawal.nonce,
                );
            }
        }
        entry
    };
    entries.into_iter().map(&mut sign).collect()
}

/// The latest protocol version's reference log, which walks every activation before most of its
/// checkpoints.
fn latest_reference() -> PathBuf {
    reference_logs().into_iter().max_by_key(|path| version(path)).expect("a reference log")
}

/// The protocol version a reference log is named for.
fn version(path: &Path) -> u32 {
    path.file_stem()
        .and_then(|stem| stem.to_str()?.strip_prefix('v')?.parse().ok())
        .expect("a log named for its version")
}

/// Every reference log's path, in order.
fn reference_logs() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/reference");
    let mut logs: Vec<_> = std::fs::read_dir(dir).unwrap().map(|entry| entry.unwrap().path()).collect();
    logs.sort();
    logs
}
