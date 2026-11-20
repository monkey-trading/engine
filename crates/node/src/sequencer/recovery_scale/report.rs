//! The run's figures against #904's bars: each capture's hold on the sequencer against one command's;
//! the full replay of 30 days of launch flow, projected from the commands the markets' calendars give
//! 30 days and the replay's rate; the restart from the newest cache to its first command, with the
//! start's reads outside the cache apart; and each tail handover's switch against
//! `SWITCH_BUDGET`, with the largest tail a switch fits by a straight line through the measured ones.

use std::time::Duration;

use crate::handover::tail::MAX_TAIL_BYTES;
use crate::handover::tail_room;
use crate::log::PrefixRing;
use crate::sequencer::recovery_scale::cache::{Cached, Restarted};
use crate::sequencer::recovery_scale::handover::Switch;
use crate::sequencer::recovery_scale::restart::Replayed;
use crate::sequencer::recovery_scale::{
    BAR_DAYS, COMMAND_BAR, DAY_SECS, Day, FULL_REPLAY_BAR, RESTART_BAR, SWITCH_BUDGET,
};

/// The reads of the log a production start makes beside a cache: the takeover cut's two of its newest
/// segments, back to its last checkpoint (`node::keys::cut_to_finalized_anchor`).
const TAKEOVER_CUT_READS: u32 = 2;

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

/// Prints each capture's hold on the sequencer, `holds`, against one command's bar, and the cache
/// taken before the hard stop.
pub(super) fn print_captures(holds: &[Duration], cached: &Cached) {
    let mut sorted = holds.to_vec();
    sorted.sort_unstable();
    let max = sorted.last().copied().unwrap_or_default();
    let p50 = sorted.get(sorted.len() / 2).copied().unwrap_or_default();
    println!(
        "RECOVERY captures count={} held_p50_s={:.4} held_max_s={:.4} bar_s={} {}",
        sorted.len(),
        p50.as_secs_f64(),
        max.as_secs_f64(),
        COMMAND_BAR.as_secs_f64(),
        verdict(max, COMMAND_BAR)
    );
    // The authenticated history index a node with paired caches keeps, which each capture waits for.
    let caught_up = cached.history_caught_up.as_secs_f64();
    println!(
        "RECOVERY paired_history behind_commands={} caught_up_s={caught_up:.1} commands_per_s={:.0}",
        cached.history_behind,
        cached.history_behind as f64 / caught_up.max(f64::EPSILON)
    );
    println!(
        "RECOVERY cache seq={} bytes={} capture_held_s={:.4} encode_on_sequencer_s={:.2} published_s={:.1}",
        cached.seq,
        cached.bytes,
        cached.held.as_secs_f64(),
        cached.encode_on_sequencer.as_secs_f64(),
        cached.published.as_secs_f64()
    );
}

/// Prints the restart from the cache and its parts timed apart.
pub(super) fn print_restart(restarted: &Restarted, cached: &Cached) {
    println!(
        "RECOVERY restart_from_cache cache_seq={} suffix_commands={} read_s={:.1} start_s={:.2} s={:.1}",
        restarted.cache_seq,
        restarted.suffix_commands,
        restarted.read.as_secs_f64(),
        restarted.start.as_secs_f64(),
        restarted.took().as_secs_f64()
    );
    assert_eq!(restarted.cache_seq, cached.seq, "the restart reads the cache taken before the stop");
    println!(
        "RECOVERY restart_parts cache_read_s={:.1} posted_seq={} since_head_commands={} posted_check_s={:.1} \
         full_prefix_pass_s={:.1} last_checkpoint={} takeover_cut_read_s={:.4}",
        restarted.cache_read.as_secs_f64(),
        restarted.posted.seq,
        restarted.cache_seq.saturating_sub(restarted.posted.seq),
        restarted.posted_check.as_secs_f64(),
        restarted.full_prefix_pass.as_secs_f64(),
        restarted.last_checkpoint,
        restarted.takeover_cut_read.as_secs_f64()
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
    // A projection, whatever the log's length: the calendars' commands at the rate measured over the log's
    // launch flow, up to the launch leg's last command, where the whole log's rate mixes in ten times it.
    for (name, replay) in [("engine_replay", replayed), ("node_recovery", recovered)] {
        let (full, rate) = (projected(bar_commands, replay), replay.launch_rate());
        println!(
            "RECOVERY bar full_replay_{BAR_DAYS}d {name} launch_commands_per_s={rate:.0} projected_s={:.0} bar_s={} {}",
            full.as_secs_f64(),
            FULL_REPLAY_BAR.as_secs(),
            verdict(full, FULL_REPLAY_BAR)
        );
    }
    let restart = restarted.took();
    println!(
        "RECOVERY bar restart_from_cache suffix_commands={} s={:.1} bar_s={} {}",
        restarted.suffix_commands,
        restart.as_secs_f64(),
        RESTART_BAR.as_secs(),
        verdict(restart, RESTART_BAR)
    );
    // Outside the cache's bound a start reads one checkpoint interval of the log, however long it is.
    println!(
        "RECOVERY start_log_reads newest_segment_reads={TAKEOVER_CUT_READS} s={:.4} restart_bar_s={}",
        restarted.takeover_cut_read.as_secs_f64() * f64::from(TAKEOVER_CUT_READS),
        RESTART_BAR.as_secs()
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

/// The time `bar_commands` of launch flow take to replay at `replayed`'s rate over its launch leg.
pub(super) fn projected(bar_commands: f64, replayed: &Replayed) -> Duration {
    Duration::from_secs_f64(bar_commands / replayed.launch_rate())
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
