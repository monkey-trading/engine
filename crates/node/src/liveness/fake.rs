//! A fake chain for the liveness rule's tests: the answers each test sets for the reads
//! `liveness::chain` makes, at one latest block.

use std::sync::Mutex;

use alloy::primitives::{Address, B256, Bytes, U256};
use alloy::sol_types::SolCall;
use anyhow::bail;
use engine_core::types::AccountId;

use crate::liveness::chain::{
    LivenessDomain, headsCall, headsReturn, keyHolderCall, latestMarkCall, latestMarkReturn, livenessCall,
    livenessReturn, promotionDueAtCall, rotationStateCall, rotationStateReturn, setOfCall, setOfReturn,
};
use crate::standby_key::chain::{
    AgreedBlock, CandidateReturn, StandbyChain, StandbyDomain, TurnReturn, candidateOfCall, checkpointSignerCall,
    promotableCall, signerEpochCall, turnAtCall, turnAtReturn,
};

pub(crate) const VAULT: Address = Address::repeat_byte(0x7a);
pub(crate) const REGISTRY: Address = Address::repeat_byte(0x6b);
pub(crate) const SCHEDULE: Address = Address::repeat_byte(0x5c);
pub(crate) const LOG_HEADS: Address = Address::repeat_byte(0x4d);
pub(crate) const DOMAIN: LivenessDomain = LivenessDomain {
    standby: StandbyDomain { chain_id: 42161, vault: VAULT, registry: REGISTRY, schedule: SCHEDULE },
    log_heads: LOG_HEADS,
};

/// What the fake chain answers at its latest block.
#[derive(Clone, Debug)]
pub(crate) struct ChainState {
    pub time: u64,
    pub number: u64,
    pub down: bool,
    pub epoch: u64,
    pub signer: Address,
    /// The registry's operator generation: zero while no turn names the operator.
    pub generation: u64,
    pub release: B256,
    pub interval: u64,
    pub misses: u16,
    pub due_at: u64,
    pub marked: (u64, u64),
    /// The turn in effect, its member and position; turn zero before any is drawn.
    pub turn: u64,
    pub named: (Address, u16),
    /// The set's members: candidate, handover endpoint, promotable standby key.
    pub members: Vec<(Address, String, Address)>,
    /// The epoch's stored head: seq, signing time, posted-at.
    pub head: (u64, u64, u64),
}

/// A chain whose latest block's answers a test sets.
pub(crate) struct FakeChain(pub Mutex<ChainState>);

/// The fake chain's latest block.
pub(crate) struct FakeBlock<'a>(&'a FakeChain);

impl Default for ChainState {
    /// Turn 20,400 names the operator at `named`, in epoch 1, on a 60 s interval of 5 misses, at
    /// block 100 and second 1,800,000,000.
    fn default() -> Self {
        Self {
            time: 1_800_000_000,
            number: 100,
            down: false,
            epoch: 1,
            signer: Address::repeat_byte(0x51),
            generation: 3,
            release: B256::repeat_byte(0x5e),
            interval: 60,
            misses: 5,
            due_at: 1_800_000_300,
            marked: (29_999_999, 99),
            turn: 20_400,
            named: (Address::repeat_byte(0xa1), 0),
            members: Vec::new(),
            head: (0, 0, 0),
        }
    }
}

impl FakeChain {
    /// A chain answering `state`.
    pub(crate) fn new(state: ChainState) -> Self {
        Self(Mutex::new(state))
    }

    /// Changes what the chain answers.
    pub(crate) fn set(&self, change: impl FnOnce(&mut ChainState)) {
        change(&mut self.0.lock().unwrap());
    }
}

impl StandbyChain for FakeChain {
    type Block<'a>
        = FakeBlock<'a>
    where
        Self: 'a;

    async fn latest(&self) -> anyhow::Result<FakeBlock<'_>> {
        anyhow::ensure!(!self.0.lock().unwrap().down, "custody RPC quorum lost for latest_block");
        Ok(FakeBlock(self))
    }
}

impl AgreedBlock for FakeBlock<'_> {
    fn time(&self) -> u64 {
        self.0.0.lock().unwrap().time
    }

    fn number(&self) -> u64 {
        self.0.0.lock().unwrap().number
    }

    fn hash(&self) -> B256 {
        B256::from(U256::from(self.number()))
    }

    async fn call(&self, _operation: &'static str, to: Address, data: Bytes) -> anyhow::Result<Bytes> {
        answer(&self.0.0.lock().unwrap(), to, &data).map(Bytes::from)
    }
}

/// The fake chain's answer to the call of `data` to `to`.
/// Error cases:
/// - a call the fake does not answer, or one that does not decode
fn answer(chain: &ChainState, to: Address, data: &[u8]) -> anyhow::Result<Vec<u8>> {
    fn is<C: SolCall>(data: &[u8]) -> bool {
        data.starts_with(&C::SELECTOR)
    }
    Ok(if to == VAULT && is::<signerEpochCall>(data) {
        signerEpochCall::abi_encode_returns(&chain.epoch)
    } else if to == VAULT && is::<checkpointSignerCall>(data) {
        checkpointSignerCall::abi_encode_returns(&chain.signer)
    } else if to == REGISTRY && is::<rotationStateCall>(data) {
        rotationStateCall::abi_encode_returns(&rotationStateReturn {
            enabled: true,
            release: chain.release,
            generation: chain.generation,
            until: 0,
        })
    } else if to == SCHEDULE && is::<livenessCall>(data) {
        livenessCall::abi_encode_returns(&livenessReturn {
            heartbeatInterval: chain.interval,
            headTolerance: 30,
            heartbeatMisses: chain.misses,
            headBlocks: 200,
        })
    } else if to == SCHEDULE && is::<promotionDueAtCall>(data) {
        promotionDueAtCall::abi_encode_returns(&chain.due_at)
    } else if to == SCHEDULE && is::<latestMarkCall>(data) {
        latestMarkCall::abi_encode_returns(&latestMarkReturn { slot: chain.marked.0, blockNumber: chain.marked.1 })
    } else if to == SCHEDULE && is::<turnAtCall>(data) {
        let named = TurnReturn {
            candidate: chain.named.0,
            position: chain.named.1,
            reason: 0,
            setId: 1,
            generation: chain.generation,
            prior: 0,
            seed: B256::ZERO,
            source: Address::ZERO,
        };
        turnAtCall::abi_encode_returns(&turnAtReturn { turn: chain.turn, _1: named })
    } else if to == SCHEDULE && is::<setOfCall>(data) {
        let candidates = chain.members.iter().map(|(candidate, ..)| *candidate).collect();
        let weights = chain.members.iter().map(|_| U256::from(1)).collect();
        setOfCall::abi_encode_returns(&setOfReturn { epoch: 1, effectiveAt: 0, candidates, weights })
    } else if to == REGISTRY && is::<candidateOfCall>(data) {
        let candidate = candidateOfCall::abi_decode(data)?.candidate;
        let endpoint = chain.members.iter().find(|(member, ..)| *member == candidate).map(|(_, endpoint, _)| endpoint);
        candidateOfCall::abi_encode_returns(&CandidateReturn {
            adminKey: Address::repeat_byte(0xad),
            enclave: Address::ZERO,
            bond: U256::ZERO,
            release: chain.release,
            registration: 0,
            apiHostname: String::new(),
            handoverEndpoint: endpoint.cloned().unwrap_or_default(),
        })
    } else if to == SCHEDULE && is::<promotableCall>(data) {
        let candidate = promotableCall::abi_decode(data)?.candidate;
        let key = chain.members.iter().find(|(member, ..)| *member == candidate).map(|(.., key)| *key);
        promotableCall::abi_encode_returns(&key.unwrap_or_default())
    } else if to == REGISTRY && is::<keyHolderCall>(data) {
        keyHolderCall::abi_encode_returns(&chain.named.0)
    } else if to == LOG_HEADS && is::<headsCall>(data) {
        let (seq, signed_at, posted_at) = chain.head;
        headsCall::abi_encode_returns(&headsReturn {
            seq,
            link: B256::from(U256::from(seq)),
            signedAt: signed_at,
            boundBlock: 0,
            postedAt: posted_at,
        })
    } else {
        bail!("the fake chain does not answer this call to {to}")
    })
}

/// A member's account: `byte` repeated.
pub(crate) fn account(byte: u8) -> AccountId {
    AccountId([byte; 20])
}

#[tokio::test]
async fn the_view_reads_every_part_at_one_block() {
    let chain = FakeChain::new(ChainState::default());
    let view = crate::liveness::chain::view(&chain.latest().await.unwrap(), &DOMAIN).await.unwrap();
    assert!(view.turn_names_operator());
    assert_eq!((view.time, view.number, view.epoch, view.due_at), (1_800_000_000, 100, 1, 1_800_000_300));
    assert_eq!((view.rule.interval, view.rule.misses, view.marked_slot, view.marked_block), (60, 5, 29_999_999, 99));
    let named = view.named.unwrap();
    assert_eq!((named.turn, named.candidate, named.set, named.generation), (20_400, account(0xa1), 1, 3));
    chain.set(|chain| chain.generation = 0);
    let view = crate::liveness::chain::view(&chain.latest().await.unwrap(), &DOMAIN).await.unwrap();
    assert!(!view.turn_names_operator(), "a zero generation: no turn names the operator");
}

impl StandbyChain for std::sync::Arc<FakeChain> {
    type Block<'a>
        = FakeBlock<'a>
    where
        Self: 'a;

    async fn latest(&self) -> anyhow::Result<FakeBlock<'_>> {
        self.as_ref().latest().await
    }
}
