//! Wires the liveness rule (#595) into an operator's node: with rotation on, `[log_heads]` and EVM
//! custody, the sequencer admits commands through the rule's gate, closed until its first read, the
//! poster posts v2 heads where the rule lets it, the API signs nothing while the fence stops the
//! enclave and carries the signed heads on every signed stream connection, and the operator vouches
//! for candidates' standby keys.

use std::sync::Arc;
use std::time::Duration;

use alloy::primitives::{Address, B256};
use anyhow::Context;
use engine_core::types::AccountId;
use futures_util::future::BoxFuture;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;
use tracing::{error, info, warn};

use crate::api::Signing;
use crate::checkpoints::CheckpointChain;
use crate::config::{CustodyProvider, NodeConfig};
use crate::custody::StandbyReader;
use crate::liveness::chain::LivenessDomain;
use crate::liveness::link::Tcp;
use crate::liveness::operator::{Operator, Signing as Signable};
use crate::liveness::{Gate, Released};
use crate::log_heads::SignedHead;
use crate::log_heads::setup::{Liveness, Setup};
use crate::node::shutdown::TaskResult;
use crate::sequencer::Request;
use crate::standby_key::chain::{StandbyChain, StandbyDomain};
use crate::standby_key::vouch::Service;
use crate::tee::sealing::KeyStore;
use crate::trusted_time::TrustedClock;

/// How long the rule waits before reading the vault's registry and schedule again once a read failed.
const DOMAIN_RETRY: Duration = Duration::from_secs(5);

/// What the operator's rule needs, and what it shares with the API.
pub(super) struct Wiring {
    gate: Gate,
    signing: watch::Sender<Signable>,
    signed: watch::Receiver<Option<SignedHead>>,
    vault_signer: watch::Sender<Option<crate::liveness::VaultSigner>>,
}

/// What the API keeps of the rule: its gate, the signed heads and the vault's signer.
pub(super) struct Shared {
    gate: Gate,
    signed: watch::Receiver<Option<SignedHead>>,
    vault_signer: watch::Receiver<Option<crate::liveness::VaultSigner>>,
}

impl Wiring {
    /// What the API keeps.
    pub(super) fn shares(&self) -> Shared {
        Shared { gate: self.gate.clone(), signed: self.signed.clone(), vault_signer: self.vault_signer.subscribe() }
    }
}

/// Whether the rule runs on this node: rotation on, with `[log_heads]` and EVM custody.
pub(super) fn applies(cfg: &NodeConfig) -> bool {
    cfg.operator.rotation_enabled && cfg.log_heads.is_some() && cfg.custody.provider == CustodyProvider::Evm
}

/// Closes the gate `setup` hands the sequencer, until the rule's first read, and gives the poster its
/// v2 heartbeat's inputs, for keys sealed in `store`.
pub(super) fn prepare(cfg: &NodeConfig, setup: Option<&mut Setup>, store: Option<&KeyStore>) -> Option<Wiring> {
    let setup = setup.filter(|_| applies(cfg))?;
    let gate = Gate::closed();
    setup.gate = gate.clone();
    let (signing, signing_rx) = watch::channel(Signable::default());
    let (signed_tx, signed) = watch::channel(None);
    let keys = store.map(|store| (B256::from(store.release().id()), store.generation()));
    setup.liveness = Some(Liveness { keys, signing: signing_rx, signed: signed_tx });
    Some(Wiring { gate, signing, signed, vault_signer: watch::channel(None).0 })
}

/// `signing`, stopped while the rule fences and carrying its signed heads, vouching with `vouching`.
pub(super) fn signing(
    signing: Signing,
    shared: Option<&Shared>,
    vouching: Option<Arc<Service<StandbyReader>>>,
) -> Signing {
    let signing = signing.with_vouching(vouching);
    match shared {
        Some(shared) => signing.with_liveness(shared.gate.clone(), shared.signed.clone(), shared.vault_signer.clone()),
        None => signing,
    }
}

/// How the operator's node started: by the promotion that made `key` the vault's signer, if one, and
/// the first seq it sequences.
pub(super) struct Started {
    pub promoted: Option<crate::standby_key::chain::Promotion>,
    pub from_seq: u64,
    pub key: AccountId,
}

/// Starts the operator's rule, with `wiring` from `prepare` and the sequencer's `released` heads, and
/// a promoted member's promotion watch; returns what the API keeps of it.
/// Error cases:
/// - a custody quorum cannot be built
pub(super) fn start(
    cfg: &NodeConfig,
    wiring: Option<Wiring>,
    released: Option<watch::Receiver<Released>>,
    started: Started,
    sequencer: &mpsc::Sender<Request>,
    trusted: TrustedClock,
    tasks: &mut JoinSet<TaskResult>,
) -> anyhow::Result<Option<Shared>> {
    let (Some(wiring), Some(released)) = (wiring, released) else { return Ok(None) };
    let shared = wiring.shares();
    if let Some(promotion) = started.promoted {
        watch_promotion(cfg, &wiring, promotion, started.key, started.from_seq, tasks)?;
    }
    spawn(cfg, wiring, started.key, released, sequencer, trusted, tasks)?;
    Ok(Some(shared))
}

/// The recovered checkpoint work a start posts: a promoted member retires what the outgoing key signed
/// that the vault never accepted, since its own next checkpoint extends the vault's latest, which the
/// promotion fixed.
pub(super) fn retire(
    mut recovery: Vec<crate::checkpoints::Work>,
    promoted: Option<crate::standby_key::chain::Promotion>,
    key: &crate::tee::EnclaveKey,
) -> Vec<crate::checkpoints::Work> {
    if promoted.is_some() {
        recovery
            .retain(|work| work.checkpoint.signer.parse::<AccountId>().is_ok_and(|signer| signer.0 == key.address()));
    }
    recovery
}

/// The checkpoint poster's task: a promoted member's, whose signer `final_signer` names, waits for
/// the promotion's finality before it posts, so its checkpoints and payouts do too.
pub(super) async fn checkpoints(
    setup: crate::checkpoints::setup::Setup,
    sequencer: mpsc::Sender<Request>,
    recovery: Vec<crate::checkpoints::Work>,
    final_signer: Option<AccountId>,
) -> TaskResult {
    if let Some(signer) = final_signer {
        crate::checkpoints::setup::await_signer_final(&setup.poster.chain, signer, setup.poster.retry).await;
        info!("the promotion is final: checkpoints resume");
    }
    ("checkpoints", setup.poster.run_with_approvals(setup.rx, sequencer, recovery, setup.approvals).await)
}

/// Spawns the operator's rule for the enclave at `key`, reading the sequencer's `released` heads.
/// Error cases:
/// - the custody quorum cannot be built
fn spawn(
    cfg: &NodeConfig,
    wiring: Wiring,
    key: AccountId,
    released: watch::Receiver<Released>,
    sequencer: &mpsc::Sender<Request>,
    trusted: TrustedClock,
    tasks: &mut JoinSet<TaskResult>,
) -> anyhow::Result<()> {
    let log_heads = cfg.log_heads.as_ref().context("the liveness rule reads [log_heads]")?.contract;
    let chain = StandbyReader::connect_named(&cfg.custody, log_heads, "liveness")?;
    let (chain_id, vault) = (cfg.custody.chain_id, Address::from(cfg.custody.vault.parse::<AccountId>()?.0));
    let (tick, sequencer) = (cfg.node.tick, sequencer.downgrade());
    tasks.spawn(async move {
        let domain = loop {
            let read: BoxFuture<'_, anyhow::Result<StandbyDomain>> =
                Box::pin(StandbyDomain::read(&chain, chain_id, vault));
            match read.await {
                Ok(standby) => break LivenessDomain { standby, log_heads },
                Err(e) => {
                    warn!(alert = "liveness_read_failed", "reading the vault's registry and schedule failed: {e:#}")
                }
            }
            tokio::time::sleep(DOMAIN_RETRY).await;
        };
        let operator = Operator {
            chain,
            asking: Tcp,
            domain,
            key,
            gate: wiring.gate,
            trusted,
            released,
            signing: wiring.signing,
            tick,
            sequencer,
            vault_signer: wiring.vault_signer,
        };
        ("liveness", operator.run().await)
    });
    Ok(())
}

/// The operator's vouching for standby keys with the enclave key, for a node with sealed keys and the
/// rule's chain access.
/// Error cases:
/// - the custody quorum cannot be built
pub(super) fn vouching(
    cfg: &NodeConfig,
    store: Option<&KeyStore>,
    key: &Arc<crate::tee::EnclaveKey>,
    trusted: TrustedClock,
) -> anyhow::Result<Option<Arc<Service<StandbyReader>>>> {
    let (Some(store), Some(log_heads)) = (store, cfg.log_heads.as_ref()) else { return Ok(None) };
    if cfg.custody.provider != CustodyProvider::Evm {
        return Ok(None);
    }
    let chain = StandbyReader::connect_named(&cfg.custody, log_heads.contract, "vouching")?;
    let vault = Address::from(cfg.custody.vault.parse::<AccountId>()?.0);
    let service =
        Service::new((cfg.tee.provider, store.platform()), trusted, chain, (cfg.custody.chain_id, vault), key.clone());
    Ok(Some(Arc::new(service)))
}

/// Watches the promotion that made this enclave's key the vault's signer until it is final: if the
/// latest agreed block shows an earlier signer epoch, or the promotion's epoch under another key, the
/// promotion was reorged away. The node then raises Critical `promotion_reorged`, fences, records the
/// history it sequenced from `from_seq` as quarantined beside the log (`quarantine.json`), and stops: a
/// restart resumes that history only while the same promotion stands, since its sealed key opens only
/// in the promotion's generation.
/// Error cases:
/// - the custody quorum cannot be built
fn watch_promotion(
    cfg: &NodeConfig,
    wiring: &Wiring,
    promotion: crate::standby_key::chain::Promotion,
    key: AccountId,
    from_seq: u64,
    tasks: &mut JoinSet<TaskResult>,
) -> anyhow::Result<()> {
    let log_heads = cfg.log_heads.as_ref().context("the liveness rule reads [log_heads]")?.contract;
    let chain = StandbyReader::connect_named(&cfg.custody, log_heads, "promotion")?;
    let vault = crate::handover::vault_reader(cfg)?.context("a promoted member reads the vault")?;
    let (chain_id, address) = (cfg.custody.chain_id, Address::from(cfg.custody.vault.parse::<AccountId>()?.0));
    let (gate, log_dir) = (wiring.gate.clone(), cfg.node.log_dir.clone());
    tasks.spawn(async move {
        let outcome = async {
            let domain: BoxFuture<'_, anyhow::Result<StandbyDomain>> = Box::pin(StandbyDomain::read(&chain, chain_id, address));
            let domain = LivenessDomain { standby: domain.await?, log_heads };
            loop {
                tokio::time::sleep(DOMAIN_RETRY).await;
                let read: BoxFuture<'_, anyhow::Result<crate::liveness::chain::View>> =
                    Box::pin(async { crate::liveness::chain::view(&chain.latest().await?, &domain).await });
                let Ok(view) = read.await else { continue };
                let reorged = view.epoch < promotion.signer_epoch
                    || (view.epoch == promotion.signer_epoch && view.signer != key && view.signer != AccountId([0; 20]));
                if reorged {
                    gate.fence(true);
                    let through = view.head.seq.max(from_seq);
                    let record = serde_json::json!({
                        "turn": promotion.turn,
                        "position": promotion.position,
                        "signer_epoch": promotion.signer_epoch,
                        "from_seq": from_seq,
                    });
                    let written = crate::tee::sealing::write_replacing(&log_dir.join("quarantine.json"), record.to_string().as_bytes());
                    error!(
                        alert = "promotion_reorged",
                        turn = promotion.turn,
                        position = promotion.position,
                        "the promotion that made this key the vault's signer is gone from the chain: signing stops, and \
                         the history sequenced from seq {from_seq} (through at least {through}) is quarantined ({written:?})"
                    );
                    anyhow::bail!("the promotion of turn {} was reorged away", promotion.turn);
                }
                let finalized = vault.observe(crate::checkpoints::View::Finalized, &[]).await;
                if finalized.is_ok_and(|at| at.state.signer == key) {
                    info!(turn = promotion.turn, "the promotion is final");
                    return std::future::pending().await;
                }
            }
        };
        ("promotion", outcome.await)
    });
    Ok(())
}
