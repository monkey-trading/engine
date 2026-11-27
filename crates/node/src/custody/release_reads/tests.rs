//! The holder's quorum reads: at the checkpoint endpoint's own block once a majority holds it final,
//! never at a block the providers do not name, which pages, and at the agreed head.

use std::sync::{Arc, Mutex as StdMutex};

use alloy::primitives::{B256, Bytes, U256};
use alloy::sol_types::SolCall;
use anyhow::bail;

use super::*;
use crate::checkpoints::{LifecycleEvidence, VaultState, View};
use crate::custody::chain::BlockLink;
use crate::custody::evm::{Finality, StakingEvent, VaultDeposit, VaultIdentity};

const CHAIN_ID: u64 = 42161;
const VAULT: Address = Address::repeat_byte(0x7a);
/// The block the checkpoint endpoint holds final.
const PINNED: u64 = 480;
/// The providers' head.
const HEAD: u64 = 520;

#[tokio::test]
async fn the_quorum_reads_at_the_endpoints_block_once_a_majority_holds_it_final() {
    let calls = Arc::new(StdMutex::new(Vec::new()));
    let reader = reader([PINNED, PINNED + 3, PINNED - 7], &calls);
    let held = reader.held(&observed(PINNED, hash(PINNED)), None).await.unwrap();
    assert_eq!(held.current.id, [0x11; 32], "the vault's current release");
    assert_eq!(held.current.generation, PINNED, "the generation is the pinned block's");
    let calls = calls.lock().unwrap();
    assert!(!calls.is_empty() && calls.iter().all(|at| *at == PINNED), "calls ran at {calls:?}");
}

#[tokio::test]
async fn a_block_the_providers_do_not_name_is_refused_and_pages() {
    let (_guard, mut alerts) = crate::test_support::capture_alerts();
    let calls = Arc::new(StdMutex::new(Vec::new()));
    let reader = reader([PINNED; 3], &calls);
    let err = reader.held(&observed(PINNED, B256::repeat_byte(0xee)), None).await.expect_err("an unnamed block read");
    assert!(format!("{err:#}").contains("name another block"), "{err:#}");
    assert!(calls.lock().unwrap().is_empty(), "a call ran at a block the providers do not name");
    let alert = alerts.try_recv().expect("the disagreement paged");
    let named = (alert.get("alert").map(String::as_str), alert.get("level").map(String::as_str));
    assert_eq!(named, (Some("checkpoint_endpoint_disagrees"), Some("WARN")));
}

#[tokio::test(start_paused = true)]
async fn a_block_a_majority_does_not_hold_final_is_refused_after_the_wait() {
    let calls = Arc::new(StdMutex::new(Vec::new()));
    let reader = reader([PINNED, PINNED - 1, PINNED - 1], &calls);
    let started = Instant::now();
    let err = reader.held(&observed(PINNED, hash(PINNED)), None).await.expect_err("an unfinal block read");
    assert!(format!("{err:#}").contains("hold no block at height 480 final"), "{err:#}");
    assert!(started.elapsed() >= FINAL_WAIT, "refused after {:?}", started.elapsed());
    assert!(calls.lock().unwrap().is_empty(), "a call ran at a block the providers do not hold final");
}

#[tokio::test]
async fn the_rotation_switch_is_read_at_the_endpoints_block_and_at_the_agreed_head() {
    let calls = Arc::new(StdMutex::new(Vec::new()));
    let reader = reader([PINNED; 3], &calls);
    assert_eq!(reader.rotation(&observed(PINNED, hash(PINNED))).await.unwrap(), (false, true));
    // Each call goes to every provider: the registry at the pinned block, then the registry and the
    // switch at the head.
    let calls = calls.lock().unwrap().clone();
    assert_eq!(calls, [[PINNED; 3].as_slice(), &[HEAD; 6]].concat(), "the calls ran at {calls:?}");
}

#[tokio::test]
async fn another_chains_quorum_is_refused() {
    let calls = Arc::new(StdMutex::new(Vec::new()));
    let mut elsewhere = reader([PINNED; 3], &calls);
    elsewhere.chain_id = CHAIN_ID + 1;
    let err = elsewhere.held(&observed(PINNED, hash(PINNED)), None).await.expect_err("another chain answered");
    assert!(format!("{err:#}").contains("serves chain"), "{err:#}");
}

// ── Helpers ──

/// A provider holding `finalized` final at the head `HEAD`, whose vault names release `0x11…` in a
/// generation that is the block's number, and a registry from `HEAD` on, whose switch is on; it records
/// the block of each call.
struct Provider {
    finalized: u64,
    calls: Arc<StdMutex<Vec<u64>>>,
}

impl VaultReader for Provider {
    async fn identity(&self) -> anyhow::Result<VaultIdentity> {
        Ok(VaultIdentity { chain_id: CHAIN_ID, token: Some(Address::repeat_byte(0xc0)) })
    }

    async fn finality(&self) -> anyhow::Result<Finality> {
        let tagged = Some(BlockRef { number: self.finalized, hash: hash(self.finalized) });
        Ok(Finality { head: HEAD, head_time: 1_800_000_000, tagged, head_support: None })
    }

    async fn headers(&self, from: u64, to: u64) -> anyhow::Result<Vec<BlockLink>> {
        let link = |number| BlockLink { number, hash: hash(number), parent_hash: hash(number - 1), timestamp: number };
        Ok((from..=to).map(link).collect())
    }

    async fn deposits(&self, _from: u64, _to: u64) -> anyhow::Result<Vec<VaultDeposit>> {
        bail!("the release reader reads no deposits")
    }

    async fn deposit_count(&self, _at: BlockRef) -> anyhow::Result<u64> {
        bail!("the release reader reads no deposit count")
    }

    async fn staking_events(&self, _from: u64, _to: BlockRef) -> anyhow::Result<Vec<StakingEvent>> {
        bail!("the release reader reads no staking events")
    }

    async fn contract_call(&self, at: BlockRef, to: Address, data: Bytes) -> anyhow::Result<Bytes> {
        anyhow::ensure!(at.hash == hash(at.number), "a call at a block off the chain");
        self.calls.lock().unwrap().push(at.number);
        let registry = if at.number >= HEAD { Address::repeat_byte(0x6b) } else { Address::ZERO };
        let answer = match (to, &data[..4]) {
            (VAULT, selector) if selector == registryCall::SELECTOR => registryCall::abi_encode_returns(&registry),
            (VAULT, selector) if selector == currentReleaseCall::SELECTOR => {
                currentReleaseCall::abi_encode_returns(&B256::repeat_byte(0x11))
            }
            (VAULT, selector) if selector == releaseGenerationCall::SELECTOR => {
                releaseGenerationCall::abi_encode_returns(&at.number)
            }
            (VAULT, selector) if selector == syncedReleaseCall::SELECTOR => {
                syncedReleaseCall::abi_encode_returns(&B256::repeat_byte(0x11))
            }
            (to, selector) if selector == rotationEnabledCall::SELECTOR && to == registry => {
                rotationEnabledCall::abi_encode_returns(&true)
            }
            _ => bail!("an unexpected call"),
        };
        Ok(answer.into())
    }
}

alloy::sol! {
    function registry() external view returns (address);
    function currentRelease() external view returns (bytes32);
    function releaseGeneration() external view returns (uint64);
    function syncedRelease() external view returns (bytes32);
    function rotationEnabled() external view returns (bool);
}

/// A reader over three providers, each holding its block final.
fn reader(finalized: [u64; 3], calls: &Arc<StdMutex<Vec<u64>>>) -> ReleaseReader<Provider> {
    let providers = finalized
        .into_iter()
        .enumerate()
        .map(|(i, finalized)| (format!("p{i}"), Provider { finalized, calls: calls.clone() }))
        .collect();
    ReleaseReader::over(Quorum::new(providers).unwrap(), CHAIN_ID, VAULT)
}

/// A finalized observation the checkpoint endpoint pinned at `height`, with `hash`.
fn observed(height: u64, hash: B256) -> Observation {
    let (zero, signer) = (engine_core::Fixed::ZERO, engine_core::types::AccountId([0x51; 20]));
    let state = VaultState {
        height,
        signer,
        checkpoint: None,
        balance: zero,
        deposits: 0,
        deposited: zero,
        distributions_paid: zero,
    };
    let lifecycle = LifecycleEvidence {
        height,
        block_hash: hash,
        signer,
        signer_epoch: 1,
        withdrawals_paused: false,
        total_withdrawn: zero,
        checkpoint_seq: None,
        checkpoint_root: None,
        approvals: Vec::new(),
    };
    Observation { view: View::Finalized, state, lifecycle: Some(lifecycle) }
}

/// Block `number`'s hash on the tests' chain.
fn hash(number: u64) -> B256 {
    B256::from(U256::from(number))
}
