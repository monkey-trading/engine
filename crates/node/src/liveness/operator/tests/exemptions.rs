//! Reads under which no promotion can pass: a set with no other member, no turn naming the operator,
//! and no schedule, each holding the gate open only until the next turn boundary, or `E` past it.

use super::*;

#[tokio::test(start_paused = true)]
async fn a_set_with_no_other_member_is_exempt() {
    let rig = Rig::spawn(10);
    rig.chain.set(|chain| chain.members.retain(|(candidate, ..)| *candidate == Address::repeat_byte(OPERATOR)));
    settle().await;
    assert!(rig.gate.sequences());
    assert_eq!(*rig.signing.borrow(), Signing::Tip);
    rig.task.abort();
}

/// Every read under which no promotion can pass holds the gate open only until the next turn boundary
/// past its block, or `E` past it: past that, with no newer read, the fence closes and pages; a fresh
/// read reopens it.
#[tokio::test(start_paused = true)]
async fn an_exempt_read_holds_only_until_the_next_turn_boundary() {
    type Exempting = fn(&mut ChainState);
    // Where no turn names the operator, no slot can be witnessed before the boundary: the exemption
    // holds 2 x DELTA past it. A one-member set's slots can be, and it ends at the later of the
    // boundary and `E`, which heads that landed through the turn put 600 s past it.
    let past = 2 * DELTA.as_secs();
    let exemptions: [(&str, Exempting, u64); 4] = [
        ("no turn names the operator", |chain| chain.generation = 0, past),
        (
            "the set has no other member",
            |chain| chain.members.retain(|(candidate, ..)| *candidate == Address::repeat_byte(OPERATOR)),
            0,
        ),
        (
            "the set has no other member, and E is past the boundary",
            |chain| {
                chain.members.retain(|(candidate, ..)| *candidate == Address::repeat_byte(OPERATOR));
                chain.due_at = next_boundary(chain.time) + 600;
            },
            600,
        ),
        ("the registry names no schedule", |chain| chain.scheduled = false, past),
    ];
    // The tests' block is at 08:00 UTC: the next boundary, 22:30, is 52,200 s on.
    assert_eq!(next_boundary(seconds(TRUE_MS)) - seconds(TRUE_MS), 52_200);
    for (case, exempting, after) in exemptions {
        let (_guard, mut alerts) = capture_alerts();
        let rig = Rig::spawn(10);
        rig.chain.set(exempting);
        settle().await;
        assert!(rig.gate.sequences() && rig.gate.signs(), "{case}: exempt");
        assert_eq!(*rig.signing.borrow(), Signing::Tip, "{case}");
        rig.chain.set(|chain| chain.down = true);
        // The upper bound is 3 s past true time at the sample, and runs 500 ppm fast after it: with
        // the boundary 52,200 s on, it reaches the boundary less DELTA after about 52,156 s.
        rig.clock.advance(Duration::from_secs(52_150 + after));
        settle().await;
        assert!(rig.gate.signs(), "{case}: before the exemption's end less DELTA, it holds");
        rig.clock.advance(Duration::from_secs(10));
        settle().await;
        assert!(!rig.gate.signs() && !rig.gate.sequences(), "{case}: no read since, past the boundary less DELTA");
        let raised: Vec<_> = std::iter::from_fn(|| alerts.try_recv().ok()).collect();
        assert!(raised.iter().any(|alert| alert["alert"] == "liveness_fenced"), "{case}: {raised:?}");
        rig.chain.set(|chain| {
            chain.down = false;
            chain.time += 52_200;
        });
        settle().await;
        assert!(rig.gate.signs() && rig.gate.sequences(), "{case}: a read at a later block moves it on");
        rig.task.abort();
    }
}
