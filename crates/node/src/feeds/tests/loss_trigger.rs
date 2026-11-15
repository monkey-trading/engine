//! The house loss trigger's dev scenario offline, as `scripts/dev-loss-trigger.sh` runs it: loopback
//! Coinbase and Kraken stand-ins price BTC-USD through a sequencer at the latest protocol, under the
//! dev genesis with production's loss threshold of zero at level 2 and a 30 s cooldown in place of
//! the five minutes, so the scenario runs in about a minute and a half. Twice:
//! 1. The client buys at level 0, paying BTC's first band. The stand-ins then move its way 10 bps at
//!    a time. While the house is still ahead over the day, BTC stays at level 0; the move that puts
//!    it behind raises BTC to level 2, and the house's loss then is at most that one move's: the
//!    bound, the threshold plus the command that crosses it.
//! 2. The client's next buy pays level 2's spread.
//! 3. The stand-ins move back until the house is ahead again. BTC holds level 2 through the cooldown
//!    and then steps down, and the client's close pays the band alone.
//!
//! The node then stops, its log replays to every root, and a restart recovers the same loss.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use engine_core::Fixed;
use engine_core::command::Command;
use engine_core::types::{MarketId, Side};
use tokio::sync::mpsc;
use tokio::task::AbortHandle;
use tokio::time::{sleep, timeout};

use crate::config::Genesis;
use crate::feeds::tests::toxicity::{
    Mid, boot_with, fill, fund, keep_time, now, query, signed, start_venues, wait_for_index,
};
use crate::replay_path;
use crate::sequencer::Request;
use crate::tee::EnclaveKey;
use crate::test_support::{dev_config, scratch_dir};

const BTC: MarketId = 1;
/// The access seed of the winning client.
const WINNER: u8 = 1;
/// The scenario's stress cooldown.
const COOLDOWN_MS: u64 = 30_000;
/// The spread a calm fill of a few BTC pays: the dev genesis's first BTC-USD band, 20 tenths of a bp,
/// at level 0's 1x.
const BAND_BPS: i64 = 2;
/// Level 2's: the dev genesis's 10 bp minimum and four times the band.
const LEVEL_TWO_BPS: i64 = 18;
/// Each move the stand-ins make, in bps of the mid, and the most moves a round makes either way.
const STEP_BPS: i64 = 10;
const MAX_STEPS: usize = 40;

#[tokio::test]
#[ignore = "dev scenario, run by scripts/dev-loss-trigger.sh"]
async fn dev_scenario_loss_trigger() {
    let dir = scratch_dir("feeds-loss-trigger");
    let key = Arc::new(EnclaveKey::generate());
    let genesis = genesis();
    let (tx, task) = boot_with(&dir, &key, &genesis, true);
    let clock = keep_time(&tx);
    let mid: Mid = Arc::new(Mutex::new(Fixed::from_int(80_000)));
    let venues = start_venues(&tx, &mid).await;
    wait_for_index(&tx, now(&mid)).await;
    fund(&tx, WINNER).await;
    assert_eq!(reading(&tx).await, (Fixed::ZERO, 0), "an idle house has lost nothing");

    for round in 1..=2 {
        // 1. A buy at level 0, then moves the client's way until the house is behind on the day.
        let entry = now(&mid);
        let buy = fill(&tx, WINNER, Side::Buy, "0.5", entry).await;
        assert_eq!(buy.spread_bps, BAND_BPS, "round {round}: {buy:?}");
        let mut before = reading(&tx).await;
        let mut fired = None;
        for _ in 0..MAX_STEPS {
            assert_eq!((before.1, level(&tx).await), (0, 0), "round {round}: the house is ahead at {}", before.0);
            let (from, to) = step(&tx, &mid, STEP_BPS).await;
            let after = reading(&tx).await;
            if after.1 > 0 {
                // The bound: the house had lost nothing over the day, so its loss is at most this one
                // move's, the client's 0.5 BTC times the move.
                assert!(after.0 <= before.0 + (to - from).mul(buy.qty), "round {round}: {after:?} after {before:?}");
                fired = Some(after);
                break;
            }
            before = after;
        }
        let fired = fired.unwrap_or_else(|| panic!("round {round}: {MAX_STEPS} moves left the house ahead"));
        assert_eq!((fired.1, level(&tx).await), (2, 2), "round {round}");
        println!("round {round}: the house lost {} over the day, and BTC is at level 2", fired.0);

        // 2. The client's next buy pays level 2.
        let add = fill(&tx, WINNER, Side::Buy, "0.5", now(&mid)).await;
        assert_eq!(add.spread_bps, LEVEL_TWO_BPS, "round {round}: {add:?}");

        // 3. Back until the house is ahead; BTC holds level 2 through the cooldown, then steps down.
        for _ in 0..MAX_STEPS {
            if !reading(&tx).await.0.is_positive() {
                break;
            }
            step(&tx, &mid, -STEP_BPS).await;
        }
        assert_eq!((reading(&tx).await.1, level(&tx).await), (0, 2), "round {round}: the cooldown holds the level");
        let stepped = async {
            while level(&tx).await != 0 {
                sleep(Duration::from_millis(250)).await;
            }
        };
        let waited = timeout(Duration::from_millis(2 * COOLDOWN_MS), stepped).await;
        assert!(waited.is_ok(), "round {round}: BTC did not step down after the cooldown");
        let close = fill(&tx, WINNER, Side::Sell, "1", now(&mid)).await;
        assert_eq!((close.spread_bps, close.position), (BAND_BPS, Fixed::ZERO), "round {round}: {close:?}");
        println!("round {round}: BTC is back at level 0 and the house's loss is {}", reading(&tx).await.0);
    }

    // 4. Stopped, the log replays to every root; restarted, the node recovers the same loss.
    venues.iter().for_each(AbortHandle::abort);
    signed(&tx, |_| Command::Tick).await;
    let stopped = reading(&tx).await;
    clock.abort();
    task.abort();
    let replayed = replay_path(&dir).expect("the log replays");
    assert!(replayed.commands > 0 && replayed.checkpoints > 0);
    let (tx, _task) = boot_with(&dir, &key, &genesis, false);
    assert_eq!(reading(&tx).await, stopped);
    println!("replayed {} commands; the restart recovered a loss of {}", replayed.commands, stopped.0);
}

/// The dev genesis with production's loss threshold of zero at level 2, and a cooldown of `COOLDOWN_MS`.
fn genesis() -> Genesis {
    let mut genesis = dev_config().genesis;
    let params = &mut genesis.params;
    params.stress_levels[2].trigger.as_mut().expect("level 2 has a trigger").house_loss = Some(Fixed::ZERO);
    params.stress_cooldown_ms = COOLDOWN_MS as i64;
    genesis
}

/// Moves the stand-ins' mid by `bps` and waits for the index to follow, returning the mid before and
/// after.
async fn step(tx: &mpsc::Sender<Request>, mid: &Mid, bps: i64) -> (Fixed, Fixed) {
    let from = now(mid);
    let to = from + from.mul_bps(bps);
    *mid.lock().unwrap() = to;
    wait_for_index(tx, to).await;
    (from, to)
}

/// The house's loss over the trailing day and the level it sets, as the last command left them.
async fn reading(tx: &mpsc::Sender<Request>) -> (Fixed, usize) {
    let reading = query(tx, |engine| engine.house_loss()).await.expect("the latest protocol reads the loss");
    (reading.loss, reading.level)
}

/// BTC-USD's stress level.
async fn level(tx: &mpsc::Sender<Request>) -> usize {
    query(tx, |engine| engine.markets()[&BTC].stress.level).await
}
