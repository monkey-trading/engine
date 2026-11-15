//! The operator's side of the liveness rule: its start check, confirmations, the lag stop and the
//! fence, against a fake chain, fake members and a fake sequencer.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use alloy::primitives::{Address, B256};
use anyhow::Context;
use engine_core::types::{AccountId, Millis};
use tokio::sync::{mpsc, watch};

use super::*;
use crate::clock::test_clock::TestClock;
use crate::liveness::DELTA;
use crate::liveness::Status;
use crate::liveness::chain::Rule;
use crate::liveness::fake::{ChainState, DOMAIN, FakeChain, account};
use crate::liveness::link::{Reply, Request};
use crate::log_heads::Head;
use crate::seed_relay::schedule::next_boundary;
use crate::tee::EnclaveKey;
use crate::test_support::capture_alerts;
use crate::trusted_time::Sample;

/// True time at the tests' trusted time sample, which bounds it to ±3 s.
const TRUE_MS: Millis = 1_800_000_000_000;
/// The operator's candidate, and a following member's.
const OPERATOR: u8 = 0xa1;
const MEMBER: u8 = 0xb2;
/// How often the rule polls in these tests.
const TEST_POLL: Duration = Duration::from_millis(50);
/// The enclave key the assessed log's commands are sealed by.
const OWN: AccountId = AccountId([0x0e; 20]);

/// A member as a test sets it: its candidate, its standby key, and its status, if durable.
type Answering = (AccountId, Arc<EnclaveKey>, Option<Status>);

/// Members that answer their status as a test sets it, by endpoint.
#[derive(Clone, Default)]
struct Members(Arc<Mutex<BTreeMap<String, Answering>>>, Arc<Mutex<BTreeMap<String, Duration>>>);

impl Asking for Members {
    async fn ask(&self, endpoint: &str, request: &Request) -> anyhow::Result<Reply> {
        let delay = self.1.lock().unwrap().get(endpoint).copied();
        if let Some(delay) = delay {
            tokio::time::sleep(delay).await;
        }
        let (member, key, status) = self.0.lock().unwrap().get(endpoint).cloned().context("unreachable")?;
        let Request::Status { epoch, nonce } = request else { anyhow::bail!("only statuses") };
        let status = Status { epoch: *epoch, ..status.context("the copy is not durable")? };
        let digest = status.digest(DOMAIN.standby.chain_id, DOMAIN.standby.vault, member, &nonce.0);
        Ok(Reply::Status { member, status, signature: key.sign_digest(digest).parse()? })
    }
}

/// What a test drives and reads of a running operator rule.
struct Rig {
    chain: Arc<FakeChain>,
    members: Members,
    gate: Gate,
    signing: watch::Receiver<Signing>,
    /// Kept open: the rule stops once the released-head watch closes.
    _released: watch::Sender<Released>,
    clock: Arc<TestClock>,
    /// This log's prefixes, as the fake sequencer answers them.
    prefixes: Arc<Mutex<BTreeMap<u64, B256>>>,
    task: tokio::task::JoinHandle<anyhow::Result<()>>,
}

#[test]
fn the_rule_refuses_a_window_within_the_stop_margin_and_a_ticker_slower_than_half_an_interval() {
    let rule = Rule { interval: 60, tolerance: 30, misses: 5, blocks: 200 };
    assert_eq!(refused(rule, Duration::from_millis(250)), None);
    assert!(refused(rule, Duration::from_secs(31)).is_some_and(|reason| reason.contains("ticker")));
    assert_eq!(refused(rule, Duration::from_secs(30)), None, "exactly half an interval");
    let narrow = Rule { interval: 25, misses: 3, ..rule };
    assert!(refused(narrow, Duration::from_millis(250)).is_some_and(|reason| reason.contains("margin")));
}

#[test]
fn the_log_lags_once_it_runs_an_interval_of_sequenced_time_unconfirmed() {
    let rule = Rule { interval: 60, tolerance: 30, misses: 5, blocks: 200 };
    let released = |stamp| Released { head: Head::default(), stamp };
    let confirmed = Signable { released: released(1_000_000), confirmer: Address::ZERO, scope: None };
    assert!(!lagging(released(1_060_000), Some(confirmed), Some(1_000_000), rule));
    assert!(lagging(released(1_060_001), Some(confirmed), Some(1_000_000), rule));
    assert!(lagging(released(1_000_000), None, None, rule), "no confirmation at all");
    // A quiet log: the run starts at the first command past the confirmed seq, not at the confirmed
    // command, however long before it was stamped.
    assert!(!lagging(released(9_060_000), Some(confirmed), Some(9_000_000), rule));
    assert!(lagging(released(9_060_001), Some(confirmed), Some(9_000_000), rule));
    assert!(!lagging(released(9_000_000), Some(confirmed), None, rule), "nothing past the confirmed seq");
}

#[tokio::test(start_paused = true)]
async fn a_start_waits_for_a_members_status_then_signs_only_where_it_confirmed() {
    let rig = Rig::spawn(10);
    settle().await;
    assert!(!rig.gate.start_checked() && !rig.gate.sequences(), "no member answered: nothing sequences");
    assert_eq!(*rig.signing.borrow(), Signing::Nothing);
    rig.answer_empty();
    settle().await;
    assert!(!rig.gate.start_checked(), "a member whose copy holds no command checks nothing past genesis");
    rig.answer(Some(8));
    settle().await;
    assert!(rig.gate.start_checked(), "a member answered and holds nothing past this log");
    assert_eq!(rig.gate.confirmed(), 8);
    assert!(!rig.gate.sequences(), "no released head at or below seq 8 to sign at: commands wait");
    assert_eq!(*rig.signing.borrow(), Signing::Nothing);
    rig.answer(Some(10));
    settle().await;
    assert!(rig.gate.sequences());
    let Signing::Confirmed(signable) = *rig.signing.borrow() else { panic!("not confirmed") };
    assert_eq!(signable.released.head, Head { seq: 10, link: link(10) });
    assert_eq!(signable.confirmer, Address::repeat_byte(MEMBER));
    // The confirmed tip was stamped long ago: the next command is admitted whenever it comes, and
    // starts the run of unconfirmed sequenced time the bound counts from.
    assert!(rig.gate.admits(TRUE_MS), "a quiet confirmed log admits its next command");
    let next = u64::try_from(TRUE_MS).unwrap();
    rig.prefixes.lock().unwrap().insert(11, link(11));
    rig._released.send_replace(Released { head: Head { seq: 11, link: link(11) }, stamp: next });
    settle().await;
    assert!(rig.gate.admits(TRUE_MS + 1_000), "within an interval of the first unconfirmed command");
    assert!(!rig.gate.admits(TRUE_MS + 3_600_000), "an hour past it is past the bound");
    rig.task.abort();
}

/// An empty log at genesis holds nothing to confirm: it opens once a member answers, an empty copy's
/// included, with no stamp bound, and once its first command is out it stops until a member confirms it.
#[tokio::test(start_paused = true)]
async fn an_empty_log_starts_unconfirmed_once_a_member_answers_and_waits_for_its_first_command_to_be_confirmed() {
    let rig = Rig::spawn(0);
    settle().await;
    assert!(!rig.gate.start_checked() && !rig.gate.sequences(), "no member answered: genesis waits");
    rig.answer_empty();
    settle().await;
    assert!(rig.gate.start_checked() && rig.gate.sequences(), "an empty copy's answer starts genesis");
    assert!(rig.gate.admits(TRUE_MS), "no stamp bound before the first command");
    rig.prefixes.lock().unwrap().insert(1, link(1));
    rig._released.send_replace(Released { head: Head { seq: 1, link: link(1) }, stamp: 5 });
    settle().await;
    assert!(!rig.gate.sequences(), "the first command waits for a member's confirmation");
    rig.answer(Some(1));
    settle().await;
    assert!(rig.gate.sequences());
    assert_eq!(rig.gate.confirmed(), 1);
    rig.task.abort();
}

#[tokio::test(start_paused = true)]
async fn a_member_holding_more_than_this_log_keeps_it_stopped_and_pages_the_range() {
    let (_guard, mut alerts) = capture_alerts();
    let rig = Rig::spawn(10);
    rig.prefixes.lock().unwrap().insert(12, link(12));
    rig.answer(Some(12));
    settle().await;
    assert!(!rig.gate.start_checked() && !rig.gate.sequences());
    let raised: Vec<_> = std::iter::from_fn(|| alerts.try_recv().ok()).collect();
    let refused: Vec<_> = raised.iter().filter(|alert| alert["alert"] == "liveness_start_refused").collect();
    assert_eq!(refused.len(), 1, "once per episode: {raised:?}");
    assert!(refused[0]["message"].contains("11..=12"), "{refused:?}");
    rig.task.abort();
}

#[tokio::test(start_paused = true)]
async fn a_member_holding_another_link_is_paged_as_a_second_history() {
    let (_guard, mut alerts) = capture_alerts();
    let rig = Rig::spawn(10);
    rig.members.0.lock().unwrap().get_mut("member:1").unwrap().2 = Some(Status {
        epoch: 1,
        durable: Some(Head { seq: 10, link: B256::repeat_byte(0xee) }),
        sealer: AccountId([0; 20]),
        seen: None,
    });
    settle().await;
    assert!(!rig.gate.start_checked());
    let raised: Vec<_> = std::iter::from_fn(|| alerts.try_recv().ok()).collect();
    assert!(raised.iter().any(|alert| alert["alert"] == "liveness_member_conflict"), "{raised:?}");
    rig.task.abort();
}

#[tokio::test(start_paused = true)]
async fn once_the_rule_starts_to_apply_nothing_passes_the_tip_until_a_member_confirms() {
    let rig = Rig::spawn(10);
    rig.chain.set(|chain| chain.generation = 0);
    settle().await;
    assert_eq!(rig.gate.confirmed(), u64::MAX, "the rule does not apply: the gate is open");
    rig.chain.set(|chain| chain.generation = 3);
    settle().await;
    assert_eq!(rig.gate.confirmed(), 0, "a turn names the operator: no checkpoint until a member confirms");
    assert!(!rig.gate.sequences() && !rig.gate.admits(6), "no command until a member confirms");
    rig.answer(Some(10));
    settle().await;
    assert_eq!(rig.gate.confirmed(), 10);
    assert!(rig.gate.sequences() && rig.gate.admits(6));
    rig.task.abort();
}

#[tokio::test(start_paused = true)]
async fn the_operator_reads_the_sets_members_again_after_a_minute_however_often_it_polls() {
    let rig = Rig::spawn(10);
    rig.answer(Some(8));
    settle().await;
    assert_eq!(rig.gate.confirmed(), 8);
    // The member registers a new standby key, and its statuses recover to it.
    let renewed = Arc::new(EnclaveKey::generate());
    rig.chain.set(|chain| chain.members[1].2 = Address::from(renewed.address()));
    rig.members.0.lock().unwrap().get_mut("member:1").unwrap().1 = renewed;
    rig.answer(Some(10));
    settle().await;
    assert_eq!(rig.gate.confirmed(), 8, "the members read a moment ago name the old key");
    tokio::time::sleep(MEMBERS_EVERY).await;
    settle().await;
    assert_eq!(rig.gate.confirmed(), 10, "read again a minute on, the renewed key's status confirms");
    rig.task.abort();
}

#[tokio::test(start_paused = true)]
async fn with_no_turn_naming_the_operator_the_gate_opens_and_heads_go_at_the_tip() {
    let rig = Rig::spawn(10);
    rig.chain.set(|chain| chain.generation = 0);
    settle().await;
    assert!(rig.gate.sequences() && rig.gate.signs());
    assert_eq!(rig.gate.confirmed(), u64::MAX, "no checkpoint waits");
    assert_eq!(*rig.signing.borrow(), Signing::Tip);
    rig.task.abort();
}

/// A read where the rule applies after reads where it did not runs the start check again: a member
/// holding more than this log keeps sequencing stopped.
#[tokio::test(start_paused = true)]
async fn the_start_check_runs_again_when_the_rule_starts_to_apply() {
    let rig = Rig::spawn(10);
    rig.chain.set(|chain| chain.generation = 0);
    settle().await;
    assert!(rig.gate.sequences());
    rig.answer(Some(12));
    rig.chain.set(|chain| chain.generation = 3);
    settle().await;
    assert!(!rig.gate.start_checked() && !rig.gate.sequences(), "the member holds seqs 11..=12 this log lacks");
    rig.task.abort();
}

#[tokio::test(start_paused = true)]
async fn the_fence_stops_the_enclave_before_the_earliest_promotion_by_its_upper_bound_on_time() {
    let (_guard, mut alerts) = capture_alerts();
    let rig = Rig::spawn(10);
    rig.answer(Some(10));
    // The earliest promotion is 20 s past the sample's true time; its upper bound is 3 s past it.
    rig.chain.set(|chain| chain.due_at = seconds(TRUE_MS) + 20);
    settle().await;
    assert!(rig.gate.sequences(), "upper bound + DELTA = 18 s, before E");
    // Two seconds on: 20 s, at E: everything stops, whatever the host clock shows.
    rig.clock.advance(Duration::from_secs(2));
    rig.clock.skew(-3_600_000);
    settle().await;
    assert!(!rig.gate.signs() && !rig.gate.sequences());
    let raised: Vec<_> = std::iter::from_fn(|| alerts.try_recv().ok()).collect();
    assert!(raised.iter().any(|alert| alert["alert"] == "liveness_fenced"), "{raised:?}");
    // A failed read changes nothing.
    rig.chain.set(|chain| chain.down = true);
    settle().await;
    assert!(!rig.gate.signs());
    // A later read with a later E reopens it.
    rig.chain.set(|chain| {
        chain.down = false;
        chain.due_at = seconds(TRUE_MS) + 300;
    });
    settle().await;
    assert!(rig.gate.signs() && rig.gate.sequences());
    rig.task.abort();
}

#[tokio::test(start_paused = true)]
async fn a_vault_naming_another_key_fences_for_good_and_ends_the_run() {
    let rig = Rig::spawn(10);
    let replacement = rig.gate.replacement();
    rig.answer(Some(10));
    settle().await;
    assert!(rig.gate.signs());
    assert!(!*replacement.borrow(), "the vault still names this key");
    rig.chain.set(|chain| {
        chain.epoch = 2;
        chain.signer = Address::repeat_byte(0x99);
    });
    settle().await;
    assert!(!rig.gate.signs());
    assert!(*replacement.borrow(), "a vault naming another key ends the node's run");
    rig.chain.set(|chain| chain.due_at = seconds(TRUE_MS) + 3_600);
    settle().await;
    assert!(!rig.gate.signs(), "a promotion away from this key is not undone");
    rig.task.abort();
}

/// A second copy of the key whose host blocks every read after its start, so it never reads the head
/// the other copy posts, still stops by its own clock at `E - DELTA` of its last read: the overlap of
/// two copies ends there however long the reads stay blocked.
#[tokio::test(start_paused = true)]
async fn a_copy_whose_reads_are_blocked_stops_by_its_last_reads_deadline() {
    let rig = Rig::spawn(10);
    rig.answer(Some(10));
    rig.chain.set(|chain| chain.due_at = seconds(TRUE_MS) + 20);
    settle().await;
    assert!(rig.gate.sequences(), "upper bound + DELTA = 18 s, before E");
    rig.chain.set(|chain| chain.down = true);
    rig.clock.advance(Duration::from_secs(1));
    settle().await;
    assert!(rig.gate.sequences(), "before the deadline, blocked reads change nothing");
    rig.clock.advance(Duration::from_secs(1));
    settle().await;
    assert!(!rig.gate.signs() && !rig.gate.sequences(), "the copy kept serving past its last read's deadline");
    rig.task.abort();
}

/// A promoted operator reads no block older than the one its start read the promotion at: there the
/// vault still names the outgoing key, and a lagging provider majority serving it neither fences the
/// operator for good nor ends its run; a block at or past it does.
#[tokio::test(start_paused = true)]
async fn a_block_older_than_the_promotion_neither_fences_nor_ends_a_promoted_operator() {
    let rig = Rig::promoted(10, 101);
    let replacement = rig.gate.replacement();
    rig.chain.set(|chain| chain.number = 101);
    rig.answer(Some(10));
    settle().await;
    assert!(rig.gate.signs());
    rig.chain.set(|chain| {
        chain.number = 100;
        chain.signer = Address::repeat_byte(0x99);
    });
    settle().await;
    assert!(rig.gate.signs() && !*replacement.borrow(), "a block before the promotion counted");
    rig.chain.set(|chain| chain.number = 101);
    settle().await;
    assert!(!rig.gate.signs() && *replacement.borrow(), "a block at the promotion's naming another key");
    rig.task.abort();
}

// ── Helpers ──

impl Rig {
    /// The rule for the operator `OPERATOR` signing with the test key, its log's tip at `tip`, with
    /// `MEMBER` as the set's other member, silent until a test sets its status.
    fn spawn(tip: u64) -> Self {
        Self::promoted(tip, 0)
    }

    /// `spawn`, for an operator a promotion read at block `floor` made.
    fn promoted(tip: u64, floor: u64) -> Self {
        let key = EnclaveKey::test_key();
        let member_key = Arc::new(EnclaveKey::generate());
        let chain = Arc::new(FakeChain::new(ChainState {
            signer: Address::from(key.address()),
            named: (Address::repeat_byte(OPERATOR), 0),
            members: vec![
                (Address::repeat_byte(OPERATOR), "operator:1".into(), Address::repeat_byte(0x0a)),
                (Address::repeat_byte(MEMBER), "member:1".into(), Address::from(member_key.address())),
            ],
            due_at: seconds(TRUE_MS) + 300,
            ..Default::default()
        }));
        let members = Members::default();
        members.0.lock().unwrap().insert("member:1".into(), (AccountId([MEMBER; 20]), member_key, None));
        let clock = Arc::new(TestClock::at(TRUE_MS));
        let sample = Sample {
            at: crate::clock::Clock::instant(clock.as_ref()),
            earliest_ms: TRUE_MS - 3_000,
            latest_ms: TRUE_MS + 3_000,
        };
        let trusted = TrustedClock::new(clock.clone(), watch::channel(Some(sample)).1, Duration::from_secs(10));
        let (released, released_rx) = watch::channel(Released { head: Head { seq: tip, link: link(tip) }, stamp: 5 });
        let (signing_tx, signing) = watch::channel(Signing::Nothing);
        let prefixes = Arc::new(Mutex::new((1..=tip).map(|seq| (seq, link(seq))).collect::<BTreeMap<_, _>>()));
        let (sequencer, mut requests) = mpsc::channel(16);
        let answering = prefixes.clone();
        tokio::spawn(async move {
            while let Some(request) = requests.recv().await {
                if let SequencerRequest::PrefixAt { seq, reply } = request {
                    let _ = reply.send(answering.lock().unwrap().get(&seq).copied());
                }
            }
        });
        let gate = Gate::closed();
        let operator = Operator {
            chain: chain.clone(),
            asking: members.clone(),
            chain_id: DOMAIN.standby.chain_id,
            vault: DOMAIN.standby.vault,
            log_heads: DOMAIN.log_heads,
            key: AccountId(key.address()),
            admin: AccountId([OPERATOR; 20]),
            gate: gate.clone(),
            trusted,
            released: released_rx,
            signing: signing_tx,
            tick: Duration::from_millis(250),
            sequencer: sequencer.downgrade(),
            vault_signer: watch::channel(None).0,
            poll: TEST_POLL,
            floor,
        };
        let task = tokio::spawn(async move {
            let _sequencer = sequencer;
            operator.run().await
        });
        Self { chain, members, gate, signing, _released: released, clock, prefixes, task }
    }

    /// The member's copy is durable through `seq`, with this log's link there.
    fn answer(&self, seq: Option<u64>) {
        let status = seq.map(|seq| Status {
            epoch: 1,
            durable: Some(Head { seq, link: link(seq) }),
            sealer: AccountId([0; 20]),
            seen: None,
        });
        self.members.0.lock().unwrap().get_mut("member:1").unwrap().2 = status;
    }

    /// The member answers from a copy that holds no command.
    fn answer_empty(&self) {
        let status = Status { epoch: 1, durable: None, sealer: AccountId([0; 20]), seen: None };
        self.members.0.lock().unwrap().get_mut("member:1").unwrap().2 = Some(status);
    }
}

/// Lets a few polls run.
async fn settle() {
    tokio::time::sleep(TEST_POLL * 4).await;
}

/// This log's cumulative prefix at `seq`.
fn link(seq: u64) -> B256 {
    B256::from(alloy::primitives::U256::from(seq))
}

/// `ms` in whole seconds.
fn seconds(ms: Millis) -> u64 {
    u64::try_from(ms / 1000).unwrap()
}

mod assess;
mod copies;
mod exemptions;
mod members;
mod rounds;
