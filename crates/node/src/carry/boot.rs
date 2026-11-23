//! A start's checks of a carried genesis. Before a fresh log's genesis carries state, the node holds under Warning
//! `carry_waiting` until the chain shows the move done: the source vault's signer retired, with a checkpoint
//! accepted; the registry following this node's vault with rotation's switch off and the config's operator key in
//! effect; and MONKEY's minter following this node's vault; and the carry file in place. The file must then name
//! the source vault's last accepted checkpoint, and the genesis it carries must apply in full on a scratch engine
//! at the vault's emission budget, or the start stops with Critical `carry_refused` before anything is written.
//! Every start also refuses a log whose carried source is not the config's.

use std::future::Future;
use std::path::Path;
use std::time::Duration;

use alloy::primitives::{Address, B256};
use anyhow::{Context, anyhow, bail};
use engine_core::carry::{CarryPart, CarrySource};
use engine_core::emissions::TokenUnits;
use engine_core::types::AccountId;
use tracing::{error, info, warn};

use crate::carry::chain::{self, MoveContracts, MoveState};
use crate::carry::{CarryFile, read};
use crate::checkpoints::emission_budget::check_recorded;
use crate::config::NodeConfig;
use crate::custody::ContractQuorum;
use crate::sequencer::genesis::check_genesis_commands;
use crate::sequencer::{Sequencer, carried_genesis_commands};

/// How long a held start waits before it reads the chain and the file again.
const RECHECK: Duration = Duration::from_secs(30);

/// What a start compares the chain with.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Expected {
    pub chain_id: u64,
    /// This node's vault.
    pub vault: Address,
    /// The source log's vault.
    pub source: Address,
    /// The config's operator key.
    pub operator: Address,
}

/// Each condition of a done move that `state` does not meet, as a sentence; none once the move is done.
pub(crate) fn unmet(state: &MoveState, expected: &Expected) -> Vec<String> {
    let mut unmet = Vec::new();
    if state.source_signer != Address::ZERO {
        unmet.push(format!("the source vault's signer {} is not retired", state.source_signer));
    }
    if state.source_checkpoint.is_none() {
        unmet.push("the source vault holds no checkpoint".to_owned());
    }
    if state.registry_vault != expected.vault {
        unmet.push(format!("the registry follows vault {}, not {}", state.registry_vault, expected.vault));
    }
    if state.rotation_switch {
        unmet.push("the registry's rotation switch is on".to_owned());
    }
    if state.admin_key != expected.operator {
        unmet.push(format!(
            "the registry's operator in effect signs with {}, not {}",
            state.admin_key, expected.operator
        ));
    }
    if let Some(minter) = state.minter_vault.filter(|minter| *minter != expected.vault) {
        unmet.push(format!("MONKEY's minter follows vault {minter}, not {}", expected.vault));
    }
    unmet
}

/// Checks that `file` carries from the source vault's last accepted checkpoint `at`, on the custody chain.
/// Error cases:
/// - the file names another chain, vault, seq or root
pub(crate) fn check_source(file: &CarryFile, (seq, root): (u64, B256), expected: &Expected) -> anyhow::Result<()> {
    let accepted =
        CarrySource { chain_id: expected.chain_id, vault: AccountId(expected.source.into_array()), seq, root };
    if file.source != accepted {
        bail!(
            "the carry file carries from {:?}, but the source vault's last accepted checkpoint is {accepted:?}",
            file.source
        );
    }
    Ok(())
}

/// Holds until the chain shows the move done and the carry file at `path` reads, raising Warning `carry_waiting`
/// with what is missing, rechecking every `RECHECK`; returns the state and the file.
/// Error cases:
/// - the file is there but does not read as a carry file
pub(crate) async fn hold<F, R>(
    read_chain: R,
    expected: &Expected,
    path: &Path,
) -> anyhow::Result<(MoveState, CarryFile)>
where
    R: Fn() -> F,
    F: Future<Output = anyhow::Result<MoveState>>,
{
    let mut waited = false;
    loop {
        let waiting = match read_chain().await {
            Ok(state) => {
                let unmet = unmet(&state, expected);
                if unmet.is_empty() && path.exists() {
                    let (file, sha256) = read(path)?;
                    if waited {
                        info!(alert_resolved = "carry_waiting", "the move is done; carrying from {}", path.display());
                    }
                    info!(block = state.block, sha256 = %B256::from(sha256), "carry file read");
                    return Ok((state, file));
                }
                let missing = (!path.exists()).then(|| format!("the carry file {} is not there", path.display()));
                unmet.into_iter().chain(missing).collect::<Vec<_>>().join("; ")
            }
            Err(e) => format!("the move's contracts cannot be read: {e:#}"),
        };
        warn!(alert = "carry_waiting", "the carried genesis waits: {waiting}");
        waited = true;
        tokio::time::sleep(RECHECK).await;
    }
}

/// The parts a fresh log's genesis carries: none without `[genesis.carry]`. With it, holds as `hold` does, then
/// checks the file names the source vault's last accepted checkpoint, and that the genesis it carries applies in
/// full on a scratch engine with an emission budget matching `emission_budget`, the vault's.
/// Error cases:
/// - the custody quorum cannot be built, or a configured address does not parse
/// - the file does not read, names another source, or carries a genesis that does not apply in full, or another
///   emission budget (alert `carry_refused`)
pub(crate) async fn carried_parts(
    cfg: &NodeConfig,
    emission_budget: Option<TokenUnits>,
) -> anyhow::Result<Vec<CarryPart>> {
    let Some(carry) = &cfg.genesis.carry else { return Ok(Vec::new()) };
    let refused = |e: &anyhow::Error| error!(alert = "carry_refused", "refusing to carry state into genesis: {e:#}");
    let evm = cfg.custody.evm.as_ref().context("[genesis.carry] needs [custody.evm]")?;
    let registry = evm.staking.as_ref().context("[genesis.carry] needs [custody.evm.staking]")?.registry.contract;
    let expected = Expected {
        chain_id: cfg.custody.chain_id,
        vault: cfg.custody.vault.parse().context("custody.vault")?,
        source: Address::from(carry.vault.0),
        operator: Address::from(cfg.operator()?.address.0),
    };
    let contracts = MoveContracts { source: expected.source, registry, minter: None };
    let quorum = ContractQuorum::connect(&cfg.custody, "carry")?;
    let read_chain = || chain::read(quorum.quorum(), contracts, (expected.chain_id, evm.usdc));
    let (state, file) = hold(read_chain, &expected, &carry.file).await.inspect_err(refused)?;
    let at = state.source_checkpoint.ok_or_else(|| anyhow!("held until the source vault holds a checkpoint"))?;
    check_source(&file, at, &expected).inspect_err(refused)?;
    let cmds = carried_genesis_commands(&cfg.genesis, &file.parts);
    let engine = check_genesis_commands(&cfg.client_auth()?, cfg.operator()?, cmds).inspect_err(refused)?;
    if let Some(budget) = emission_budget {
        check_recorded(budget, engine.emissions()).inspect_err(refused)?;
    }
    info!(
        seq = file.source.seq,
        root = %file.source.root,
        accounts = file.totals.accounts,
        entitled = %file.totals.entitled,
        staked = %file.totals.staked,
        awards = file.totals.awards,
        "genesis carries the source's state"
    );
    Ok(file.parts)
}

/// Refuses a log whose carried source is not the config's (`check_recovered`, alert `carry_refused`), then, on a
/// fresh log, queues genesis, carrying the source's state when the config names one (`carried_parts`).
/// Error cases:
/// - see `check_recovered`, `carried_parts` and `Sequencer::carried_genesis` (alert `genesis_refused`)
pub(crate) async fn genesis(
    cfg: &NodeConfig,
    seq: &mut Sequencer,
    emission_budget: Option<TokenUnits>,
) -> anyhow::Result<()> {
    check_recovered(cfg, seq).inspect_err(|e| error!(alert = "carry_refused", "refusing to serve: {e:#}"))?;
    if seq.needs_genesis() {
        let carry = carried_parts(cfg, emission_budget).await?;
        info!(
            markets = cfg.genesis.markets.len(),
            carried = !carry.is_empty(),
            "genesis queued for the first trusted time sample"
        );
        seq.carried_genesis(&cfg.genesis, &carry)
            .inspect_err(|e| error!(alert = "genesis_refused", "refusing to serve: {e:#}"))?;
    }
    Ok(())
}

/// Refuses a log whose carried genesis names another source vault or chain than the config's `[genesis.carry]`,
/// and a config naming one over a log whose genesis carried nothing.
/// Error cases:
/// - see `check_carried_source`
pub(crate) fn check_recovered(cfg: &NodeConfig, seq: &Sequencer) -> anyhow::Result<()> {
    let configured = cfg.genesis.carry.as_ref().map(|carry| (cfg.custody.chain_id, carry.vault));
    check_carried_source(seq.carried_from(), configured, seq.needs_genesis())
}

/// Checks a log's carried source, `carried`, against the chain and source vault the config names, `configured`;
/// `fresh` while the log's genesis has yet to run.
/// Error cases:
/// - the log carried from another vault or chain, or from anywhere when the config names no `[genesis.carry]`
/// - the config names `[genesis.carry]` and the log's genesis ran but carried nothing
pub(crate) fn check_carried_source(
    carried: Option<CarrySource>,
    configured: Option<(u64, AccountId)>,
    fresh: bool,
) -> anyhow::Result<()> {
    match (carried, configured) {
        (Some(source), Some(configured)) if (source.chain_id, source.vault) == configured => Ok(()),
        (None, Some(_)) if fresh => Ok(()),
        (None, None) => Ok(()),
        (Some(source), configured) => bail!(
            "the log's genesis carried from vault {} on chain {}, but the config's [genesis.carry] names {configured:?}",
            source.vault,
            source.chain_id
        ),
        (None, Some(_)) => bail!("the config names [genesis.carry], but the log's genesis carried nothing"),
    }
}

#[cfg(test)]
mod tests;
