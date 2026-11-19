//! The recovery scenario's figures, on small replays.

use std::time::Duration;

use engine_core::command::{Command, Sequenced};

use crate::log::{CommandEntry, LogEntry};
use crate::sequencer::recovery_scale::report::projected;
use crate::sequencer::recovery_scale::restart::{Replayed, Through};

fn command(seq: u64) -> LogEntry {
    LogEntry::Command(CommandEntry {
        sequenced: Sequenced { seq, ts: 0, cmd: Command::Tick },
        events: None,
        seal: None,
    })
}

/// A replay whose whole log runs at 100 commands a second and whose launch leg at 50.
fn mixed() -> Replayed {
    Replayed {
        commands: 1_000,
        checkpoints: 1,
        took: Duration::from_secs(10),
        launch: Some((500, Duration::from_secs(10))),
    }
}

#[test]
fn the_projection_uses_the_launch_leg_rate() {
    // 5,000 commands at 50 a second, not at the whole log's 100.
    assert_eq!(projected(5_000.0, &mixed()), Duration::from_secs(100));
}

#[test]
fn a_replay_is_timed_through_the_launch_leg_s_last_command() {
    let mut through = Through::new(3);
    (1..=5).for_each(|seq| through.observe(&command(seq)));
    let (commands, _) = through.finish(Duration::from_secs(9));
    assert_eq!(commands, 3);
}

#[test]
fn a_log_ending_with_the_launch_leg_is_timed_to_its_end() {
    let mut through = Through::new(5);
    (1..=5).for_each(|seq| through.observe(&command(seq)));
    assert_eq!(through.finish(Duration::from_secs(9)), (5, Duration::from_secs(9)));
}
