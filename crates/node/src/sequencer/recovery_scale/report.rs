//! The run's figures against #904's recovery bar: the full replay of 30 days of launch flow, projected
//! from the commands the markets' calendars give 30 days and the replay's rate; the restart from the
//! newest cache to its first command; and each tail handover's switch against `SWITCH_BUDGET`, with
//! the largest tail a switch fits by a straight line through the measured ones.

use std::time::Duration;

use crate::handover::tail::MAX_TAIL_BYTES;
use crate::handover::tail_room;
use crate::log::PrefixRing;
use crate::sequencer::recovery_scale::handover::Switch;
use crate::sequencer::recovery_scale::restart::{Replayed, Restarted};
use crate::sequencer::recovery_scale::{BAR_DAYS, DAY_SECS, Day, FULL_REPLAY_BAR, RESTART_BAR, SWITCH_BUDGET};

/// Prints a replay's commands, checkpoints, rate and wall time.
pub(super) fn print_replay(name: &str, replayed: &Replayed) {
    println!(
        "RECOVERY {name} commands={} checkpoints={} commands_per_s={:.0} s={:.1}",
        replayed.commands,
        replayed.checkpoints,
        replayed.rate(),
        replayed.took.as_secs_f64()
    );
}

/// Prints one tail handover, its window with `start`, the member's start from its replica, and what a
/// release build's predecessor would refuse it for, if anything.
pub(super) fn print_switch(switch: &Switch, start: Duration) {
    println!(
        "RECOVERY tail bytes={} entries={} read_s={:.3} take_s={:.3} start_s={:.3} window_s={:.3} confirmed={} \
         admitted={}",
        switch.bytes,
        switch.entries,
        switch.read.as_secs_f64(),
        switch.take.as_secs_f64(),
        start.as_secs_f64(),
        switch.window(start).as_secs_f64(),
        switch.confirmed,
        switch.refused.map_or("yes".to_owned(), |why| format!("no ({why})"))
    );
}

/// The log's bytes for each command of launch flow.
pub(super) fn bytes_per_command(days: &[Day]) -> f64 {
    let launch = days.iter().filter(|day| day.multiple == 1.0);
    let (bytes, commands) = launch.fold((0, 0), |(bytes, commands), day| (bytes + day.bytes, commands + day.commands));
    bytes as f64 / commands.max(1) as f64
}

/// Prints each figure against its bar: `bar_commands` are the commands `BAR_DAYS` of launch flow
/// sequence on its markets' calendars, `peak` the log's bytes a second with every market open, and
/// `start` the member's start from its replica.
pub(super) fn conclude(
    days: &[Day],
    (bar_commands, peak): (f64, f64),
    (replayed, recovered): (&Replayed, &Replayed),
    restarted: &Restarted,
    (switches, start): (&[Switch], Duration),
) {
    let launch: Vec<&Day> = days.iter().filter(|day| day.multiple == 1.0).collect();
    let (commands, bytes, secs) = launch.iter().fold((0, 0, 0), |(commands, bytes, secs), day| {
        (commands + day.commands, bytes + day.bytes, secs + day.sequenced.secs)
    });
    let per_command = bytes as f64 / commands.max(1) as f64;
    println!(
        "RECOVERY launch_flow written_days={:.2} commands={commands} bytes={bytes} bytes_per_command={per_command:.0} \
         bar_log_days={BAR_DAYS} bar_log_commands={bar_commands:.0} bar_log_bytes={:.0}",
        secs as f64 / DAY_SECS as f64,
        bar_commands * per_command
    );
    let per_day = bar_commands / BAR_DAYS as f64;
    // A projection, whatever the log's length: the calendars' commands at the measured rate.
    for (name, rate) in [("engine_replay", replayed.rate()), ("node_recovery", recovered.rate())] {
        let full = Duration::from_secs_f64(bar_commands / rate);
        println!(
            "RECOVERY bar full_replay_{BAR_DAYS}d {name} projected_s={:.0} bar_s={} {}",
            full.as_secs_f64(),
            FULL_REPLAY_BAR.as_secs(),
            verdict(full, FULL_REPLAY_BAR)
        );
    }
    let restart = restarted.read + restarted.suffix.took + restarted.start;
    // The launch flow a restart can replay past its cache within the bar, after reading it and starting.
    let room = RESTART_BAR.saturating_sub(restarted.read + restarted.start).as_secs_f64() * restarted.suffix.rate();
    println!(
        "RECOVERY bar restart_from_cache read_s={:.1} suffix_commands={} suffix_s={:.1} start_s={:.2} s={:.1} bar_s={} {} \
         suffix_within_bar_hours={:.1}",
        restarted.read.as_secs_f64(),
        restarted.suffix.commands,
        restarted.suffix.took.as_secs_f64(),
        restarted.start.as_secs_f64(),
        restart.as_secs_f64(),
        RESTART_BAR.as_secs(),
        verdict(restart, RESTART_BAR),
        room / per_day * 24.0
    );
    for switch in switches {
        println!(
            "RECOVERY bar switch tail_bytes={} window_s={:.3} budget_s={} {}",
            switch.bytes,
            switch.window(start).as_secs_f64(),
            SWITCH_BUDGET.as_secs(),
            verdict(switch.window(start), SWITCH_BUDGET)
        );
    }
    match largest_tail(switches, start) {
        Some(bytes) => {
            println!("RECOVERY largest_tail_within_switch_budget bytes={bytes:.0} max_tail_bytes={MAX_TAIL_BYTES}")
        }
        None => println!("RECOVERY largest_tail_within_switch_budget unmeasured: no two tails rising with their bytes"),
    }
    // A predecessor keeps a release build's ring of the latest commands' prefixes, and refuses a tip
    // older than it: at the launch flow's bytes a command, a tail of that many commands.
    let ring = PrefixRing::RELEASE_CAPACITY as f64 * per_command;
    println!(
        "RECOVERY largest_tail_a_predecessor_admits prefix_ring_commands={} bytes={ring:.0} tail_room_bytes={} \
         seconds_of_peak_flow={:.0}",
        PrefixRing::RELEASE_CAPACITY,
        tail_room(),
        ring / peak
    );
}

/// The largest tail whose switch, with `start`, the member's start from its replica, fits
/// `SWITCH_BUDGET`, on the straight line through the first and last confirmed tails, smallest to
/// largest by `TAILS`' order; `None` unless the larger took longer.
fn largest_tail(switches: &[Switch], start: Duration) -> Option<f64> {
    let confirmed: Vec<&Switch> = switches.iter().filter(|switch| switch.confirmed).collect();
    let (first, last) = (*confirmed.first()?, *confirmed.last()?);
    let at = |switch: &Switch| (switch.bytes as f64, (switch.read + switch.take).as_secs_f64());
    let ((small, small_s), (large, large_s)) = (at(first), at(last));
    if large <= small || large_s <= small_s {
        return None;
    }
    let per_byte = (large_s - small_s) / (large - small);
    let fixed = small_s - per_byte * small + start.as_secs_f64();
    Some(((SWITCH_BUDGET.as_secs_f64() - fixed) / per_byte).max(0.0))
}

/// Whether `took` held `bar`.
fn verdict(took: Duration, bar: Duration) -> &'static str {
    if took <= bar { "held" } else { "broken" }
}
