//! A replay from a daily snapshot's carried state against a whole replay of the same log, and the stats worker's
//! fold against the state a replay carries.

use std::path::Path;

use engine_core::Engine;
use engine_core::eip712::B256;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::AccountId;
use tokio::sync::watch;

use crate::daily_snapshots::DailySnapshot;
use crate::log::{CheckpointDomain, LogEntry, LogWriter, Manifest, Offset, SegmentedLog};
use crate::sequencer::replay::{Carried, Fold, LiveReplay, Replay, Replayed};
use crate::tee::EnclaveKey;
use crate::test_support::{SEGMENT_COMMANDS, log_entries, scratch_dir};

/// The deployment the test sequencer seals and signs for.
pub(super) const DOMAIN: CheckpointDomain = CheckpointDomain { chain_id: 31_337, vault: AccountId([0; 20]) };

/// A replica that goes on from a daily snapshot's engine and the state it carries, reading nothing of the copy
/// before its checkpoint, holds what a replica that replays the whole copy holds, whichever checkpoint the
/// snapshot stands at: the state, the command prefix after every command it retains, the last checkpoint, and
/// what its start finishes with, the checkpoint accounting, the approvals in payout order with their
/// authorizations, the genesis, the activations and the checkpoints' roots among them; but for the first command
/// it applied, which its own log check counts from (`log_check::own_check`).
#[tokio::test]
async fn a_replica_from_a_snapshot_holds_what_a_whole_replay_holds() {
    let dir = scratch_dir("carried-equals-whole");
    crate::sequencer::fixture_tests::write_funded_log(&dir, 4);
    let entries = log_entries(&dir);
    let approvals = entries.iter().filter(|entry| matches!(entry, LogEntry::Withdrawal(_))).count();
    assert!(approvals >= 2, "the log authorizes approvals on both sides of a snapshot");
    let mut whole = LiveReplay::new(DOMAIN, [signer()].into());
    whole.sample_marks_with(Box::new(|_, _, _| {}));
    entries.iter().for_each(|entry| whole.apply(entry).unwrap());
    let root = whole.state_root();
    let expected = finished(whole, &dir);
    let starts = snapshot_starts(&entries);
    assert!(starts.len() >= 4, "the log holds checkpoints a snapshot can stand at: {starts:?}");
    for at in starts {
        let mut from = resumed(&entries, at).unwrap_or_else(|e| panic!("the snapshot at entry {at}: {e:#}"));
        assert_eq!(from.state_root(), root, "the snapshot at entry {at}");
        let LogEntry::Checkpoint(checkpoint) = &entries[at] else { unreachable!("a snapshot's checkpoint") };
        assert_eq!(from.takings().applied_from(), Some(checkpoint.seq + 1), "the snapshot at entry {at}");
        let links: Vec<_> = (0..=expected.engine.last_seq().unwrap()).map(|seq| from.link_at(seq)).collect();
        assert!(links.iter().all(Option::is_some), "the snapshot at entry {at} retains every link");
        let actual = finished(from, &dir);
        assert_eq!(facts(&actual), facts(&expected), "the snapshot at entry {at}");
        for seq in 0..=expected.engine.last_seq().unwrap() {
            assert_eq!(links[seq as usize].map(|link| link.link), expected.prefix_ring.prefix_at(seq), "seq {seq}");
        }
    }
}

/// The stats worker's fold, fed each command an engine applies with its link and seal, and each withdrawal and
/// checkpoint entry, carries at every checkpoint what a whole replay of the log holds there; and a replay that
/// starts from a snapshot carries what the whole replay carries at every later checkpoint, so the snapshots a
/// node writes after a snapshot start are the ones a whole replay checks.
#[tokio::test]
async fn the_fold_and_a_resumed_replay_carry_what_a_whole_replay_carries() {
    let dir = scratch_dir("carried-fold");
    crate::sequencer::fixture_tests::write_funded_log(&dir, 4);
    let entries = log_entries(&dir);
    let carried = whole_carried(&entries);
    assert!(carried.len() >= 4, "the log holds checkpoints: {}", carried.len());
    let mut fold = Fold::of(&Replay::default()).unwrap();
    let (mut engine, mut link) = (Engine::default(), [0; 32]);
    for (index, entry) in entries.iter().enumerate() {
        match entry {
            LogEntry::Command(command) => {
                let before = engine.protocol_version();
                let events = engine.apply(&command.sequenced).unwrap();
                link = crate::log::extend_command_prefix(link, crate::log::command_digest(command).unwrap());
                let sealer = command.seal.as_ref().map(|seal| seal.signer);
                fold.command(&engine, &command.sequenced, &events, before, (B256::from(link), sealer)).unwrap();
            }
            LogEntry::Withdrawal(withdrawal) => fold.withdrawal(withdrawal).unwrap(),
            LogEntry::Checkpoint(checkpoint) => {
                fold.checkpoint(checkpoint).unwrap();
                let folded = fold.carried(checkpoint.seq).unwrap();
                assert_eq!(digest(&folded), digest(&carried[&index]), "the fold at entry {index}");
            }
        }
    }
    let first = *snapshot_starts(&entries).first().unwrap();
    let mut from = resumed_replay(&entries, first).unwrap();
    for (index, entry) in entries.iter().enumerate().skip(first + 1) {
        from.apply(entry).unwrap();
        if matches!(entry, LogEntry::Checkpoint(_)) {
            let mut resumed = from.carried().unwrap();
            // A replay from a snapshot notes that it applied nothing before it; a snapshot carries the records whole.
            resumed.takings = resumed.takings.applied_throughout();
            assert_eq!(digest(&resumed), digest(&carried[&index]), "after the snapshot, at entry {index}");
        }
    }
}

/// An access source failure episode open at a snapshot's checkpoint stays open in a replay that goes on from it,
/// and in what that replay carries at its next checkpoint, while no fresh decision after the failure resolves it;
/// with none open, none is carried.
#[tokio::test]
async fn a_replay_from_a_snapshot_keeps_an_open_access_failure() {
    let dir = scratch_dir("carried-access");
    crate::sequencer::fixture_tests::write_funded_log(&dir, 4);
    let entries = log_entries(&dir);
    let at = *snapshot_starts(&entries).first().unwrap();
    let next = entries.iter().enumerate().skip(at + 1).find(|(_, entry)| matches!(entry, LogEntry::Checkpoint(_)));
    let next = next.map(|(index, _)| index).expect("a checkpoint after the snapshot's");
    // A failure later than every decision the log holds, so none of them resolves it.
    let failed = Some(i64::MAX - 1);
    for since in [failed, None] {
        let DailySnapshot { offset, checkpoint, engine, carried, .. } = snapshot_at(&entries, at, None, None);
        let carried = Carried { access_failed_since: since, ..carried.unwrap() };
        let mut replay = Replay::copy(DOMAIN, [signer()].into()).resume(engine, carried, checkpoint, offset).unwrap();
        assert_eq!(replay.access_alerts.failed(), since.is_some());
        entries[at + 1..=next].iter().for_each(|entry| replay.apply(entry).unwrap());
        assert_eq!(replay.carried().unwrap().access_failed_since, since, "at the next checkpoint");
    }
}

/// A recovery anchored at the vault's finalized checkpoint goes on from a snapshot as a whole one ends: anchored at
/// the snapshot's checkpoint or at a later one, it ends with the anchor's work, the custody work after the anchor and
/// the anchor's time that a whole replay gives; anchored before the snapshot's checkpoint, whose custody work the
/// snapshot does not carry, it is refused.
#[tokio::test]
async fn a_recovery_anchored_at_or_after_the_snapshot_ends_as_a_whole_one() {
    let dir = scratch_dir("carried-anchor");
    crate::sequencer::fixture_tests::write_funded_log(&dir, 4);
    let entries = log_entries(&dir);
    let starts = snapshot_starts(&entries);
    let (at, later) = (starts[0], *starts.last().unwrap());
    assert!(at < later, "two checkpoints a snapshot can stand at");
    let parent = |index: usize| {
        let LogEntry::Checkpoint(checkpoint) = &entries[index] else { panic!("no checkpoint at entry {index}") };
        crate::log::CheckpointParent {
            has_checkpoint: true,
            seq: checkpoint.seq,
            root: checkpoint.root.parse().unwrap(),
        }
    };
    let ends = |replayed: &Replayed| {
        let anchor = serde_json::to_value(&replayed.recovery_anchor_work).unwrap();
        let work = serde_json::to_value(&replayed.recovery_work).unwrap();
        serde_json::json!({ "anchor": anchor, "work": work, "anchor_ts": replayed.anchor_ts })
    };
    let resumed_from = |snapshot: usize, anchor: usize| {
        let DailySnapshot { offset, checkpoint, engine, carried, .. } = snapshot_at(&entries, snapshot, None, None);
        Replay::with_recovery(Some(DOMAIN), Some(signer()), parent(anchor)).resume(
            engine,
            carried.unwrap(),
            checkpoint,
            offset,
        )
    };
    for anchor in [at, later] {
        let mut whole = Replay::with_recovery(Some(DOMAIN), Some(signer()), parent(anchor));
        entries.iter().for_each(|entry| whole.apply(entry).unwrap());
        let whole = whole.finish();
        assert!(whole.recovery_anchor_work.is_some(), "the whole replay anchors at entry {anchor}");
        let mut resumed = resumed_from(at, anchor).unwrap();
        entries[at + 1..].iter().for_each(|entry| resumed.apply(entry).unwrap());
        assert_eq!(ends(&resumed.finish()), ends(&whole), "anchored at entry {anchor}");
    }
    let refused = format!("{:#}", resumed_from(later, at).err().expect("an anchor before the snapshot is refused"));
    assert!(refused.contains("is before the snapshot's"), "{refused}");
}

/// A carried state that does not stand for its snapshot's engine and checkpoint is refused before any entry
/// after it is replayed, each at the check it names; and an engine whose root is not its checkpoint's is refused.
#[tokio::test]
async fn a_carried_state_that_does_not_stand_for_its_snapshot_is_refused() {
    let dir = scratch_dir("carried-refused");
    crate::sequencer::fixture_tests::write_funded_log(&dir, 4);
    let entries = log_entries(&dir);
    let at = *snapshot_starts(&entries).last().unwrap();
    let carried = whole_carried(&entries).remove(&at).unwrap();
    assert!(!carried.payout_order.is_empty(), "the snapshot holds approvals");
    assert!(!carried.authorized.is_empty(), "the snapshot holds authorizations");
    type Alter = fn(&mut Carried);
    let cases: [(&str, Alter); 11] = [
        ("payout order is not the engine's approvals", |carried| _ = carried.payout_order.pop()),
        ("lacks its checkpoint's root", |carried| carried.checkpoint_roots.clear()),
        ("lacks its checkpoint's signer", |carried| carried.checkpoint_signers.clear()),
        ("do not end at the checkpoint's command", |carried| _ = carried.links.pop()),
        ("do not end at the checkpoint's command", |carried| carried.command_prefix = B256::repeat_byte(7)),
        ("names no command", |carried| carried.commands = 0),
        ("not the last activation's", |carried| carried.activations.push((ProtocolVersion::V22, u64::MAX))),
        ("of no approval the engine holds", |carried| carried.authorized.iter_mut().for_each(|w| w.nonce = u64::MAX)),
        ("not of consecutive commands", |carried| _ = carried.links.remove(1)),
        ("deferred approval is after the checkpoint", |carried| {
            carried.deferred.push((u64::MAX, AccountId([1; 20]), engine_core::Fixed::ONE, 1))
        }),
        // Custody finality activated after an approval the snapshot owes: its credits' provenance needs the log.
        ("approved before custody finality", |carried| {
            let finality = carried.activations.iter_mut().find(|(version, _)| version.supports_deposit_finality());
            finality.expect("the log activates custody finality").1 = u64::MAX;
            carried.deferred.push((1, AccountId([1; 20]), engine_core::Fixed::ONE, 1));
        }),
    ];
    for (said, alter) in cases {
        let mut altered = carried.clone();
        alter(&mut altered);
        let error = format!("{:#}", resume_with(&entries, at, Some(altered), None).err().expect("refused"));
        assert!(error.contains(said), "{said}: {error}");
    }
    let mut engine = snapshot_engine(&entries, at);
    let account = engine.accounts().keys().next().copied().unwrap();
    let balance = engine.accounts()[&account].balance;
    let mut encoded = serde_json::to_value(&engine).unwrap();
    encoded["accounts"][account.to_string()]["balance"] =
        serde_json::json!((balance + engine_core::Fixed::ONE).to_string());
    engine = serde_json::from_value(encoded).unwrap();
    engine.rebuild_resting();
    engine.restore_indexes();
    let error = format!("{:#}", resume_with(&entries, at, None, Some(engine)).err().expect("refused"));
    assert!(error.contains("state root mismatch"), "{error}");
}

// ── Helpers ──

/// The entries at which a snapshot can stand: each checkpoint of a state with custody finality.
pub(super) fn snapshot_starts(entries: &[LogEntry]) -> Vec<usize> {
    let mut replica = LiveReplay::new(DOMAIN, [signer()].into());
    let mut starts = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        replica.apply(entry).unwrap();
        if matches!(entry, LogEntry::Checkpoint(_)) && replica.engine().deposit_finality_enabled() {
            starts.push(index);
        }
    }
    starts
}

/// The state a whole replay of `entries` carries after each checkpoint, by the checkpoint's index.
pub(super) fn whole_carried(entries: &[LogEntry]) -> std::collections::BTreeMap<usize, Carried> {
    let mut replay = Replay::copy(DOMAIN, [signer()].into());
    let mut carried = std::collections::BTreeMap::new();
    for (index, entry) in entries.iter().enumerate() {
        replay.apply(entry).unwrap();
        if matches!(entry, LogEntry::Checkpoint(_)) && replay.logs_events {
            carried.insert(index, replay.carried().unwrap());
        }
    }
    carried
}

/// The engine a daily snapshot of the checkpoint at `entries[at]` holds, as a start reads one: encoded and
/// decoded, its indexes rebuilt.
fn snapshot_engine(entries: &[LogEntry], at: usize) -> Engine {
    let mut replica = LiveReplay::new(DOMAIN, [signer()].into());
    entries[..=at].iter().for_each(|entry| replica.apply(entry).unwrap());
    let mut engine: Engine = serde_json::from_str(&serde_json::to_string(replica.engine()).unwrap()).unwrap();
    engine.rebuild_resting();
    engine.restore_indexes();
    engine
}

/// The offset of `entries[at]`.
fn offset_at(entries: &[LogEntry], at: usize) -> Offset {
    entries[..=at].iter().fold(None, |last, entry| crate::log::offset_of(last, entry).ok()).unwrap()
}

/// A daily snapshot of the checkpoint at `entries[at]`, carrying `carried`, or what a whole replay carries
/// there, and holding `engine`, or the replayed one.
fn snapshot_at(entries: &[LogEntry], at: usize, carried: Option<Carried>, engine: Option<Engine>) -> DailySnapshot {
    let LogEntry::Checkpoint(checkpoint) = entries[at].clone() else { panic!("no checkpoint at entry {at}") };
    DailySnapshot {
        schema: 3,
        offset: offset_at(entries, at),
        checkpoint,
        engine: engine.unwrap_or_else(|| snapshot_engine(entries, at)),
        state: None,
        carried: Some(carried.unwrap_or_else(|| whole_carried(entries).remove(&at).unwrap())),
        history: None,
        signature: None,
    }
}

/// A replica from the snapshot of the checkpoint at `entries[at]`, with the entries after it applied.
/// Error cases:
/// - the replica does not resume, or an entry fails it
fn resumed(entries: &[LogEntry], at: usize) -> anyhow::Result<LiveReplay> {
    let mut replica = resume_with(entries, at, None, None)?;
    // As a member's build does (`standby::replica::build`).
    replica.sample_marks_with(Box::new(|_, _, _| {}));
    entries[at + 1..].iter().try_for_each(|entry| replica.apply(entry))?;
    Ok(replica)
}

/// A replica resumed from the snapshot of the checkpoint at `entries[at]`, as `snapshot_at` makes it.
/// Error cases:
/// - as `LiveReplay::resume`
fn resume_with(
    entries: &[LogEntry],
    at: usize,
    carried: Option<Carried>,
    engine: Option<Engine>,
) -> anyhow::Result<LiveReplay> {
    LiveReplay::resume(DOMAIN, [signer()].into(), snapshot_at(entries, at, carried, engine), &[])
}

/// A replay resumed from the snapshot of the checkpoint at `entries[at]`.
/// Error cases:
/// - as `Replay::resume`
fn resumed_replay(entries: &[LogEntry], at: usize) -> anyhow::Result<Replay> {
    let DailySnapshot { offset, checkpoint, engine, carried, .. } = snapshot_at(entries, at, None, None);
    Replay::copy(DOMAIN, [signer()].into()).resume(engine, carried.unwrap(), checkpoint, offset)
}

/// The SHA-256 of a carried state's encoding.
fn digest(carried: &Carried) -> [u8; 32] {
    carried.digest().unwrap()
}

/// What `replica`'s start finishes with, at the end of the log in `dir`.
fn finished(replica: LiveReplay, dir: &Path) -> Replayed {
    replica.into_log(writer(dir), None, signer(), None).unwrap().replayed
}

/// What a start from a replica reads besides the engine, as JSON.
pub(crate) fn facts(replayed: &Replayed) -> serde_json::Value {
    let roots: Vec<_> = (0..=replayed.engine.last_seq().unwrap()).map(|seq| replayed.checkpoint_root(seq)).collect();
    serde_json::json!({
        "engine": serde_json::to_value(&replayed.engine).unwrap(),
        "commands": replayed.commands,
        "checkpoints": replayed.checkpoints,
        "prefix": hex::encode(replayed.command_prefix),
        "accounting": format!("{:?}", replayed.accounting),
        "unpaid": replayed.unpaid,
        "payout_order": replayed.payout_order,
        "authorized": serde_json::to_value(replayed.authorized.iter().collect::<Vec<_>>()).unwrap(),
        "signers": replayed.checkpoint_signers,
        "roots": roots,
        "activations": replayed.activations,
        "genesis": serde_json::to_value(&replayed.genesis).unwrap(),
        "params_seq": replayed.params_seq,
        "deferred": replayed.deferred,
        "unsigned": replayed.unsigned,
        "gate": replayed.requires_checkpoint_gate,
        "last_checkpoint": replayed.last_signed_checkpoint,
        "last_checkpoint_offset": replayed.last_checkpoint_offset.map(|offset| offset.to_string()),
        "last_checkpoint_ts": replayed.last_checkpoint_ts,
        "last_marks": replayed.last_marks,
        // A replica from a snapshot notes that it applied nothing before it (`Takings::set_applied_from`), which
        // `own_check` reads; the records are the whole replay's.
        "takings": format!("{:?}", replayed.takings.applied_throughout()),
        "fold": replayed.fold.as_ref().map(|fold| hex::encode(fold.digest())),
    })
}

/// The test sequencer's enclave, which seals and signs its log.
fn signer() -> AccountId {
    AccountId(EnclaveKey::test_key().address())
}

/// A writer at the end of the log in `dir`.
fn writer(dir: &Path) -> LogWriter {
    let (manifest, _) = watch::channel(Manifest::default());
    SegmentedLog::open(dir, |_| Ok(())).unwrap().into_writer(SEGMENT_COMMANDS, manifest).unwrap()
}
