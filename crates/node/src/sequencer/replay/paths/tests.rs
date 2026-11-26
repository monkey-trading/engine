//! A replay of a copy from a daily snapshot (`replay_copy_from`) against a replay of the whole copy: a copy that
//! lacks every segment before the snapshot's replays to what the whole copy replays to, one the whole replay
//! refuses after the snapshot is refused, and a snapshot the replay cannot go on from gives way to the whole copy.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use sha2::{Digest, Sha256};

use super::*;
use crate::daily_snapshots::{Listed, Signing, open_signed};
use crate::log::{Manifest, SegmentInfo};
use crate::tee::EnclaveKey;
use crate::test_support::{capture_alerts, copy_dir, scratch_dir};

/// The deployment the test sequencer seals and signs for.
const DOMAIN: CheckpointDomain = CheckpointDomain { chain_id: 31_337, vault: AccountId([0; 20]) };

/// A copy whose segments before the snapshot's are gone replays from the snapshot, reading none of them, to the
/// state, command prefix, checkpoints and fold the whole copy replays to; the copy's whole replay is refused,
/// naming the archive. With no snapshot, the whole copy replays.
#[test]
fn a_copy_without_its_segments_before_a_snapshot_replays_from_it_to_the_whole_copys_state() {
    let (log, snapshots, listed) = logged("paths-copy-from");
    let copy = trimmed(&log, &listed, "paths-copy-from-copy");

    let whole = replay_copy(&log, DOMAIN, signers(), workers()).unwrap();
    let (from, seq) = replay_copy_from(Some(signed(&snapshots, &listed)), &copy, DOMAIN, signers(), workers()).unwrap();
    assert_eq!(seq, Some(listed.seq), "the replay did not go on from the snapshot");
    assert_eq!(facts(&from), facts(&whole));
    let refused = replay_copy(&copy, DOMAIN, signers(), workers()).err().expect("the whole copy replayed");
    assert!(format!("{refused:#}").contains("public archive"), "{refused:#}");

    let (none, seq) = replay_copy_from(None, &log, DOMAIN, signers(), workers()).unwrap();
    assert_eq!((seq, facts(&none)), (None, facts(&whole)), "with no snapshot the whole copy replays");
}

/// A copy whose entries after the snapshot's checkpoint the whole replay refuses is refused from the snapshot
/// too, in the same segment and for the same cause, and not handed to a replay of the whole copy.
#[test]
fn a_copy_the_whole_replay_refuses_after_the_snapshot_is_refused_from_it() {
    let (log, snapshots, listed) = logged("paths-copy-refused");
    let altered = poison_after(&log, listed.seq);
    let copy = trimmed(&log, &listed, "paths-copy-refused-copy");

    let whole = replay_copy(&log, DOMAIN, signers(), workers()).err().expect("the altered copy replayed whole");
    let from = replay_copy_from(Some(signed(&snapshots, &listed)), &copy, DOMAIN, signers(), workers())
        .err()
        .expect("the altered copy replayed from the snapshot");
    assert_eq!(from.root_cause().to_string(), whole.root_cause().to_string(), "{from:#}");
    assert!(format!("{whole:#}").contains(&altered), "{whole:#}");
    assert!(format!("{from:#}").contains(&altered), "{from:#}");
}

/// A snapshot whose replay state does not stand for its checkpoint is paged as Warning `snapshot_start_refused`
/// and the whole copy replays instead; a copy without the segments before it then has nothing to replay from.
#[test]
fn a_snapshot_the_replay_cannot_go_on_from_gives_way_to_the_whole_copy() {
    let (log, snapshots, listed) = logged("paths-copy-fallback");
    let copy = trimmed(&log, &listed, "paths-copy-fallback-copy");
    let whole = replay_copy(&log, DOMAIN, signers(), workers()).unwrap();
    let unfit = || {
        let mut start = signed(&snapshots, &listed);
        start.snapshot.carried.as_mut().unwrap().checkpoint_roots.clear();
        Some(start)
    };
    let (_guard, mut alerts) = capture_alerts();

    let (replayed, seq) = replay_copy_from(unfit(), &log, DOMAIN, signers(), workers()).unwrap();
    assert_eq!((seq, facts(&replayed)), (None, facts(&whole)), "the whole copy did not replay");
    let alert = alerts.try_recv().expect("the refusal is paged");
    assert_eq!((alert["alert"].as_str(), alert["level"].as_str()), ("snapshot_start_refused", "WARN"));
    assert!(alert["message"].contains("lacks its checkpoint's root"), "{alert:?}");
    let refused = replay_copy_from(unfit(), &copy, DOMAIN, signers(), workers()).err().expect("the copy replayed");
    assert!(format!("{refused:#}").contains("public archive"), "{refused:#}");
}

// ── Helpers ──

/// A funded log in a directory named for `name`, and a directory of snapshots holding one daily snapshot of it at
/// a checkpoint in a later segment than its first, with entries after it.
fn logged(name: &str) -> (PathBuf, PathBuf, Listed) {
    let (log, snapshots) = (scratch_dir(name), scratch_dir(&format!("{name}-daily")));
    crate::sequencer::fixture_tests::write_funded_log(&log, 4);
    let listed = publish_daily_snapshot(&log, &snapshots, signing(), |held| held / 2);
    (log, snapshots, listed)
}

/// A copy of the log in `log`, in a directory named for `name`, without the segments before the one holding
/// `listed`'s checkpoint.
fn trimmed(log: &Path, listed: &Listed, name: &str) -> PathBuf {
    let copy = scratch_dir(name);
    copy_dir(log, &copy);
    assert!(remove_segments_before(&copy, listed.seq) > 0, "the copy holds segments before the snapshot's");
    copy
}

/// The snapshot `listed` names in `snapshots`, opened as a start opens it.
fn signed(snapshots: &Path, listed: &Listed) -> Signed {
    open_signed(&snapshots.join(&listed.file), DOMAIN, &signers()).unwrap()
}

/// Adds an event to the first command of the last listed segment of the log in `dir`, which must start after
/// seq `seq`, and lists the segment under the altered bytes, as a mirror serving its own manifest does: the
/// segment's file name.
fn poison_after(dir: &Path, seq: u64) -> String {
    let listings = Manifest::load(dir).unwrap().segments;
    let last = listings.last().cloned().unwrap();
    assert!(last.first_seq > seq, "the last listed segment starts after the snapshot");
    let mut bytes = Vec::new();
    for (n, line) in std::fs::read(dir.join(&last.file)).unwrap().split_inclusive(|byte| *byte == b'\n').enumerate() {
        let mut entry: LogEntry = serde_json::from_slice(line).unwrap();
        if let (0, LogEntry::Command(command)) = (n, &mut entry) {
            let rejected = engine_core::event::Event::AdminRejected { reason: "poisoned".into() };
            command.events.get_or_insert_with(Vec::new).push(rejected);
        }
        bytes.extend(serde_json::to_vec(&entry).unwrap());
        bytes.push(b'\n');
    }
    std::fs::write(dir.join(&last.file), &bytes).unwrap();
    let text: String = listings
        .iter()
        .cloned()
        .map(|mut listed: SegmentInfo| {
            if listed.file == last.file {
                (listed.bytes, listed.sha256) = (bytes.len() as u64, hex::encode(Sha256::digest(&bytes)));
            }
            serde_json::to_string(&listed).unwrap() + "\n"
        })
        .collect();
    std::fs::write(dir.join(crate::log::MANIFEST), text).unwrap();
    last.file
}

/// What a start reads from a replay of a copy, as JSON.
fn facts(replayed: &Replayed) -> serde_json::Value {
    serde_json::json!({
        "engine": serde_json::to_value(&replayed.engine).unwrap(),
        "commands": replayed.commands,
        "checkpoints": replayed.checkpoints,
        "prefix": hex::encode(replayed.command_prefix),
        "last_checkpoint": replayed.last_signed_checkpoint,
        "signers": replayed.checkpoint_signers,
        "payout_order": replayed.payout_order,
        "fold": replayed.fold.as_ref().map(|fold| hex::encode(fold.digest())),
    })
}

/// The test sequencer's enclave, which seals its commands and signs its checkpoints and snapshots.
fn signers() -> BTreeSet<AccountId> {
    [AccountId(EnclaveKey::test_key().address())].into()
}

/// The signing the test sequencer's enclave does.
fn signing() -> Signing {
    Signing { key: Arc::new(EnclaveKey::test_key()), domain: DOMAIN }
}

/// The threads that check seals ahead of a replay.
fn workers() -> ReplayWorkers {
    ReplayWorkers::available()
}
