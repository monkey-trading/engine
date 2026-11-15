//! The staking contract's epoch and unstake cooldown against the engine's `InitStaking`. Both are
//! immutable in the contract (`Staking`'s constructor) and recorded once in the engine, which cannot
//! read the chain: a cooldown shorter than the epoch breaks the lock-up that prices jumping in just
//! before a snapshot, and an epoch that is not the contract's closes on another schedule than the
//! cooldown assumes. So the node compares them before it sequences `InitStaking`, and `engine admin
//! init-staking` before it signs, reading the contract through the custody RPC quorum.

use std::fmt;
use std::future::Future;
use std::sync::Arc;

use alloy::primitives::{Address, U256};
use alloy::sol;
use alloy::sol_types::SolCall;
use anyhow::{Context, bail, ensure};
use engine_core::staking::StakingConfig;
use futures_util::future::BoxFuture;

use crate::config::CustodySection;
use crate::custody::{ContractQuorum, Quorum, VaultReader, read_final};

// The staking contract's immutables, as `contracts/src/Staking.sol` declares them.
sol! {
    function epoch() external view returns (uint256);
    function cooldown() external view returns (uint256);
}

/// The epoch and cooldown the staking contract holds, in seconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ContractTerms {
    pub(super) epoch_secs: u64,
    pub(super) cooldown_secs: u64,
}

/// Why `InitStaking` may not go on.
#[derive(Debug)]
pub enum Refused {
    /// The command's epoch or cooldown is not the contract's.
    Differs(String),
    /// The contract could not be read, so nothing is known to match.
    Unread(anyhow::Error),
}

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Differs(reason) => f.write_str(reason),
            Self::Unread(cause) => write!(f, "cannot read the staking contract's epoch and cooldown: {cause:#}"),
        }
    }
}

/// A reader of the configured staking contract's epoch and cooldown.
#[derive(Clone)]
pub struct StakingTerms {
    read: Arc<dyn Fn() -> BoxFuture<'static, anyhow::Result<ContractTerms>> + Send + Sync>,
    /// One read at a time: a quorum's provider admission is shared state that each read begins by
    /// resetting, so overlapping reads would leave each other's providers unadmitted.
    gate: Arc<tokio::sync::Mutex<()>>,
}

impl StakingTerms {
    /// The reader for the configured staking contract, over its own custody quorum; `None` without EVM
    /// custody or a staking contract, as under mock custody. Builds clients without accessing the
    /// network.
    /// Error cases:
    /// - fewer than three providers, or `custody.vault` or a provider's client does not parse or build
    pub fn connect(custody: &CustodySection) -> anyhow::Result<Option<Self>> {
        let Some((evm, staking)) = custody.evm.as_ref().and_then(|evm| Some((evm, evm.staking.as_ref()?))) else {
            return Ok(None);
        };
        let quorum = Arc::new(ContractQuorum::connect(custody, "staking_terms")?);
        let (contract, chain) = (staking.contract, (custody.chain_id, evm.usdc));
        Ok(Some(Self::new(move || {
            let quorum = quorum.clone();
            async move { read(&quorum.0, contract, chain).await }
        })))
    }

    /// A reader that calls `read` for each check.
    pub(super) fn new<F>(read: impl Fn() -> F + Send + Sync + 'static) -> Self
    where
        F: Future<Output = anyhow::Result<ContractTerms>> + Send + 'static,
    {
        Self { read: Arc::new(move || Box::pin(read())), gate: Arc::default() }
    }

    /// A reader that answers `terms`, or fails when `None`.
    #[cfg(test)]
    pub(crate) fn fixed(terms: Option<(u64, u64)>) -> Self {
        Self::counting(terms, Arc::default())
    }

    /// `fixed`, adding one to `reads` for each read of the contract it makes.
    #[cfg(test)]
    pub(crate) fn counting(terms: Option<(u64, u64)>, reads: Arc<std::sync::atomic::AtomicUsize>) -> Self {
        let terms = terms.map(|(epoch_secs, cooldown_secs)| ContractTerms { epoch_secs, cooldown_secs });
        Self::new(move || {
            reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async move { terms.context("the providers name no final block") }
        })
    }

    /// Whether `config` has the epoch and cooldown the contract holds.
    /// Error cases:
    /// - the contract cannot be read
    /// - the epoch or cooldown differs from the contract's (see `check`)
    pub async fn check(&self, config: &StakingConfig) -> Result<(), Refused> {
        let terms = {
            let _one_at_a_time = self.gate.lock().await;
            (self.read)().await.map_err(Refused::Unread)?
        };
        check(terms, config).map_err(Refused::Differs)
    }
}

/// The contract's epoch and cooldown, read at one final block by a strict majority of providers that
/// serve `chain`, the configured chain id and the vault's token.
/// Error cases:
/// - the providers lack an identity, finality, header or call quorum
/// - the providers agree on another chain or token than the configured deployment's
/// - the address holds no staking contract, or a value does not fit a `u64` of seconds
pub(super) async fn read<R: VaultReader + Sync>(
    quorum: &Quorum<R>,
    staking: Address,
    (chain_id, token): (u64, Address),
) -> anyhow::Result<ContractTerms> {
    let identity = quorum.identity().await?;
    ensure!(
        (identity.chain_id, identity.token) == (chain_id, Some(token)),
        "the providers serve chain {} and token {:?}, not the configured chain {chain_id} and token {token}",
        identity.chain_id,
        identity.token
    );
    let (_, answers) = read_final(quorum, staking, |_| {
        vec![
            ("staking_epoch", epochCall {}.abi_encode().into()),
            ("staking_cooldown", cooldownCall {}.abi_encode().into()),
        ]
    })
    .await?;
    let [epoch, cooldown] = answers.as_slice() else { bail!("the staking contract's reads came back short") };
    let secs = |value: U256, name: &str| u64::try_from(value).with_context(|| format!("the contract's {name} is huge"));
    Ok(ContractTerms {
        epoch_secs: secs(
            epochCall::abi_decode_returns(epoch).context("the address holds no staking contract")?,
            "epoch",
        )?,
        cooldown_secs: secs(cooldownCall::abi_decode_returns(cooldown).context("decoding the cooldown")?, "cooldown")?,
    })
}

/// Refuses an `InitStaking` whose epoch or cooldown, in milliseconds, is not the contract's seconds.
/// Error cases:
/// - the epoch or the cooldown differs, naming both values
pub(super) fn check(contract: ContractTerms, config: &StakingConfig) -> Result<(), String> {
    let millis = |secs: u64| i64::try_from(secs).ok().and_then(|secs| secs.checked_mul(1000));
    for (name, set, held) in
        [("epoch", config.epoch_ms, contract.epoch_secs), ("cooldown", config.cooldown_ms, contract.cooldown_secs)]
    {
        if millis(held) != Some(set) {
            return Err(format!("{name} {set} ms differs from the staking contract's {held} s"));
        }
    }
    Ok(())
}
