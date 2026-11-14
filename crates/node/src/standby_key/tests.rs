//! The standby keys' tests, over a fake chain whose answers each test sets: the seal and its opening
//! rule (`seal`), and the vouch (`vouch`).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

use alloy::primitives::{Address, B256, Bytes, U256, keccak256};
use alloy::sol_types::{SolCall, SolValue};
use anyhow::{bail, ensure};
use engine_core::types::Millis;

use super::chain::{
    AgreedBlock, AssignmentReturn, CandidateReturn, RevisionReturn, StandbyChain, StandbyDomain, TurnReturn,
    assignmentOfCall, assignmentOfReturn, candidateOfCall, checkpointSignerCall, promotableCall, registryCall,
    releaseApprovedCall, scheduleCall, signerEpochCall, standbyHolderCall, turnAtCall, turnAtReturn,
};
use super::*;
use crate::tee::sealing::MockSealer;
use crate::test_support::scratch_dir;

mod seal;
mod vouch;

const CHAIN_ID: u64 = 42161;
const VAULT: Address = Address::repeat_byte(0x7a);
const REGISTRY: Address = Address::repeat_byte(0x6b);
const SCHEDULE: Address = Address::repeat_byte(0x5c);
const DOMAIN: StandbyDomain =
    StandbyDomain { chain_id: CHAIN_ID, vault: VAULT, registry: REGISTRY, schedule: SCHEDULE };
/// The member whose standby keys the tests seal.
const CANDIDATE: AccountId = AccountId([0xca; 20]);
const LAUNCH: [u8; 48] = [0x11; 48];
const CONFIG: [u8; 32] = [0xc1; 32];
/// The tests' trusted time, and the latest block's time.
const NOW_MS: Millis = 1_800_000_000_000;

/// What the fake chain answers at its latest block.
struct ChainState {
    /// The latest block's time, in seconds.
    time: u64,
    /// The custody quorum is lost.
    down: bool,
    signer: Address,
    epoch: u64,
    /// Each registered candidate's release.
    releases: BTreeMap<Address, B256>,
    /// The releases the timelock approved, by candidate.
    approved: BTreeSet<(Address, B256)>,
    /// What `promotable` answers, by candidate.
    promotable: BTreeMap<Address, Address>,
    /// What `standbyHolder` answers, by key.
    holders: BTreeMap<Address, Address>,
    /// The turn in effect at `time`, and its latest assignment's member, position, reason and
    /// generation.
    turn: u64,
    named: (Address, u16, u8, u64),
    assignments: BTreeMap<(u64, u16), (AssignmentReturn, Vec<RevisionReturn>)>,
}

/// A chain whose latest block's answers a test sets.
struct FakeChain(Mutex<ChainState>);

/// The fake chain's latest block.
struct FakeBlock<'a>(&'a FakeChain);

impl StandbyChain for FakeChain {
    type Block<'a>
        = FakeBlock<'a>
    where
        Self: 'a;

    async fn latest(&self) -> anyhow::Result<FakeBlock<'_>> {
        ensure!(!self.0.lock().unwrap().down, "custody RPC quorum lost for latest_block");
        Ok(FakeBlock(self))
    }
}

impl AgreedBlock for FakeBlock<'_> {
    fn time(&self) -> u64 {
        self.0.0.lock().unwrap().time
    }

    async fn call(&self, _operation: &'static str, to: Address, data: Bytes) -> anyhow::Result<Bytes> {
        answer(&self.0.0.lock().unwrap(), to, &data).map(Bytes::from)
    }
}

impl FakeChain {
    /// A chain at `NOW_MS` whose vault is in signer epoch 1 under another key, with no candidate
    /// registered and no turn drawn.
    fn new() -> Self {
        Self(Mutex::new(ChainState {
            time: u64::try_from(NOW_MS / 1000).unwrap(),
            down: false,
            signer: Address::repeat_byte(0x51),
            epoch: 1,
            releases: BTreeMap::new(),
            approved: BTreeSet::new(),
            promotable: BTreeMap::new(),
            holders: BTreeMap::new(),
            turn: 0,
            named: (Address::ZERO, 0, 0, 0),
            assignments: BTreeMap::new(),
        }))
    }

    /// Changes what the chain answers.
    fn set(&self, change: impl FnOnce(&mut ChainState)) {
        change(&mut self.0.lock().unwrap());
    }

    /// Registers `candidate` on `release`, approved for it unless `approved` is false.
    fn register(&self, candidate: AccountId, release: [u8; 32], approved: bool) {
        let (candidate, release) = (address(candidate), B256::from(release));
        self.set(|chain| {
            chain.releases.insert(candidate, release);
            if approved {
                chain.approved.insert((candidate, release));
            }
        });
    }

    /// Lists `key` as `candidate`'s promotable standby key.
    fn list_promotable(&self, candidate: AccountId, key: AccountId) {
        self.set(|chain| {
            chain.promotable.insert(address(candidate), address(key));
            chain.holders.insert(address(key), address(candidate));
        });
    }

    /// Promotes `candidate`'s `key` as turn 9's assignment at position 3, in signer epoch 2 and
    /// generation 7, fixing `PROMOTION`'s prefixes: the key is the vault's signer, no longer promotable.
    fn promote(&self, candidate: AccountId, key: AccountId) {
        let promotion = PROMOTION;
        let assignment = AssignmentReturn {
            candidate: address(candidate),
            adminKey: Address::repeat_byte(0xad),
            setId: 1,
            reason: 2,
            standbyKey: address(key),
            signerEpoch: promotion.signer_epoch,
            headSeq: promotion.head_seq,
            headLink: promotion.head_link,
            checkpointSeq: promotion.checkpoint_seq,
            checkpointRoot: promotion.checkpoint_root,
            at: 0,
        };
        let revision = RevisionReturn { release: B256::from(release().id()), generation: promotion.generation };
        self.set(|chain| {
            chain.promotable.remove(&address(candidate));
            chain.holders.insert(address(key), address(candidate));
            (chain.signer, chain.epoch) = (address(key), promotion.signer_epoch);
            chain.turn = promotion.turn;
            chain.named = (address(candidate), promotion.position, 2, promotion.generation);
            chain.assignments.insert((promotion.turn, promotion.position), (assignment, vec![revision]));
        });
    }
}

/// The promotion `FakeChain::promote` makes.
const PROMOTION: Promotion = Promotion {
    turn: 9,
    position: 3,
    signer_epoch: 2,
    generation: 7,
    head_seq: 120,
    head_link: B256::repeat_byte(0x4c),
    checkpoint_seq: 100,
    checkpoint_root: B256::repeat_byte(0x7e),
};

// ── Helpers ──

/// The fake chain's answer to the call of `data` to `to`.
/// Error cases:
/// - a call the fake does not answer, or one that does not decode
fn answer(chain: &ChainState, to: Address, data: &[u8]) -> anyhow::Result<Vec<u8>> {
    fn is<C: SolCall>(data: &[u8]) -> bool {
        data.starts_with(&C::SELECTOR)
    }
    Ok(if to == VAULT && is::<registryCall>(data) {
        registryCall::abi_encode_returns(&REGISTRY)
    } else if to == REGISTRY && is::<scheduleCall>(data) {
        scheduleCall::abi_encode_returns(&SCHEDULE)
    } else if to == VAULT && is::<checkpointSignerCall>(data) {
        checkpointSignerCall::abi_encode_returns(&chain.signer)
    } else if to == VAULT && is::<signerEpochCall>(data) {
        signerEpochCall::abi_encode_returns(&chain.epoch)
    } else if to == REGISTRY && is::<candidateOfCall>(data) {
        let release = chain.releases.get(&candidateOfCall::abi_decode(data)?.candidate);
        candidateOfCall::abi_encode_returns(&CandidateReturn {
            adminKey: if release.is_some() { Address::repeat_byte(0xad) } else { Address::ZERO },
            enclave: Address::ZERO,
            bond: U256::ZERO,
            release: release.copied().unwrap_or_default(),
            registration: 0,
            apiHostname: String::new(),
            handoverEndpoint: String::new(),
        })
    } else if to == REGISTRY && is::<releaseApprovedCall>(data) {
        let approval = releaseApprovedCall::abi_decode(data)?.approval;
        let approved = chain.approved.iter().any(|pair| keccak256(pair.abi_encode()) == approval);
        releaseApprovedCall::abi_encode_returns(&approved)
    } else if to == SCHEDULE && is::<standbyHolderCall>(data) {
        let key = standbyHolderCall::abi_decode(data)?.key;
        standbyHolderCall::abi_encode_returns(&chain.holders.get(&key).copied().unwrap_or_default())
    } else if to == SCHEDULE && is::<promotableCall>(data) {
        let candidate = promotableCall::abi_decode(data)?.candidate;
        promotableCall::abi_encode_returns(&chain.promotable.get(&candidate).copied().unwrap_or_default())
    } else if to == SCHEDULE && is::<turnAtCall>(data) {
        ensure!(turnAtCall::abi_decode(data)?.timestamp == chain.time, "the turn was read at another time");
        let (candidate, position, reason, generation) = chain.named;
        let named = TurnReturn {
            candidate,
            position,
            reason,
            setId: 1,
            generation,
            prior: 0,
            seed: B256::ZERO,
            source: Address::ZERO,
        };
        turnAtCall::abi_encode_returns(&turnAtReturn { turn: chain.turn, _1: named })
    } else if to == SCHEDULE && is::<assignmentOfCall>(data) {
        let call = assignmentOfCall::abi_decode(data)?;
        let (assignment, revisions) = chain
            .assignments
            .get(&(call.turn, call.position))
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no assignment"))?;
        assignmentOfCall::abi_encode_returns(&assignmentOfReturn { _0: assignment, _1: revisions })
    } else {
        bail!("the fake chain does not answer this call to {to}")
    })
}

/// The release the tests' member runs.
fn release() -> Release {
    Release { launch_digest: LAUNCH, config_sha256: CONFIG }
}

/// The tests' platform: the mock TEE's, at `LAUNCH`.
fn sealer() -> Box<dyn Sealer> {
    Box::new(MockSealer { launch_digest: LAUNCH })
}

/// `CANDIDATE`'s standby keys in `dir` under `CONFIG`, as a start finds them on `chain`.
/// Error cases:
/// - see `StandbyKeys::open_or_generate`
async fn start(dir: &Path, chain: &FakeChain) -> anyhow::Result<StandbyStart> {
    StandbyKeys::open_or_generate(dir, sealer(), CONFIG, chain, &DOMAIN, CANDIDATE).await
}

/// The standby keys a start opened or sealed, failing on a promotion.
fn standby(start: StandbyStart) -> (StandbyKeys, bool) {
    match start {
        StandbyStart::Standby { keys, generated } => (keys, generated),
        StandbyStart::Promoted { keys, .. } => panic!("opened {} as a promotion's key", keys.address()),
    }
}

/// New standby keys of `CANDIDATE`'s sealed in `dir` on a chain that lists nothing.
async fn fresh_in(dir: &Path) -> StandbyKeys {
    standby(start(dir, &FakeChain::new()).await.unwrap()).0
}

/// `account` as a chain address.
fn address(account: AccountId) -> Address {
    Address::from(account.0)
}
