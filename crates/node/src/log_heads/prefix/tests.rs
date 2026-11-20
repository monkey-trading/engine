use alloy::primitives::B256;
use engine_core::Engine;
use engine_core::command::{Command, Sequenced};

use super::*;
use crate::log::{Checkpoint, CommandEntry};
use crate::tee::EnclaveKey;

const DOMAIN: CheckpointDomain = CheckpointDomain { chain_id: 31_337, vault: AccountId([7; 20]) };
/// Commands a fixture log's segment holds before the next one starts.
const SEGMENT_COMMANDS: u64 = 10;

#[test]
fn the_check_since_the_head_accepts_the_snapshots_history_and_stamps_the_next_command() {
    // Each command is stamped at its seq, so the stamp the alarm starts from names the command.
    let (dir, _, prefixes) = sealed_log("since-accept", 25, &[0]);
    let base = 20;
    for seq in [0, 9, 10, 19] {
        let head = Head { seq, link: prefixes[seq as usize].into() };
        let ts = since(&dir, head, base, 99, prefixes[base as usize]).unwrap();
        assert_eq!(ts, Millis::try_from(seq + 1).unwrap(), "the first command after the head at seq {seq}");
    }
    // A head at the base needs no read: the snapshot's own stamp anchors the alarm.
    let head = Head { seq: base, link: prefixes[base as usize].into() };
    assert_eq!(since(&dir, head, base, 99, prefixes[base as usize]).unwrap(), 99);
}

#[test]
fn the_check_since_the_head_refuses_a_head_or_a_snapshot_of_another_history() {
    let (dir, _, prefixes) = sealed_log("since-divergent", 25, &[0]);
    let base = 20;
    for seq in [0, 15, base] {
        let foreign = Head { seq, link: B256::from([4; 32]) };
        let error = since(&dir, foreign, base, 0, prefixes[base as usize]).unwrap_err();
        assert!(error.is::<Rollback>(), "a head this history does not hold refuses: {error:#}");
        // A certified prefix from another history (even one differing by a byte) refuses too.
        let head = Head { seq, link: prefixes[seq as usize].into() };
        let foreign = log::extend_command_prefix(prefixes[base as usize], [9; 32]);
        assert!(since(&dir, head, base, 0, foreign).unwrap_err().is::<Rollback>(), "seq {seq}");
    }
}

#[test]
fn the_check_since_the_head_reads_no_segment_before_the_heads() {
    // The first segment, seqs 0 to 9, is garbled: the full pass cannot read it, and the check from a
    // head in a later segment never opens it.
    let (dir, signer, prefixes) = sealed_log("since-bounded", 25, &[0]);
    std::fs::write(dir.join(segment(0)), b"not a log\n").unwrap();
    let base = 20;
    assert!(verify(&dir, DOMAIN, signer, 0, base, prefixes[base as usize]).is_err(), "the full pass reads it");
    let head = Head { seq: 12, link: prefixes[12].into() };
    assert_eq!(since(&dir, head, base, 0, prefixes[base as usize]).unwrap(), 13);
}

#[test]
fn the_check_since_the_head_ignores_commands_beyond_the_base() {
    // Commands past the base are the suffix's: the check stops at the base, so a mismatch beyond it
    // never reaches the comparison.
    let (dir, _, prefixes) = sealed_log("since-suffix", 25, &[0]);
    let head = Head { seq: 3, link: prefixes[3].into() };
    assert_eq!(since(&dir, head, 7, 0, prefixes[7]).unwrap(), 4);
}

#[test]
fn the_check_since_the_head_rejects_a_log_that_ends_or_skips_before_the_base() {
    // A log that cannot be read through the base is a cache to reject and fall back from, not a
    // rollback: the digests were never compared.
    let (dir, _, prefixes) = sealed_log("since-short", 25, &[0]);
    let head = Head { seq: 3, link: prefixes[3].into() };
    let error = since(&dir, head, 30, 0, prefixes[24]).unwrap_err();
    assert!(
        !error.is::<Rollback>() && format!("{error:#}").contains("no command at the snapshot base 30"),
        "{error:#}"
    );
    std::fs::remove_file(dir.join(segment(10))).unwrap();
    let error = since(&dir, head, 20, 0, prefixes[20]).unwrap_err();
    assert!(!error.is::<Rollback>() && format!("{error:#}").contains("seq 20 where seq 10"), "{error:#}");
}

#[test]
fn the_anchor_pass_stamps_the_finalized_checkpoint() {
    // No head is posted: the pass binds the on-disk prefix to the snapshot and returns the stamp of
    // the command the vault-finalized checkpoint covers.
    let (dir, signer, prefixes) = sealed_log("prefix-anchor", 2, &[0]);
    for anchor in [0, 1] {
        let verified = verify(&dir, DOMAIN, signer, anchor, 1, prefixes[1]).unwrap();
        let stamp = Millis::try_from(anchor).unwrap();
        assert_eq!(verified, Verified { base_prefix: prefixes[1], anchor_ts: stamp });
    }
    // With no head on chain, a foreign prefix is a cache recovery rejects, not a rollback.
    let error = verify(&dir, DOMAIN, signer, 0, 1, [9; 32]).unwrap_err();
    assert!(!error.is::<Rollback>() && format!("{error:#}").contains("certified prefix"), "{error:#}");
}

#[test]
fn the_anchor_pass_judges_the_prefix_before_a_missing_anchor() {
    // The anchor's command is missing: a prefix that is not the snapshot's is the mismatch, judged
    // first; with the snapshot's prefix, the missing anchor is the error.
    let (dir, signer, prefixes) = sealed_log("prefix-anchor-order", 2, &[0]);
    let error = verify(&dir, DOMAIN, signer, 5, 1, [9; 32]).unwrap_err();
    assert!(format!("{error:#}").contains("certified prefix"), "{error:#}");
    let error = verify(&dir, DOMAIN, signer, 5, 1, prefixes[1]).unwrap_err();
    assert!(format!("{error:#}").contains("finalized checkpoint's seq 5"), "{error:#}");
}

#[test]
fn the_anchor_pass_ignores_commands_beyond_the_base() {
    // A nonempty suffix must not contaminate the certified-prefix comparison: the boundary is
    // checked before the first command beyond the base is processed.
    let (dir, signer, prefixes) = sealed_log("prefix-suffix", 3, &[0]);
    assert_eq!(verify(&dir, DOMAIN, signer, 0, 1, prefixes[1]).unwrap().base_prefix, prefixes[1]);
}

#[test]
fn the_anchor_pass_refuses_a_seal_failure_and_an_unsealed_span() {
    let key = EnclaveKey::test_key();
    let signer = AccountId(key.address());
    let other = EnclaveKey::generate();
    let mut oracle = Engine::new();
    let mut entries = Vec::new();
    let mut link = engine_core::eip712::B256::ZERO;
    let mut command_prefix = [0u8; 32];
    for seq in 0..2u64 {
        let sequenced = Sequenced { seq, ts: i64::try_from(seq).unwrap(), cmd: Command::Tick };
        let events = oracle.apply(&sequenced).unwrap();
        let mut entry = CommandEntry { sequenced, events: Some(events), seal: None };
        link = crate::log::command_seal::command_link(link, &entry).unwrap();
        // seq 0 is sealed by a key the trusted authority does not name; seq 1 is unsealed.
        if seq == 0 {
            entry.seal = Some(other.seal_command(DOMAIN, link));
        }
        command_prefix = log::extend_command_prefix(command_prefix, log::command_digest(&entry).unwrap());
        entries.push(LogEntry::Command(entry));
    }
    let dir = write_log("prefix-seals", &entries);
    let error = verify(&dir, DOMAIN, signer, 1, 1, command_prefix).unwrap_err();
    assert!(!error.is::<Rollback>(), "a seal failure is a plain refusal, not a digest mismatch: {error:#}");
    assert!(format!("{error:#}").contains("prefix"), "{error:#}");
}

// ── Helpers ──

/// A log of `commands` sealed ticks, each stamped at its seq, with a signed checkpoint after each seq
/// in `checkpoints`, in segments of `SEGMENT_COMMANDS`; with each seq's cumulative command-prefix
/// digest, which checkpoints do not reset, as the replay that certifies a snapshot computes it.
fn sealed_log(label: &str, commands: u64, checkpoints: &[u64]) -> (std::path::PathBuf, AccountId, Vec<[u8; 32]>) {
    let key = EnclaveKey::test_key();
    let signer = AccountId(key.address());
    let mut oracle = Engine::new();
    let mut entries = Vec::new();
    let mut link = engine_core::eip712::B256::ZERO;
    let mut command_prefix = [0u8; 32];
    let mut prefixes = Vec::new();
    for seq in 0..commands {
        let sequenced = Sequenced { seq, ts: i64::try_from(seq).unwrap(), cmd: Command::Tick };
        let events = oracle.apply(&sequenced).unwrap();
        let mut entry = CommandEntry { sequenced, events: Some(events), seal: None };
        link = crate::log::command_seal::command_link(link, &entry).unwrap();
        entry.seal = Some(key.seal_command(DOMAIN, link));
        command_prefix = log::extend_command_prefix(command_prefix, log::command_digest(&entry).unwrap());
        prefixes.push(command_prefix);
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
            link = crate::log::command_seal::checkpoint_link(&checkpoint).unwrap();
            entries.push(LogEntry::Checkpoint(checkpoint));
        }
    }
    (write_log(label, &entries), signer, prefixes)
}

/// The file name of the segment that starts at `first_seq`.
fn segment(first_seq: u64) -> String {
    format!("{first_seq:020}.jsonl")
}

/// Writes `entries` to a fresh log directory for `label`, in segments of `SEGMENT_COMMANDS`.
fn write_log(label: &str, entries: &[LogEntry]) -> std::path::PathBuf {
    let dir = crate::test_support::scratch_dir(label);
    std::fs::create_dir_all(&dir).unwrap();
    let log = SegmentedLog::open(&dir, |_| Ok(())).unwrap();
    let mut writer = log.into_writer(SEGMENT_COMMANDS, tokio::sync::watch::channel(Default::default()).0).unwrap();
    for entry in entries {
        writer.append(entry).unwrap();
    }
    writer.flush().unwrap();
    dir
}

/// `super::verify` with one seal check worker per core.
fn verify(
    dir: &Path,
    domain: CheckpointDomain,
    signer: AccountId,
    anchor: u64,
    base: u64,
    certified_prefix: [u8; 32],
) -> anyhow::Result<Verified> {
    super::verify(dir, domain, signer, anchor, base, certified_prefix, ReplayWorkers::default())
}
