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
    // Where no turn names the operator but a set is in effect, no slot can be witnessed before the
    // boundary: the exemption holds 2 x DELTA past it. A one-member set's slots can be, and it ends at
    // the later of the boundary and `E`, which heads that landed through the turn put 600 s past it.
    let past = 2 * DELTA.as_secs();
    let exemptions: [(&str, Exempting, u64); 3] = [
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

/// A read under which no turn can start at the next boundary holds past it, until 2 x DELTA past the
/// first boundary a drawn turn could start at: with no schedule, or no rotation set in effect or
/// waiting, eight days on, as a set adopted now takes effect seven days later and draws a day before
/// its boundary; with a set waiting three days, four.
#[tokio::test(start_paused = true)]
async fn a_turnless_read_holds_until_a_turn_could_start() {
    type Exempting = fn(&mut ChainState);
    const DAY: u64 = 86_400;
    let cases: [(&str, Exempting, u64); 3] = [
        ("the registry names no schedule", |chain| chain.scheduled = false, 8 * DAY),
        ("no set is in effect or waiting", |chain| (chain.generation, chain.sets) = (0, (0, 0, 0)), 8 * DAY),
        ("a set waits three days", |chain| (chain.generation, chain.sets) = (0, (0, 2, chain.time + 3 * DAY)), 4 * DAY),
    ];
    for (case, exempting, later) in cases {
        let rig = Rig::spawn(10);
        rig.chain.set(exempting);
        settle().await;
        assert!(rig.gate.signs() && rig.gate.sequences(), "{case}: exempt");
        rig.chain.set(|chain| chain.down = true);
        // The next boundary is 52,200 s on; the upper bound starts 3 s past true time and runs 500 ppm
        // fast, so it reaches the end less DELTA after (52,200 + later + 2 x DELTA - DELTA - 3) / 1.0005 s.
        let closes = (52_200 + later + DELTA.as_secs() - 3) * 1_000_000 / 1_000_500;
        rig.clock.advance(Duration::from_secs(closes - 60));
        settle().await;
        assert!(rig.gate.signs(), "{case}: before the exemption's end less DELTA, it holds");
        rig.clock.advance(Duration::from_secs(120));
        settle().await;
        assert!(!rig.gate.signs() && !rig.gate.sequences(), "{case}: no read since, past its end less DELTA");
        rig.task.abort();
    }
}
