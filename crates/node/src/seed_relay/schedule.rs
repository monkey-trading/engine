//! `SeedSchedule` (`contracts/src/SeedSchedule.sol`): which beacon epoch seeds each turn, and when
//! its seed becomes final, at the build's turn length (`engine_core::turns`). The relayer picks the
//! turns it reads with these; the contracts check every relay against their own, so a drift between
//! the two costs a relay that reverts.

use engine_core::turns::{BOUNDARY_OFFSET_MS, DRAW_LEAD_MS, TURN_MS};

/// A turn, in seconds: the schedule's `turnLength`.
pub(crate) const TURN_SECS: u64 = TURN_MS.unsigned_abs() / 1000;
/// A turn's boundary past a multiple of the turn: 22:30 UTC's.
pub(crate) const BOUNDARY_OFFSET: u64 = BOUNDARY_OFFSET_MS.unsigned_abs() / 1000;
/// How long before its boundary a turn's seed is final: a turn.
pub(crate) const DRAW_LEAD: u64 = DRAW_LEAD_MS.unsigned_abs() / 1000;
/// How long before the draw deadline the seed's epoch ends: two turns.
const RELAY_WINDOW: u64 = 2 * TURN_SECS;
/// A beacon slot, and an epoch of 32 of them.
pub(crate) const SECONDS_PER_SLOT: u64 = 12;
pub(crate) const SLOTS_PER_EPOCH: u64 = 32;
const SECONDS_PER_EPOCH: u64 = SECONDS_PER_SLOT * SLOTS_PER_EPOCH;

/// When turn `turn`'s seed becomes final, a turn before its boundary, or `None` for a turn the clock
/// cannot hold.
pub(crate) fn draw_deadline(turn: u64) -> Option<u64> {
    turn.checked_mul(TURN_SECS)?.checked_add(BOUNDARY_OFFSET)?.checked_sub(DRAW_LEAD)
}

/// The beacon epoch whose mix seeds `turn`, or `None` for a turn before `genesis` or past the clock.
pub(crate) fn seed_epoch(turn: u64, genesis: u64) -> Option<u64> {
    let elapsed = draw_deadline(turn)?.checked_sub(RELAY_WINDOW)?.checked_sub(genesis)?;
    Some(elapsed / SECONDS_PER_EPOCH)
}

/// The first turn boundary after `now`: from it a drawn turn may name the operator.
pub(crate) fn next_boundary(now: u64) -> u64 {
    now.saturating_add(TURN_SECS - BOUNDARY_OFFSET) / TURN_SECS * TURN_SECS + BOUNDARY_OFFSET
}

/// The latest turn whose draw deadline is at or before `now`.
pub(crate) fn last_final_turn(now: u64) -> u64 {
    now.saturating_add(DRAW_LEAD - BOUNDARY_OFFSET) / TURN_SECS
}

#[cfg(all(test, not(feature = "short-turns")))]
mod tests {
    use super::*;

    /// A day, in seconds: the turn of a build without `short-turns`.
    const DAY: u64 = TURN_SECS;

    #[test]
    fn the_next_boundary_is_the_first_22_30_utc_after_now() {
        let boundary = 20_763 * DAY + BOUNDARY_OFFSET;
        assert_eq!(next_boundary(boundary - 1), boundary);
        assert_eq!(next_boundary(boundary), boundary + DAY);
        assert_eq!(next_boundary(boundary + DAY - 1), boundary + DAY);
        assert_eq!(next_boundary(0), BOUNDARY_OFFSET);
    }

    /// The values `SeedReceiver.t.sol` checks for mainnet's genesis time.
    #[test]
    fn mainnet_turns_match_the_contracts_schedule() {
        let genesis = 1_606_824_023;
        assert_eq!(draw_deadline(20763), Some(1_794_004_200 - DAY));
        assert_eq!(seed_epoch(20763, genesis), Some(486_773));
        assert_eq!(seed_epoch(20764, genesis), Some(486_773 + 225));
        assert_eq!(seed_epoch(18599, genesis), None);
        assert!(seed_epoch(18600, genesis).is_some());
    }

    /// A time or turn from the host's network past what the clock holds gives no answer, never a panic.
    #[test]
    fn turns_past_the_clock_have_no_deadline_or_epoch() {
        assert_eq!(draw_deadline(u64::MAX / DAY + 1), None);
        assert_eq!(draw_deadline(0), None, "turn 0's draw deadline is before the epoch");
        assert_eq!(seed_epoch(u64::MAX, 1), None);
        assert_eq!(seed_epoch(2, 1), None);
        assert_eq!(draw_deadline(last_final_turn(u64::MAX)), None);
    }

    #[test]
    fn the_last_final_turn_changes_at_each_draw_deadline() {
        let deadline = draw_deadline(20763).unwrap();
        assert_eq!(last_final_turn(deadline), 20763);
        assert_eq!(last_final_turn(deadline - 1), 20762);
        assert_eq!(last_final_turn(deadline + DAY - 1), 20763);
    }
}

#[cfg(all(test, feature = "short-turns"))]
mod short_tests {
    use super::*;

    /// A boundary every five minutes, 22:30 UTC on 2026-11-06 among them.
    const HALF_PAST_TEN: u64 = 1_794_004_200;

    #[test]
    fn five_minute_turns_start_every_five_minutes() {
        assert_eq!((TURN_SECS, BOUNDARY_OFFSET, DRAW_LEAD, RELAY_WINDOW), (300, 0, 300, 600));
        assert_eq!(next_boundary(HALF_PAST_TEN - 1), HALF_PAST_TEN);
        assert_eq!(next_boundary(HALF_PAST_TEN), HALF_PAST_TEN + 300);
        assert_eq!(next_boundary(HALF_PAST_TEN + 299), HALF_PAST_TEN + 300);
    }

    /// The values `SeedReceiver.t.sol` checks for a five-minute schedule on mainnet's genesis time.
    #[test]
    fn five_minute_turns_match_the_contracts_schedule() {
        let (genesis, turn) = (1_606_824_023, HALF_PAST_TEN / 300);
        assert_eq!(draw_deadline(turn), Some(HALF_PAST_TEN - 300));
        assert_eq!(seed_epoch(turn, genesis), Some((HALF_PAST_TEN - 900 - genesis) / 384));
        assert_eq!(last_final_turn(HALF_PAST_TEN - 300), turn);
        assert_eq!(last_final_turn(HALF_PAST_TEN - 301), turn - 1);
    }
}
