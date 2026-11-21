//! The pass before a daily snapshot against a whole replay of the same copy.

use super::*;

use std::path::Path;

use tokio::sync::watch;

use crate::log::{LogWriter, Manifest, SegmentedLog};
use crate::sequencer::replay::Replayed;
use crate::tee::EnclaveKey;
use crate::test_support::{SEGMENT_COMMANDS, log_entries, scratch_dir};

/// The deployment the test sequencer seals and signs for.
const DOMAIN: CheckpointDomain = CheckpointDomain { chain_id: 31_337, vault: AccountId([0; 20]) };

/// A replica that passes over the copy before a snapshot's checkpoint and replays the copy from the snapshot
/// holds what a replica that replays the whole copy holds, whichever checkpoint the snapshot stands at: the
/// state, the command prefix after every command it retains, the last checkpoint, and what its start finishes
/// with, the checkpoint accounting, the approvals in payout order with their authorizations, the genesis, the
/// activations and the checkpoints' roots among them.
#[tokio::test]
async fn a_replica_from_a_snapshot_holds_what_a_whole_replay_holds() {
    let dir = scratch_dir("before-equals-whole");
    write_log(&dir).await;
    let entries = log_entries(&dir);
    let approvals = entries.iter().filter(|entry| matches!(entry, LogEntry::Withdrawal(_))).count();
    assert!(approvals >= 2, "the log authorizes approvals on both sides of a snapshot");
    let mut whole = replica(&entries, None).unwrap();
    let starts = snapshot_starts(&entries);
    assert!(starts.len() >= 4, "the log holds checkpoints a snapshot can stand at: {starts:?}");
    let root = whole.state_root();
    let expected = finished(whole, &dir);
    for at in starts {
        let mut from = replica(&entries, Some(at)).unwrap_or_else(|e| panic!("the snapshot at entry {at}: {e:#}"));
        assert_eq!(from.state_root(), root, "the snapshot at entry {at}");
        let links: Vec<_> = (0..=expected.engine.last_seq().unwrap()).map(|seq| from.link_at(seq)).collect();
        assert!(links.iter().all(Option::is_some), "the snapshot at entry {at} retains every link");
        let actual = finished(from, &dir);
        assert_eq!(facts(&actual), facts(&expected), "the snapshot at entry {at}");
        for seq in 0..=expected.engine.last_seq().unwrap() {
            assert_eq!(links[seq as usize].map(|link| link.link), expected.prefix_ring.prefix_at(seq), "seq {seq}");
        }
    }
}

/// The pass refuses a copy a whole replay refuses before the snapshot, each at the check it names: a command whose
/// seal no longer signs it, a checkpoint by a signer the vault has not recorded, a second checkpoint at one seq
/// naming another root, a withdrawal entry whose authorization no longer signs it, one that comes before its
/// approval, one that names a checkpoint the copy holds only after it, and one whose approval the snapshot holds
/// otherwise.
#[tokio::test]
async fn the_pass_refuses_what_a_whole_replay_refuses() {
    let dir = scratch_dir("before-refuses");
    write_log(&dir).await;
    let entries = log_entries(&dir);
    let paid = entries.iter().position(|entry| matches!(entry, LogEntry::Withdrawal(_))).expect("a withdrawal");
    let LogEntry::Withdrawal(withdrawal) = entries[paid].clone() else { unreachable!("a withdrawal entry") };
    let auth = withdrawal.authorization.clone().expect("a typed authorization");
    let approval = position(
        &entries,
        |entry| matches!(entry, LogEntry::Command(c) if Some(c.sequenced.seq) == withdrawal.approved_seq),
    );
    let named = position(&entries, |entry| matches!(entry, LogEntry::Checkpoint(cp) if cp.seq == auth.checkpoint_seq));
    assert!(approval < named && named < paid, "the approval, the checkpoint it is proved at, then its authorization");
    let at =
        *snapshot_starts(&entries).iter().find(|start| **start > paid).expect("a snapshot after the authorization");
    let engine = snapshot(&entries, at);
    let moved = |to: usize| {
        let mut moved = entries.clone();
        let entry = moved.remove(paid);
        moved.insert(to, entry);
        moved
    };
    let last_checkpoint_before = |entries: &[LogEntry]| {
        entries[..at].iter().rposition(|entry| matches!(entry, LogEntry::Checkpoint(cp) if cp.parent.is_none()))
    };
    let mut cases: Vec<(&str, Vec<LogEntry>, &str)> = vec![
        ("an authorization before its approval", moved(approval), "typed payout was never approved"),
        (
            "an authorization before its checkpoint",
            moved(named),
            "typed payout checkpoint is absent from the preceding log",
        ),
    ];
    let mut stamped = entries.clone();
    let command = stamped[..at].iter().rposition(|entry| matches!(entry, LogEntry::Command(_))).unwrap();
    if let LogEntry::Command(command) = &mut stamped[command] {
        command.sequenced.ts += 1;
    }
    cases.push(("a command stamped otherwise than sealed", stamped, "invalid command origin signature"));
    let mut unrecorded = entries.clone();
    let index = last_checkpoint_before(&unrecorded).expect("a checkpoint before the snapshot");
    if let LogEntry::Checkpoint(checkpoint) = &mut unrecorded[index] {
        let key = EnclaveKey::generate();
        let root: B256 = checkpoint.root.parse().unwrap();
        checkpoint.signature = key.sign_checkpoint(DOMAIN.chain_id, &DOMAIN.vault.0, checkpoint.seq, &root.0);
        checkpoint.signer = key.address_hex();
    }
    cases.push(("a checkpoint by an unrecorded signer", unrecorded, "has no verified attestation"));
    let mut forked = entries.clone();
    let index = last_checkpoint_before(&forked).expect("a checkpoint before the snapshot");
    let alternate = alternate(&forked[index]);
    forked.insert(index, alternate);
    cases.push(("a second checkpoint naming another root", forked, "names another root"));
    let mut resigned = entries.clone();
    if let LogEntry::Withdrawal(withdrawal) = &mut resigned[paid] {
        withdrawal.amount += Fixed::from_int(1);
    }
    cases.push(("an authorization of another amount", resigned, "invalid typed withdrawal signature"));
    for (case, copy, said) in cases {
        let end = position(&copy, |entry| *entry == entries[at]);
        let error = format!("{:#}", pass_over(&copy, &engine, end).err().unwrap_or_else(|| panic!("{case} passed")));
        assert!(error.contains(said), "{case}: {error}");
    }

    // The snapshot holds the approval otherwise than the log approved it.
    let mut otherwise = serde_json::to_value(&engine).unwrap();
    alter_approval(&mut otherwise["withdrawal_approvals"], withdrawal.nonce);
    let otherwise: Engine = serde_json::from_value(otherwise).unwrap();
    let error = format!("{:#}", pass_over(&entries, &otherwise, at).err().expect("another approval passed"));
    assert!(error.contains("typed payout differs from its immutable approval"), "{error}");
}

/// The replica does not start from a snapshot that stands at another command than the pass's last, at whose seq
/// the pass took a checkpoint naming another root, or whose state is not its checkpoint's; a checkpoint at its seq
/// before its own that names the same root, as a node writes a second one, is taken.
#[tokio::test]
async fn the_replica_starts_only_from_the_state_the_pass_ends_at() {
    let dir = scratch_dir("before-ends");
    write_log(&dir).await;
    let entries = log_entries(&dir);
    let starts = snapshot_starts(&entries);
    let at = starts[starts.len() / 2];
    let LogEntry::Checkpoint(checkpoint) = entries[at].clone() else { unreachable!("a snapshot's checkpoint") };
    let engine = || snapshot(&entries, at);

    let later = snapshot(&entries, *starts.last().unwrap());
    let error =
        format!("{:#}", pass_over(&entries, &engine(), at).unwrap().into_live(later, &checkpoint).err().unwrap());
    assert!(error.contains("not at the copy's command before its checkpoint"), "{error}");

    let mut forked = entries.clone();
    forked.insert(at, alternate(&entries[at]));
    let pass = pass_over(&forked, &engine(), at + 1).unwrap();
    let error = format!("{:#}", pass.into_live(engine(), &checkpoint).err().expect("two roots at the snapshot's seq"));
    assert!(error.contains("names another root"), "{error}");
    let mut repeated = entries.clone();
    repeated.insert(at, entries[at].clone());
    let mut replica = pass_over(&repeated, &engine(), at + 1).unwrap().into_live(engine(), &checkpoint).unwrap();
    repeated[at + 1..].iter().for_each(|entry| replica.apply(entry).unwrap());

    let mut altered: serde_json::Value = serde_json::to_value(engine()).unwrap();
    let accounts = altered["accounts"].as_object_mut().unwrap();
    accounts.values_mut().next().unwrap()["balance"] = serde_json::json!("1");
    let mut altered: Engine = serde_json::from_value(altered).unwrap();
    altered.rebuild_resting();
    altered.restore_indexes();
    let mut replica = pass_over(&entries, &altered, at).unwrap().into_live(altered, &checkpoint).unwrap();
    let error = format!("{:#}", replica.apply(&entries[at]).unwrap_err());
    assert!(error.contains("state root mismatch"), "{error}");
}

// ── Helpers ──

/// Writes a log of the dev genesis and screened credits and withdrawals, each authorized as mock custody does,
/// and ticks, across several checkpoints, ending at one (`write_funded_log`).
async fn write_log(dir: &Path) {
    crate::sequencer::fixture_tests::write_funded_log(dir, 4);
}

/// The entries at which a snapshot can stand: each checkpoint of a state with custody finality.
fn snapshot_starts(entries: &[LogEntry]) -> Vec<usize> {
    let mut replica = LiveReplay::new(DOMAIN, [signer()].into());
    let mut starts = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        if matches!(entry, LogEntry::Checkpoint(_)) && replica.engine().deposit_finality_enabled() {
            starts.push(index);
        }
        replica.apply(entry).unwrap();
    }
    starts
}

/// The engine a daily snapshot of the checkpoint at `entries[at]` holds, as a member reads one: encoded and
/// decoded, its indexes rebuilt.
fn snapshot(entries: &[LogEntry], at: usize) -> Engine {
    let mut replica = LiveReplay::new(DOMAIN, [signer()].into());
    entries[..at].iter().for_each(|entry| replica.apply(entry).unwrap());
    let mut engine: Engine = serde_json::from_str(&serde_json::to_string(replica.engine()).unwrap()).unwrap();
    engine.rebuild_resting();
    engine.restore_indexes();
    engine
}

/// A replica of `entries`, by a whole replay, or from a snapshot of the checkpoint at `entries[at]`.
/// Error cases:
/// - an entry fails the pass or the replica
fn replica(entries: &[LogEntry], at: Option<usize>) -> anyhow::Result<LiveReplay> {
    let Some(at) = at else {
        let mut replica = LiveReplay::new(DOMAIN, [signer()].into());
        entries.iter().try_for_each(|entry| replica.apply(entry))?;
        return Ok(replica);
    };
    let engine = snapshot(entries, at);
    let LogEntry::Checkpoint(checkpoint) = &entries[at] else { anyhow::bail!("no checkpoint at entry {at}") };
    let mut replica = pass_over(entries, &engine, at)?.into_live(engine, checkpoint)?;
    entries[at..].iter().try_for_each(|entry| replica.apply(entry))?;
    Ok(replica)
}

/// The pass over `entries` before `entries[end]`, toward a snapshot whose engine is `engine`.
/// Error cases:
/// - an entry fails the pass
fn pass_over(entries: &[LogEntry], engine: &Engine, end: usize) -> anyhow::Result<Before> {
    let mut pass = Before::new(DOMAIN, [signer()].into(), engine, &[]);
    entries[..end].iter().try_for_each(|entry| pass.entry(entry))?;
    Ok(pass)
}

/// The index of the first of `entries` that `is` picks.
fn position(entries: &[LogEntry], is: impl Fn(&LogEntry) -> bool) -> usize {
    entries.iter().position(is).expect("the entry")
}

/// A checkpoint at `original`'s seq, signed by the test sequencer's enclave, that names another root.
fn alternate(original: &LogEntry) -> LogEntry {
    let LogEntry::Checkpoint(mut alternate) = original.clone() else { panic!("not a checkpoint") };
    let root = B256::repeat_byte(9);
    alternate.root = format!("0x{}", hex::encode(root));
    alternate.signature =
        EnclaveKey::test_key().sign_checkpoint(DOMAIN.chain_id, &DOMAIN.vault.0, alternate.seq, &root.0);
    LogEntry::Checkpoint(alternate)
}

/// Gives the approval of `nonce` in `approvals`, an engine's encoding of them, another amount.
fn alter_approval(approvals: &mut serde_json::Value, nonce: u64) {
    let found = match approvals {
        serde_json::Value::Object(map) => map.values_mut().find(|approval| approval["nonce"] == nonce),
        serde_json::Value::Array(list) => list.iter_mut().find(|approval| approval["nonce"] == nonce),
        _ => None,
    };
    found.expect("the snapshot holds the approval")["amount"] = serde_json::json!("1");
}

/// What `replica`'s start finishes with, at the end of the log in `dir`.
fn finished(replica: LiveReplay, dir: &Path) -> Replayed {
    replica.into_log(writer(dir), None, signer(), None).unwrap().replayed
}

/// What a start from a replica reads besides the engine, as JSON.
fn facts(replayed: &Replayed) -> serde_json::Value {
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
        // A replica from a snapshot notes that it applied nothing before it (`Takings::set_applied_from`), which
        // `own_check` reads; the records are the whole replay's.
        "takings": format!("{:?}", replayed.takings.applied_throughout()),
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
