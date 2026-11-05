use super::*;

use engine_core::Fixed;
use engine_core::types::AccountId;

use crate::checkpoints::{VaultState, View};
use crate::log::Checkpoint;

/// A schedule's drawn turns, newest last.
struct Schedule(Vec<ScheduledTurn>);

impl CheckpointChain for Schedule {
    async fn state(&self, _view: View) -> anyhow::Result<VaultState> {
        let zero = Fixed::ZERO;
        Ok(VaultState {
            height: 1,
            signer: AccountId([0; 20]),
            checkpoint: None,
            balance: zero,
            deposits: 0,
            deposited: zero,
            distributions_paid: zero,
        })
    }

    async fn post(&self, _checkpoint: &Checkpoint) -> anyhow::Result<()> {
        anyhow::bail!("this vault takes no checkpoints")
    }

    async fn scheduled_turn(&self, _at: &Observation, now_ms: Millis) -> anyhow::Result<Option<ScheduledTurn>> {
        Ok(self.0.iter().rev().find(|turn| turn.boundary <= now_ms).cloned())
    }
}

fn operator(byte: u8, endpoint: &str) -> HandoverSource {
    HandoverSource {
        candidate: AccountId([byte; 20]),
        release: [byte; 32],
        api_hostname: format!("node-{byte}.test"),
        handover_endpoint: endpoint.into(),
    }
}

fn drawn(turn: u64, member: u8, source: Option<HandoverSource>) -> ScheduledTurn {
    ScheduledTurn {
        turn,
        boundary: engine_core::turns::boundary(turn).unwrap(),
        candidate: AccountId([member; 20]),
        admin_key: AccountId([member + 0x10; 20]),
        release: [member; 32],
        generation: turn,
        source,
    }
}

#[tokio::test]
async fn a_member_looks_back_past_members_that_never_took_their_turns() {
    let (a, b, c, x) = (operator(0xa, "a:1"), operator(0xb, "b:1"), operator(0xc, "c:1"), operator(0xe, ""));
    let schedule = Schedule(vec![
        drawn(20_750, 0xb, Some(a.clone())),
        drawn(20_751, 0xe, Some(b.clone())),
        drawn(20_752, 0xa, Some(x)),
        drawn(20_753, 0xb, Some(c)),
        drawn(20_755, 0xb, Some(a.clone())),
        drawn(20_756, 0xc, Some(b.clone())),
    ]);
    let at = schedule.observe(View::Finalized, &[]).await.unwrap();
    let turn = schedule.0.last().unwrap().clone();
    let found = sources(&schedule, &at, &turn).await.unwrap();
    // B, then A, skipping the gap at 20_754, the operator with no endpoint, the member itself, and
    // operators listed already.
    assert_eq!(found, [b, a]);
}

#[tokio::test]
async fn the_look_back_ends_a_week_before_the_turn() {
    let mut turns: Vec<_> = (0..9).map(|k| drawn(20_750 + k, 0xc, Some(operator(0xc, "c:1")))).collect();
    turns[0].source = Some(operator(0xa, "a:1"));
    turns[1].source = Some(operator(0xb, "b:1"));
    let schedule = Schedule(turns);
    let at = schedule.observe(View::Finalized, &[]).await.unwrap();
    let turn = schedule.0.last().unwrap().clone();
    let found = sources(&schedule, &at, &turn).await.unwrap();
    assert_eq!(found, [operator(0xb, "b:1")], "the turn and the seven before it");
}
