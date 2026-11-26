//! The staking contract's epoch and unstake cooldown against the engine's `InitStaking`. Both are
//! immutable in the contract (`Staking`'s constructor) and recorded once in the engine, which cannot
//! read the chain: a cooldown shorter than the epoch breaks the lock-up that prices jumping in just
//! before a snapshot, and an epoch that is not the contract's closes on another schedule than the
//! cooldown assumes. So the node compares them before it sequences `InitStaking`, and `engine admin
//! init-staking` before it signs, reading the contract through the custody RPC quorum. The contract read is
//! the configured one only once the configured registry records it as the one it deployed.

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
use crate::custody::evm::{VaultIdentity, VaultReader};
use crate::custody::{ContractQuorum, Quorum, read_final_on};

// The staking contract's immutables, as `contracts/src/Staking.sol` declares them, and the registry's record of
// the staking contract it deployed, as `OperatorRegistry.sol` declares it.
sol! {
    function epoch() external view returns (uint256);
    function cooldown() external view returns (uint256);
    function staking() external view returns (address);
}

/// The epoch and cooldown the staking contract holds, in seconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContractTerms {
    pub(super) epoch_secs: u64,
    pub(super) cooldown_secs: u64,
}

impl ContractTerms {
    /// Whether `config` has these epoch and cooldown.
    /// Error cases:
    /// - the epoch or cooldown differs (see `check`)
    pub fn check(self, config: &StakingConfig) -> Result<(), Refused> {
        check(self, config).map_err(Refused::Differs)
    }
}

/// Why `InitStaking` may not go on.
#[derive(Debug)]
pub enum Refused {
    /// The command's epoch or cooldown is not the contract's, or the configured contract is not the one the
    /// configured registry deployed.
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
    read: Arc<dyn Fn() -> BoxFuture<'static, Result<ContractTerms, Refused>> + Send + Sync>,
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
        let (contracts, chain) = ((staking.registry.contract, staking.contract), (custody.chain_id, evm.usdc));
        Ok(Some(Self::new(move || {
            let quorum = quorum.clone();
            async move { read(&quorum.0, contracts, chain).await }
        })))
    }

    /// A reader that calls `read` for each check.
    pub(super) fn new<F>(read: impl Fn() -> F + Send + Sync + 'static) -> Self
    where
        F: Future<Output = Result<ContractTerms, Refused>> + Send + 'static,
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
            async move { terms.context("the providers name no final block").map_err(Refused::Unread) }
        })
    }

    /// The epoch and cooldown the contract holds, from one read.
    /// Error cases:
    /// - the contract cannot be read
    /// - the configured registry deployed another staking contract
    pub async fn read(&self) -> Result<ContractTerms, Refused> {
        let _one_at_a_time = self.gate.lock().await;
        (self.read)().await
    }

    /// Whether `config` has the epoch and cooldown the contract holds.
    /// Error cases:
    /// - see `read`
    /// - the epoch or cooldown differs from the contract's (see `check`)
    pub async fn check(&self, config: &StakingConfig) -> Result<(), Refused> {
        self.read().await?.check(config)
    }
}

/// The epoch and cooldown of the staking contract `registry` deployed, read at one final block by a strict
/// majority of providers that serve `chain`, the configured chain id and the vault's token, once the registry
/// there records `staking` as that contract.
/// Error cases:
/// - unread: the providers lack an identity, finality, header or call quorum
/// - unread: the providers agree on another chain or token than the configured deployment's
/// - unread: `registry` holds no registry, `staking` no staking contract, or a value does not fit a `u64` of
///   seconds
/// - differs: the registry deployed another staking contract than `staking`
pub(super) async fn read<R: VaultReader + Sync>(
    quorum: &Quorum<R>,
    (registry, staking): (Address, Address),
    chain: (u64, Address),
) -> Result<ContractTerms, Refused> {
    let (deployed, terms) = read_held(quorum, (registry, staking), chain).await.map_err(Refused::Unread)?;
    if deployed != staking {
        return Err(Refused::Differs(format!(
            "custody.evm.staking.contract is {staking}, but the registry {registry} deployed staking contract {deployed}"
        )));
    }
    Ok(terms)
}

/// The staking contract `registry` records as the one it deployed, and the epoch and cooldown `staking` holds,
/// all at one final block.
/// Error cases:
/// - the providers lack an identity, finality, header or call quorum, or agree on another chain or token than
///   the configured deployment's
/// - `registry` holds no registry, `staking` no staking contract, or a value does not fit a `u64` of seconds
async fn read_held<R: VaultReader + Sync>(
    quorum: &Quorum<R>,
    (registry, staking): (Address, Address),
    (chain_id, token): (u64, Address),
) -> anyhow::Result<(Address, ContractTerms)> {
    let admit = |identity: &VaultIdentity| {
        ensure!(
            (identity.chain_id, identity.token) == (chain_id, Some(token)),
            "the providers serve chain {} and token {:?}, not the configured chain {chain_id} and token {token}",
            identity.chain_id,
            identity.token
        );
        Ok(())
    };
    let (_, answers) = read_final_on(quorum, admit, |_| {
        vec![
            ("staking_deployed", registry, stakingCall {}.abi_encode().into()),
            ("staking_epoch", staking, epochCall {}.abi_encode().into()),
            ("staking_cooldown", staking, cooldownCall {}.abi_encode().into()),
        ]
    })
    .await?;
    let [deployed, epoch, cooldown] = answers.as_slice() else { bail!("the staking contract's reads came back short") };
    let deployed = stakingCall::abi_decode_returns(deployed).context("the registry's address holds no registry")?;
    let secs = |value: U256, name: &str| u64::try_from(value).with_context(|| format!("the contract's {name} is huge"));
    let terms = ContractTerms {
        epoch_secs: secs(
            epochCall::abi_decode_returns(epoch).context("the address holds no staking contract")?,
            "epoch",
        )?,
        cooldown_secs: secs(cooldownCall::abi_decode_returns(cooldown).context("decoding the cooldown")?, "cooldown")?,
    };
    Ok((deployed, terms))
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
