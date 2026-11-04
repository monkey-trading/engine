//! `SeedSchedule` (`contracts/src/SeedSchedule.sol`): which beacon epoch seeds each turn, and when
//! its seed becomes final. The relayer picks the turns it reads with these; the contracts check
//! every relay against their own, so a drift between the two costs a relay that reverts.

/// A day, in seconds.
const DAY: u64 = 86_400;
/// A turn's boundary, 22:30 UTC.
const BOUNDARY_OFFSET: u64 = 22 * 3600 + 30 * 60;
/// How long before its boundary a turn's seed is final.
const DRAW_LEAD: u64 = DAY;
/// How long before the draw deadline the seed's epoch ends.
const RELAY_WINDOW: u64 = 2 * DAY;
/// A beacon slot, and an epoch of 32 of them.
pub(crate) const SECONDS_PER_SLOT: u64 = 12;
pub(crate) const SLOTS_PER_EPOCH: u64 = 32;
const SECONDS_PER_EPOCH: u64 = SECONDS_PER_SLOT * SLOTS_PER_EPOCH;

/// When turn `turn`'s seed becomes final, a day before its boundary.
pub(crate) fn draw_deadline(turn: u64) -> u64 {
    turn * DAY + BOUNDARY_OFFSET - DRAW_LEAD
}

/// The beacon epoch whose mix seeds `turn`, or `None` for a turn before `genesis`.
pub(crate) fn seed_epoch(turn: u64, genesis: u64) -> Option<u64> {
    (draw_deadline(turn) - RELAY_WINDOW).checked_sub(genesis).map(|elapsed| elapsed / SECONDS_PER_EPOCH)
}

/// The latest turn whose draw deadline is at or before `now`.
pub(crate) fn last_final_turn(now: u64) -> u64 {
    (now + DRAW_LEAD - BOUNDARY_OFFSET) / DAY
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The values `SeedReceiver.t.sol` checks for mainnet's genesis time.
    #[test]
    fn mainnet_turns_match_the_contracts_schedule() {
        let genesis = 1_606_824_023;
        assert_eq!(draw_deadline(20763), 1_794_004_200 - DAY);
        assert_eq!(seed_epoch(20763, genesis), Some(486_773));
        assert_eq!(seed_epoch(20764, genesis), Some(486_773 + 225));
        assert_eq!(seed_epoch(18599, genesis), None);
        assert!(seed_epoch(18600, genesis).is_some());
    }

    #[test]
    fn the_last_final_turn_changes_at_each_draw_deadline() {
        let deadline = draw_deadline(20763);
        assert_eq!(last_final_turn(deadline), 20763);
        assert_eq!(last_final_turn(deadline - 1), 20762);
        assert_eq!(last_final_turn(deadline + DAY - 1), 20763);
    }
}
