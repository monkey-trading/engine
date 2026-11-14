//! The scenario node's chain-facing pieces: the checkpoint worker and the custody section.

use std::time::Duration;

use engine_core::Fixed;
use engine_core::staking::StakingConfig;
use engine_core::types::{AccountId, Millis};

use super::super::chain::Chain;
use super::super::{COOLDOWN_SECS, DISTRIBUTION_EPOCH_SECS, T0_SECS};
use crate::checkpoints::{CheckpointRpc, Poster};
use crate::config::{CheckpointSection, CustodySection, FinalityTag, Genesis, RpcProviderSection};
use crate::log::CheckpointDomain;
use crate::tee::EnclaveKey;

/// The checkpoint worker posting to the chain's vault at the latest block.
pub(super) fn poster(chain: &Chain, dir: &std::path::Path, key: &EnclaveKey) -> Poster<CheckpointRpc> {
    let section: CheckpointSection = toml::from_str(&format!(
        "rpc_url = {:?}\nrelayer = '{:#x}'\nretry_interval = '40ms'\nrpc_timeout = '2s'",
        chain.url, chain.accounts[9]
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
        relayer_gas: None,
    }
}

/// The governance the chain's vote contract runs, when it has one.
pub(super) fn governance(chain: &Chain) -> Option<engine_core::governance::GovernanceConfig> {
    chain.vote.map(|(vote, ttl)| engine_core::governance::GovernanceConfig {
        vote: AccountId(vote.into()),
        restriction_ttl_ms: ttl,
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
    evm.finality_tag = FinalityTag::Latest;
    evm.withdrawal_finality_tag = chain.release_tag;
    evm.confirmations = 0;
    evm.start_block = 0;
    evm.poll_interval = Duration::from_millis(30);
    evm.cursor_path = dir.join("custody.json");
    evm.staking = Some(
        toml::from_str(&format!(
            "contract = '{:#x}'\ncursor_path = '{}'\n[registry]\ncontract = '{:#x}'{}{}{}",
            chain.staking,
            dir.join("staking.json").display(),
            chain.registry,
            chain.lens.map(|lens| format!("\nlens = '{lens:#x}'")).unwrap_or_default(),
            chain.schedule.map(|schedule| format!("\nschedule = '{schedule:#x}'")).unwrap_or_default(),
            chain
                .vote
                .map(|(vote, ttl)| format!("\n[registry.vote]\ncontract = '{vote:#x}'\nrestriction_ttl = '{ttl}ms'"))
                .unwrap_or_default()
        ))
        .unwrap(),
    );
    custody.chain_id = 31337;
    custody.evm = Some(evm);
    custody
}

/// `genesis` for the scenarios: no house capital, the flat 10% profit fee (the holding-time fee has a
/// scenario of its own), and staking from the scenarios' start.
pub(super) fn scenario_genesis(mut genesis: Genesis) -> Genesis {
    genesis.house_capital = Fixed::ZERO;
    genesis.params.profit_fee_curve.clear();
    genesis.staking = Some(StakingConfig {
        liquidity_floor: Fixed::from_int(1_000_000),
        epoch_ms: DISTRIBUTION_EPOCH_SECS as Millis * 1000,
        start_ms: T0_SECS as Millis * 1000,
        cooldown_ms: COOLDOWN_SECS as Millis * 1000,
    });
    genesis
}
