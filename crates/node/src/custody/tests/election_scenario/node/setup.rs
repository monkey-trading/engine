//! The scenario node's chain-facing pieces: the checkpoint worker and the custody section.

use std::sync::Arc;
use std::time::Duration;

use alloy::sol;
use engine_core::Fixed;
use engine_core::carry::CarryPart;
use engine_core::emissions::TokenUnits;
use engine_core::staking::StakingConfig;
use engine_core::types::{AccountId, Millis};
use tokio::sync::{mpsc, watch};

use super::super::chain::{Chain, monkeyCall};
use super::super::{COOLDOWN_SECS, DISTRIBUTION_EPOCH_SECS, T0_SECS};
use crate::checkpoints::{CheckpointRpc, Poster};
use crate::config::{
    CarrySection, CheckpointSection, CustodySection, FinalityTag, Genesis, NodeConfig, RpcProviderSection, TeeProvider,
};
use crate::log::CheckpointDomain;
use crate::log_check::Due;
use crate::log_check::ask::Asker;
use crate::sequencer::Request;
use crate::tee::EnclaveKey;
use crate::tee::sealing::KeyStore;
use crate::trusted_time::TrustedClock;

sol! {
    function cap() external view returns (uint256);
}

/// The checkpoint worker posting to the chain's vault at the latest block from `relayer`.
pub(super) fn poster(
    chain: &Chain,
    dir: &std::path::Path,
    key: &EnclaveKey,
    relayer: alloy::primitives::Address,
) -> Poster<CheckpointRpc> {
    let section: CheckpointSection = toml::from_str(&format!(
        "rpc_url = {:?}\nrelayer = '{relayer:#x}'\nretry_interval = '40ms'\nrpc_timeout = '2s'",
        chain.url
    ))
    .unwrap();
    let domain = CheckpointDomain { chain_id: 31337, vault: AccountId(chain.vault.into()) };
    let signer = AccountId(key.address());
    Poster {
        chain: CheckpointRpc::new(&section, domain, chain.usdc, FinalityTag::Latest, 0, dir, signer).unwrap(),
        registry: None,
        domain,
        signer,
        retry: section.retry_interval,
        pending_deposits_critical_after: Duration::from_secs(1),
        post_withheld: Default::default(),
        episodes: Default::default(),
        relayer_gas: None,
    }
}

/// A seat's asking for the boundary's log check: the vault read at the chain's release tag, questions
/// signed with `key`, quotes verified against the mock TEE's platform `store` seals under, at
/// `trusted` time.
pub(super) fn asker(
    chain: &Chain,
    sequencer: &mpsc::Sender<Request>,
    due: watch::Receiver<Option<Due>>,
    (store, key): (&KeyStore, Arc<EnclaveKey>),
    trusted: TrustedClock,
) -> Asker<CheckpointRpc> {
    let section: CheckpointSection = toml::from_str(&format!(
        "rpc_url = {:?}\nrelayer = '{:#x}'\nretry_interval = '40ms'\nrpc_timeout = '2s'",
        chain.url, chain.accounts[9]
    ))
    .unwrap();
    let domain = CheckpointDomain { chain_id: 31337, vault: AccountId(chain.vault.into()) };
    let vault = CheckpointRpc::read_only(&section, domain, chain.usdc, chain.release_tag, 0).unwrap();
    Asker::new(vault, sequencer, due, (domain, TeeProvider::Mock, Vec::new(), store.platform()), key, trusted)
}

/// The governance the chain's vote contract runs, when it has one.
pub(super) fn governance(chain: &Chain) -> Option<engine_core::governance::GovernanceConfig> {
    chain.vote.map(|(vote, ttl)| engine_core::governance::GovernanceConfig {
        vote: AccountId(vote.into()),
        restriction_ttl_ms: ttl,
    })
}

/// The fault proof contract the chain runs and its penalty, when it has one.
pub(super) fn fault_proof(chain: &Chain) -> Option<engine_core::operator_faults::FaultProofConfig> {
    chain.fault_proof.map(|(contract, penalty)| engine_core::operator_faults::FaultProofConfig {
        contract: AccountId(contract.into()),
        penalty,
    })
}

/// EVM custody over the chain's proxies, crediting at the latest block and releasing at the chain's
/// release tag, with the staking contract, the registry, and any schedule mirrored.
pub(crate) fn custody_section(chain: &Chain, dir: &std::path::Path, mut custody: CustodySection) -> CustodySection {
    let anvil: crate::config::NodeConfig =
        toml::from_str(&std::fs::read_to_string(crate::test_support::ANVIL_CONFIG).unwrap()).unwrap();
    let mut evm = anvil.custody.evm.unwrap();
    evm.providers = chain
        .proxies
        .iter()
        .enumerate()
        .map(|(index, url)| {
            toml::from_str::<RpcProviderSection>(&format!("name = 'scenario-{index}'\nrpc_url = '{url}'")).unwrap()
        })
        .collect();
    evm.usdc = chain.usdc;
    evm.minter = chain.minter;
    evm.finality_tag = FinalityTag::Latest;
    evm.withdrawal_finality_tag = chain.release_tag;
    evm.confirmations = 0;
    evm.start_block = 0;
    evm.poll_interval = Duration::from_millis(30);
    evm.cursor_path = dir.join("custody.json");
    evm.staking = Some(
        toml::from_str(&format!(
            "contract = '{:#x}'\ncursor_path = '{}'\n[registry]\ncontract = '{:#x}'{}{}{}{}",
            chain.staking,
            dir.join("staking.json").display(),
            chain.registry,
            chain.lens.map(|lens| format!("\nlens = '{lens:#x}'")).unwrap_or_default(),
            chain.schedule.map(|schedule| format!("\nschedule = '{schedule:#x}'")).unwrap_or_default(),
            chain
                .vote
                .map(|(vote, ttl)| format!("\n[registry.vote]\ncontract = '{vote:#x}'\nrestriction_ttl = '{ttl}ms'"))
                .unwrap_or_default(),
            chain
                .fault_proof
                .map(|(contract, penalty)| format!(
                    "\n[registry.fault_proof]\ncontract = '{contract:#x}'\npenalty = {{ initial_bps = {}, decay = '{}ms' }}",
                    penalty.initial_bps, penalty.decay_ms
                ))
                .unwrap_or_default()
        ))
        .unwrap(),
    );
    custody.chain_id = 31337;
    custody.evm = Some(evm);
    custody
}

/// `genesis` for the scenarios: no house capital, the flat 10% profit fee (the holding-time fee has a
/// scenario of its own), and staking from a fresh node's start (`start_ms`).
pub(super) fn scenario_genesis(chain: &Chain, mut genesis: Genesis) -> Genesis {
    genesis.house_capital = Fixed::ZERO;
    genesis.params.profit_fee_curve.clear();
    genesis.staking = Some(StakingConfig {
        liquidity_floor: Fixed::from_int(1_000_000),
        epoch_ms: DISTRIBUTION_EPOCH_SECS as Millis * 1000,
        start_ms: start_ms(chain),
        cooldown_ms: COOLDOWN_SECS as Millis * 1000,
    });
    genesis
}

/// Where a fresh node's clock starts: the scenarios' start, or the start a carried genesis names.
pub(super) fn start_ms(chain: &Chain) -> Millis {
    chain.carry.as_ref().map_or(T0_SECS as Millis * 1000, |(_, _, at)| *at)
}

/// The config a node whose fresh log carries the state `chain.carry` names starts under, its log and custody
/// cursors in `dir`: EVM custody over the chain, and the scenario genesis with `[genesis.carry]`.
pub(in crate::custody::tests::election_scenario) fn carry_config(chain: &Chain, dir: &std::path::Path) -> NodeConfig {
    let (source, file, _) = chain.carry.clone().expect("the chain names a carry");
    let mut cfg = crate::test_support::dev_config();
    cfg.custody.provider = crate::config::CustodyProvider::Evm;
    cfg.custody.vault = format!("{:#x}", chain.vault);
    cfg.custody = custody_section(chain, dir, cfg.custody);
    cfg.genesis = scenario_genesis(chain, cfg.genesis);
    cfg.genesis.carry = Some(CarrySection { vault: AccountId(source.into()), file });
    cfg
}

/// The parts a fresh node's genesis carries: none unless `chain.carry` names a source, and then those a start
/// takes once the chain shows the move done (`carry::boot::carried_parts`), at the minter's emission budget.
pub(super) async fn carried(chain: &Chain, dir: &std::path::Path) -> Vec<CarryPart> {
    if chain.carry.is_none() {
        return Vec::new();
    }
    let budget = emission_budget(chain).await;
    crate::carry::boot::carried_parts(&carry_config(chain, dir), Some(budget)).await.expect("the carry applies")
}

/// The emission period budget the minter's MONKEY cap was derived from.
pub(in crate::custody::tests::election_scenario) async fn emission_budget(chain: &Chain) -> TokenUnits {
    let monkey = chain.view(chain.minter, monkeyCall {}).await;
    let cap = chain.view(monkey, capCall {}).await;
    crate::checkpoints::emission_budget::budget_from_cap(cap).expect("the minter's cap gives a budget")
}
