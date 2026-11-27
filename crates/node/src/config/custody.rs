//! The `[custody.evm]` section, the independent providers whose URLs may carry credentials, and the checks that tie
//! the custody section to its provider. A provider's key may be delivered instead of written here
//! (`SecretUrl`), but its origin stays in the attested file, since these providers' quorum credits
//! deposits.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::Duration;

use alloy::primitives::Address;
use anyhow::{Context, bail};
use engine_core::Fixed;
use serde::Deserialize;

use crate::config::{CustodyProvider, NodeConfig, SecretUrl, TeeProvider};

/// The most blocks one custody scan may read in a production build: about forty minutes of Arbitrum's
/// quarter-second blocks, so one staking scan carries the mirror past at most one of the daily epoch
/// ends a held close waits on, each closed at the stake mirrored when the frontier passed it.
const PRODUCTION_MAX_BLOCK_RANGE: u64 = 10_000;

/// The block tag whose number marks finality on the custody chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinalityTag {
    Latest,
    /// On Arbitrum One, the newest block whose L2 batch is posted to Ethereum.
    Safe,
    Finalized,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvmCustodySection {
    /// HTTPS unless it is on this machine (`127.0.0.1` or `[::1]`). It must serve state at past
    /// blocks back to `start_block` (an archive node), which catch-up after downtime and rescans
    /// read.
    pub providers: Vec<RpcProviderSection>,
    /// The USDC contract the vault must hold; the watcher checks it on every poll.
    pub usdc: Address,
    /// The `MonkeyMinter` that mints MONKEY against the vault's checkpoints. With `[checkpoints]`, the
    /// node reads the emission period budget from its token's cap.
    pub minter: Address,
    /// Trading credit follows this tag, less `confirmations`; production requires `latest`.
    /// Earlier protocols use this tag as the sole final-deposit rule.
    pub finality_tag: FinalityTag,
    /// Releases confirmed principal at this tag, without another confirmations delay.
    pub withdrawal_finality_tag: FinalityTag,
    pub confirmations: u64,
    /// The vault's deployment block, where scanning starts when there is no cursor.
    pub start_block: u64,
    /// Most blocks asked for in one `eth_getLogs` request, and most headers in one JSON-RPC
    /// batch; set it within both of the provider's limits. The watcher also keeps one proven
    /// block hash per this many blocks of backlog. At most 10,000 in a production build.
    pub max_block_range: u64,
    #[serde(with = "humantime_serde")]
    pub poll_interval: Duration,
    /// Deadline for each RPC request.
    #[serde(with = "humantime_serde")]
    pub rpc_timeout: Duration,
    /// A deposit stall, reads trailing the final block, an endpoint head that stops
    /// advancing, or a checkpoint deposit left uncredited, that lasts this long raises a
    /// Critical alert. Sized for the credit tag's cadence: on Arbitrum One, above the
    /// longest gap between L1 batch posts. A checkpoint deposit's bound must also exceed the
    /// credit tag's lag, one release-tag finality delay, and the wait for the next periodic
    /// checkpoint, since a deposit waits that long for a final checkpoint that credits it.
    #[serde(with = "humantime_serde")]
    pub stall_critical_after: Duration,
    /// The release tag (`withdrawal_finality_tag`, `finalized` in production) may trail a
    /// moving head, and a restarted node's pending checkpoint inclusion may await that tag
    /// at startup, for this long before `custody_finality_stalled` pages or startup refuses
    /// to open the log. Sized for the release tag's cadence, not the batch-posting cadence:
    /// on Arbitrum One the `finalized` tag follows batch posting by one Ethereum finality
    /// period (about 15 minutes), so production sets this to 30m. A checkpoint held back this
    /// long with none becoming final raises `checkpoint_backlog`.
    #[serde(with = "humantime_serde")]
    pub finality_stalled_after: Duration,
    /// Where the deposit cursor is kept between restarts.
    pub cursor_path: PathBuf,
    /// The staking contract whose events the deposit watcher mirrors into the engine as
    /// `Stake`/`Unstake` commands, once staking is initialised. Absent when the deployment
    /// has no staking.
    pub staking: Option<StakingCustodySection>,
}

/// The staking contract the watcher mirrors. Events only exist from `V25`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StakingCustodySection {
    /// The `Staking` contract, deployed after the vault with the cooldown the genesis records.
    pub contract: Address,
    /// Where the staking cursor is kept between restarts.
    pub cursor_path: PathBuf,
    /// The operator registry that deployed the staking contract, whose rotation switches and
    /// elections the watcher mirrors with the staking events from `V37`. The watcher refuses a
    /// staking contract that this registry's `staking()` does not name.
    pub registry: RegistryCustodySection,
    /// The block the staking contract was deployed in, from which a log whose genesis carried another
    /// log's state mirrors the stakes again (`V93`); a config naming `[genesis.carry]` needs it.
    #[serde(default)]
    pub deployment_block: Option<u64>,
}

/// The operator registry the watcher mirrors.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegistryCustodySection {
    /// The `OperatorRegistry` contract, whose `staking()` is the staking contract above.
    pub contract: Address,
    /// The `RegistryLens` beside the registry, which `GET /v1/operators` reads the operator list
    /// through. Absent, that route answers 404.
    #[serde(default)]
    pub lens: Option<Address>,
    /// The `OperatorSchedule` the registry names, whose rotation sets and turns the watcher mirrors
    /// with the registry's events from `V51`. The watcher refuses a schedule whose `registry()` is not
    /// the registry above. Absent, no turn is mirrored.
    #[serde(default)]
    pub schedule: Option<Address>,
    /// The `ParameterVote` contract over the schedule, whose passed proposals the watcher mirrors
    /// from `V56`, and the restriction lifetime the node records with it. Absent, no vote is
    /// mirrored, and while rotation is on nothing changes the governed parameters.
    #[serde(default)]
    pub vote: Option<VoteCustodySection>,
    /// The `FaultProof` contract over the vault, whose proven faults the watcher mirrors from `V70`, and
    /// the penalty the node records with it. Absent, no fault is mirrored.
    #[serde(default)]
    pub fault_proof: Option<FaultProofCustodySection>,
}

/// The vote contract the watcher mirrors, which the node records in the log once `V56` applies.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VoteCustodySection {
    /// The `ParameterVote` contract, whose `schedule()` is the schedule above; the watcher refuses
    /// another.
    pub contract: Address,
    /// How long a fast-path restriction lasts after it is logged, unless another fast-path vote
    /// renews it: protocol, recorded once, at most seven days.
    #[serde(with = "humantime_serde")]
    pub restriction_ttl: Duration,
}

impl VoteCustodySection {
    /// The restriction lifetime in milliseconds, as the engine records it.
    /// Error cases:
    /// - zero, past seven days, or not a whole number of milliseconds
    pub fn ttl_ms(&self) -> anyhow::Result<engine_core::types::Millis> {
        let ms = i64::try_from(self.restriction_ttl.as_millis()).unwrap_or(i64::MAX);
        if Duration::from_millis(ms.unsigned_abs()) != self.restriction_ttl
            || !(1..=engine_core::operator_bounds::NOTICE_MS).contains(&ms)
        {
            bail!("custody.evm.staking.registry.vote.restriction_ttl must be whole milliseconds from 1ms to 7 days");
        }
        Ok(ms)
    }

    /// What the node records in the log once `V56` applies.
    /// Error cases:
    /// - as `ttl_ms`
    pub fn governance(&self) -> anyhow::Result<engine_core::governance::GovernanceConfig> {
        Ok(engine_core::governance::GovernanceConfig {
            vote: engine_core::types::AccountId(self.contract.0.0),
            restriction_ttl_ms: self.ttl_ms()?,
        })
    }
}

/// The fault proof contract the watcher mirrors, which the node records in the log once `V70` applies.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FaultProofCustodySection {
    /// The `FaultProof` contract, whose `vault()` is `custody.vault`; the watcher refuses another.
    pub contract: Address,
    /// The haircut a proven fault puts on its member's operator share: protocol, recorded once.
    pub penalty: PenaltySection,
}

/// A proven fault's haircut.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PenaltySection {
    /// The haircut when the fault is mirrored, in bps of the member's share: 1 to 10,000.
    pub initial_bps: i64,
    /// How long it takes to fall linearly to nothing: whole milliseconds, at most ten years.
    #[serde(with = "humantime_serde")]
    pub decay: Duration,
}

impl FaultProofCustodySection {
    /// What the node records in the log once `V70` applies, checked as the engine checks it, since a
    /// value it refused would stop the node at the record.
    /// Error cases:
    /// - the zero address
    /// - a haircut outside 1 to 10,000 bps, or a decay of zero, past ten years, or not whole milliseconds
    pub fn config(&self) -> anyhow::Result<engine_core::operator_faults::FaultProofConfig> {
        use engine_core::operator_faults::{FaultProofConfig, MAX_DECAY_MS, Penalty};
        if self.contract.is_zero() {
            bail!("custody.evm.staking.registry.fault_proof.contract must be a nonzero address");
        }
        let decay_ms = i64::try_from(self.penalty.decay.as_millis()).unwrap_or(i64::MAX);
        if Duration::from_millis(decay_ms.unsigned_abs()) != self.penalty.decay
            || !(1..=MAX_DECAY_MS).contains(&decay_ms)
        {
            bail!(
                "custody.evm.staking.registry.fault_proof.penalty.decay must be whole milliseconds from 1ms to 3650 days"
            );
        }
        if !(1..=10_000).contains(&self.penalty.initial_bps) {
            bail!("custody.evm.staking.registry.fault_proof.penalty.initial_bps must be from 1 to 10000");
        }
        let penalty = Penalty { initial_bps: self.penalty.initial_bps, decay_ms };
        Ok(FaultProofConfig { contract: engine_core::types::AccountId(self.contract.0.0), penalty })
    }
}

/// Independently operated RPC service; its name identifies alerts without exposing credentials.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RpcProviderSection {
    pub name: String,
    pub rpc_url: SecretUrl,
}

impl NodeConfig {
    /// The vote contract and restriction lifetime `[custody.evm.staking.registry.vote]` names, which the
    /// node records once `V56` applies; `None` without the section.
    /// Error cases:
    /// - as `VoteCustodySection::governance`
    pub fn governance(&self) -> anyhow::Result<Option<engine_core::governance::GovernanceConfig>> {
        let staking = self.custody.evm.as_ref().and_then(|evm| evm.staking.as_ref());
        staking.and_then(|staking| staking.registry.vote.as_ref()).map(VoteCustodySection::governance).transpose()
    }

    /// The fault proof contract and penalty `[custody.evm.staking.registry.fault_proof]` names, which
    /// the node records once `V70` is in force; `None` without the section.
    /// Error cases:
    /// - as `FaultProofCustodySection::config`
    pub fn fault_proof(&self) -> anyhow::Result<Option<engine_core::operator_faults::FaultProofConfig>> {
        let evm = self.custody.evm.as_ref();
        let section =
            evm.and_then(|evm| evm.staking.as_ref()).and_then(|staking| staking.registry.fault_proof.as_ref());
        section.map(FaultProofCustodySection::config).transpose()
    }

    /// Refuses a custody scan wider than the production value when `production` is set.
    /// Error cases:
    /// - `production` is set and `custody.evm.max_block_range` is above `PRODUCTION_MAX_BLOCK_RANGE`
    pub(super) fn refuse_wide_custody_scans(&self, production: bool) -> anyhow::Result<()> {
        let range = self.custody.evm.as_ref().map_or(0, |evm| evm.max_block_range);
        if production && range > PRODUCTION_MAX_BLOCK_RANGE {
            bail!(
                "a production build allows custody.evm.max_block_range at most {PRODUCTION_MAX_BLOCK_RANGE}, and the \
                 config sets {range}"
            );
        }
        Ok(())
    }

    /// Checks the custody section against its provider, and that genesis credits house capital
    /// only under mock custody.
    /// Error cases:
    /// - `provider = "evm"` without `[custody.evm]`, or `provider = "mock"` with it
    /// - `custody.evm.providers[].rpc_url` is not a URL, or is plain HTTP to another machine, where the host
    ///   could rewrite every answer
    /// - a range, interval, or timeout in `[custody.evm]` is zero
    /// - `custody.evm.minter` is the token, the vault, or the staking contract, which answers `monkey()`
    ///   as the minter does
    /// - `custody.evm.staking.registry.lens` is the token, the vault, the staking contract, or the registry
    /// - `custody.evm.staking.registry.schedule` is any of those, or the lens
    /// - `custody.evm.staking.registry.vote` without a schedule, its contract any of those or the
    ///   schedule, or a restriction lifetime of zero, past seven days, or not whole milliseconds
    /// - `custody.evm.staking.registry.fault_proof` without a schedule, its contract any of those, the
    ///   vote contract or the log heads, or a contract or penalty `FaultProofCustodySection::config` refuses
    /// - `genesis.house_capital` is not zero with EVM custody
    pub(super) fn validate_custody(&self) -> anyhow::Result<()> {
        let evm = match (self.custody.provider, &self.custody.evm) {
            (CustodyProvider::Mock, None) => return Ok(()),
            (CustodyProvider::Mock, Some(_)) => bail!("[custody.evm] is set but custody.provider is \"mock\""),
            (CustodyProvider::Evm, None) => bail!("custody.provider = \"evm\" needs a [custody.evm] section"),
            (CustodyProvider::Evm, Some(evm)) => evm,
        };
        if self.tee.provider != TeeProvider::Mock
            && (evm.finality_tag != FinalityTag::Latest || evm.withdrawal_finality_tag != FinalityTag::Finalized)
        {
            bail!("production custody requires finality_tag = latest and withdrawal_finality_tag = finalized");
        }
        check_providers(&evm.providers, "custody.evm.providers")?;
        if evm.max_block_range == 0 || evm.poll_interval.is_zero() || evm.rpc_timeout.is_zero() {
            bail!("custody.evm.max_block_range, poll_interval, and rpc_timeout must be non-zero");
        }
        if evm.finality_stalled_after.is_zero() {
            bail!("custody.evm.finality_stalled_after must be non-zero");
        }
        let vault: Address = self.custody.vault.parse().context("custody.vault")?;
        if [Some(evm.usdc), Some(vault), evm.staking.as_ref().map(|staking| staking.contract)]
            .contains(&Some(evm.minter))
        {
            bail!("custody.evm.minter must be the MonkeyMinter, not the token, vault, or staking contract");
        }
        if let Some(staking) = &evm.staking {
            if staking.contract == evm.usdc || staking.contract == vault {
                bail!("custody.evm.staking.contract must be the staking contract, not the token or the vault");
            }
            if [evm.usdc, vault, staking.contract].contains(&staking.registry.contract) {
                bail!(
                    "custody.evm.staking.registry.contract must be the operator registry, not the token, vault, or staking contract"
                );
            }
            if let Some(lens) = staking.registry.lens
                && [evm.usdc, vault, staking.contract, staking.registry.contract].contains(&lens)
            {
                bail!(
                    "custody.evm.staking.registry.lens must be the registry lens, not the token, vault, staking contract, or registry"
                );
            }
            if let Some(schedule) = staking.registry.schedule
                && [
                    Some(evm.usdc),
                    Some(vault),
                    Some(staking.contract),
                    Some(staking.registry.contract),
                    staking.registry.lens,
                ]
                .contains(&Some(schedule))
            {
                bail!(
                    "custody.evm.staking.registry.schedule must be the operator schedule, not the token, vault, staking contract, registry, or lens"
                );
            }
            if let Some(vote) = &staking.registry.vote {
                let schedule =
                    staking.registry.schedule.context("custody.evm.staking.registry.vote needs a schedule")?;
                let others = [evm.usdc, vault, staking.contract, staking.registry.contract, schedule];
                if others.contains(&vote.contract) || staking.registry.lens == Some(vote.contract) {
                    bail!(
                        "custody.evm.staking.registry.vote.contract must be the vote contract, not the token, vault, staking contract, registry, lens, or schedule"
                    );
                }
                vote.ttl_ms()?;
            }
            if let Some(fault_proof) = &staking.registry.fault_proof {
                let schedule =
                    staking.registry.schedule.context("custody.evm.staking.registry.fault_proof needs a schedule")?;
                // `LogHeads` answers `vault()` too, so the watcher's binding read alone would pass for it.
                let others = [
                    Some(evm.usdc),
                    Some(vault),
                    Some(staking.contract),
                    Some(staking.registry.contract),
                    Some(schedule),
                    staking.registry.lens,
                    staking.registry.vote.as_ref().map(|vote| vote.contract),
                    self.log_heads.as_ref().map(|heads| heads.contract),
                ];
                if others.contains(&Some(fault_proof.contract)) {
                    bail!(
                        "custody.evm.staking.registry.fault_proof.contract must be the fault proof contract, not the token, vault, staking contract, registry, schedule, lens, vote contract, or log heads"
                    );
                }
                fault_proof.config()?;
            }
        }
        if self.genesis.house_capital != Fixed::ZERO {
            bail!("genesis.house_capital must be zero with EVM custody; deposit to the house instead");
        }
        Ok(())
    }
}

/// Checks a quorum's providers: at least three, each named, each at its own URL, and each over HTTPS
/// unless it is on this machine.
/// Error cases:
/// - fewer than three, an empty or repeated name, a repeated URL, or plain HTTP to another machine
pub(super) fn check_providers(providers: &[RpcProviderSection], field: &str) -> anyhow::Result<()> {
    if providers.len() < 3 {
        bail!("{field} requires at least three independent RPC providers");
    }
    let (mut names, mut urls) = (BTreeSet::new(), BTreeSet::new());
    for provider in providers {
        let url = provider.rpc_url.attested().with_context(|| format!("{field}.rpc_url"))?;
        if provider.name.trim().is_empty() || !names.insert(&provider.name) || !urls.insert(url.clone()) {
            bail!("{field} requires non-empty unique names and distinct URLs");
        }
        // Literal addresses only: `localhost` resolves through the host-controlled `/etc/hosts`.
        let on_this_machine = matches!(url.host_str(), Some("127.0.0.1" | "[::1]"));
        if url.scheme() != "https" && !(url.scheme() == "http" && on_this_machine) {
            bail!("{field}.rpc_url must use https unless it is on this machine");
        }
    }
    Ok(())
}
