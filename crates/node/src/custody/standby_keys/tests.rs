use std::sync::{Arc, Mutex as StdMutex};

use alloy::primitives::{B256, U256};
use alloy::sol_types::SolCall;
use anyhow::bail;

use super::*;
use crate::custody::chain::BlockLink;
use crate::custody::evm::{Finality, PostedHead, StakingEvent, VaultDeposit, VaultIdentity};
use crate::standby_key::chain::StandbyDomain;

const CHAIN_ID: u64 = 42161;
const VAULT: Address = Address::repeat_byte(0x7a);
const REGISTRY: Address = Address::repeat_byte(0x6b);
const SCHEDULE: Address = Address::repeat_byte(0x5c);
const LOG_HEADS: Address = Address::repeat_byte(0x10);
/// The block a strict majority of the tests' providers has reached, and its time.
const AGREED: u64 = 501;
const AGREED_TIME: u64 = 1_800_000_000;

#[tokio::test]
async fn reads_run_at_the_highest_block_a_majority_agrees_on_with_its_header_time() {
    let calls = Arc::new(StdMutex::new(Vec::new()));
    let reader = reader([(AGREED - 1, REGISTRY), (AGREED, REGISTRY), (AGREED + 1, REGISTRY)], &calls);
    let block = reader.latest().await.unwrap();
    assert_eq!(block.time(), AGREED_TIME, "the time is the agreed block's");
    drop(block);

    let domain = StandbyDomain::read(&reader, CHAIN_ID, VAULT).await.unwrap();
    assert_eq!(domain, StandbyDomain { chain_id: CHAIN_ID, vault: VAULT, registry: REGISTRY, schedule: SCHEDULE });
    let calls = calls.lock().unwrap();
    assert!(!calls.is_empty() && calls.iter().all(|at| *at == AGREED), "calls ran at {calls:?}");
}

#[tokio::test]
async fn a_minority_cannot_change_an_answer_and_a_split_or_another_chain_refuses() {
    let calls = Arc::new(StdMutex::new(Vec::new()));
    let liar = Address::repeat_byte(0x66);
    let outvoted = reader([(AGREED, REGISTRY), (AGREED, liar), (AGREED, REGISTRY)], &calls);
    assert_eq!(StandbyDomain::read(&outvoted, CHAIN_ID, VAULT).await.unwrap().registry, REGISTRY);

    let split = reader([(AGREED, REGISTRY), (AGREED, liar), (AGREED, Address::repeat_byte(0x67))], &calls);
    assert!(StandbyDomain::read(&split, CHAIN_ID, VAULT).await.is_err(), "a split vote answered");

    let mut elsewhere = reader([(AGREED, REGISTRY); 3], &calls);
    elsewhere.chain_id = CHAIN_ID + 1;
    let err = elsewhere.latest().await.err().expect("another chain's quorum answered");
    assert!(format!("{err:#}").contains("serves chain"), "{err:#}");
}

// ── Helpers ──

/// A provider at `tip` whose vault names `registry`, recording the block of each call.
struct Provider {
    tip: u64,
    registry: Address,
    calls: Arc<StdMutex<Vec<u64>>>,
}

impl VaultReader for Provider {
    async fn identity(&self) -> anyhow::Result<VaultIdentity> {
        Ok(VaultIdentity { chain_id: CHAIN_ID, token: Some(Address::repeat_byte(0xc0)) })
    }

    async fn finality(&self) -> anyhow::Result<Finality> {
        bail!("the standby reader reads no finality")
    }

    async fn headers(&self, from: u64, to: u64) -> anyhow::Result<Vec<BlockLink>> {
        Ok((from..=to)
            .map(|number| BlockLink {
                number,
                hash: hash(number),
                parent_hash: hash(number - 1),
                timestamp: AGREED_TIME + number - AGREED,
            })
            .collect())
    }

    async fn deposits(&self, _from: u64, _to: u64) -> anyhow::Result<Vec<VaultDeposit>> {
        bail!("the standby reader reads no deposits")
    }

    async fn deposit_count(&self, _at: BlockRef) -> anyhow::Result<u64> {
        bail!("the standby reader reads no deposit count")
    }

    async fn staking_events(&self, _blocks: &[BlockRef]) -> anyhow::Result<Vec<StakingEvent>> {
        bail!("the standby reader reads no staking events")
    }

    async fn latest_block(&self) -> anyhow::Result<(u64, B256)> {
        Ok((self.tip, hash(self.tip)))
    }

    async fn log_head(&self, contract: Address, _at: BlockRef) -> anyhow::Result<PostedHead> {
        anyhow::ensure!(contract == LOG_HEADS, "read another LogHeads contract");
        Ok(PostedHead { epoch: 1, head: None })
    }

    async fn contract_call(&self, at: BlockRef, to: Address, data: Bytes) -> anyhow::Result<Bytes> {
        anyhow::ensure!(at.hash == hash(at.number), "a call at a block off the chain");
        self.calls.lock().unwrap().push(at.number);
        let answer = match (to, &data[..4]) {
            (VAULT, selector) if selector == registryCall::SELECTOR => registryCall::abi_encode_returns(&self.registry),
            (registry, selector) if selector == scheduleCall::SELECTOR && registry == self.registry => {
                scheduleCall::abi_encode_returns(&SCHEDULE)
            }
            _ => bail!("an unexpected call"),
        };
        Ok(answer.into())
    }
}

alloy::sol! {
    function registry() external view returns (address);
    function schedule() external view returns (address);
}

/// A reader over three providers, each at its tip naming its registry.
fn reader(providers: [(u64, Address); 3], calls: &Arc<StdMutex<Vec<u64>>>) -> StandbyReader<Provider> {
    let providers = providers
        .into_iter()
        .enumerate()
        .map(|(i, (tip, registry))| (format!("p{i}"), Provider { tip, registry, calls: calls.clone() }))
        .collect();
    let quorum = Quorum::new(providers).unwrap().named("standby_keys", false);
    StandbyReader { quorum, log_heads: LOG_HEADS, chain_id: CHAIN_ID, session: Mutex::new(()) }
}

/// Block `number`'s hash on the tests' chain.
fn hash(number: u64) -> B256 {
    B256::from(U256::from(number))
}
