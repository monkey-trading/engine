//! What a round of statuses rests on: the vault's signer before any exemption, the set whose members'
//! confirmations count, the last read when reads fail, and a view and a set's members read within an
//! interval.

use super::*;
use crate::liveness::chain::{members, view};

/// A vault naming another key ends the run and keeps the fence closed whatever exemption the same read
/// would give: a replaced operator whose reads resume never reopens.
#[tokio::test(start_paused = true)]
async fn a_vault_naming_another_key_ends_the_run_under_every_exemption() {
    type Exempting = fn(&mut ChainState);
    let exemptions: [(&str, Exempting); 3] = [
        ("no turn names the operator", |chain| chain.generation = 0),
        ("the set has no other member", |chain| {
            chain.members.retain(|(candidate, ..)| *candidate == Address::repeat_byte(OPERATOR));
        }),
        ("the registry names no schedule", |chain| chain.scheduled = false),
    ];
    for (case, exempting) in exemptions {
        let rig = Rig::spawn(10);
        let replacement = rig.gate.replacement();
        rig.answer(Some(10));
        settle().await;
        assert!(rig.gate.sequences(), "{case}: the rule applies and a member confirmed");
        rig.chain.set(|chain| {
            exempting(chain);
            chain.epoch = 2;
            chain.signer = Address::repeat_byte(0x99);
        });
        settle().await;
        assert!(*replacement.borrow(), "{case}: a vault naming another key ends the node's run");
        assert!(!rig.gate.signs() && !rig.gate.sequences(), "{case}: the exemption reopened a replaced key");
        rig.task.abort();
    }
}

/// A confirmation counts only under the set it was given in: once the turn's set changes, the former
/// member's confirmation leaves nothing to sign at and nothing confirmed until a member of the new set
/// answers.
#[tokio::test(start_paused = true)]
async fn a_former_members_confirmation_counts_for_no_later_set() {
    const NEWCOMER: u8 = 0xc3;
    let rig = Rig::spawn(10);
    rig.answer(Some(10));
    settle().await;
    assert!(matches!(*rig.signing.borrow(), Signing::Confirmed(_)));
    let newcomer = Arc::new(EnclaveKey::generate());
    let promotable = Address::from(newcomer.address());
    rig.chain.set(|chain| {
        chain.set = 2;
        chain.members = vec![
            (Address::repeat_byte(OPERATOR), "operator:1".into(), Address::repeat_byte(0x0a)),
            (Address::repeat_byte(NEWCOMER), "member:2".into(), promotable),
        ];
    });
    settle().await;
    assert_eq!(*rig.signing.borrow(), Signing::Nothing, "the former member's confirmation still signs");
    assert_eq!(rig.gate.confirmed(), 0, "a checkpoint could sign at the former member's seq");
    assert!(!rig.gate.sequences(), "nothing the new set confirmed: commands wait");
    let status =
        Status { epoch: 1, durable: Some(Head { seq: 10, link: link(10) }), sealer: AccountId([0; 20]), seen: None };
    rig.members.0.lock().unwrap().insert("member:2".into(), (AccountId([NEWCOMER; 20]), newcomer, Some(status)));
    settle().await;
    let Signing::Confirmed(signable) = *rig.signing.borrow() else { panic!("the new set's member confirmed") };
    assert_eq!(signable.confirmer, Address::repeat_byte(NEWCOMER));
    assert_eq!(rig.gate.confirmed(), 10);
    assert!(rig.gate.sequences());
    rig.task.abort();
}

/// While chain reads fail, the members still confirm under the last read: the log runs on as they
/// confirm, until the fence, not the stamp bound, stops it.
#[tokio::test(start_paused = true)]
async fn failed_reads_keep_the_members_confirming_until_the_fence() {
    let rig = Rig::spawn(10);
    rig.answer(Some(10));
    settle().await;
    rig.chain.set(|chain| chain.down = true);
    let stamp = u64::try_from(TRUE_MS).unwrap() + 30_000;
    rig.prefixes.lock().unwrap().insert(11, link(11));
    rig._released.send_replace(Released { head: Head { seq: 11, link: link(11) }, stamp });
    rig.answer(Some(11));
    rig.clock.advance(Duration::from_secs(90));
    settle().await;
    assert_eq!(rig.gate.confirmed(), 11, "the member's newer copy confirmed nothing");
    assert!(rig.gate.admits(TRUE_MS + 120_000), "a confirmed log stopped one interval after the reads failed");
    // E stands at TRUE + 300 s from the last read: the fence stops it by E - DELTA.
    rig.clock.advance(Duration::from_secs(200));
    settle().await;
    assert!(!rig.gate.signs() && !rig.gate.sequences(), "past E - DELTA the fence stops it");
    rig.task.abort();
}

/// With the reads failing and the member's stream withheld once it confirmed the tip, the first
/// command past it starts the run: the bound lapses one interval after that command, so the log never
/// runs unconfirmed for longer than that.
#[tokio::test(start_paused = true)]
async fn a_bound_at_a_confirmed_tip_lapses_when_the_reads_that_renew_it_fail() {
    let rig = Rig::spawn(10);
    rig.answer(Some(10));
    settle().await;
    assert!(rig.gate.admits(TRUE_MS + 30_000), "within an interval of the round");
    rig.chain.set(|chain| chain.down = true);
    rig.answer(None);
    let stamp = u64::try_from(TRUE_MS).unwrap() + 1_000;
    rig.prefixes.lock().unwrap().insert(11, link(11));
    rig._released.send_replace(Released { head: Head { seq: 11, link: link(11) }, stamp });
    rig.clock.advance(Duration::from_secs(120));
    settle().await;
    assert!(rig.gate.admits(TRUE_MS + 61_000), "within an interval of the first unconfirmed command");
    assert!(!rig.gate.admits(TRUE_MS + 61_001), "no member confirmed it: past the interval it lapses");
    rig.task.abort();
}

/// A full set of 101 members, each call taking 250 ms, reads in a few seconds, well within an
/// interval: three calls per member one after another would take 75 s.
#[tokio::test(start_paused = true)]
async fn a_full_set_reads_well_within_an_interval() {
    let listed: Vec<(Address, String, Address)> = (0..101_u8)
        .map(|byte| (Address::repeat_byte(byte), format!("member:{byte}"), Address::repeat_byte(byte.wrapping_add(1))))
        .collect();
    let chain = FakeChain::new(ChainState {
        members: listed.clone(),
        latency: Duration::from_millis(250),
        ..Default::default()
    });
    let block = chain.latest().await.unwrap();
    let started = tokio::time::Instant::now();
    let read = members(&block, &DOMAIN.standby, (1, 20_400, 0, AccountId([0; 20]))).await.unwrap();
    assert!(started.elapsed() <= Duration::from_secs(5), "{:?}", started.elapsed());
    let candidates: Vec<Address> = read.iter().map(|member| Address::from(member.candidate.0)).collect();
    assert_eq!(candidates, listed.iter().map(|(candidate, ..)| *candidate).collect::<Vec<_>>(), "in the set's order");
    assert_eq!(read[7].endpoint, "member:7");
}

/// A view's calls at one block go out together, but for the head's, which is under the signer epoch:
/// each call taking 250 ms, the view reads in two round trips, not the nine its calls one after another
/// took, which a new operator's first commands waited behind.
#[tokio::test(start_paused = true)]
async fn a_view_reads_in_two_round_trips() {
    let latency = Duration::from_millis(250);
    let chain = FakeChain::new(ChainState { latency, head: (12, 0, 0), ..Default::default() });
    let block = chain.latest().await.unwrap();
    let started = tokio::time::Instant::now();
    let read = view(&block, &DOMAIN).await.unwrap();
    assert_eq!(started.elapsed(), 2 * latency);
    assert_eq!((read.epoch, read.head.seq, read.due_at), (1, 12, 1_800_000_300), "the view is the chain's");
}

/// The guardian's retirement names no other key: the run does not end, and the fence, which a
/// timelock's installation and a later promotion could pass, still stops the operator by `E - DELTA`;
/// a key the timelock installs ends the run.
#[tokio::test(start_paused = true)]
async fn a_retired_signer_ends_no_run_and_the_fence_still_holds() {
    let rig = Rig::spawn(10);
    let replacement = rig.gate.replacement();
    rig.answer(Some(10));
    settle().await;
    rig.chain.set(|chain| chain.signer = Address::ZERO);
    settle().await;
    assert!(!*replacement.borrow(), "a retirement ended the run as a replacement");
    assert!(rig.gate.signs(), "before E - DELTA");
    rig.clock.advance(Duration::from_secs(600));
    settle().await;
    assert!(!rig.gate.signs() && !rig.gate.sequences(), "a retirement exempted the operator past E");
    rig.chain.set(|chain| (chain.epoch, chain.signer) = (2, Address::repeat_byte(0x99)));
    settle().await;
    assert!(*replacement.borrow(), "the key the timelock installed did not end the run");
    rig.task.abort();
}

#[tokio::test(start_paused = true)]
async fn a_former_holders_tail_is_exempt_only_under_its_own_key() {
    let (own_key, others_key) = (Address::repeat_byte(0x71), Address::repeat_byte(0x72));
    let cases = [
        ("the turn's keys", Address::repeat_byte(0x73), true),
        ("its own standby key", own_key, true),
        ("another member's promoted key", others_key, false),
    ];
    for (case, sealer, exempt) in cases {
        let rig = Rig::spawn(10);
        rig.chain.set(|chain| {
            chain.held = vec![Address::repeat_byte(MEMBER)];
            chain.holders = vec![(own_key, Address::repeat_byte(MEMBER)), (others_key, Address::repeat_byte(0xc3))];
        });
        let status = Status {
            epoch: 1,
            durable: Some(Head { seq: 12, link: link(12) }),
            sealer: AccountId(sealer.0.0),
            seen: None,
        };
        rig.members.0.lock().unwrap().get_mut("member:1").unwrap().2 = Some(status);
        settle().await;
        assert_eq!(rig.gate.start_checked(), exempt, "{case}");
        rig.task.abort();
    }
}

/// A member that held the turn drawn before the one in effect, and kept its keys into it because the
/// turn's member never took them, reports its own cut-off tail as no copy of this key, as a holder of
/// the turn itself does.
#[tokio::test(start_paused = true)]
async fn a_prior_turns_holder_tail_is_exempt_as_a_turn_holders_is() {
    let rig = Rig::spawn(10);
    rig.chain.set(|chain| chain.prior = (20_399, vec![Address::repeat_byte(MEMBER)]));
    let sealer = AccountId([0x73; 20]);
    let status = Status { epoch: 1, durable: Some(Head { seq: 12, link: link(12) }), sealer, seen: None };
    rig.members.0.lock().unwrap().get_mut("member:1").unwrap().2 = Some(status);
    settle().await;
    assert!(rig.gate.start_checked(), "the prior turn's holder's tail refused the start");
    rig.task.abort();
}

/// The turn's source, which the untaken-turn promotion moved past while it beat on, reports its last
/// released head, which its replaced run seeds into its member link's copy, as its own cut-off tail: it
/// holds no assignment of the turn or of the one before it, but `holdsTurn` counts it on chain as a
/// holder, and so does the start check.
#[tokio::test(start_paused = true)]
async fn a_turn_sources_tail_is_exempt_as_a_turn_holders_is() {
    for (case, source, exempt) in [("the turn's source", MEMBER, true), ("another candidate's", 0xc9, false)] {
        let rig = Rig::spawn(10);
        rig.chain.set(|chain| chain.source = Address::repeat_byte(source));
        // The source's run ended `Replaced` past its seq 12, sealed by the turn's keys.
        let copy = crate::liveness::link::Copy::default();
        copy.seed(Head { seq: 12, link: link(12) }, AccountId([0x73; 20]));
        let status = copy.status(1).expect("a seeded copy answers its status");
        rig.members.0.lock().unwrap().get_mut("member:1").unwrap().2 = Some(status);
        settle().await;
        assert_eq!(rig.gate.start_checked(), exempt, "{case}");
        rig.task.abort();
    }
}

/// A promoted operator, whose reads count only from the block its start read the promotion at, still
/// stops by its last read's deadline once its reads are blocked, so a later promotion can pass it.
#[tokio::test(start_paused = true)]
async fn a_promoted_operator_whose_reads_are_blocked_stops_by_its_last_reads_deadline() {
    let rig = Rig::promoted(10, 100);
    rig.answer(Some(10));
    rig.chain.set(|chain| chain.due_at = seconds(TRUE_MS) + 20);
    settle().await;
    assert!(rig.gate.sequences(), "the promoted operator sequences at its promotion's block");
    rig.chain.set(|chain| chain.down = true);
    rig.clock.advance(Duration::from_secs(2));
    settle().await;
    assert!(!rig.gate.signs() && !rig.gate.sequences(), "it kept serving past its last read's deadline");
    rig.task.abort();
}

/// After a reorg removed the promotion that replaced it, the incumbent, back under its epoch, starts
/// and sequences on a member that follows it while the orphaned holder answers nothing (it waits,
/// with no member link, until a final block settles its record); once its quarantine has rebuilt its
/// copy from the incumbent's archive, its confirmation counts like any other.
#[tokio::test(start_paused = true)]
async fn the_returning_incumbent_sequences_beside_the_silent_orphaned_holder() {
    const FOLLOWER: u8 = 0xc3;
    let rig = Rig::spawn(10);
    let follower_key = Arc::new(EnclaveKey::generate());
    let promotable = Address::from(follower_key.address());
    rig.chain.set(|chain| chain.members.push((Address::repeat_byte(FOLLOWER), "member:2".into(), promotable)));
    let status =
        Status { epoch: 1, durable: Some(Head { seq: 10, link: link(10) }), sealer: AccountId([0; 20]), seen: None };
    rig.members.0.lock().unwrap().insert("member:2".into(), (AccountId([FOLLOWER; 20]), follower_key, Some(status)));
    settle().await;
    assert!(rig.gate.start_checked() && rig.gate.sequences(), "the silent orphaned holder held the incumbent up");
    let Signing::Confirmed(signable) = *rig.signing.borrow() else { panic!("the follower confirmed nothing") };
    assert_eq!(signable.confirmer, Address::repeat_byte(FOLLOWER));
    rig.answer(Some(10));
    settle().await;
    assert!(rig.gate.sequences() && rig.gate.confirmed() == 10, "the rebuilt holder's confirmation");
    rig.task.abort();
}

/// A checkpoint that waits for a confirmation takes the first member's status that reaches it, however
/// slowly another member answers, and its wake asks at once: the pause is about one round trip, not
/// the slowest member's answer.
#[tokio::test(start_paused = true)]
async fn a_waiting_checkpoint_takes_the_first_status_that_reaches_it_not_the_slowest() {
    let rig = Rig::spawn(10);
    let slow = Arc::new(EnclaveKey::generate());
    rig.chain.set(|chain| {
        chain.members.push((Address::repeat_byte(0xc4), "member:2".into(), Address::from(slow.address())));
    });
    let behind =
        Status { epoch: 1, durable: Some(Head { seq: 10, link: link(10) }), sealer: AccountId([0; 20]), seen: None };
    rig.members.0.lock().unwrap().insert("member:2".into(), (AccountId([0xc4; 20]), slow, Some(behind)));
    rig.members.1.lock().unwrap().insert("member:2".into(), Duration::from_millis(2_900));
    rig.answer(Some(10));
    crate::test_support::wait_for("the start check", async {
        while !rig.gate.sequences() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    // The tip moves to 11, which the quick member's copy holds; a checkpoint at 11 starts to wait.
    rig.prefixes.lock().unwrap().insert(11, link(11));
    rig._released.send_replace(Released { head: Head { seq: 11, link: link(11) }, stamp: 6 });
    rig.answer(Some(11));
    let waited = tokio::time::Instant::now();
    rig.gate.want(11);
    while rig.gate.confirmed() < 11 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(waited.elapsed() < Duration::from_millis(500), "the checkpoint waited {:?}", waited.elapsed());
    rig.task.abort();
}

/// Before the start check passes, a round takes every member's answer, though a checkpoint waits and
/// a quicker member's status covers it: the slower member's longer copy refuses the start.
#[tokio::test(start_paused = true)]
async fn a_start_check_weighs_every_answer_though_a_checkpoint_waits() {
    let rig = Rig::spawn(10);
    let slow = Arc::new(EnclaveKey::generate());
    rig.chain.set(|chain| {
        chain.members.push((Address::repeat_byte(0xc4), "member:2".into(), Address::from(slow.address())));
    });
    let ahead =
        Status { epoch: 1, durable: Some(Head { seq: 12, link: link(12) }), sealer: AccountId([0; 20]), seen: None };
    rig.members.0.lock().unwrap().insert("member:2".into(), (AccountId([0xc4; 20]), slow, Some(ahead)));
    rig.members.1.lock().unwrap().insert("member:2".into(), Duration::from_millis(2_900));
    rig.gate.want(10);
    rig.answer(Some(10));
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert!(!rig.gate.start_checked(), "the start check passed on the first covering answer alone");
    rig.task.abort();
}

/// The log's unconfirmed run starts at the first command released past the newest confirmation, though
/// that command and later ones are released while a round waits on a slow member: the stamp bound counts
/// from it, not from the tip the round next sees.
#[tokio::test(start_paused = true)]
async fn a_slow_round_does_not_move_the_unconfirmed_runs_start_later() {
    let rig = Rig::spawn(10);
    rig.answer(Some(10));
    settle().await;
    assert!(rig.gate.sequences() && rig.gate.confirmed() == 10);
    rig.members.1.lock().unwrap().insert("member:1".into(), Duration::from_millis(2_900));
    // Past the round in flight, so the next waits on the member while both commands are released.
    tokio::time::sleep(Duration::from_secs(4)).await;
    let first = u64::try_from(TRUE_MS).unwrap() + 1_000;
    for (seq, stamp) in [(11, first), (12, first + 30_000)] {
        rig.prefixes.lock().unwrap().insert(seq, link(seq));
        rig._released.send_replace(Released { head: Head { seq, link: link(seq) }, stamp });
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    tokio::time::sleep(Duration::from_secs(8)).await;
    let first = i64::try_from(first).unwrap();
    assert!(rig.gate.admits(first + 60_000), "within an interval of the first unconfirmed command");
    assert!(!rig.gate.admits(first + 60_001), "the bound counts from the first unconfirmed command");
    rig.task.abort();
}
