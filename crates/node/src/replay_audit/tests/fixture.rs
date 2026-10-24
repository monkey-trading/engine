//! Signed audit logs and a read-only canonical history adapter.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use alloy::primitives::B256;
use engine_core::auth::{ClientAuth, Operator};
use engine_core::command::{AdminCommand, Command, Sequenced};
use engine_core::event::Event;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::AccountId;
use engine_core::{Engine, Fixed};

use crate::checkpoints::{
    APPROVAL_PAGE_SIZE, AcceptedCheckpoint, ApprovalId, CheckpointChain, LifecycleEvidence, Observation, SignerHistory,
    VaultState, View,
};
use crate::log::{Checkpoint, CheckpointDomain, CheckpointParent, CommandEntry, LogEntry};
use crate::sequencer::SignedWithdrawal;
use crate::tee::EnclaveKey;
use crate::test_support::{TRADER, deposit, scratch_dir, withdraw};
use crate::withdrawals::{Authorization, LIFETIME_SECONDS};

type HistoryQuery = (B256, Vec<u64>);

#[derive(Clone)]
pub(super) struct Chain {
    pub(super) at: Observation,
    pub(super) accepted: Arc<Mutex<BTreeMap<u64, AcceptedCheckpoint>>>,
    pub(super) pages: Arc<Mutex<Vec<HistoryQuery>>>,
    pub(super) epochs: Arc<Mutex<BTreeMap<u64, AccountId>>>,
    pub(super) epoch_pages: Arc<Mutex<Vec<HistoryQuery>>>,
}

impl CheckpointChain for Chain {
    fn reserved_checkpoint(&self) -> anyhow::Result<Option<Checkpoint>> {
        panic!("audit must never consult a posting journal")
    }

    async fn state(&self, view: View) -> anyhow::Result<VaultState> {
        assert_eq!(view, View::Finalized);
        Ok(self.at.state.clone())
    }

    async fn observe(&self, view: View, approvals: &[ApprovalId]) -> anyhow::Result<Observation> {
        assert_eq!(view, View::Finalized);
        assert!(approvals.is_empty());
        Ok(self.at.clone())
    }

    async fn accepted_checkpoints(
        &self,
        at: &Observation,
        sequences: &[u64],
    ) -> anyhow::Result<Vec<AcceptedCheckpoint>> {
        assert_eq!(at.view, View::Finalized);
        assert!(sequences.len() <= APPROVAL_PAGE_SIZE);
        let hash = at.lifecycle.as_ref().unwrap().block_hash;
        assert_eq!(hash, self.at.lifecycle.as_ref().unwrap().block_hash);
        self.pages.lock().unwrap().push((hash, sequences.to_vec()));
        sequences
            .iter()
            .map(|seq| {
                self.accepted
                    .lock()
                    .unwrap()
                    .get(seq)
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("missing accepted checkpoint history"))
            })
            .collect()
    }

    async fn post(&self, _: &Checkpoint) -> anyhow::Result<()> {
        panic!("audit must never send a transaction")
    }

    async fn epoch_signers(&self, at: &Observation, epochs: &[u64]) -> anyhow::Result<Vec<AccountId>> {
        assert_eq!(at.view, View::Finalized);
        assert!(epochs.len() <= APPROVAL_PAGE_SIZE);
        let hash = at.lifecycle.as_ref().unwrap().block_hash;
        assert_eq!(hash, self.at.lifecycle.as_ref().unwrap().block_hash);
        self.epoch_pages.lock().unwrap().push((hash, epochs.to_vec()));
        epochs
            .iter()
            .map(|epoch| {
                self.epochs
                    .lock()
                    .unwrap()
                    .get(epoch)
                    .copied()
                    .ok_or_else(|| anyhow::anyhow!("missing immutable signer epoch"))
            })
            .collect()
    }
}

// ── Helpers ──

pub(super) struct Fixture {
    pub(super) entries: Vec<LogEntry>,
    pub(super) path: PathBuf,
    pub(super) domain: CheckpointDomain,
    pub(super) old: EnclaveKey,
    pub(super) current: EnclaveKey,
    pub(super) chain: Chain,
}

impl Fixture {
    pub(super) fn new(name: &str) -> Self {
        let domain = CheckpointDomain { chain_id: 31337, vault: AccountId([9; 20]) };
        let old = EnclaveKey::test_key();
        let current = EnclaveKey::generate();
        let mut entries = Vec::new();
        let mut engine = Engine::new();
        for cmd in [
            Command::Admin(AdminCommand::SetClientAuth(ClientAuth::Unsigned)),
            Command::Admin(AdminCommand::SetOperator(Operator {
                address: AccountId([8; 20]),
                chain_id: domain.chain_id,
                vault: domain.vault,
            })),
            Command::Admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V22 }),
            deposit(100, "audit-deposit"),
            withdraw(40),
        ] {
            append_command(&mut entries, &mut engine, cmd);
        }
        let approval = *engine.withdrawal_approval(TRADER, 1).expect("V22 withdrawal approved");
        assert_eq!(approval.amount, Fixed::from_int(40));
        let first = checkpoint(&mut engine, &old, domain, CheckpointParent::default());
        entries.push(LogEntry::Checkpoint(first.clone()));
        entries.push(LogEntry::Withdrawal(payout(&engine, &old, domain, 1)));
        append_command(&mut entries, &mut engine, Command::Tick);
        let second = checkpoint(&mut engine, &current, domain, checkpoint_parent(&first));
        entries.push(LogEntry::Checkpoint(second.clone()));
        entries.push(LogEntry::Withdrawal(payout(&engine, &current, domain, 2)));
        let state = VaultState {
            height: 99,
            signer: AccountId(current.address()),
            checkpoint: Some((second.seq, second.root.clone())),
            balance: Fixed::from_int(100),
            deposits: 1,
        };
        let lifecycle = LifecycleEvidence {
            height: 99,
            block_hash: B256::repeat_byte(0x88),
            signer: state.signer,
            signer_epoch: 2,
            total_withdrawn: Fixed::ZERO,
            checkpoint_seq: Some(second.seq),
            checkpoint_root: Some(second.root.clone()),
            approvals: Vec::new(),
        };
        let chain = Chain {
            at: Observation { view: View::Finalized, state, lifecycle: Some(lifecycle) },
            accepted: Arc::new(Mutex::new(BTreeMap::from([
                (first.seq, accepted(&first, 1)),
                (second.seq, accepted(&second, 2)),
            ]))),
            pages: Default::default(),
            epochs: Arc::new(Mutex::new(BTreeMap::from([
                (1, AccountId(old.address())),
                (2, AccountId(current.address())),
            ]))),
            epoch_pages: Default::default(),
        };
        let dir = scratch_dir(name);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("public.jsonl");
        write_file(&path, &entries);
        Self { entries, path, domain, old, current, chain }
    }

    pub(super) fn signer(&self) -> AccountId {
        AccountId(self.current.address())
    }

    pub(super) fn parent(&self) -> CheckpointParent {
        let (seq, root) = self.chain.at.state.checkpoint.as_ref().unwrap();
        CheckpointParent { has_checkpoint: true, seq: *seq, root: root.parse().unwrap() }
    }

    pub(super) fn history(&self) -> SignerHistory {
        SignerHistory {
            checkpoints: self.chain.accepted.lock().unwrap().values().cloned().collect(),
            epochs: self.chain.epochs.lock().unwrap().clone(),
        }
    }
}

pub(super) fn append_command(entries: &mut Vec<LogEntry>, engine: &mut Engine, cmd: Command) {
    let sequenced = Sequenced { seq: engine.last_seq().map_or(0, |seq| seq + 1), ts: 1_000, cmd };
    let events = engine.apply(&sequenced).unwrap();
    assert!(
        !events.iter().any(|event| matches!(event, Event::AdminRejected { .. } | Event::WithdrawalRejected { .. }))
    );
    entries.push(LogEntry::Command(CommandEntry { sequenced, events: Some(events) }));
}

pub(super) fn checkpoint(
    engine: &mut Engine,
    key: &EnclaveKey,
    domain: CheckpointDomain,
    parent: CheckpointParent,
) -> Checkpoint {
    let seq = engine.last_seq().unwrap();
    let root = engine.state_root();
    Checkpoint {
        parent: Some(parent),
        seq,
        root: format!("0x{}", hex::encode(root)),
        signature: key.sign_checkpoint_v2(domain.chain_id, &domain.vault.0, seq, &root, parent),
        signer: key.address_hex(),
        domain: Some(domain),
        deferred_withdrawals: true,
    }
}

pub(super) fn checkpoint_parent(cp: &Checkpoint) -> CheckpointParent {
    CheckpointParent { has_checkpoint: true, seq: cp.seq, root: cp.root.parse().unwrap() }
}

pub(super) fn accepted(cp: &Checkpoint, signer_epoch: u64) -> AcceptedCheckpoint {
    AcceptedCheckpoint { seq: cp.seq, root: cp.root.clone(), signer: cp.signer.parse().unwrap(), signer_epoch }
}

fn payout(engine: &Engine, key: &EnclaveKey, domain: CheckpointDomain, signer_epoch: u64) -> SignedWithdrawal {
    let proof = engine.approval_proof(TRADER, 1).unwrap();
    let mut withdrawal = SignedWithdrawal {
        account: TRADER,
        amount: proof.approval.amount,
        nonce: 1,
        chain_id: domain.chain_id,
        vault: domain.vault,
        signature: String::new(),
        approved_seq: Some(proof.approval.approved_seq),
        authorization: Some(Authorization {
            signer: AccountId(key.address()),
            signer_epoch,
            issued_at: 1,
            expires_at: 1 + LIFETIME_SECONDS,
            checkpoint_seq: proof.seq,
            checkpoint_root: proof.root,
            siblings: proof.siblings,
        }),
    };
    withdrawal.signature = key.sign_payout(&withdrawal).unwrap();
    withdrawal
}

pub(super) fn write_file(path: &Path, entries: &[LogEntry]) {
    let text = entries.iter().map(|entry| serde_json::to_string(entry).unwrap() + "\n").collect::<String>();
    fs::write(path, text).unwrap();
}
