//! `engine replay`'s seal check: every seal's form, and in full the last seal of each run of one signer's
//! seals, which authenticates every command of the run; and the every-seal check that `--every-seal` and a
//! copy of the log keep.

use std::collections::BTreeSet;
use std::path::PathBuf;

use engine_core::Engine;
use engine_core::command::{Command, Sequenced};
use engine_core::eip712::B256;
use engine_core::event::Event;
use engine_core::types::AccountId;
use k256::ecdsa::Signature as EcdsaSignature;
use k256::elliptic_curve::PrimeField;

use crate::log::command_seal::{checkpoint_link, command_link};
use crate::log::{Checkpoint, CheckpointDomain, CommandEntry, LogEntry, ReplayWorkers, SealChecks, SegmentedLog};
use crate::sequencer::replay::{replay, replay_copy, replay_path_with_trust};
use crate::tee::EnclaveKey;

const DOMAIN: CheckpointDomain = CheckpointDomain { chain_id: 31337, vault: AccountId([7; 20]) };
const OTHER_DOMAIN: CheckpointDomain = CheckpointDomain { chain_id: 31338, vault: AccountId([7; 20]) };

/// A replay of ten ticks with checkpoints after seqs 3 and 7 checks three seals in full, one for each run:
/// 0 to 3, 4 to 7, and the tail, 8 and 9, which its read's end settles; one checking every seal checks
/// each one, and takes no run.
#[test]
fn engine_replay_checks_one_seal_in_full_for_each_run() {
    let key = EnclaveKey::test_key();
    let entries = ticks(10, &[3, 7], |_| Some((&key, DOMAIN)));
    assert_eq!(replay(&entries).unwrap().origin.runs_checked(), 3);
    let dir = written("run-ends-count", &entries);
    let signer = Some(AccountId(key.address()));
    let ends = replay_path_with_trust(&dir, Some(DOMAIN), signer, SealChecks::RunEnds).unwrap();
    assert_eq!(ends.origin.runs_checked(), 3);
    let every = replay_path_with_trust(&dir, Some(DOMAIN), signer, every_seal()).unwrap();
    assert_eq!(every.origin.runs_checked(), 0);
}

/// A seal mid-run whose signature bytes are another valid signature by its signer, over another link,
/// leaves the run's commands authenticated by its last seal, so `engine replay` passes it, with another
/// command prefix than the writer's; `--every-seal` and a copy's replay, which checks every seal, refuse it.
#[test]
fn an_intermediate_seals_bytes_are_left_to_every_seal_and_to_a_copy() {
    let key = EnclaveKey::test_key();
    let signer = AccountId(key.address());
    let entries = ticks(6, &[5], |_| Some((&key, DOMAIN)));
    let mut altered = entries.clone();
    command(&mut altered, 2).seal = Some(key.seal_command(DOMAIN, B256::repeat_byte(9)));

    let written_prefix = replay(&entries).unwrap().command_prefix;
    let replayed = replay(&altered).unwrap();
    assert_ne!(replayed.command_prefix, written_prefix, "the prefix reads every seal's bytes");
    let dir = written("run-ends-intermediate", &altered);
    replay_path_with_trust(&dir, Some(DOMAIN), Some(signer), SealChecks::RunEnds).unwrap();
    for refused in [
        replay_path_with_trust(&dir, Some(DOMAIN), Some(signer), every_seal()).map(drop),
        replay_copy(&dir, DOMAIN, BTreeSet::from([signer]), ReplayWorkers::available()).map(drop),
    ] {
        let refused = refused.unwrap_err();
        assert!(format!("{refused:#}").contains("invalid command origin signature"), "{refused:#}");
    }
}

/// A command whose stamp was changed after it was sealed still applies, to the same events and root, and
/// fails at the end of its run, naming the run, when the last seal does not verify over the chain.
#[test]
fn a_command_changed_after_sealing_fails_at_the_end_of_its_run() {
    let key = EnclaveKey::test_key();
    let mut entries = ticks(6, &[3], |_| Some((&key, DOMAIN)));
    command(&mut entries, 2).sequenced.ts = 1;
    let refused = format!("{:#}", replay(&entries).err().expect("refused"));
    assert!(refused.contains("the seal at seq 3 by"), "{refused}");
    assert!(refused.contains("over the commands from seq 0"), "{refused}");
    assert!(refused.contains("invalid command origin signature"), "{refused}");
}

/// A seal that is not well formed fails at its own command, whatever run it is in.
#[test]
fn a_malformed_seal_fails_at_its_own_command() {
    let key = EnclaveKey::test_key();
    let entries = ticks(6, &[5], |_| Some((&key, DOMAIN)));
    let high_s = |signature: &str| {
        let mut bytes: [u8; 65] = hex::decode(signature.trim_start_matches("0x")).unwrap().try_into().unwrap();
        let low = EcdsaSignature::from_slice(&bytes[..64]).unwrap();
        let high = EcdsaSignature::from_scalars(low.r().to_repr(), (-low.s()).to_repr()).unwrap();
        bytes[..64].copy_from_slice(&high.to_bytes());
        bytes[64] = 55 - bytes[64];
        format!("0x{}", hex::encode(bytes))
    };
    let cases: [(&str, Reseal, &str); 4] = [
        ("an Ethereum recovery id", |seal| seal.signature.replace_range(130..132, "00"), "malformed"),
        ("a zero r", |seal| seal.signature.replace_range(2..66, &"0".repeat(64)), "malformed"),
        ("a signer", |seal| seal.signer = AccountId([0; 20]), "zero signer"),
        ("hex", |seal| seal.signature = "0xnothex".into(), "command origin signature"),
    ];
    for (lacking, alter, said) in cases {
        let mut altered = entries.clone();
        alter(command(&mut altered, 2).seal.as_mut().unwrap());
        let refused = format!("{:#}", replay(&altered).err().expect("refused"));
        assert!(refused.contains("the seal at seq 2") && refused.contains(said), "without {lacking}: {refused}");
    }
    let mut altered = entries;
    let seal = command(&mut altered, 2).seal.as_mut().unwrap();
    seal.signature = high_s(&seal.signature);
    let refused = format!("{:#}", replay(&altered).err().expect("refused"));
    assert!(refused.contains("the seal at seq 2") && refused.contains("malformed"), "a high s: {refused}");
}

/// A run ends at a seal that names another signer or domain, and at a command with no seal, so the last
/// seal before it is checked in full: one whose bytes do not verify fails though the later run's verifies.
#[test]
fn a_run_ends_at_another_signer_or_domain_and_at_a_command_with_no_seal() {
    let (key, other) = (EnclaveKey::test_key(), EnclaveKey::generate());
    for after in ["another signer", "another domain", "no seal"] {
        // Seq 4 starts the later run, or has no seal.
        let sealer = |seq: u64| match after {
            "another signer" => Some(if seq < 4 { (&key, DOMAIN) } else { (&other, DOMAIN) }),
            "another domain" => Some((&key, if seq < 4 { DOMAIN } else { OTHER_DOMAIN })),
            _ => (seq != 4).then_some((&key, DOMAIN)),
        };
        let mut entries = ticks(7, &[], sealer);
        replay(&entries).unwrap_or_else(|e| panic!("{after}: the log as written: {e:#}"));
        command(&mut entries, 3).seal = Some(key.seal_command(DOMAIN, B256::repeat_byte(9)));
        let refused = format!("{:#}", replay(&entries).err().expect("refused"));
        assert!(refused.contains("the seal at seq 3 by"), "{after}: {refused}");
        assert!(refused.contains("over the commands from seq 0"), "{after}: {refused}");
    }
}

/// A run's last seal that does not verify is the earlier fault, reported ahead of its checkpoint's own,
/// a root that differs, ahead of a later command's, events that differ, and ahead of nothing at the
/// log's end, where the replay's read settles it.
#[test]
fn a_runs_failed_seal_is_reported_ahead_of_any_later_fault() {
    let key = EnclaveKey::test_key();
    let wrong_seal = |entries: &mut Vec<LogEntry>, seq| {
        command(entries, seq).seal = Some(key.seal_command(DOMAIN, B256::repeat_byte(9)));
    };
    // The checkpoint after seq 3, its root changed, and so its signature.
    let mut entries = ticks(6, &[3], |_| Some((&key, DOMAIN)));
    wrong_seal(&mut entries, 3);
    checkpoint(&mut entries, 3).root = format!("0x{}", hex::encode([1; 32]));
    let refused = format!("{:#}", replay(&entries).err().expect("refused"));
    assert!(refused.contains("the seal at seq 3 by"), "at a checkpoint: {refused}");
    // A later command of the same run, its events changed, which also changes its link.
    let mut entries = ticks(6, &[], |_| Some((&key, DOMAIN)));
    command(&mut entries, 1).sequenced.ts = 0;
    command(&mut entries, 4).events = Some(vec![Event::AdminApplied]);
    let refused = format!("{:#}", replay(&entries).err().expect("refused"));
    assert!(refused.contains("the seal at seq 4 by"), "at a later command: {refused}");
    // The tail after the last checkpoint, which nothing after it ends.
    let mut entries = ticks(6, &[2], |_| Some((&key, DOMAIN)));
    wrong_seal(&mut entries, 5);
    let refused = format!("{:#}", replay(&entries).err().expect("refused"));
    assert!(refused.contains("the seal at seq 5 by") && refused.contains("from seq 3"), "at the end: {refused}");
}

// ── Helpers ──

/// A change to a command's seal.
type Reseal = fn(&mut crate::log::CommandSeal);

/// A log of `commands` ticks, each stamped at its seq, each sealed by the key and in the domain `sealer`
/// gives it, if any, with a checkpoint the test key signs in `DOMAIN` after each seq in `checkpoints`.
fn ticks<'k>(
    commands: u64,
    checkpoints: &[u64],
    sealer: impl Fn(u64) -> Option<(&'k EnclaveKey, CheckpointDomain)>,
) -> Vec<LogEntry> {
    let key = EnclaveKey::test_key();
    let mut oracle = Engine::default();
    let (mut entries, mut link) = (Vec::new(), B256::ZERO);
    for seq in 0..commands {
        let sequenced = Sequenced { seq, ts: i64::try_from(seq).unwrap(), cmd: Command::Tick };
        let events = oracle.apply(&sequenced).unwrap();
        let mut entry = CommandEntry { sequenced, events: Some(events), seal: None };
        link = command_link(link, &entry).unwrap();
        entry.seal = sealer(seq).map(|(key, domain)| key.seal_command(domain, link));
        entries.push(LogEntry::Command(entry));
        if checkpoints.contains(&seq) {
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

/// The command at `seq` in `entries`.
fn command(entries: &mut [LogEntry], seq: u64) -> &mut CommandEntry {
    entries
        .iter_mut()
        .find_map(|entry| match entry {
            LogEntry::Command(command) if command.sequenced.seq == seq => Some(command),
            _ => None,
        })
        .expect("the log holds the command")
}

/// The checkpoint at `seq` in `entries`.
fn checkpoint(entries: &mut [LogEntry], seq: u64) -> &mut Checkpoint {
    entries
        .iter_mut()
        .find_map(|entry| match entry {
            LogEntry::Checkpoint(checkpoint) if checkpoint.seq == seq => Some(checkpoint),
            _ => None,
        })
        .expect("the log holds the checkpoint")
}

/// Every seal checked in full, on one worker per core.
fn every_seal() -> SealChecks {
    SealChecks::Every(ReplayWorkers::available())
}

/// `entries`, written to a fresh log directory for `label` in segments of four commands.
fn written(label: &str, entries: &[LogEntry]) -> PathBuf {
    let dir = crate::test_support::scratch_dir(label);
    std::fs::create_dir_all(&dir).unwrap();
    let log = SegmentedLog::open(&dir, |_| Ok(())).unwrap();
    let mut writer = log.into_writer(4, tokio::sync::watch::channel(Default::default()).0).unwrap();
    for entry in entries {
        writer.append(entry).unwrap();
    }
    writer.flush().unwrap();
    dir
}
