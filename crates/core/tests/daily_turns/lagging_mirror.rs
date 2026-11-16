//! `V73`: a set is forgotten by the block time the staking mirror has completed, never past the
//! sequenced time, so a mirror that lags the chain keeps a set until the draws from it are mirrored and
//! forgets nothing before it records a block time; an earlier version counts from the sequenced time.

use super::*;

use engine_core::turns::{DRAW_LEAD_MS, TURN_MS};

const HOUR_MS: Millis = 60 * 60 * 1000;
/// How long after the next set takes effect a set is past drawing.
const PAST_DRAWING_MS: Millis = DRAW_LEAD_MS + TURN_MS;

/// Sequences a completed staking scan whose last block is stamped `block_time`.
fn frontier(h: &mut Harness, block_time: Millis) {
    let (block, block_hash) = (h.log.len() as u64, alloy_primitives::B256::repeat_byte(0x73));
    let events = h.cmd(Command::StakingProgress { block, block_hash, block_time: Some(block_time), named_turn: None });
    assert!(!events.iter().any(|event| matches!(event, Event::AdminRejected { .. })), "{events:?}");
    let completed = h.engine.staking().and_then(|staking| staking.completed_through);
    assert_eq!(completed.and_then(|completed| completed.time), Some(block_time));
}

fn sets(h: &Harness) -> Vec<u32> {
    h.engine.daily_turns().sets.keys().copied().collect()
}

/// `rotating` under `version` with sets 1 and 2 mirrored, set 2 taking effect a day after set 1; then,
/// at `clock` past set 2's effective time and with the mirror's scan completed at a block stamped
/// `block_time` past it, if any, set 3 mirrored. Returns the sets kept.
fn kept(version: ProtocolVersion, clock: Millis, block_time: Option<Millis>) -> Vec<u32> {
    let mut h = rotating();
    h.admin(AdminCommand::ActivateProtocol { version });
    assert!(matches!(adopt(&mut h, 1, members()).last(), Some(Event::RotationSetAdopted { set: 1, .. })));
    let second = h.engine.daily_turns().sets[&1].effective_at + DAY_MS;
    assert!(matches!(adopt_effective(&mut h, 2, members(), second).last(), Some(Event::RotationSetAdopted { .. })));
    h.advance_to(second + clock);
    if let Some(block_time) = block_time {
        frontier(&mut h, second + block_time);
    }
    assert!(matches!(adopt(&mut h, 3, members()).last(), Some(Event::RotationSetAdopted { set: 3, .. })));
    sets(&h)
}

#[test]
fn a_set_goes_once_the_mirror_has_reached_two_days_past_the_next_set() {
    let late = PAST_DRAWING_MS + DAY_MS;
    assert_eq!(kept(ProtocolVersion::V73, late, Some(PAST_DRAWING_MS - 1)), [1, 2, 3]);
    assert_eq!(kept(ProtocolVersion::V73, late, Some(PAST_DRAWING_MS)), [2, 3]);
}

#[test]
fn nothing_is_forgotten_before_the_mirror_records_a_block_time() {
    assert_eq!(kept(ProtocolVersion::V73, PAST_DRAWING_MS + DAY_MS, None), [1, 2, 3]);
}

#[test]
fn a_block_time_ahead_of_the_sequenced_time_counts_as_the_sequenced_time() {
    let ahead = Some(PAST_DRAWING_MS + DAY_MS);
    assert_eq!(kept(ProtocolVersion::V73, PAST_DRAWING_MS - 1, ahead), [1, 2, 3]);
    assert_eq!(kept(ProtocolVersion::V73, PAST_DRAWING_MS, ahead), [2, 3]);
}

#[test]
fn before_v73_the_sequenced_time_alone_counts() {
    let late = PAST_DRAWING_MS + DAY_MS;
    assert_eq!(kept(ProtocolVersion::V72, late, Some(0)), [2, 3]);
    assert_eq!(kept(ProtocolVersion::V72, late, None), [2, 3]);
}

/// `rotating` under `version`: a turn drawn from set 1 is in effect; set 2 is in effect at the next
/// turn's draw deadline and set 3 from six hours after it. The mirror completes a scan at that deadline
/// and stalls, and two and a half days on mirrors, in block order, set 4, adopted half an hour after the
/// deadline, and the next turn's draw from set 2, which is refused as ended. Returns that turn.
fn stalled(version: ProtocolVersion) -> (Harness, u64) {
    let mut h = rotating();
    h.admin(AdminCommand::ActivateProtocol { version });
    assert!(matches!(adopt(&mut h, 1, members()).last(), Some(Event::RotationSetAdopted { set: 1, .. })));
    let first = next_turn(h.ts + 8 * DAY_MS);
    let turn = first + 1;
    let deadline = boundary(turn).unwrap() - DRAW_LEAD_MS;
    adopt_effective(&mut h, 2, members(), deadline - HOUR_MS);
    adopt_effective(&mut h, 3, members(), deadline + 6 * HOUR_MS);
    h.advance_to(boundary(first).unwrap() - DRAW_LEAD_MS);
    assert!(matches!(assign(&mut h, first, 0, 0, TurnReason::Draw).last(), Some(Event::TurnAssigned { .. })));
    h.advance_to(deadline);
    frontier(&mut h, deadline);
    assert_eq!(h.engine.daily_turns().in_effect.map(|in_effect| in_effect.turn), Some(first));
    h.advance_to(deadline + 5 * DAY_MS / 2);
    let events = adopt_effective(&mut h, 4, members(), deadline + HOUR_MS / 2 + NOTICE_MS);
    assert!(matches!(events.last(), Some(Event::RotationSetAdopted { set: 4, .. })), "{events:?}");
    turn_refused(&assign_from(&mut h, 2, turn, 0, 0, TurnReason::Draw), "ended before its assignment was mirrored");
    (h, turn)
}

#[test]
fn a_lagging_mirror_keeps_the_set_of_a_draw_it_has_yet_to_mirror_from_v73() {
    let (mut h, turn) = stalled(ProtocolVersion::V72);
    assert_eq!(sets(&h), [1, 3, 4], "set 2 goes by the sequenced time, before its draw is mirrored");
    let events = assign_from(&mut h, 2, turn, 2, 1, TurnReason::Promotion);
    turn_refused(&events, "set 2 is not among the sets mirrored");
    assert_eq!(operator_key(&h), key(0));

    let (mut h, turn) = stalled(ProtocolVersion::V73);
    assert_eq!(sets(&h), [1, 2, 3, 4], "the mirror has reached only the draw deadline");
    assert_eq!(h.engine.daily_turns().refused.get(&turn), Some(&2));
    let mirrored = h.ts;
    let events = assign_from(&mut h, 2, turn, 2, 1, TurnReason::Promotion);
    assert!(
        matches!(events.last(), Some(Event::TurnAssigned { applies_at, position: 1, .. }) if *applies_at == mirrored),
        "{events:?}"
    );
    assert_eq!(signed_by(&mut h, SEEDS[2], false), Some(Event::AdminApplied));

    // Once the mirror has caught up, the next adoption forgets set 1, which no turn can draw from.
    let now = h.ts;
    frontier(&mut h, now);
    adopt(&mut h, 5, members());
    assert_eq!(sets(&h), [2, 3, 4, 5]);
}
