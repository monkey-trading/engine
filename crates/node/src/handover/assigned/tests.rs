use super::*;

use std::collections::{BTreeMap, VecDeque};
use std::sync::Mutex;

use alloy::primitives::{Bytes, U256};

use crate::handover::discovery::Registered;
use crate::liveness::chain::{rotationStateReturn, setOfCall, setOfReturn};
use crate::standby_key::chain::{
    AssignmentReturn, CandidateReturn, RevisionReturn, TurnReturn, assignmentOfReturn, candidateOfCall, heldTurnCall,
    registryCall, scheduleCall, turnAtReturn,
};

pub(crate) const VAULT: Address = Address::repeat_byte(0x7a);
pub(crate) const REGISTRY: Address = Address::repeat_byte(0x6b);
pub(crate) const SCHEDULE: Address = Address::repeat_byte(0x5c);
pub(crate) const LOG_HEADS: Address = Address::repeat_byte(0x4d);
pub(crate) const CHAIN_ID: u64 = 42161;
/// The block's time, in turn 20,800's day, after its boundary.
const TIME: u64 = 20_800 * 86_400 + 81_000 + 600;
const TURN: u64 = 20_800;

/// A block answering exactly the calls a test scripts, each by its calldata.
#[derive(Default)]
pub(crate) struct Scripted {
    answers: BTreeMap<(Address, Vec<u8>), Vec<u8>>,
}

impl Scripted {
    /// Answers `call` to `to` with `answer`.
    pub(crate) fn answer<C: SolCall>(&mut self, to: Address, call: C, answer: &C::Return) -> &mut Self {
        self.answers.insert((to, call.abi_encode()), C::abi_encode_returns(answer));
        self
    }

    /// A vault naming the registry, which names the schedule, with a turn naming the operator.
    pub(crate) fn scheduled() -> Self {
        let mut block = Self::default();
        block.answer(VAULT, registryCall {}, &REGISTRY).answer(REGISTRY, scheduleCall {}, &SCHEDULE);
        let rotation = rotationStateReturn { enabled: true, release: B256::ZERO, generation: 9, until: 0 };
        block.answer(REGISTRY, rotationStateCall {}, &rotation);
        block
    }
}

impl StandbyChain for Scripted {
    type Block<'a> = &'a Scripted;

    async fn latest(&self) -> anyhow::Result<&Scripted> {
        Ok(self)
    }

    async fn finalized(&self) -> anyhow::Result<&Scripted> {
        Ok(self)
    }
}

impl AgreedBlock for &Scripted {
    fn time(&self) -> u64 {
        TIME
    }

    fn number(&self) -> u64 {
        1_000
    }

    fn hash(&self) -> B256 {
        B256::repeat_byte(0x0b)
    }

    async fn call(&self, operation: &'static str, to: Address, data: Bytes) -> anyhow::Result<Bytes> {
        let answer = self.answers.get(&(to, data.to_vec()));
        answer.cloned().map(Bytes::from).with_context(|| format!("no answer scripted for {operation}"))
    }
}

/// Assignments a test sets, read in order: each read takes the next scripted answer, and the last
/// stands; with none scripted, `None`.
#[derive(Default)]
pub(crate) struct FakeAssignments {
    pub script: Mutex<VecDeque<Option<Assigned>>>,
    pub members: Mutex<Vec<Registered>>,
    pub reads: std::sync::atomic::AtomicUsize,
    /// While set, every read fails, as with no quorum.
    pub failing: std::sync::atomic::AtomicBool,
}

impl FakeAssignments {
    /// Reads answer `assigned` from now on.
    pub(crate) fn set(&self, assigned: Option<Assigned>) {
        *self.script.lock().unwrap() = VecDeque::from([assigned]);
    }

    /// Reads answer each of `answers` in turn, the last standing.
    pub(crate) fn then(&self, answers: impl IntoIterator<Item = Option<Assigned>>) {
        *self.script.lock().unwrap() = answers.into_iter().collect();
    }
}

impl Assignments for FakeAssignments {
    fn assigned(&self) -> BoxFuture<'_, anyhow::Result<Option<Assigned>>> {
        self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let mut script = self.script.lock().unwrap();
        let answer = if script.len() > 1 { script.pop_front().flatten() } else { script.front().copied().flatten() };
        let failing = self.failing.load(std::sync::atomic::Ordering::SeqCst);
        Box::pin(async move { if failing { anyhow::bail!("no quorum") } else { Ok(answer) } })
    }

    fn members<'a>(&'a self, _assigned: &'a Assigned) -> BoxFuture<'a, anyhow::Result<Vec<Registered>>> {
        let members = self.members.lock().unwrap().clone();
        Box::pin(async move { Ok(members) })
    }
}

/// A skip of turn `turn` to `candidate` at `position` in `generation`, running `release`, under signer
/// epoch 1.
pub(crate) fn skip(turn: u64, position: u16, candidate: u8, release: [u8; 32], generation: u64) -> Assigned {
    Assigned {
        binding: Binding { epoch: 1, turn, position, generation },
        boundary: engine_core::turns::boundary(turn).unwrap(),
        reason: Reason::Skip,
        set: 1,
        candidate: AccountId([candidate; 20]),
        admin_key: AccountId([candidate + 1; 20]),
        release,
        head_release: None,
        revised: false,
    }
}

/// Scripts the turn in effect's assignment at `position`, for `reason`, naming `candidate` with the
/// releases `revisions`, under signer epoch 3, with the epoch's newest head's release.
fn script_turn(block: &mut Scripted, (position, reason): (u16, u8), revisions: &[(B256, u64)]) {
    let candidate = Address::repeat_byte(0xc1);
    let named = TurnReturn {
        candidate,
        position,
        reason,
        setId: 2,
        generation: revisions.last().unwrap().1,
        prior: TURN - 1,
        seed: B256::ZERO,
        source: Address::repeat_byte(0xa1),
    };
    block.answer(SCHEDULE, turnAtCall { timestamp: TIME }, &turnAtReturn { turn: TURN, _1: named });
    block.answer(VAULT, signerEpochCall {}, &3);
    let made = AssignmentReturn {
        candidate,
        adminKey: Address::repeat_byte(0xd1),
        setId: 2,
        reason,
        standbyKey: Address::ZERO,
        signerEpoch: 0,
        headSeq: 0,
        headLink: B256::ZERO,
        checkpointSeq: 0,
        checkpointRoot: B256::ZERO,
        at: TIME - 60,
    };
    let revisions =
        revisions.iter().map(|&(release, generation)| RevisionReturn { release, generation }).collect::<Vec<_>>();
    block.answer(SCHEDULE, assignmentOfCall { turn: TURN, position }, &assignmentOfReturn { _0: made, _1: revisions });
    block.answer(LOG_HEADS, headReleaseCall { epoch: 3 }, &B256::repeat_byte(0xee));
}

#[tokio::test]
async fn a_skip_reads_as_its_assignment_with_its_latest_release_and_generation() {
    let mut block = Scripted::scheduled();
    script_turn(&mut block, (4, 1), &[(B256::repeat_byte(1), 7), (B256::repeat_byte(2), 8)]);
    let assigned = read(&&block, CHAIN_ID, VAULT, LOG_HEADS).await.unwrap().expect("a turn names the operator");
    assert_eq!(assigned.binding, Binding { epoch: 3, turn: TURN, position: 4, generation: 8 });
    assert_eq!(assigned.reason, Reason::Skip);
    assert_eq!((assigned.set, assigned.candidate), (2, AccountId([0xc1; 20])));
    assert_eq!(assigned.admin_key, AccountId([0xd1; 20]), "the key the set adopted");
    assert_eq!(assigned.release, [2; 32], "the latest revision's release");
    assert_eq!(assigned.head_release, Some([0xee; 32]));
    assert_eq!(assigned.boundary, engine_core::turns::boundary(TURN).unwrap());
    assert!(assigned.names([2; 32], 8) && !assigned.names([1; 32], 8) && !assigned.names([2; 32], 7));
}

#[tokio::test]
async fn reasons_read_as_the_schedule_numbers_them_and_another_is_refused() {
    for (code, reason) in [(0, Reason::Draw), (1, Reason::Skip), (2, Reason::Promotion)] {
        let mut block = Scripted::scheduled();
        script_turn(&mut block, (0, code), &[(B256::repeat_byte(1), 7)]);
        let assigned = read(&&block, CHAIN_ID, VAULT, LOG_HEADS).await.unwrap().unwrap();
        assert_eq!(assigned.reason, reason);
    }
    let mut block = Scripted::scheduled();
    script_turn(&mut block, (0, 3), &[(B256::repeat_byte(1), 7)]);
    let err = read(&&block, CHAIN_ID, VAULT, LOG_HEADS).await.unwrap_err();
    assert!(format!("{err:#}").contains("reason 3"), "{err:#}");
}

#[tokio::test]
async fn no_schedule_no_turn_and_no_turn_naming_the_operator_read_as_none() {
    let mut block = Scripted::default();
    block.answer(VAULT, registryCall {}, &Address::ZERO);
    assert_eq!(read(&&block, CHAIN_ID, VAULT, LOG_HEADS).await.unwrap(), None, "no registry");
    let mut block = Scripted::scheduled();
    script_turn(&mut block, (0, 0), &[(B256::repeat_byte(1), 7)]);
    let ended = rotationStateReturn { enabled: false, release: B256::ZERO, generation: 0, until: 0 };
    block.answer(REGISTRY, rotationStateCall {}, &ended);
    assert_eq!(read(&&block, CHAIN_ID, VAULT, LOG_HEADS).await.unwrap(), None, "an installation ended the turns");
    let mut block = Scripted::scheduled();
    let none = TurnReturn {
        candidate: Address::ZERO,
        position: 0,
        reason: 0,
        setId: 0,
        generation: 0,
        prior: 0,
        seed: B256::ZERO,
        source: Address::ZERO,
    };
    block.answer(SCHEDULE, turnAtCall { timestamp: TIME }, &turnAtReturn { turn: 0, _1: none });
    assert_eq!(read(&&block, CHAIN_ID, VAULT, LOG_HEADS).await.unwrap(), None, "no turn drawn by the block's time");
}

#[tokio::test]
async fn an_epoch_with_no_v2_head_names_no_head_release() {
    let mut block = Scripted::scheduled();
    script_turn(&mut block, (0, 0), &[(B256::repeat_byte(1), 7)]);
    block.answer(LOG_HEADS, headReleaseCall { epoch: 3 }, &B256::ZERO);
    assert_eq!(read(&&block, CHAIN_ID, VAULT, LOG_HEADS).await.unwrap().unwrap().head_release, None);
}

#[tokio::test]
async fn a_set_reads_each_member_with_its_registration_and_its_assignments_position() {
    let mut block = Scripted::scheduled();
    let (a, b, c) = (Address::repeat_byte(0xa1), Address::repeat_byte(0xb1), Address::repeat_byte(0xc1));
    let set = setOfReturn { epoch: 1, effectiveAt: 0, candidates: vec![a, b, c], weights: vec![U256::from(1); 3] };
    block.answer(SCHEDULE, setOfCall { setId: 2 }, &set);
    // Assignments at positions 1 (c, drawn) and 3 (a, skipped to); none at 0 or 2.
    for position in 0..=3u16 {
        let candidate = match position {
            1 => c,
            3 => a,
            _ => Address::ZERO,
        };
        let made = AssignmentReturn {
            candidate,
            adminKey: Address::ZERO,
            setId: 2,
            reason: 0,
            standbyKey: Address::ZERO,
            signerEpoch: 0,
            headSeq: 0,
            headLink: B256::ZERO,
            checkpointSeq: 0,
            checkpointRoot: B256::ZERO,
            at: 0,
        };
        let call = assignmentOfCall { turn: TURN, position };
        block.answer(SCHEDULE, call, &assignmentOfReturn { _0: made, _1: Vec::new() });
    }
    for (candidate, held, host) in [(a, true, "a.test"), (b, false, "b.test"), (c, true, "c.test")] {
        let registered = CandidateReturn {
            adminKey: Address::ZERO,
            enclave: Address::ZERO,
            bond: U256::ZERO,
            release: B256::repeat_byte(candidate.0[0]),
            registration: 0,
            apiHostname: host.into(),
            handoverEndpoint: format!("{host}:7443"),
        };
        block.answer(REGISTRY, candidateOfCall { candidate }, &registered);
        block.answer(SCHEDULE, heldTurnCall { turn: TURN, candidate }, &held);
    }
    let assigned = Assigned { set: 2, ..skip(TURN, 3, 0xa1, [0; 32], 1) };
    let members = crate::handover::discovery::members(&&block, CHAIN_ID, VAULT, &assigned).await.unwrap();
    assert_eq!(members.len(), 3, "in the set's order");
    assert_eq!(members[0].source.candidate, AccountId([0xa1; 20]));
    assert_eq!(members[0].source.release, [0xa1; 32]);
    assert_eq!(members[0].source.api_hostname, "a.test");
    assert_eq!(members[0].source.handover_endpoint, "a.test:7443");
    assert_eq!(members.iter().map(|member| member.held).collect::<Vec<_>>(), [Some(3), None, Some(1)]);
}

#[test]
fn a_start_takes_the_skip_of_its_turn_naming_its_own_release_and_the_finalised_turn_otherwise() {
    let release = Release { launch_digest: [0x11; 48], config_sha256: [0x22; 32] };
    let finalised = ScheduledTurn {
        turn: TURN,
        boundary: engine_core::turns::boundary(TURN).unwrap(),
        candidate: AccountId([0xb0; 20]),
        admin_key: AccountId([0xb1; 20]),
        release: [0x66; 32],
        generation: 9,
        set: 2,
        source: None,
    };
    let own = skip(TURN, 3, 0xc1, release.id(), 11);
    let started = skip_naming(Some(finalised.clone()), Some(own), &release).unwrap();
    assert_eq!((started.release, started.generation, started.candidate), (release.id(), 11, AccountId([0xc1; 20])));
    assert_eq!(started.boundary, finalised.boundary);
    let others = [
        ("a skip naming another release", Some(skip(TURN, 3, 0xc1, [0x77; 32], 11))),
        ("a skip of another turn", Some(skip(TURN + 1, 3, 0xc1, release.id(), 11))),
        ("a promotion", Some(Assigned { reason: Reason::Promotion, ..own })),
        ("a draw", Some(Assigned { reason: Reason::Draw, ..own })),
        ("no assignment", None),
    ];
    for (what, latest) in others {
        assert_eq!(skip_naming(Some(finalised.clone()), latest, &release), Some(finalised.clone()), "{what}");
    }
    assert_eq!(skip_naming(None, Some(own), &release), None, "no turn in effect at the start");
}

#[test]
fn a_binding_changes_the_report_data_by_each_of_its_fields() {
    let domain = crate::log::CheckpointDomain { chain_id: CHAIN_ID, vault: engine_core::types::AccountId([0x7a; 20]) };
    let base = [5u8; 64];
    let binding = Binding { epoch: 2, turn: TURN, position: 3, generation: 9 };
    let bound = crate::handover::bound_report_data(domain, &base, &binding);
    assert_ne!(bound, base);
    for other in [
        Binding { epoch: 3, ..binding },
        Binding { turn: TURN + 1, ..binding },
        Binding { position: 4, ..binding },
        Binding { generation: 10, ..binding },
    ] {
        assert_ne!(crate::handover::bound_report_data(domain, &base, &other), bound, "{other:?}");
    }
    assert_ne!(crate::handover::bound_report_data(domain, &[6u8; 64], &binding), bound, "the request it binds");
}
