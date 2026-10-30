//! Historical V19 withdrawal/storage fixtures; current envelopes are covered by access tests.
//! Signature, fencing, retry, and collateral regression tests.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use engine_core::command::{Command, Sequenced};
use engine_core::types::AccountId;
use engine_core::{Engine, Fixed};
use tokio::sync::mpsc;

use crate::checkpoints::{CheckpointChain, Health, Poster, VaultState, View, Work, signature};
use crate::log::{Checkpoint, CheckpointDomain, CommandEntry, LogEntry};
use crate::sequencer::{Request, replay};
use crate::tee::EnclaveKey;
mod distributions;
mod receipts;

mod collateral;
mod torn;

/// Corrupts a hex signature without the fixed-byte no-op case.
pub(super) fn tamper_signature(signature: &mut String) {
    let original = signature.clone();
    let byte = u8::from_str_radix(&signature[4..6], 16).unwrap() ^ 1;
    signature.replace_range(4..6, &format!("{byte:02x}"));
    assert_ne!(*signature, original, "signature tampering must change the signed bytes");
}

#[test]
fn signature_tampering_changes_every_original_byte() {
    for byte in 0_u8..=u8::MAX {
        let mut signature = format!("0x00{byte:02x}{}", "00".repeat(63));
        tamper_signature(&mut signature);
        assert_eq!(hex::decode(&signature[2..]).unwrap().len(), 65);
    }
}

#[derive(Clone)]
pub(super) struct Chain {
    state: Arc<Mutex<VaultState>>,
    finalized: Option<Arc<Mutex<VaultState>>>,
    fails: Arc<Mutex<usize>>,
    posts: Arc<Mutex<usize>>,
    /// The vault's running deposit total after each deposit, in deposit order.
    through: Arc<Mutex<Vec<Fixed>>>,
    /// Whether the vault's exit has started.
    exit: Arc<Mutex<bool>>,
}

impl Chain {
    /// Records a vault deposit that the ledger has not credited.
    fn deposit(&self, amount: Fixed) {
        let mut state = self.state.lock().unwrap();
        state.deposits += 1;
        state.deposited += amount;
        state.balance += amount;
        self.through.lock().unwrap().push(state.deposited);
    }
}

impl CheckpointChain for Chain {
    async fn state(&self, view: View) -> anyhow::Result<VaultState> {
        let state = if view == View::Finalized { self.finalized.as_ref().unwrap_or(&self.state) } else { &self.state };
        Ok(state.lock().unwrap().clone())
    }
    async fn deposited_through(&self, at: &crate::checkpoints::Observation, count: u64) -> anyhow::Result<Fixed> {
        anyhow::ensure!(count > 0 && count <= at.state.deposits, "deposit total is outside the observed deposits");
        Ok(self.through.lock().unwrap()[usize::try_from(count - 1)?])
    }
    async fn post(&self, cp: &Checkpoint) -> anyhow::Result<()> {
        let mut fails = self.fails.lock().unwrap();
        if *fails > 0 {
            *fails -= 1;
            anyhow::bail!("RPC unavailable")
        }
        *self.posts.lock().unwrap() += 1;
        self.state.lock().unwrap().checkpoint = Some((cp.seq, cp.root.clone()));
        Ok(())
    }
    async fn exit_started(&self) -> anyhow::Result<bool> {
        Ok(*self.exit.lock().unwrap())
    }
}

#[test]
fn strict_replay_rejects_missing_forged_domain_and_signer_signatures() {
    let key = EnclaveKey::generate();
    let mut engine = Engine::new();
    let command = Sequenced { seq: 0, ts: 0, cmd: Command::Tick };
    let events = engine.apply(&command).unwrap();
    let cp = checkpoint(&key, 0, engine.state_root());
    let entries = [
        LogEntry::Command(CommandEntry { seal: None, sequenced: command, events: Some(events) }),
        LogEntry::Checkpoint(cp.clone()),
    ];
    replay(&entries).unwrap();
    for mutate in [0, 1, 2, 3, 4] {
        let mut altered = cp.clone();
        match mutate {
            0 => altered.signature.clear(),
            1 => altered.signature = "0x".into(),
            2 => altered.signer = EnclaveKey::generate().address_hex(),
            3 => altered.domain.as_mut().unwrap().chain_id += 1,
            _ => altered.domain.as_mut().unwrap().vault = AccountId([8; 20]),
        }
        assert!(replay(&[entries[0].clone(), LogEntry::Checkpoint(altered)]).is_err(), "mutation {mutate}");
    }
    let mut altered = cp;
    tamper_signature(&mut altered.signature);
    assert!(signature::verify(&altered, &engine).is_err());
}

#[tokio::test]
async fn startup_refuses_truncation_wrong_root_and_unrecovered_signer() {
    let (poster, work, _key) = poster();
    poster.startup(None).await.unwrap();
    poster.chain.post(&work.checkpoint).await.unwrap();
    poster.startup(Some(&work.checkpoint)).await.unwrap();
    assert!(poster.startup(None).await.unwrap_err().to_string().contains("shorter"));
    let mut fork = work.checkpoint.clone();
    fork.root = format!("0x{}", "ff".repeat(32));
    assert!(poster.startup(Some(&fork)).await.is_err());
    poster.chain.state.lock().unwrap().signer = AccountId([9; 20]);
    assert!(poster.startup(Some(&work.checkpoint)).await.unwrap_err().to_string().contains("signer"));
}

#[tokio::test]
async fn mismatch_pauses_withdrawals_but_reconciliation_preserves_posting() {
    let (poster, work, _key) = poster();
    assert_eq!(poster.reconcile(&work).await.unwrap(), (Some(5), Health::Healthy));
    poster.chain.state.lock().unwrap().balance -= Fixed::from_int(1);
    assert_eq!(poster.reconcile(&work).await.unwrap(), (Some(5), Health::Diverged));
    poster.chain.state.lock().unwrap().balance += Fixed::from_int(1);
    assert_eq!(poster.reconcile(&work).await.unwrap(), (Some(5), Health::Healthy));
    assert_eq!(*poster.chain.posts.lock().unwrap(), 1, "retry must not repost an included checkpoint");
}

#[tokio::test(start_paused = true)]
async fn failed_post_is_retried_and_never_releases_a_signature_early() {
    let (poster, work, _key) = poster();
    *poster.chain.fails.lock().unwrap() = 1;
    let chain = poster.chain.clone();
    let (work_tx, work_rx) = mpsc::channel(4);
    let (tx, mut rx) = mpsc::channel(4);
    work_tx.send(work).await.unwrap();
    let task = tokio::spawn(poster.run(work_rx, tx));
    assert!(matches!(rx.recv().await, Some(Request::CheckpointStatus { posted: None, health: None, .. })));
    assert!(chain.state.lock().unwrap().checkpoint.is_none());
    assert!(matches!(
        rx.recv().await,
        Some(Request::CheckpointStatus { posted: Some(5), health: Some(Health::Healthy), .. })
    ));
    assert_eq!(*chain.posts.lock().unwrap(), 1);
    drop(work_tx);
    task.await.unwrap().unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_failed_post_once_the_vault_exit_has_started_stops_the_worker_with_a_critical_alert() {
    let (_guard, mut alerts) = crate::test_support::capture_alerts();
    let (poster, work, _key) = poster();
    *poster.chain.fails.lock().unwrap() = 1;
    *poster.chain.exit.lock().unwrap() = true;
    let (work_tx, work_rx) = mpsc::channel(4);
    let (tx, _rx) = mpsc::channel(4);
    work_tx.send(work).await.unwrap();
    let task = tokio::spawn(poster.run(work_rx, tx));
    let error =
        tokio::time::timeout(Duration::from_secs(60), task).await.expect("the worker stops").unwrap().unwrap_err();
    assert!(error.to_string().contains("exit"), "{error:#}");
    let alert = alerts.recv().await.unwrap();
    assert_eq!(alert.get("alert").map(String::as_str), Some("vault_exit_started"), "{alert:?}");
    assert_eq!(alert.get("level").map(String::as_str), Some("ERROR"), "{alert:?}");
}

#[test]
fn signed_parent_metadata_cannot_be_rewritten_by_the_relayer() {
    let key = EnclaveKey::generate();
    let mut cp = checkpoint(&key, 10, [3; 32]);
    cp.parent = Some(crate::log::CheckpointParent { has_checkpoint: true, seq: 5, root: [7; 32].into() });
    let domain = cp.domain.unwrap();
    cp.signature = key.sign_checkpoint_v2(domain.chain_id, &domain.vault.0, cp.seq, &[3; 32], cp.parent.unwrap());
    signature::verify_domain(&cp, domain).unwrap();
    for mutation in 0..4 {
        let mut altered = cp.clone();
        match mutation {
            0 => altered.parent.as_mut().unwrap().seq += 1,
            1 => altered.parent.as_mut().unwrap().root = [8; 32].into(),
            2 => altered.parent.as_mut().unwrap().has_checkpoint = false,
            _ => altered.parent = None,
        }
        assert!(signature::verify_domain(&altered, domain).is_err(), "mutation {mutation}");
    }
}

#[tokio::test]
async fn stale_parent_fails_closed_before_posting_a_divergent_higher_sequence() {
    let (poster, work, _key) = poster();
    poster.chain.state.lock().unwrap().checkpoint = Some((4, format!("0x{}", "ab".repeat(32))));
    assert!(poster.reconcile(&work).await.unwrap_err().is::<super::Conflict>());
    assert_eq!(*poster.chain.posts.lock().unwrap(), 0);
}

#[tokio::test]
async fn latest_inclusion_never_authorizes_release_and_waiting_finality_does_not_repost() {
    let (mut poster, work, _key) = poster();
    let finalized = Arc::new(Mutex::new(poster.chain.state.lock().unwrap().clone()));
    poster.chain.finalized = Some(finalized.clone());
    for _ in 0..3 {
        assert_eq!(poster.reconcile(&work).await.unwrap(), (None, Health::Unvalidated));
    }
    assert_eq!(*poster.chain.posts.lock().unwrap(), 1, "latest inclusion waits without a duplicate relay");
    poster.startup(None).await.unwrap(); // An unfinalized inclusion cannot become a rollback guarantee.
    poster.chain.state.lock().unwrap().checkpoint = None; // Honest shallow reorg orphans inclusion.
    assert_eq!(poster.reconcile(&work).await.unwrap(), (None, Health::Unvalidated));
    assert_eq!(*poster.chain.posts.lock().unwrap(), 2, "orphaned inclusion may be relayed again");
    *finalized.lock().unwrap() = poster.chain.state.lock().unwrap().clone();
    assert_eq!(poster.reconcile(&work).await.unwrap(), (Some(work.checkpoint.seq), Health::Healthy));
    assert!(poster.startup(None).await.is_err(), "finalized commitment is now the rollback boundary");
}

#[tokio::test]
async fn a_provisional_then_orphaned_approval_never_releases_a_signature() {
    let key = Arc::new(EnclaveKey::test_key());
    let dir = crate::test_support::scratch_dir("checkpoint-finality-release");
    let (work_tx, mut work_rx) = mpsc::channel(64);
    let (signed_tx, mut signed_rx) = mpsc::unbounded_channel();
    let cfg = crate::sequencer::SequencerConfig {
        checkpoints: Some(work_tx),
        ..crate::test_support::sequencer_config(&dir, tokio::sync::watch::channel(crate::log::Manifest::default()).0)
    };
    let mut seq =
        crate::sequencer::Sequencer::recover(cfg, key.clone(), tokio::sync::broadcast::channel(1024).0, signed_tx)
            .unwrap();
    // Before V22, an approval is signed once its checkpoint is posted and reconciled.
    seq.activate_protocol_at(engine_core::protocol::ProtocolVersion::V18).unwrap();
    seq.genesis(&crate::test_support::dev_config().genesis).unwrap();
    seq.trust(crate::test_support::trusted_now()).unwrap();
    while work_rx.try_recv().is_ok() {}
    let posted = seq.last_seq();
    let (tx, rx) = mpsc::channel(64);
    let task = tokio::spawn(seq.run(rx));
    tx.send(Request::CheckpointStatus { posted, health: Some(Health::Healthy), lifecycle: None }).await.unwrap();
    submit(&tx, crate::test_support::deposit(100, "0xfinality-deposit")).await;
    submit(&tx, crate::test_support::withdraw(40)).await;
    let mut work = None;
    while let Ok(queued) = work_rx.try_recv() {
        work = Some(queued);
    }
    let work = work.unwrap();
    assert_eq!(work.deposits, 1, "the vault credit is the checkpoint's one deposit");
    let parent = work.checkpoint.parent.unwrap();
    let state = VaultState {
        height: 0,
        signer: AccountId(key.address()),
        checkpoint: Some((parent.seq, parent.root.to_string())),
        balance: work.liabilities,
        deposits: work.deposits,
        deposited: work.liabilities,
        distributions_paid: Fixed::ZERO,
    };
    let finalized = Arc::new(Mutex::new(state.clone()));
    let chain = Chain {
        state: Arc::new(Mutex::new(state.clone())),
        finalized: Some(finalized.clone()),
        fails: Arc::new(Mutex::new(0)),
        posts: Arc::new(Mutex::new(0)),
        through: Arc::new(Mutex::new(vec![work.liabilities])),
        exit: Arc::new(Mutex::new(false)),
    };
    let poster = Poster {
        registry: None,
        chain,
        domain: work.checkpoint.domain.unwrap(),
        signer: AccountId(key.address()),
        retry: Duration::from_millis(1),
        pending_deposits_critical_after: Duration::from_secs(1),
        post_withheld: Default::default(),
    };
    for orphan in [false, true] {
        if orphan {
            *poster.chain.state.lock().unwrap() = state.clone();
        }
        let (posted, health) = poster.reconcile(&work).await.unwrap();
        assert_eq!((posted, health), (None, Health::Unvalidated));
        tx.send(Request::CheckpointStatus { posted, health: Some(health), lifecycle: None }).await.unwrap();
        sync(&tx).await;
        assert!(signed_rx.try_recv().is_err(), "unfinalized or orphaned inclusion cannot release an approval");
    }
    *finalized.lock().unwrap() = poster.chain.state.lock().unwrap().clone();
    let (posted, health) = poster.reconcile(&work).await.unwrap();
    tx.send(Request::CheckpointStatus { posted, health: Some(health), lifecycle: None }).await.unwrap();
    sync(&tx).await;
    let released = signed_rx.try_recv().expect("finalized healthy approval releases");
    assert_eq!(released.approved_seq, Some(work.checkpoint.seq));
    assert_eq!(released.amount, Fixed::from_int(40));
    drop(tx);
    task.await.unwrap().unwrap();
}

async fn submit(tx: &mpsc::Sender<Request>, cmd: Command) {
    let (reply, rx) = tokio::sync::oneshot::channel();
    tx.send(Request::Submit { cmd, reply: Some(reply) }).await.unwrap();
    rx.await.unwrap();
}

async fn sync(tx: &mpsc::Sender<Request>) {
    let (reply, rx) = tokio::sync::oneshot::channel();
    tx.send(Request::Sync(reply)).await.unwrap();
    rx.await.unwrap();
}

pub(super) fn checkpoint(key: &EnclaveKey, seq: u64, root: [u8; 32]) -> Checkpoint {
    let domain = CheckpointDomain { chain_id: 31337, vault: AccountId([7; 20]) };
    Checkpoint {
        parent: Some(Default::default()),
        seq,
        root: format!("0x{}", hex::encode(root)),
        signer: key.address_hex(),
        signature: key.sign_checkpoint_v2(domain.chain_id, &domain.vault.0, seq, &root, Default::default()),
        domain: Some(domain),
        deferred_withdrawals: true,
    }
}

pub(super) fn poster() -> (Poster<Chain>, Work, EnclaveKey) {
    let key = EnclaveKey::generate();
    let checkpoint = checkpoint(&key, 5, [3; 32]);
    let work = Work {
        checkpoint: checkpoint.clone(),
        liabilities: Fixed::from_int(100),
        deposits: 1,
        authorized: Fixed::ZERO,
        coverage: crate::checkpoints::DepositCoverage::Historical,
        distributed: engine_core::UsdcUnits(0),
    };
    let chain = Chain {
        state: Arc::new(Mutex::new(VaultState {
            height: 0,
            signer: AccountId(key.address()),
            checkpoint: None,
            balance: work.liabilities,
            deposits: work.deposits,
            deposited: work.liabilities,
            distributions_paid: Fixed::ZERO,
        })),
        finalized: None,
        fails: Arc::new(Mutex::new(0)),
        posts: Arc::new(Mutex::new(0)),
        through: Arc::new(Mutex::new(vec![work.liabilities])),
        exit: Arc::new(Mutex::new(false)),
    };
    (
        Poster {
            registry: None,
            chain,
            domain: checkpoint.domain.unwrap(),
            signer: AccountId(key.address()),
            retry: Duration::from_millis(10),
            pending_deposits_critical_after: Duration::from_secs(1),
            post_withheld: Default::default(),
        },
        work,
        key,
    )
}

#[tokio::test]
async fn public_relayer_advanced_prefix_reconciles_against_its_own_accounting() {
    let (poster, first, _key) = poster();
    let key = EnclaveKey::test_key();
    // The fixture poster uses another key; make a self-consistent signed two-checkpoint chain.
    let mut first = first;
    first.checkpoint.signer = key.address_hex();
    let domain = first.checkpoint.domain.unwrap();
    let root = first.checkpoint.root.parse::<engine_core::eip712::B256>().unwrap();
    first.checkpoint.signature = key.sign_checkpoint_v2(
        domain.chain_id,
        &domain.vault.0,
        first.checkpoint.seq,
        &root.0,
        first.checkpoint.parent.unwrap(),
    );
    let mut second = first.clone();
    second.checkpoint.seq += 1;
    second.checkpoint.parent =
        Some(crate::log::CheckpointParent { has_checkpoint: true, seq: first.checkpoint.seq, root });
    second.liabilities += Fixed::from_int(10);
    second.deposits += 1;
    second.checkpoint.signature = key.sign_checkpoint_v2(
        domain.chain_id,
        &domain.vault.0,
        second.checkpoint.seq,
        &root.0,
        second.checkpoint.parent.unwrap(),
    );
    let mut poster = poster;
    poster.signer = AccountId(key.address());
    {
        let mut state = poster.chain.state.lock().unwrap();
        state.signer = poster.signer;
        state.checkpoint = Some((second.checkpoint.seq, second.checkpoint.root.clone()));
    }
    poster.chain.deposit(Fixed::from_int(10));
    assert_eq!(
        poster.reconcile_known(&first, &[&first, &second]).await.unwrap(),
        (Some(second.checkpoint.seq), Health::Healthy)
    );
    assert_eq!(*poster.chain.posts.lock().unwrap(), 0);
    // A backend which still sees the predecessor is retried, without treating it as a fork.
    poster.chain.state.lock().unwrap().checkpoint = None;
    let error = poster.reconcile(&second).await.unwrap_err();
    assert!(!error.is::<super::Conflict>());
    assert_eq!(*poster.chain.posts.lock().unwrap(), 0);
}

#[tokio::test]
async fn publicly_relayed_work_beyond_the_full_worker_queue_is_recognized() {
    let (mut poster, first, _key) = poster();
    let key = EnclaveKey::test_key();
    let domain = first.checkpoint.domain.unwrap();
    let root = first.checkpoint.root.parse::<engine_core::eip712::B256>().unwrap();
    poster.signer = AccountId(key.address());
    let registry: super::Registry = Default::default();
    poster.registry = Some(registry.clone());
    let mut chain = Vec::new();
    let mut parent = first.checkpoint.parent.unwrap();
    for offset in 0..65 {
        let mut work = first.clone();
        work.checkpoint.seq += offset;
        work.checkpoint.signer = key.address_hex();
        work.checkpoint.parent = Some(parent);
        work.checkpoint.signature =
            key.sign_checkpoint_v2(domain.chain_id, &domain.vault.0, work.checkpoint.seq, &root.0, parent);
        work.liabilities += Fixed::from_int(offset as i64);
        parent = crate::log::CheckpointParent { has_checkpoint: true, seq: work.checkpoint.seq, root };
        registry.lock().unwrap().insert(work.checkpoint.seq, work.clone());
        chain.push(work);
    }
    let advanced = chain.pop().unwrap();
    {
        let mut state = poster.chain.state.lock().unwrap();
        state.signer = poster.signer;
        state.checkpoint = Some((advanced.checkpoint.seq, advanced.checkpoint.root.clone()));
        // The fixture's later credits carry no vault deposit; the vault holds them in its first.
        state.balance = advanced.liabilities;
        state.deposited = advanced.liabilities;
        *poster.chain.through.lock().unwrap() = vec![advanced.liabilities];
    }
    let (sink, input) = mpsc::channel(64);
    sink.send(advanced.clone()).await.unwrap();
    let (tx, mut status) = mpsc::channel(8);
    let task = tokio::spawn(poster.run_with_recovery(input, tx, chain));
    assert!(
        matches!(status.recv().await, Some(Request::CheckpointStatus { posted: Some(seq), health: Some(Health::Healthy), .. }) if seq == advanced.checkpoint.seq)
    );
    assert_eq!(registry.lock().unwrap().len(), 1, "finalized covered prefix retires from the registry");
    drop(sink);
    task.await.unwrap().unwrap();
}
