//! Verifying a posted head at or under a snapshot's base without paying for full engine replay.
//! The snapshot proves which state its signer produced; it does not prove that the log on disk is
//! the history that state came from. This pass hashes the on-disk prefix through the snapshot's
//! base, following the command seals' chain, and compares the running cumulative command-prefix
//! digest with the snapshot's certified `context.command_prefix` at the base and with the posted
//! value at the posted seq. The two digest comparisons are the binding; the seal pass is a
//! consistency check that does not authenticate checkpoint entries itself. No engine apply runs,
//! so snapshot recovery keeps its savings, and one pass binds the scanned bytes
//! to the snapshot the node is about to resume. A host that serves one fork to this pass and
//! another to suffix recovery fails the prefix comparison; a seal failure or a posted-digest
//! mismatch refuses startup.

use std::path::Path;

use anyhow::Context;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::AccountId;
use tracing::error;

use super::Head;
use crate::log::{self, CheckpointDomain, LogEntry, SegmentedLog, Stop};
use crate::sequencer::origin::Origin;

/// Marks a posted-head refusal below the snapshot base: the candidate loop must not treat it as a
/// rejectable cache and fall back — the node refuses to start.
#[derive(Debug)]
pub(crate) struct Rollback;

impl std::fmt::Display for Rollback {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("the on-chain log head conflicts with this log below the snapshot base")
    }
}

impl std::error::Error for Rollback {}

/// Whether the on-disk log through the snapshot base `base` carries the posted head's cumulative
/// command-prefix digest and the snapshot's certified command prefix, verified from the log's
/// start along `signer`'s seal chain. Only the command-prefix hash and the seals are checked — the
/// engine is not applied — and the pass stops at the base: the boundary is checked before an
/// entry beyond it is processed, so a nonempty suffix never contaminates the comparison. Returns
/// the running digest at the base and the sequenced time of the command at the posted seq, where
/// the stall alarm starts.
/// Error cases:
/// - the log cannot be opened or a seal is invalid, an unsealed span lacks the trusted authority,
///   the log holds no command at the posted seq or the base, or the posted digest or the command
///   prefix differs (the Critical names the cause)
pub(crate) fn verify(
    dir: &Path,
    domain: CheckpointDomain,
    signer: AccountId,
    posted: Head,
    base: u64,
    certified_prefix: [u8; 32],
) -> anyhow::Result<([u8; 32], engine_core::types::Millis)> {
    let mut origin = Origin::new(Some(domain), Some(signer));
    let mut command_prefix = [0u8; 32];
    let mut previous = None;
    let mut posted_prefix = None;
    let mut base_prefix = None;
    let result = SegmentedLog::open(dir, |entry| {
        let offset = log::offset_of(previous, &entry)?;
        if offset.seq > base {
            // The boundary is checked before the entry is processed: no origin update, no prefix
            // extension for the first command beyond the base.
            return Err(Stop.into());
        }
        previous = Some(offset);
        match &entry {
            LogEntry::Command(command) => {
                // The version gates only legacy-staking bookkeeping, which this pass never reads.
                origin.command(command, ProtocolVersion::LATEST)?;
                command_prefix =
                    crate::log::extend_command_prefix(command_prefix, crate::log::command_digest(command)?);
                if command.sequenced.seq == posted.seq {
                    posted_prefix = Some((command_prefix, command.sequenced.ts));
                }
                if command.sequenced.seq == base {
                    base_prefix = Some(command_prefix);
                }
            }
            LogEntry::Checkpoint(checkpoint) => {
                origin.checkpoint(checkpoint, domain, true)?;
            }
            LogEntry::Withdrawal(_) => {}
        }
        Ok(())
    });
    match result {
        Ok(_) => {}
        Err(cause) if cause.is::<Stop>() => {}
        Err(cause) => {
            return Err(cause).context("verifying the on-chain log head against the log's prefix");
        }
    }
    // Unsealed spans the trusted authority does not cover refuse the start, as writable
    // recovery's coverage check does.
    origin.require_audit(signer).context("verifying the on-chain log head against the log's prefix")?;
    let (prefix, posted_ts) = posted_prefix
        .with_context(|| format!("the log holds no command at the on-chain log head's seq {}", posted.seq))?;
    if prefix != posted.link {
        error!(
            alert = "log_rollback",
            seq = posted.seq,
            posted_link = %posted.link,
            log_prefix = %engine_core::eip712::B256::from(prefix),
            "the on-chain log head's cumulative prefix differs at seq {} at or below the snapshot base: \
             refusing to start",
            posted.seq
        );
        return Err(anyhow::Error::new(Rollback));
    }
    if command_prefix != certified_prefix {
        error!(
            alert = "log_rollback",
            seq = base,
            "the log's command prefix through the snapshot base differs from the snapshot's certified \
             prefix: the on-disk prefix is not the snapshot's history; refusing to start"
        );
        return Err(anyhow::Error::new(Rollback));
    }
    Ok((base_prefix.context("the log holds no command at the snapshot base")?, posted_ts))
}

#[cfg(test)]
mod tests {
    use alloy::primitives::B256;
    use engine_core::Engine;
    use engine_core::command::{Command, Sequenced};

    use super::*;
    use crate::log::{Checkpoint, CommandEntry};
    use crate::tee::EnclaveKey;

    const DOMAIN: CheckpointDomain = CheckpointDomain { chain_id: 31_337, vault: AccountId([7; 20]) };

    /// A log of two sealed commands with a signed checkpoint between them, the snapshot base at
    /// seq 1, and optionally a third command beyond the base: the cumulative command-prefix
    /// digests at seqs 0 and 1 (the certified prefix, which checkpoints do not reset) and, with a
    /// suffix, the digest at seq 2 the pass must never reach.
    fn sealed_log(label: &str, suffix: bool) -> (std::path::PathBuf, AccountId, [u8; 32], [u8; 32], [u8; 32]) {
        let key = EnclaveKey::test_key();
        let signer = AccountId(key.address());
        let mut oracle = Engine::new();
        let mut entries = Vec::new();
        let mut link = engine_core::eip712::B256::ZERO;
        let mut command_prefix = [0u8; 32];
        let mut prefixes = Vec::new();
        for seq in 0..(if suffix { 3 } else { 2 }) {
            let sequenced = Sequenced { seq, ts: i64::try_from(seq).unwrap(), cmd: Command::Tick };
            let events = oracle.apply(&sequenced).unwrap();
            let mut entry = CommandEntry { sequenced, events: Some(events), seal: None };
            link = crate::log::command_seal::command_link(link, &entry).unwrap();
            entry.seal = Some(key.seal_command(DOMAIN, link));
            // The digest binds the sealed entry, as the replay that certified the snapshot's
            // prefix computed it.
            command_prefix =
                crate::log::extend_command_prefix(command_prefix, crate::log::command_digest(&entry).unwrap());
            prefixes.push(command_prefix);
            entries.push(LogEntry::Command(entry));
            if seq == 0 {
                let root = oracle.state_root();
                let checkpoint = Checkpoint {
                    parent: None,
                    seq: 0,
                    root: format!("0x{}", hex::encode(root)),
                    signature: key.sign_checkpoint(DOMAIN.chain_id, &DOMAIN.vault.0, 0, &root),
                    signer: key.address_hex(),
                    domain: Some(DOMAIN),
                    deferred_withdrawals: false,
                };
                link = crate::log::command_seal::checkpoint_link(&checkpoint).unwrap();
                entries.push(LogEntry::Checkpoint(checkpoint));
            }
        }
        let dir = crate::test_support::scratch_dir(label);
        std::fs::create_dir_all(&dir).unwrap();
        let log = SegmentedLog::open(&dir, |_| Ok(())).unwrap();
        let mut writer = log.into_writer(10, tokio::sync::watch::channel(Default::default()).0).unwrap();
        for entry in &entries {
            writer.append(entry).unwrap();
        }
        writer.flush().unwrap();
        // The certified prefix covers the base: commands beyond it are the suffix's.
        (dir, signer, prefixes[0], prefixes[1], prefixes[1])
    }

    #[test]
    fn the_prefix_pass_accepts_the_matching_head_and_returns_the_base_prefix() {
        // The fixture stamps each command at its seq: the pass returns the posted command's time.
        let (dir, signer, at_zero, at_one, prefix) = sealed_log("prefix-accept", false);
        let (base, ts) = verify(&dir, DOMAIN, signer, Head { seq: 0, link: at_zero.into() }, 1, prefix).unwrap();
        assert_eq!((base, ts), (at_one, 0), "the pass returns the cumulative prefix at the base");
        let (base, ts) = verify(&dir, DOMAIN, signer, Head { seq: 1, link: at_one.into() }, 1, prefix).unwrap();
        assert_eq!((base, ts), (at_one, 1));
    }

    #[test]
    fn the_prefix_pass_ignores_commands_beyond_the_base() {
        // A nonempty suffix must not contaminate the certified-prefix comparison: the boundary is
        // checked before the first command beyond the base is processed.
        let (dir, signer, at_zero, at_one, prefix) = sealed_log("prefix-suffix", true);
        let (base, _) = verify(&dir, DOMAIN, signer, Head { seq: 0, link: at_zero.into() }, 1, prefix).unwrap();
        assert_eq!(base, at_one);
        let (base, _) = verify(&dir, DOMAIN, signer, Head { seq: 1, link: at_one.into() }, 1, prefix).unwrap();
        assert_eq!(base, at_one);
    }

    #[test]
    fn the_prefix_pass_refuses_a_divergent_posted_head() {
        let (dir, signer, _, _, prefix) = sealed_log("prefix-divergent", false);
        for seq in [0, 1] {
            let error = verify(&dir, DOMAIN, signer, Head { seq, link: B256::from([4; 32]) }, 1, prefix).unwrap_err();
            assert!(error.is::<Rollback>(), "a divergent posted head refuses: {error:#}");
        }
    }

    #[test]
    fn the_prefix_pass_refuses_a_prefix_the_snapshot_does_not_certify() {
        let (dir, signer, at_zero, _, prefix) = sealed_log("prefix-binding", false);
        // A certified prefix from another history (even one differing by a byte) refuses: the
        // scanned bytes are not bound to the selected snapshot.
        let foreign = crate::log::extend_command_prefix(prefix, [9; 32]);
        let error = verify(&dir, DOMAIN, signer, Head { seq: 0, link: at_zero.into() }, 1, foreign).unwrap_err();
        assert!(error.is::<Rollback>(), "an uncertified command prefix refuses: {error:#}");
    }

    #[test]
    fn the_prefix_pass_refuses_a_seal_failure_and_an_unsealed_span() {
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
            command_prefix =
                crate::log::extend_command_prefix(command_prefix, crate::log::command_digest(&entry).unwrap());
            entries.push(LogEntry::Command(entry));
        }
        let dir = crate::test_support::scratch_dir("prefix-seals");
        std::fs::create_dir_all(&dir).unwrap();
        let log = SegmentedLog::open(&dir, |_| Ok(())).unwrap();
        let mut writer = log.into_writer(10, tokio::sync::watch::channel(Default::default()).0).unwrap();
        for entry in &entries {
            writer.append(entry).unwrap();
        }
        writer.flush().unwrap();
        let posted = Head { seq: 1, link: command_prefix.into() };
        let error = verify(&dir, DOMAIN, signer, posted, 1, command_prefix).unwrap_err();
        assert!(!error.is::<Rollback>(), "a seal failure is a plain refusal, not a digest mismatch: {error:#}");
        assert!(format!("{error:#}").contains("prefix"), "{error:#}");
    }
}
