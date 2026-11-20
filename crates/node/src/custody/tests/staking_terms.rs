//! The staking contract's epoch and cooldown against `InitStaking`: what the quorum reads decodes, and
//! a command that differs in either value is refused, as is one whose contract cannot be read.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use alloy::primitives::{Address, Bytes, U256};
use alloy::sol_types::SolCall;
use engine_core::Fixed;
use engine_core::staking::StakingConfig;

use crate::custody::chain::BlockLink;
use crate::custody::evm::{BlockRef, Finality, StakingEvent, VaultDeposit, VaultIdentity};
use crate::custody::staking_terms::{ContractTerms, Refused, StakingTerms, check, cooldownCall, epochCall, read};
use crate::custody::tests::fakes::{CHAIN_ID, FakeChain, USDC_TOKEN};
use crate::custody::{Quorum, VaultReader};

const DAY_MS: i64 = 86_400_000;
const CONTRACT: ContractTerms = ContractTerms { epoch_secs: 86_400, cooldown_secs: 2_419_200 };

/// A provider over a fake chain whose staking contract holds the epoch and cooldown it is given, or
/// no contract.
#[derive(Clone)]
struct Provider {
    chain: FakeChain,
    terms: Option<(U256, U256)>,
    /// The identity this provider names after its first answer, when it changes network.
    then: Option<VaultIdentity>,
    /// Identity answers this provider has given.
    answered: Arc<AtomicUsize>,
    /// Identity answers every provider has given.
    identities: Arc<AtomicUsize>,
}

impl VaultReader for Provider {
    async fn identity(&self) -> anyhow::Result<VaultIdentity> {
        self.identities.fetch_add(1, Ordering::SeqCst);
        match self.then {
            Some(then) if self.answered.fetch_add(1, Ordering::SeqCst) > 0 => Ok(then),
            _ => self.chain.identity().await,
        }
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

    async fn staking_events(&self, _: u64, _: BlockRef) -> anyhow::Result<Vec<StakingEvent>> {
        unreachable!("the terms read no events")
    }

    async fn contract_call(&self, _: BlockRef, _: Address, data: Bytes) -> anyhow::Result<Bytes> {
        let Some((epoch, cooldown)) = self.terms else { return Ok(Bytes::new()) };
        let selector = &data[..4];
        if selector == epochCall::SELECTOR {
            return Ok(epochCall::abi_encode_returns(&epoch).into());
        }
        assert_eq!(selector, cooldownCall::SELECTOR, "only the epoch and the cooldown are read");
        Ok(cooldownCall::abi_encode_returns(&cooldown).into())
    }
}

fn quorum(terms: Option<(U256, U256)>) -> Quorum<Provider> {
    quorum_on(terms, None)
}

/// A quorum whose providers serve `identity`, the configured deployment's when `None`.
fn quorum_on(terms: Option<(U256, U256)>, identity: Option<VaultIdentity>) -> Quorum<Provider> {
    changing(terms, identity, None).0
}

/// `quorum_on`, whose providers name `then` from their second identity answer on, and the number of
/// identity answers they have given between them.
fn changing(
    terms: Option<(U256, U256)>,
    identity: Option<VaultIdentity>,
    then: Option<VaultIdentity>,
) -> (Quorum<Provider>, Arc<AtomicUsize>) {
    let identities = Arc::new(AtomicUsize::new(0));
    let providers = (0..3)
        .map(|i| {
            let chain = FakeChain::default();
            {
                let mut state = chain.0.lock().unwrap();
                state.head = 5;
                state.final_block = Some(5);
                state.identity = identity;
            }
            (format!("p{i}"), Provider { chain, terms, then, answered: Arc::default(), identities: identities.clone() })
        })
        .collect();
    (Quorum::new(providers).unwrap(), identities)
}

fn init(epoch_ms: i64, cooldown_ms: i64) -> StakingConfig {
    StakingConfig { liquidity_floor: Fixed::from_int(30_500_000), epoch_ms, start_ms: 0, cooldown_ms }
}

const CHAIN: (u64, Address) = (CHAIN_ID, USDC_TOKEN);

fn secs(epoch: u64, cooldown: u64) -> Option<(U256, U256)> {
    Some((U256::from(epoch), U256::from(cooldown)))
}

#[tokio::test]
async fn the_quorums_read_decodes_the_contracts_epoch_and_cooldown() {
    let held = read(&quorum(secs(86_400, 2_419_200)), Address::repeat_byte(1), CHAIN).await.unwrap();
    assert_eq!(held, CONTRACT);

    // An address with no staking contract answers nothing, and a value past a u64 does not fit.
    let none = format!("{:#}", read(&quorum(None), Address::repeat_byte(1), CHAIN).await.unwrap_err());
    assert!(none.contains("no staking contract"), "{none}");
    let past = Some((U256::from(u64::MAX) + U256::from(1), U256::from(1)));
    let huge = format!("{:#}", read(&quorum(past), Address::repeat_byte(1), CHAIN).await.unwrap_err());
    assert!(huge.contains("epoch is huge"), "{huge}");
    let top = read(&quorum(secs(u64::MAX, 1)), Address::repeat_byte(1), CHAIN).await.unwrap();
    assert!(check(top, &init(DAY_MS, 1_000)).is_err(), "a u64 of seconds reads back, and is no milliseconds i64");
}

#[tokio::test]
async fn providers_that_agree_on_another_chain_or_token_than_the_configured_ones_are_refused() {
    let held = secs(86_400, 2_419_200);
    let elsewhere = VaultIdentity { chain_id: CHAIN_ID + 1, token: Some(USDC_TOKEN) };
    let other_token = VaultIdentity { chain_id: CHAIN_ID, token: Some(Address::repeat_byte(1)) };
    for identity in [elsewhere, other_token] {
        let refused =
            format!("{:#}", read(&quorum_on(held, Some(identity)), Address::repeat_byte(1), CHAIN).await.unwrap_err());
        assert!(refused.contains("not the configured chain"), "{refused}");
    }
}

#[tokio::test]
async fn one_identity_vote_admits_the_providers_and_is_the_one_checked() {
    let held = secs(86_400, 2_419_200);
    // Providers that change network after their first answer: the vote that was checked is the one that
    // admitted them, and no second vote follows to admit them on another chain unchecked.
    let elsewhere = VaultIdentity { chain_id: CHAIN_ID + 1, token: Some(USDC_TOKEN) };
    let (quorum, identities) = changing(held, None, Some(elsewhere));
    read(&quorum, Address::repeat_byte(1), CHAIN).await.unwrap();
    assert_eq!(identities.load(Ordering::SeqCst), 3, "each of three providers answered one identity round");
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

#[tokio::test]
async fn checks_that_overlap_read_the_contract_one_at_a_time() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let (running, peak) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let terms = {
        let (running, peak) = (running.clone(), peak.clone());
        StakingTerms::new(move || {
            let (running, peak) = (running.clone(), peak.clone());
            async move {
                peak.fetch_max(running.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst);
                for _ in 0..8 {
                    tokio::task::yield_now().await;
                }
                running.fetch_sub(1, Ordering::SeqCst);
                Ok(CONTRACT)
            }
        })
    };
    let config = init(DAY_MS, 28 * DAY_MS);
    for checked in futures_util::future::join_all((0..4).map(|_| terms.check(&config))).await {
        checked.unwrap();
    }
    assert_eq!(peak.load(Ordering::SeqCst), 1, "two reads were in flight together");
}
