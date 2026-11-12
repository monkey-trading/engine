//! The generations a node's `TurnTaken` waits on: read from the finalised schedule for each turn the
//! log assigns the node's key and has yet to see taken, when the schedule names the node's release.

use super::*;

use std::sync::Mutex;

use engine_core::Fixed;
use engine_core::auth::Operator;
use engine_core::eip712::B256;
use engine_core::types::Millis;

use crate::checkpoints::{Observation, ScheduledTurn, VaultState};
use crate::log::Checkpoint;
use crate::sequencer::tests::operator::{recover_turns, turn_log};
use crate::test_support::dev_config;

const RELEASE: Release = Release { launch_digest: [0x11; 48], config_sha256: [0xc1; 32] };

#[tokio::test]
async fn the_generation_of_the_logs_untaken_turn_for_this_key_is_published() {
    let (dir, clock, turn, member) = turn_log("turn-generations");
    let recorded = dev_config().operator().unwrap();
    let seq = recover_turns(&dir, clock, Operator { address: member, ..recorded }, B256::repeat_byte(0x56));
    let (tx, rx) = mpsc::channel(16);
    let _run = tokio::spawn(seq.run(rx));
    let schedule = Schedule(Mutex::new(vec![drawn(turn, RELEASE.id(), 7)]));
    let (generations, mut published) = watch::channel(BTreeMap::new());
    let every = Duration::from_millis(50);
    let _watch = tokio::spawn(watch_turn_generations(schedule, RELEASE, member, tx.clone(), generations, every));
    tokio::time::timeout(Duration::from_secs(5), published.changed()).await.expect("nothing published").unwrap();
    assert_eq!(*published.borrow(), BTreeMap::from([(turn, 7)]));

    // Another key's log assigns it no turn.
    assert_eq!(untaken_turns(&tx, AccountId([0xee; 20])).await.unwrap(), Vec::<u64>::new());
}

#[tokio::test]
async fn only_a_drawn_turn_naming_this_release_has_its_generation_read() {
    let (turn, other) = (20_754, [0x99; 32]);
    let schedule = Schedule(Mutex::new(vec![drawn(turn - 1, RELEASE.id(), 5), drawn(turn + 1, RELEASE.id(), 6)]));
    // The schedule has not drawn the turn: the latest at its boundary is the one before.
    assert_eq!(read_generations(&schedule, &RELEASE, &[turn]).await.unwrap(), BTreeMap::new());
    schedule.0.lock().unwrap().push(drawn(turn, other, 7));
    assert_eq!(read_generations(&schedule, &RELEASE, &[turn]).await.unwrap(), BTreeMap::new(), "another release's");
    schedule.0.lock().unwrap().last_mut().unwrap().release = RELEASE.id();
    let read = read_generations(&schedule, &RELEASE, &[turn, turn + 1]).await.unwrap();
    assert_eq!(read, BTreeMap::from([(turn, 7), (turn + 1, 6)]));
}

// ── Helpers ──

/// A finalised schedule's drawn turns.
struct Schedule(Mutex<Vec<ScheduledTurn>>);

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

    async fn scheduled_turn(&self, at: &Observation, now_ms: Millis) -> anyhow::Result<Option<ScheduledTurn>> {
        anyhow::ensure!(at.view == View::Finalized, "the schedule is read at finality");
        let turns = self.0.lock().unwrap();
        Ok(turns.iter().filter(|turn| turn.boundary <= now_ms).max_by_key(|turn| turn.turn).cloned())
    }
}

/// `turn`, drawn for a member running `release` in `generation`.
fn drawn(turn: u64, release: [u8; 32], generation: u64) -> ScheduledTurn {
    ScheduledTurn {
        turn,
        boundary: engine_core::turns::boundary(turn).unwrap(),
        candidate: AccountId([0xc1; 20]),
        admin_key: AccountId([0xb1; 20]),
        release,
        generation,
        set: 1,
        source: None,
    }
}
