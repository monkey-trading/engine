//! Seals checked ahead of a replay (`Replay::check_seals_ahead`) leave the replay where a replay that
//! checks every seal in place leaves it: a seal that fails fails at its own command with the same
//! error, wherever that command falls, and a log that verifies reaches the same roots.

use super::*;

use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};

use engine_core::command::{Command, Sequenced};
use engine_core::eip712::B256;

use crate::log::command_seal::{checkpoint_link, command_link};
use crate::log::{Checkpoint, CommandEntry, CommandSeal, ReplayWorkers, SegmentedLog};
use crate::tee::EnclaveKey;
use crate::test_support::{file_entries, scratch_dir};

const DOMAIN: CheckpointDomain = CheckpointDomain { chain_id: 31337, vault: AccountId([7; 20]) };
/// The deployment the reference logs record.
const REFERENCE: CheckpointDomain = CheckpointDomain { chain_id: 31337, vault: AccountId([0xaa; 20]) };
/// A checkpoint after every this many commands.
const EVERY: u64 = 10;
/// Commands a segment holds.
const SEGMENT: u64 = 16;
/// Past the most checks the reader holds ahead of the replay.
const COMMANDS: u64 = 1100;

/// A change to a log's entries.
type Change = fn(&mut Vec<LogEntry>);

/// How a seal is spoilt.
#[derive(Clone, Copy, Debug)]
enum Spoilt {
    /// Naming the trusted key, signed over its link by another.
    ByAnotherKey,
    /// A signature that is not hex.
    NotHex,
    /// Sealed by the trusted key over another command's link.
    OverAnotherLink,
}

/// A spoilt seal at the log's first command, after a checkpoint, at a segment's first and last
/// commands, in the middle, past the reader's bound and at the last command fails the replay at that
/// command, with the error and the state a replay checking each seal in place reaches.
#[test]
fn a_spoilt_seal_fails_at_its_command_as_an_in_place_check_fails_it() {
    let at = [0, 2 * EVERY, 3 * SEGMENT, 4 * SEGMENT - 1, 517, 1030, COMMANDS - 1];
    for (seq, spoilt) in
        at.into_iter().zip([Spoilt::ByAnotherKey, Spoilt::NotHex, Spoilt::OverAnotherLink].iter().cycle())
    {
        let case = format!("{spoilt:?} at seq {seq}");
        let dir = scratch_dir(&format!("ahead-spoilt-{seq}"));
        write_log(&dir, &ticks(COMMANDS, Some((seq, *spoilt))));
        let in_place = replay_dir(&dir, DOMAIN, None);
        let ahead = replay_dir(&dir, DOMAIN, Some(workers(3)));
        assert_eq!(ahead.outcome, in_place.outcome, "{case}: the error");
        assert_eq!(ahead.last_seq, in_place.last_seq, "{case}: where the replay stopped");
        assert_eq!(in_place.last_seq, seq.checked_sub(1), "{case}: the replay applied every command before it");
        assert!(in_place.outcome.as_ref().is_err_and(|e| e.contains("command origin signature")), "{case}");
        assert_eq!(ahead.taken_ahead, seq + 1, "{case}: a verdict came from elsewhere than the workers");
    }
}

/// A command the log holds twice, or two commands it holds in the wrong order, with more commands
/// after them than the reader may run ahead, fails the replay where a replay checking each seal in
/// place fails it, with the same error, rather than leaving it waiting on a verdict.
#[test]
fn a_repeated_or_reordered_command_fails_as_an_in_place_replay_fails_it() {
    let cases: [(&str, Change); 2] = [
        ("repeated", |entries| entries.insert(31, entries[30].clone())),
        ("reordered", |entries| entries.swap(40, 41)),
    ];
    for (case, change) in cases {
        let file = scratch_dir(&format!("ahead-{case}"));
        std::fs::create_dir_all(&file).unwrap();
        let file = file.join("log.jsonl");
        let mut entries = ticks(COMMANDS, None);
        change(&mut entries);
        std::fs::write(&file, lines(&entries)).unwrap();
        let in_place = replay_dir(&file, DOMAIN, None);
        let ahead = replay_dir(&file, DOMAIN, Some(workers(3)));
        assert!(in_place.outcome.is_err(), "{case}: the replay passed");
        assert_eq!((&ahead.outcome, ahead.last_seq), (&in_place.outcome, in_place.last_seq), "{case}");
    }
}

/// A log whose seals all verify replays to the same roots, checkpoints and commands, every seal's
/// verdict taken from the workers, through each kind of log the replay reads: segments and a single
/// file.
#[test]
fn a_verified_log_reaches_the_roots_an_in_place_replay_reaches() {
    let dir = scratch_dir("ahead-verified");
    let entries = ticks(COMMANDS, None);
    write_log(&dir, &entries);
    let file = scratch_dir("ahead-verified-file");
    std::fs::create_dir_all(&file).unwrap();
    let file = file.join("log.jsonl");
    std::fs::write(&file, lines(&entries)).unwrap();
    for path in [&dir, &file] {
        let in_place = replay_dir(path, DOMAIN, None);
        let ahead = replay_dir(path, DOMAIN, Some(workers(3)));
        assert_eq!(ahead.outcome, in_place.outcome, "{}", path.display());
        assert!(in_place.outcome.is_ok(), "{}: {:?}", path.display(), in_place.outcome);
        assert_eq!(ahead.taken_ahead, COMMANDS, "{}: every verdict came from the workers", path.display());
    }
}

/// Every reference log, its commands sealed and its checkpoints and withdrawals signed by the test
/// key, replays to the roots a replay checking each seal in place reaches, every seal's verdict taken
/// from the workers.
#[test]
fn every_reference_log_reaches_the_roots_an_in_place_replay_reaches() {
    let scratch = scratch_dir("ahead-reference");
    std::fs::create_dir_all(&scratch).unwrap();
    for reference in reference_logs() {
        let file = scratch.join(reference.file_name().unwrap());
        let entries = resealed(&reference);
        std::fs::write(&file, lines(&entries)).unwrap();
        let in_place = replay_dir(&file, REFERENCE, None);
        let ahead = replay_dir(&file, REFERENCE, Some(workers(3)));
        let name = reference.display();
        assert!(in_place.outcome.is_ok(), "{name}: {:?}", in_place.outcome);
        assert_eq!((&ahead.outcome, ahead.last_seq), (&in_place.outcome, in_place.last_seq), "{name}");
        let commands = entries.iter().filter(|entry| matches!(entry, LogEntry::Command(_))).count();
        assert_eq!(ahead.taken_ahead, u64::try_from(commands).unwrap(), "{name}: every verdict came from the workers");
    }
}

// ── Helpers ──

/// What a replay of a log reached: the root it ended at or the error that stopped it, the last
/// command it applied, and how many seals' verdicts it took from the workers.
struct Reached {
    outcome: Result<String, String>,
    last_seq: Option<u64>,
    taken_ahead: u64,
}

/// `count` workers.
fn workers(count: usize) -> ReplayWorkers {
    ReplayWorkers::from(NonZeroUsize::new(count).unwrap())
}

/// Replays the log at `path` in `domain` under the test key, checking its seals ahead on `workers`, or
/// each in place without them.
fn replay_dir(path: &Path, domain: CheckpointDomain, workers: Option<ReplayWorkers>) -> Reached {
    let mut replay = Replay::with_trust(Some(domain), Some(AccountId(EnclaveKey::test_key().address())));
    if let Some(workers) = workers {
        replay.check_seals_ahead(path, workers);
    }
    let read = if path.is_dir() {
        SegmentedLog::open(path, |entry| replay.apply(&entry)).map(drop)
    } else {
        crate::log::read_file(path, |entry| replay.apply(&entry))
    };
    let taken_ahead = replay.origin.seals_taken_ahead();
    let last_seq = replay.engine.last_seq();
    let outcome = read.map(|()| hex::encode(replay.engine.state_root())).map_err(|e| format!("{e:#}"));
    Reached { outcome, last_seq, taken_ahead }
}

/// A log of `count` ticks, each sealed by the test key, with a checkpoint it signed after every
/// `EVERY`th, and the seal at `spoilt`'s seq spoilt as it says.
fn ticks(count: u64, spoilt: Option<(u64, Spoilt)>) -> Vec<LogEntry> {
    let key = EnclaveKey::test_key();
    let (mut oracle, mut link, mut entries) = (Engine::new(), B256::default(), Vec::new());
    for seq in 0..count {
        let sequenced = Sequenced { seq, ts: i64::try_from(seq).unwrap(), cmd: Command::Tick };
        let events = oracle.apply(&sequenced).unwrap();
        let mut command = CommandEntry { sequenced, events: Some(events), seal: None };
        link = command_link(link, &command).unwrap();
        command.seal = Some(match spoilt.filter(|(at, _)| *at == seq).map(|(_, how)| how) {
            None => key.seal_command(DOMAIN, link),
            Some(Spoilt::ByAnotherKey) => {
                CommandSeal { signer: AccountId(key.address()), ..EnclaveKey::generate().seal_command(DOMAIN, link) }
            }
            Some(Spoilt::NotHex) => CommandSeal { signature: "0xnot-hex".into(), ..key.seal_command(DOMAIN, link) },
            Some(Spoilt::OverAnotherLink) => key.seal_command(DOMAIN, B256::from([5; 32])),
        });
        entries.push(LogEntry::Command(command));
        if (seq + 1) % EVERY == 0 {
            let root = oracle.state_root();
            let checkpoint = Checkpoint {
                parent: None,
                seq,
                root: format!("0x{}", hex::encode(root)),
                signature: key.sign_checkpoint(DOMAIN.chain_id, &DOMAIN.vault.0, seq, &root),
                signer: key.address_hex(),
                domain: Some(DOMAIN),
                deferred_withdrawals: false,
            };
            link = checkpoint_link(&checkpoint).unwrap();
            entries.push(LogEntry::Checkpoint(checkpoint));
        }
    }
    entries
}

/// `entries` as a segmented log in `dir`, `SEGMENT` commands a segment.
fn write_log(dir: &Path, entries: &[LogEntry]) {
    std::fs::create_dir_all(dir).unwrap();
    let log = SegmentedLog::open(dir, |_| Ok(())).unwrap();
    let mut writer = log.into_writer(SEGMENT, tokio::sync::watch::channel(Default::default()).0).unwrap();
    for entry in entries {
        writer.append(entry).unwrap();
    }
    writer.flush().unwrap();
}

/// `entries`, one JSON line each.
fn lines(entries: &[LogEntry]) -> String {
    entries.iter().map(|entry| format!("{}\n", serde_json::to_string(entry).unwrap())).collect()
}

/// The reference log at `path`, written with placeholder signatures, with each command sealed and
/// each checkpoint and withdrawal signed by the test key in the deployment the logs record.
fn resealed(path: &Path) -> Vec<LogEntry> {
    let key = EnclaveKey::test_key();
    let mut link = B256::default();
    let sign = |mut entry: LogEntry, link: &mut B256| {
        match &mut entry {
            LogEntry::Command(command) => {
                *link = command_link(*link, command).unwrap();
                command.seal = Some(key.seal_command(REFERENCE, *link));
            }
            LogEntry::Checkpoint(checkpoint) => {
                let root: B256 = checkpoint.root.parse().unwrap();
                checkpoint.signature =
                    key.sign_checkpoint(REFERENCE.chain_id, &REFERENCE.vault.0, checkpoint.seq, &root.0);
                checkpoint.signer = key.address_hex();
                *link = checkpoint_link(checkpoint).unwrap();
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
    file_entries(path).into_iter().map(|entry| sign(entry, &mut link)).collect()
}

/// Every reference log's path, in order.
fn reference_logs() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/reference");
    let mut logs: Vec<_> = std::fs::read_dir(dir).unwrap().map(|entry| entry.unwrap().path()).collect();
    logs.sort();
    logs
}
