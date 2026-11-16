//! A start's own check against a schedule read that does or does not cover the range's in-office turn.

use super::*;

/// A log that stops leaves a promotion unmirrored however long it stops for: a start's own check counts
/// only when the schedule read covers the range's in-office turn, as one from that turn does, and counts
/// for nothing when the read begins after it, as a tail take's does before the keys name the range.
#[tokio::test]
async fn an_own_check_counts_only_when_the_read_covers_a_checkpoint_older_than_eight_turns() {
    // A takes turn 2 at 10; B, promoted in it, sequenced 21 to 30, and its checkpoint at 20 was accepted
    // before it failed. The log stopped there until turn 12, when B is promoted again and starts.
    let begin = |turn: u64| engine_core::turns::boundary(turn).unwrap();
    let mut recorded = Takings::default();
    recorded.command(10, &[Event::TurnTaken { turn: 2, candidate: candidate(A), release: A }]);
    recorded.stamped(10, begin(2) + 1);
    recorded.stamped(34, begin(12) + 1);
    let schedule = Promoted(std::sync::Mutex::new(Vec::new()));
    let own =
        |read: &Reassigned| own_check_read(Some(head(20)), &recorded, &[], head(34), (B2, &member(B)), Some(read));
    // A promoted start names the in-office turn, A's, and its read from it finds B's earlier promotion.
    let promotion = crate::standby_key::chain::Promotion {
        turn: 12,
        position: 3,
        signer_epoch: 3,
        generation: 9,
        head_seq: 34,
        head_link: B256::ZERO,
        checkpoint_seq: 34,
        checkpoint_root: B256::ZERO,
        at: 0,
        block: 0,
        carried: 12,
    };
    let promoted = crate::node::log_check::read_promoted(&schedule, &recorded, Some(head(20)), &promotion);
    let promoted = promoted.await.unwrap();
    assert_eq!((schedule.0.lock().unwrap().pop(), promoted.from), (Some(2..13), 2), "read from the in-office turn");
    assert_eq!(own(&promoted), Some(head(20)), "B's member checked its own work");
    // A tail take reads eight turns back, which leaves the in-office turn out: the check counts for nothing.
    let taken = crate::node::log_check::read_reassigned(&schedule, (12, None), None, &member(B)).await.unwrap();
    assert_eq!((schedule.0.lock().unwrap().pop(), taken.from), (Some(4..13), 4));
    assert_eq!(own(&taken), Some(head(20)), "a read that missed the in-office turn cleared the range");
    // The same read from the in-office turn, without B's promotion, clears the range.
    assert_eq!(own(&Reassigned { assigned: Vec::new(), from: 2 }), Some(head(34)));
}

// ── Helpers ──

/// A schedule whose turn 2 was drawn for A and promoted to B, recording each range of turns read.
struct Promoted(std::sync::Mutex<Vec<std::ops::Range<u64>>>);

impl crate::checkpoints::CheckpointChain for Promoted {
    async fn state(&self, _view: crate::checkpoints::View) -> anyhow::Result<crate::checkpoints::VaultState> {
        anyhow::bail!("this chain reads no vault state")
    }

    async fn post(&self, _checkpoint: &crate::log::Checkpoint) -> anyhow::Result<()> {
        anyhow::bail!("this chain posts nothing")
    }

    async fn reassignments(&self, turns: std::ops::Range<u64>) -> anyhow::Result<Vec<Assigned>> {
        self.0.lock().unwrap().push(turns.clone());
        let at =
            |position, release, reassigned| Assigned { turn: 2, position, candidate: candidate(release), reassigned };
        Ok(if turns.contains(&2) { vec![at(0, A, false), at(1, B, true)] } else { Vec::new() })
    }
}
