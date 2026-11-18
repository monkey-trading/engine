//! The sources a turn's member takes the keys from (`sources`), walked in the order it tries them.

use super::*;

use std::collections::BTreeMap;

use engine_core::Fixed;
use engine_core::types::AccountId;

use crate::checkpoints::{VaultState, View};
use crate::log::Checkpoint;

/// A schedule's drawn turns, newest last, the candidacies `handover_source_at` answers, and, by turn,
/// the members of its assignments before the latest.
#[derive(Default)]
struct Schedule {
    turns: Vec<ScheduledTurn>,
    registered: Vec<HandoverSource>,
    superseded: BTreeMap<u64, Vec<HandoverSource>>,
}

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
        Ok(self.turns.iter().rev().find(|turn| turn.boundary <= now_ms).cloned())
    }

    async fn handover_source_at(
        &self,
        _at: &Observation,
        candidate: AccountId,
    ) -> anyhow::Result<Option<HandoverSource>> {
        Ok(self.registered.iter().find(|source| source.candidate == candidate).cloned())
    }

    async fn superseded_members(&self, _at: &Observation, turn: u64) -> anyhow::Result<Vec<HandoverSource>> {
        Ok(self.superseded.get(&turn).cloned().unwrap_or_default())
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
        set: 1,
        source,
    }
}

#[tokio::test]
async fn a_member_looks_back_past_members_that_never_took_their_turns() {
    let (a, b, c, x) = (operator(0xa, "a:1"), operator(0xb, "b:1"), operator(0xc, "c:1"), operator(0xe, ""));
    let turns = vec![
        drawn(20_750, 0xb, Some(a.clone())),
        drawn(20_751, 0xe, Some(b.clone())),
        drawn(20_752, 0xa, Some(x)),
        drawn(20_753, 0xb, Some(c)),
        drawn(20_755, 0xb, Some(a.clone())),
        drawn(20_756, 0xc, Some(b.clone())),
    ];
    let schedule = Schedule { turns, ..Schedule::default() };
    let at = schedule.observe(View::Finalized, &[]).await.unwrap();
    let turn = schedule.turns.last().unwrap().clone();
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
    let schedule = Schedule { turns, ..Schedule::default() };
    let at = schedule.observe(View::Finalized, &[]).await.unwrap();
    let turn = schedule.turns.last().unwrap().clone();
    let found = sources(&schedule, &at, &turn).await.unwrap();
    assert_eq!(found, [operator(0xb, "b:1")], "the turn and the seven before it");
}

#[tokio::test]
async fn a_member_asks_each_member_an_earlier_turn_skipped_past_after_the_walk() {
    // Turn 20_751 is drawn for D while A holds 20_750, so A is its source. In 20_750 a skip names B, who
    // takes the keys from A, and a skip past B names C, who never takes them: B holds them. 20_750 was
    // drawn while F held 20_749, whose source is G.
    let (a, b, c, d) = (operator(0xa, "a:1"), operator(0xb, "b:1"), operator(0xc, "c:1"), operator(0xd, "d:1"));
    let (f, g) = (operator(0xf, "f:1"), operator(0x9, "g:1"));
    let schedule = Schedule {
        turns: vec![
            drawn(20_749, 0xf, Some(g.clone())),
            drawn(20_750, 0xc, Some(f.clone())),
            drawn(20_751, 0xd, Some(a.clone())),
        ],
        registered: vec![a.clone(), b.clone(), c.clone(), d, f.clone(), g.clone()],
        superseded: BTreeMap::from([(20_750, vec![a.clone(), b.clone()])]),
    };
    let at = schedule.observe(View::Finalized, &[]).await.unwrap();
    let turn = schedule.turns.last().unwrap().clone();
    let found = sources(&schedule, &at, &turn).await.unwrap();
    // A, the source; C, 20_750's latest member, and F, its source; G, 20_749's source (F again is
    // listed already); then B, whom the skip to C passed (A again is listed already).
    assert_eq!(found, [a, c, f, g, b]);
}
