//! A start's checks of a carried genesis. Before a fresh log's genesis carries state, the node holds under Warning
//! `carry_waiting` until the chain shows the move done: the source vault's signer retired, with a checkpoint
//! accepted; the registry following this node's vault with rotation's switch off and the config's operator key in
//! effect; and MONKEY's minter following this node's vault; and the carry file in place. The configured registry
//! must then be the one the source vault reads, the minter must mint the token the staking contract holds, the file
//! must name the source vault's last accepted checkpoint, and the genesis it carries must apply in full on a scratch
//! engine at the minter's emission budget, or the start stops with Critical `carry_refused` before anything is
//! written.
//! Every start also refuses a log whose carried source is not the config's, and a member refuses it before it takes a
//! turn's keys or registers its standby key (from the operator's archive while it holds no copy, `archive`), as a
//! whole-log take does before it confirms the handover.

use std::future::Future;
use std::path::Path;
use std::time::Duration;

use alloy::primitives::{Address, B256};
use anyhow::{Context, anyhow, bail};
use engine_core::Engine;
use engine_core::carry::{CarryPart, CarrySource};
use engine_core::emissions::TokenUnits;
use engine_core::types::AccountId;
use tracing::{error, info, warn};

use crate::carry::chain::{self, MoveContracts, MoveState};
use crate::carry::{CarryFile, read};
use crate::checkpoints::emission_budget::check_recorded;
use crate::clock::Clock;
use crate::config::NodeConfig;
use crate::custody::ContractQuorum;
use crate::sequencer::genesis::check_carried_genesis_at;
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
    if state.minter_vault != expected.vault {
        unmet.push(format!("MONKEY's minter follows vault {}, not {}", state.minter_vault, expected.vault));
    }
    unmet
}

/// Checks that the configured contracts are the ones the move keeps: the registry is the one the source vault reads,
/// whose staking contract the source log mirrored, and the minter mints the token the staking contract holds.
/// Error cases:
/// - the configured registry is not the source vault's, or the tokens differ
pub(crate) fn check_kept(state: &MoveState, registry: Address) -> anyhow::Result<()> {
    if state.source_registry != registry {
        bail!(
            "custody.evm.staking.registry is {registry}, but the source vault reads registry {}: the carried stakes \
             mirror that registry's staking contract",
            state.source_registry
        );
    }
    if state.minter_token != state.staking_token {
        bail!(
            "custody.evm.minter mints {}, but the staking contract holds {}: the config names another minter",
            state.minter_token,
            state.staking_token
        );
    }
    Ok(())
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
/// with what is missing, rechecking every `RECHECK`; returns the state and the file. Each read of the chain is held
/// to `check_kept` at once, as waiting for a move that the configured contracts can never complete is no wait.
/// Error cases:
/// - the file is there but does not read as a carry file
/// - a read of the chain shows the configured `registry` or minter are not the ones the move keeps (`check_kept`)
pub(crate) async fn hold<F, R>(
    read_chain: R,
    (expected, registry): (&Expected, Address),
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
                check_kept(&state, registry)?;
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
/// full on a scratch engine, stamped at `clock`'s time once the hold ends, with an emission budget matching
/// `emission_budget`, the minter's: a hold past the genesis's staking start is refused here, not at its stamp.
/// Error cases:
/// - the custody quorum cannot be built, or a configured address does not parse
/// - the file does not read, names another source, or carries a genesis that does not apply in full at the time,
///   or another emission budget (alert `carry_refused`)
pub(crate) async fn carried_parts(
    cfg: &NodeConfig,
    emission_budget: Option<TokenUnits>,
    clock: &dyn Clock,
) -> anyhow::Result<Vec<CarryPart>> {
    let Some(carry) = &cfg.genesis.carry else { return Ok(Vec::new()) };
    let refused = |e: &anyhow::Error| error!(alert = "carry_refused", "refusing to carry state into genesis: {e:#}");
    let evm = cfg.custody.evm.as_ref().context("[genesis.carry] needs [custody.evm]")?;
    let staking = evm.staking.as_ref().context("[genesis.carry] needs [custody.evm.staking]")?;
    let expected = Expected {
        chain_id: cfg.custody.chain_id,
        vault: cfg.custody.vault.parse().context("custody.vault")?,
        source: Address::from(carry.vault.0),
        operator: Address::from(cfg.operator()?.address.0),
    };
    let contracts = MoveContracts {
        source: expected.source,
        registry: staking.registry.contract,
        minter: evm.minter,
        staking: staking.contract,
    };
    let quorum = ContractQuorum::connect(&cfg.custody, "carry")?;
    let read_chain = || chain::read(quorum.quorum(), contracts, (expected.chain_id, evm.usdc));
    let (state, file) = hold(read_chain, (&expected, contracts.registry), &carry.file).await.inspect_err(refused)?;
    let at = state.source_checkpoint.ok_or_else(|| anyhow!("held until the source vault holds a checkpoint"))?;
    check_source(&file, at, &expected).inspect_err(refused)?;
    let cmds = carried_genesis_commands(&cfg.genesis, &file.parts);
    let now = clock.now_ms();
    let engine = check_carried_genesis_at(&cfg.client_auth()?, cfg.operator()?, cmds, now).inspect_err(refused)?;
    check_finished(&engine, &file).inspect_err(refused)?;
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

/// Checks that the scratch engine a carried genesis ran on, which finished its carry
/// (`check_carried_genesis_at`), carried from `file`'s source.
/// Error cases:
/// - the engine records another source
pub(crate) fn check_finished(engine: &Engine, file: &CarryFile) -> anyhow::Result<()> {
    if engine.carried_from() != Some(file.source) {
        bail!("the carry file names {:?}, but its parts carried from {:?}", file.source, engine.carried_from());
    }
    Ok(())
}

/// Refuses a log whose carried source is not the config's (`check_recovered`, alert `carry_refused`), then, on a
/// fresh log, queues genesis, carrying the source's state when the config names one (`carried_parts`).
/// Error cases:
/// - see `check_recovered`, `carried_parts` and `Sequencer::carried_genesis` (alert `genesis_refused`)
pub(crate) async fn genesis(
    cfg: &NodeConfig,
    seq: &mut Sequencer,
    emission_budget: Option<TokenUnits>,
    clock: &dyn Clock,
) -> anyhow::Result<()> {
    check_recovered(cfg, seq).inspect_err(|e| error!(alert = "carry_refused", "refusing to serve: {e:#}"))?;
    if seq.needs_genesis() {
        let carry = carried_parts(cfg, emission_budget, clock).await?;
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
    check_carried_source(seq.carried_from(), configured(cfg), seq.needs_genesis())
}

/// Refuses a copy whose genesis carried from another source than the config's `[genesis.carry]`, or from any when
/// the config names none, and, unless `open`, from none when it names one: the start would refuse to serve it
/// (`check_recovered`) once this node held the keys. A member checks before it takes a turn's keys, and a whole-log
/// take before it seals the keys or confirms, each over a copy that holds a closed genesis; a member checks before
/// it registers its standby key too, while its copy may still be before its genesis (`open`). Pages Critical
/// `carry_refused`.
/// Error cases:
/// - see `check_carried_source`
pub(crate) fn check_copy(cfg: &NodeConfig, copy: &Engine, open: bool) -> anyhow::Result<()> {
    check_carried_source(copy.carried_from(), configured(cfg), open)
        .inspect_err(|e| error!(alert = "carry_refused", "refusing to hold this log's keys: {e:#}"))
}

/// The chain and source vault the config's `[genesis.carry]` names.
pub(super) fn configured(cfg: &NodeConfig) -> Option<(u64, AccountId)> {
    cfg.genesis.carry.as_ref().map(|carry| (cfg.custody.chain_id, carry.vault))
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
