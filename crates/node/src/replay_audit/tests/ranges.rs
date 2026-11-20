//! A replay in ranges of a log whose withdrawal entries, after the daily snapshot a range starts from, name
//! a checkpoint and a deferred approval from before it: under a pinned signer and under the vault's signer
//! history, the replay in ranges agrees with the whole replay, and refuses what the whole replay refuses.

use std::num::NonZeroUsize;
use std::path::PathBuf;

use engine_core::Engine;
use engine_core::auth::{ClientAuth, Operator};
use engine_core::command::AdminCommand;
use engine_core::protocol::ProtocolVersion;

use super::*;
use crate::daily_snapshots::{Job, publish_now};
use crate::log::{Offset, SegmentedLog};
use crate::sequencer::SignedWithdrawal;
use crate::sequencer::replay::Audited;
use crate::sequencer::replay::ranges::{Trust, replay_ranges};
use crate::snapshot::sealing::Encoding;
use crate::tee::EnclaveKey;
use crate::test_support::{deposit, withdraw};

/// A range whose withdrawal entries release an approval a checkpoint before the range deferred, and pay it
/// through that checkpoint, replays to what the whole log replays to, under a pinned signer and under the
/// vault's signer history.
#[test]
fn withdrawals_that_name_what_came_before_their_range_replay_as_the_whole_log_does() {
    let ranged = Ranged::new("ranges-withdrawals", false);
    let signer = AccountId(ranged.key.address());
    let pinned = Trust::Pinned { domain: Some(ranged.domain), signer: Some(signer) };
    let whole = crate::sequencer::replay_path_with_trust(&ranged.dir, Some(ranged.domain), Some(signer), seals())
        .expect("the whole log replays under its signer");
    agree(Audited::from(whole), replay_ranges(&ranged.dir, &ranged.snapshots, pinned, jobs()).expect("in ranges"));

    let (parent, authorities) = ranged.history();
    let whole = crate::sequencer::replay_path_with_authorities(
        &ranged.dir,
        ranged.domain,
        signer,
        parent,
        authorities.clone(),
        seals(),
    )
    .expect("the whole log replays under the vault's history");
    let custody = Trust::Custody { domain: ranged.domain, signer, parent, authorities };
    agree(Audited::from(whole), replay_ranges(&ranged.dir, &ranged.snapshots, custody, jobs()).expect("in ranges"));
}

/// A typed payout after the snapshot that names a seq no checkpoint before it holds is refused by the
/// pass over the ranges, as the whole replay refuses it.
#[test]
fn a_payout_naming_a_checkpoint_no_earlier_range_holds_is_refused() {
    let ranged = Ranged::new("ranges-withdrawals-absent", true);
    let signer = AccountId(ranged.key.address());
    let whole = crate::sequencer::replay_path_with_trust(&ranged.dir, Some(ranged.domain), Some(signer), seals())
        .err()
        .expect("the whole replay refuses it");
    let trust = Trust::Pinned { domain: Some(ranged.domain), signer: Some(signer) };
    let in_ranges = replay_ranges(&ranged.dir, &ranged.snapshots, trust, jobs()).err().expect("refused in ranges");
    for error in [whole, in_ranges] {
        assert!(format!("{error:#}").contains("typed payout checkpoint is absent from the preceding log"), "{error:#}");
    }
}

// ── Helpers ──

/// A log directory, under one key: a deposit and a withdrawal approved under `V22`, the checkpoint that
/// defers its approval, an activation of `V27` and its checkpoint, at which a daily snapshot is published;
/// then the deferred approval's release and its typed payout, which name the first checkpoint, a tick and a
/// last checkpoint. `absent` names a seq the log holds no checkpoint at in the payout.
struct Ranged {
    dir: PathBuf,
    snapshots: PathBuf,
    domain: CheckpointDomain,
    key: EnclaveKey,
    checkpoints: Vec<Checkpoint>,
}

impl Ranged {
    fn new(name: &str, absent: bool) -> Self {
        let domain = CheckpointDomain { chain_id: 31337, vault: AccountId([9; 20]) };
        let key = EnclaveKey::generate();
        let (mut entries, mut engine) = (Vec::new(), Engine::new());
        let operator = Operator { address: AccountId([8; 20]), chain_id: domain.chain_id, vault: domain.vault };
        for cmd in [
            Command::Admin(AdminCommand::SetClientAuth(ClientAuth::Unsigned)),
            Command::Admin(AdminCommand::SetOperator(operator)),
            Command::Admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V22 }),
            deposit(100, "ranged-deposit"),
            withdraw(40),
        ] {
            append_command(&mut entries, &mut engine, cmd);
        }
        let first = checkpoint(&mut engine, &key, domain, CheckpointParent::default());
        entries.push(LogEntry::Checkpoint(first.clone()));
        let mut typed = super::fixture::payout(&engine, &key, domain, 1);
        if absent {
            let auth = typed.authorization.as_mut().expect("typed");
            auth.checkpoint_seq -= 1;
            typed.signature = key.sign_payout(&typed).unwrap();
        }
        let activate = Command::Admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V27 });
        append_command(&mut entries, &mut engine, activate);
        let second = checkpoint(&mut engine, &key, domain, checkpoint_parent(&first));
        entries.push(LogEntry::Checkpoint(second.clone()));
        let snapshot = Job {
            offset: Offset { seq: second.seq, index: 1 },
            checkpoint: second.clone(),
            ts: engine.now(),
            engine: Encoding::Encoded(serde_json::to_string(&engine).unwrap()),
        };
        entries.push(LogEntry::Withdrawal(release(&engine, &key, domain)));
        entries.push(LogEntry::Withdrawal(typed));
        append_command(&mut entries, &mut engine, Command::Tick);
        let third = checkpoint(&mut engine, &key, domain, checkpoint_parent(&second));
        entries.push(LogEntry::Checkpoint(third.clone()));

        let dir = scratch_dir(name);
        let manifest = watch::channel(Manifest::default()).0;
        let mut writer = SegmentedLog::open(&dir, |_| Ok(())).unwrap().into_writer(2, manifest).unwrap();
        for entry in &entries {
            writer.append(entry).unwrap();
        }
        writer.flush().unwrap();
        let snapshots = scratch_dir(&format!("{name}-snapshots"));
        publish_now(&snapshots, snapshot).unwrap();
        Self { dir, snapshots, domain, key, checkpoints: vec![first, second, third] }
    }

    /// The vault's finalized anchor, the last checkpoint, and its signer history: every checkpoint under
    /// the one key, in epoch 1.
    fn history(&self) -> (CheckpointParent, crate::checkpoints::SignerHistory) {
        let anchor = checkpoint_parent(self.checkpoints.last().unwrap());
        let history = crate::checkpoints::SignerHistory {
            checkpoints: self.checkpoints.iter().map(|cp| accepted(cp, 1)).collect(),
            epochs: [(1, AccountId(self.key.address()))].into(),
        };
        (anchor, history)
    }
}

/// The untyped release of the deferred approval `engine` holds for `TRADER`'s nonce 1.
fn release(engine: &Engine, key: &EnclaveKey, domain: CheckpointDomain) -> SignedWithdrawal {
    let approval = engine.withdrawal_approval(TRADER, 1).expect("approved");
    SignedWithdrawal {
        account: TRADER,
        amount: approval.amount,
        nonce: 1,
        chain_id: domain.chain_id,
        vault: domain.vault,
        signature: key.sign_withdrawal(domain.chain_id, &domain.vault.0, TRADER, approval.amount, 1),
        approved_seq: Some(approval.approved_seq),
        authorization: None,
    }
}

fn jobs() -> NonZeroUsize {
    NonZeroUsize::new(2).unwrap()
}

/// Run-end seal checks, as a replay in ranges checks them.
fn seals() -> SealChecks {
    SealChecks::RunEnds
}

/// Requires the whole replay and the replay in ranges, in two ranges, to agree on what an audit reports.
fn agree(whole: Audited, in_ranges: Audited) {
    assert_eq!(in_ranges.ranges, 2);
    assert_eq!(
        (whole.commands, whole.checkpoints, whole.pending, &whole.checkpoint_signers),
        (in_ranges.commands, in_ranges.checkpoints, in_ranges.pending, &in_ranges.checkpoint_signers)
    );
    let state = |audited: &Audited| serde_json::to_string(&audited.engine).unwrap();
    assert_eq!(state(&whole), state(&in_ranges));
}
