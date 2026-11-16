//! Wires the liveness rule (#595) into an operator's node: with rotation on, `[log_heads]` and EVM
//! custody, the sequencer admits commands through the rule's gate, closed until its first read, the
//! poster posts v2 heads where the rule lets it, the API signs nothing while the fence stops the
//! enclave and carries the signed heads on every signed stream connection, and the operator vouches
//! for candidates' standby keys.

use std::sync::Arc;

use alloy::primitives::{Address, B256};
use anyhow::Context;
use engine_core::types::AccountId;
use futures_util::future::BoxFuture;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;
use tracing::{info, warn};

use crate::api::Signing;
use crate::config::{CustodyProvider, NodeConfig};
use crate::custody::StandbyReader;
use crate::liveness::link::Tcp;
use crate::liveness::operator::{Operator, Signing as Signable};
use crate::liveness::{Gate, Released};
use crate::log_heads::SignedHead;
use crate::log_heads::setup::{Liveness, Setup};
use crate::log_heads::v2::HeldKeys;
use crate::node::member::promoted::watch::{Watched, watch};
use crate::node::shutdown::TaskResult;
use crate::sequencer::Request;
use crate::standby_key::chain::StandbyDomain;
use crate::standby_key::vouch::Service;
use crate::tee::sealing::KeyStore;
use crate::trusted_time::TrustedClock;

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

/// Whether the rule runs on this node: with `[log_heads]` and EVM custody, whatever
/// `operator.rotation_enabled` says, since the chain can turn rotation on while the node runs and the
/// rule reads whether a turn names the operator itself.
pub(super) fn applies(cfg: &NodeConfig) -> bool {
    cfg.log_heads.is_some() && cfg.custody.provider == CustodyProvider::Evm
}

/// Closes the gate `setup` hands the sequencer, until the rule's first read, and gives the poster its
/// v2 heartbeat's inputs, for keys sealed in `store`, whose generation each head reads afresh.
pub(super) fn prepare(cfg: &NodeConfig, setup: Option<&mut Setup>, store: Option<&Arc<KeyStore>>) -> Option<Wiring> {
    let setup = setup.filter(|_| applies(cfg))?;
    let gate = Gate::closed();
    setup.gate = gate.clone();
    let (signing, signing_rx) = watch::channel(Signable::default());
    let (signed_tx, signed) = watch::channel(None);
    let keys = store.cloned().map(|store| {
        let read: HeldKeys = Arc::new(move || (B256::from(store.release().id()), store.generation()));
        read
    });
    setup.liveness = Some(Liveness { keys, signing: signing_rx, signed: signed_tx });
    Some(Wiring { gate, signing, signed, vault_signer: watch::channel(None).0 })
}

/// What the API and the node's supervision keep of the rule: what `start` shares, and the vouching
/// for candidates' standby keys, which a node with sealed keys and the rule's chain access answers.
pub(super) struct Serving {
    shared: Option<Shared>,
    vouching: Option<Arc<Service<StandbyReader>>>,
}

impl Serving {
    /// `signing`, stopped while the rule fences and carrying its signed heads, vouching as it can.
    pub(super) fn signing(self, signing: Signing) -> Signing {
        let signing = signing.with_vouching(self.vouching);
        match self.shared {
            Some(shared) => signing.with_liveness(shared.gate, shared.signed, shared.vault_signer),
            None => signing,
        }
    }

    /// Whether the vault named another key than this enclave's, while the rule runs.
    pub(super) fn replacement(&self) -> Option<watch::Receiver<bool>> {
        self.shared.as_ref().map(|shared| shared.gate.replacement())
    }
}

/// How the operator's node started: by the promotion that made `key` the vault's signer, if one, and
/// the first seq it sequences, with keys sealed in `store`.
pub(super) struct Started<'a> {
    pub promoted: Option<crate::standby_key::chain::Promotion>,
    pub from_seq: u64,
    pub key: &'a Arc<crate::tee::EnclaveKey>,
    pub store: Option<&'a KeyStore>,
    /// The start's key choices, which a promoted operator's replacement standby key seals under.
    pub keys: &'a crate::node::KeyStart,
}

/// Starts the operator's rule, with `wiring` from `prepare` and the sequencer's `released` heads, a
/// promoted member's promotion watch, and a candidate's standby key renewal; returns what the API and
/// the node's supervision keep of it.
/// Error cases:
/// - a custody quorum or the replacement key's attestor cannot be built
pub(super) fn start(
    cfg: &NodeConfig,
    (wiring, released): (Option<Wiring>, Option<watch::Receiver<Released>>),
    started: Started<'_>,
    sequencer: &mpsc::Sender<Request>,
    trusted: TrustedClock,
    tasks: &mut JoinSet<TaskResult>,
) -> anyhow::Result<Serving> {
    let vouching = vouching(cfg, started.store, started.key, trusted.clone())?;
    let (Some(wiring), Some(released)) = (wiring, released) else { return Ok(Serving { shared: None, vouching }) };
    let shared = wiring.shares();
    let key = AccountId(started.key.address());
    if let Some(promotion) = started.promoted {
        watch_promotion(cfg, &wiring, promotion, key, started.from_seq, tasks)?;
    }
    // A serving candidate keeps a promotable standby key for its release: a promotion consumed the
    // last one, and a release move, as an upgrade by `--predecessor` makes, voids it.
    if crate::node::member::standby::applies(cfg) {
        let renewal = crate::node::member::standby::renewal::Renewal::new(cfg, started.keys, vouching.clone())?;
        let trusted = trusted.clone();
        tasks.spawn(async move { ("standby renewal", renewal.run(trusted).await) });
    }
    let floor = started.promoted.map_or(0, |promotion| promotion.block);
    let platform = started.store.map(|store| (cfg.tee.provider, store.platform()));
    spawn(cfg, wiring, (key, floor, platform), released, sequencer, trusted, tasks)?;
    Ok(Serving { shared: Some(shared), vouching })
}

/// Spawns the checkpoint poster `setup` names, if any (see `checkpoints`).
pub(super) fn spawn_checkpoints(
    tasks: &mut JoinSet<TaskResult>,
    setup: Option<crate::checkpoints::setup::Setup>,
    sequencer: &mpsc::Sender<Request>,
    recovery: Vec<crate::checkpoints::Work>,
    final_signer: Option<AccountId>,
) {
    if let Some(setup) = setup {
        tasks.spawn(checkpoints(setup, sequencer.clone(), recovery, final_signer));
    }
}

/// The checkpoint poster's task: a promoted member's, whose signer `final_signer` names, waits for
/// the promotion's finality before it posts, so its checkpoints and payouts do too.
async fn checkpoints(
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

/// The watch over the release a node's keys are sealed in (`keys::watch_release`): a promoted node's,
/// whose signer `promoted` names, starts once its promotion is final, since the finalised schedule
/// names the outgoing holding until then, which the watch would page as a supersession.
/// Error cases:
/// - none; it runs until the node stops
pub(super) async fn watch_release(
    vault: impl crate::checkpoints::CheckpointChain,
    (store, promoted): (Arc<crate::tee::sealing::KeyStore>, Option<AccountId>),
    every: std::time::Duration,
    trusted: crate::trusted_time::TrustedClock,
    assignments: Option<Arc<dyn crate::handover::assigned::Assignments>>,
) -> anyhow::Result<()> {
    if let Some(signer) = promoted {
        crate::checkpoints::setup::await_signer_final(&vault, signer, every).await;
    }
    super::keys::watch_release(vault, store, every, trusted, assignments).await
}

/// Spawns the operator's rule for the enclave at `key` on `platform`, reading the sequencer's `released`
/// heads, and no block below `floor`, the one a promoted start read its promotion at.
/// Error cases:
/// - the custody quorum cannot be built
fn spawn(
    cfg: &NodeConfig,
    wiring: Wiring,
    (key, floor, platform): (AccountId, u64, Option<(crate::config::TeeProvider, crate::tee::sealing::Platform)>),
    released: watch::Receiver<Released>,
    sequencer: &mpsc::Sender<Request>,
    trusted: TrustedClock,
    tasks: &mut JoinSet<TaskResult>,
) -> anyhow::Result<()> {
    let log_heads = cfg.log_heads.as_ref().context("the liveness rule reads [log_heads]")?.contract;
    let chain = StandbyReader::connect_named(&cfg.custody, log_heads, "liveness")?;
    let (chain_id, vault) = (cfg.custody.chain_id, Address::from(cfg.custody.vault.parse::<AccountId>()?.0));
    let (tick, sequencer, admin) = (cfg.node.tick, sequencer.downgrade(), cfg.operator()?.address);
    tasks.spawn(async move {
        let operator = Operator {
            chain,
            asking: Tcp,
            chain_id,
            vault,
            log_heads,
            key,
            admin,
            gate: wiring.gate,
            trusted,
            released,
            signing: wiring.signing,
            tick,
            sequencer,
            vault_signer: wiring.vault_signer,
            poll: crate::liveness::operator::POLL,
            floor,
            platform,
            kept: Default::default(),
        };
        ("liveness", operator.run().await)
    });
    Ok(())
}

/// The operator's vouching for standby keys with the enclave key, for a node with sealed keys and the
/// rule's chain access.
/// Error cases:
/// - the custody quorum cannot be built
fn vouching(
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

/// Spawns the watch over the promotion that made this enclave's key the vault's signer
/// (`member::promoted::watch`). The next start after it stops leaves the history sequenced from
/// `from_seq` untouched until a final block settles the promotion: it resumes the same promotion, and
/// quarantines a removed one (`member::promoted`).
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
    let (chain_id, address) = (cfg.custody.chain_id, Address::from(cfg.custody.vault.parse::<AccountId>()?.0));
    let gate = wiring.gate.clone();
    let sealing = cfg.tee.sealing.as_ref().map(|sealing| sealing.dir.clone());
    tasks.spawn(async move {
        let outcome = async {
            let domain: BoxFuture<'_, anyhow::Result<StandbyDomain>> =
                Box::pin(StandbyDomain::read(&chain, chain_id, address));
            let domain = domain.await?;
            let watching = Watched { domain, key, promotion, from_seq };
            let watch: BoxFuture<'_, anyhow::Result<()>> =
                Box::pin(watch(&chain, &watching, &gate, sealing.as_deref()));
            watch.await
        };
        ("promotion", outcome.await)
    });
    Ok(())
}

/// Deletes the sealed copy of the keys a confirmed handover gave away (#595): matching prefixes show a
/// log complete, not that one enclave alone holds its keys, so a predecessor keeps no copy a host could
/// reopen beside its successor. A failure is logged; the generation it held is the successor's either
/// way.
pub(super) fn forget(cfg: &NodeConfig, store: Option<&KeyStore>) {
    let (Some(store), Some(sealing)) = (store, cfg.tee.sealing.as_ref()) else { return };
    let path = crate::tee::sealing::file_for(&sealing.dir, &store.release(), store.generation());
    let removed = std::fs::remove_file(&path)
        .or_else(|e| if e.kind() == std::io::ErrorKind::NotFound { Ok(()) } else { Err(e) })
        .and_then(|()| std::fs::File::open(&sealing.dir)?.sync_all());
    match removed {
        Ok(()) => info!(generation = store.generation(), "deleted the sealed keys the handover gave away"),
        Err(e) => warn!(path = %path.display(), "deleting the sealed keys the handover gave away failed: {e}"),
    }
}

#[cfg(test)]
mod tests;
