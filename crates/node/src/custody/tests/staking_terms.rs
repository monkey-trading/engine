//! The staking contract's epoch and cooldown against `InitStaking`: what the quorum reads decodes, and
//! a command that differs in either value is refused, as is one whose contract cannot be read.

use alloy::primitives::{Address, Bytes, U256};
use alloy::sol_types::SolCall;
use engine_core::Fixed;
use engine_core::staking::StakingConfig;

use crate::custody::chain::BlockLink;
use crate::custody::evm::{BlockRef, Finality, StakingEvent, VaultDeposit, VaultIdentity};
use crate::custody::staking_terms::{ContractTerms, Refused, StakingTerms, check, cooldownCall, epochCall, read};
use crate::custody::tests::fakes::FakeChain;
use crate::custody::{Quorum, VaultReader};

const DAY_MS: i64 = 86_400_000;
const CONTRACT: ContractTerms = ContractTerms { epoch_secs: 86_400, cooldown_secs: 2_419_200 };

/// A provider over a fake chain whose staking contract holds the epoch and cooldown it is given, or
/// no contract.
#[derive(Clone)]
struct Provider {
    chain: FakeChain,
    terms: Option<(u64, u64)>,
}

impl VaultReader for Provider {
    async fn identity(&self) -> anyhow::Result<VaultIdentity> {
        self.chain.identity().await
    }

    async fn finality(&self) -> anyhow::Result<Finality> {
        self.chain.finality().await
    }

    async fn headers(&self, from: u64, to: u64) -> anyhow::Result<Vec<BlockLink>> {
        self.chain.headers(from, to).await
    }

    async fn deposits(&self, _: u64, _: u64) -> anyhow::Result<Vec<VaultDeposit>> {
        unreachable!("the terms read no deposits")
    }

    async fn deposit_count(&self, _: BlockRef) -> anyhow::Result<u64> {
        unreachable!("the terms read no deposit count")
    }

    async fn staking_events(&self, _: &[BlockRef]) -> anyhow::Result<Vec<StakingEvent>> {
        unreachable!("the terms read no events")
    }

    async fn contract_call(&self, _: BlockRef, _: Address, data: Bytes) -> anyhow::Result<Bytes> {
        let Some((epoch, cooldown)) = self.terms else { return Ok(Bytes::new()) };
        let selector = &data[..4];
        if selector == epochCall::SELECTOR {
            return Ok(epochCall::abi_encode_returns(&U256::from(epoch)).into());
        }
        assert_eq!(selector, cooldownCall::SELECTOR, "only the epoch and the cooldown are read");
        Ok(cooldownCall::abi_encode_returns(&U256::from(cooldown)).into())
    }
}

fn quorum(terms: Option<(u64, u64)>) -> Quorum<Provider> {
    let providers = (0..3)
        .map(|i| {
            let chain = FakeChain::default();
            {
                let mut state = chain.0.lock().unwrap();
                state.head = 5;
                state.final_block = Some(5);
            }
            (format!("p{i}"), Provider { chain, terms })
        })
        .collect();
    Quorum::new(providers).unwrap()
}

fn init(epoch_ms: i64, cooldown_ms: i64) -> StakingConfig {
    StakingConfig { liquidity_floor: Fixed::from_int(30_500_000), epoch_ms, start_ms: 0, cooldown_ms }
}

#[tokio::test]
async fn the_quorums_read_decodes_the_contracts_epoch_and_cooldown() {
    let held = read(&quorum(Some((86_400, 2_419_200))), Address::repeat_byte(1)).await.unwrap();
    assert_eq!(held, CONTRACT);

    // An address with no staking contract answers nothing, and a value past a u64 does not fit.
    let none = format!("{:#}", read(&quorum(None), Address::repeat_byte(1)).await.unwrap_err());
    assert!(none.contains("no staking contract"), "{none}");
    let huge = read(&quorum(Some((u64::MAX, 1))), Address::repeat_byte(1)).await.unwrap();
    assert_eq!(huge.epoch_secs, u64::MAX, "a u64 of seconds reads back; the check then refuses it as milliseconds");
    assert!(check(huge, &init(DAY_MS, 1_000)).is_err());
}

#[test]
fn only_the_contracts_epoch_and_cooldown_in_milliseconds_pass() {
    assert_eq!(check(CONTRACT, &init(DAY_MS, 28 * DAY_MS)), Ok(()));
    let epoch = check(CONTRACT, &init(DAY_MS / 48, 28 * DAY_MS)).unwrap_err();
    assert_eq!(epoch, "epoch 1800000 ms differs from the staking contract's 86400 s");
    let cooldown = check(CONTRACT, &init(DAY_MS, DAY_MS)).unwrap_err();
    assert_eq!(cooldown, "cooldown 86400000 ms differs from the staking contract's 2419200 s");
    // Seconds read as milliseconds is the likely slip.
    assert!(check(CONTRACT, &init(86_400, 2_419_200)).is_err());
}

#[tokio::test]
async fn a_reader_refuses_a_difference_and_an_unreadable_contract_apart() {
    let held = StakingTerms::fixed(Some((86_400, 2_419_200)));
    assert!(held.check(&init(DAY_MS, 28 * DAY_MS)).await.is_ok());
    let differs = held.check(&init(DAY_MS, 7 * DAY_MS)).await.unwrap_err();
    assert!(matches!(&differs, Refused::Differs(reason) if reason.starts_with("cooldown 604800000 ms")), "{differs}");
    let unread = Refused::Unread(anyhow::anyhow!("x"));
    assert!(matches!(StakingTerms::fixed(None).check(&init(DAY_MS, 28 * DAY_MS)).await, Err(Refused::Unread(_))));
    assert!(unread.to_string().contains("cannot read the staking contract"), "{unread}");
}
