//! The soak (#909): the node load scenario run for hours at ten times launch flow, on a node that
//! takes what a long run gives it. Run by `scripts/dev-soak.sh`; `SOAK_HOURS` sets its length.
//!
//! The node is the load scenario's, run with the launch configuration's storage (`archived`, `durable`): its
//! log's ranges and its daily snapshots go to a loopback stand-in for the bucket, the log's local window and the
//! older snapshots leave the host, and a restart goes on from the newest daily snapshot and the log after it. That
//! shape runs the unpaired history index of the release configuration beside the window (`indexed`), reporting its
//! bytes and lag each interval and the `history_start` it opens at after each start;
//! `SOAK_STORAGE=0` runs the node as one with `[snapshots]` instead (`durable`), whose paired index the log's window
//! cannot run beside: its history owner publishes a recovery cache every `SOAK_SNAPSHOT_SECS` of sequenced time. The generator drives it through its API and
//! stream as the load scenario's does, in intervals of `SOAK_REPORT_MINUTES`, each a leg of the generator's, judged
//! in windows of `SOAK_WINDOW_SECS`. While it runs:
//! - every `SOAK_FAULT_MINUTES` one of #867's faults is injected, in turn (`faults`): a stalled
//!   venue, a dropped one, a slow link, a quiet product, both venues down, and trusted time down;
//! - every `SOAK_RESTART_HOURS` the node is stopped and started again from its newest daily snapshot (its newest
//!   cache with `SOAK_STORAGE=0`) on the same API address, the generator's connections and subscribers
//!   reconnecting as clients do. The first stop drains; the next drops the node as a crash does (`Node::kill`: no
//!   drain, no final checkpoint), so the start replays the log past its snapshot or cache; and so on in turn.
//!
//! At the end of each interval it prints the figures the load scenario prints, then the interval's
//! report (`SOAK`): the windows judged against #904's launch bar, those a fault, a restart or the start
//! excuses apart (`timeline`); memory (macOS's physical footprint, which counts compressed pages, else
//! resident) against the first interval's, allowed the growth of the state, as the newest snapshot's or cache's
//! size sets it, and the stand-in bucket's bytes; the bytes the log and the daily snapshots, or the caches, hold on
//! disk and the log's growth a command; what the bucket holds and the host has let go (`stored`); the stats
//! worker's lag; each fault's recovery and each restart's; and the alerts no event excused. It passes if every
//! interval does (`hours`) and the log replays to its roots from the book at the end, as the load
//! scenario's does, the segments the host lacks read back from the bucket first. It runs no marks cache or alert
//! outbox, so it reports no disk for them. The storage runs at a test build's timings (the window passes every
//! 20 ms, the uploaders poll every 20 ms) and, past the sizes `archive::set_range_seqs` sets, a test build's
//! bounds.
//!
//! `SOAK_HOURS` (8), `SOAK_REPORT_MINUTES` (60), `SOAK_FAULT_MINUTES` (20), `SOAK_RESTART_HOURS` (2,
//! 0 for none), `SOAK_WINDOW_SECS` (120), `SOAK_STORAGE` (1; 0 for `[snapshots]`' caches), `SOAK_DAY_MINUTES` (twice
//! the report interval: the day snapshots and the windows count in), `SOAK_SNAPSHOT_SECS` (300, the template's,
//! with `SOAK_STORAGE=0`), `SOAK_MULTIPLE`
//! (10), `SOAK_SHORT` (1 for a run that tests the harness, as CI's does: an interval with every window
//! excused passes and memory is not bounded), `SOAK_KNOWN_ALERTS` (alerts to expect, by name) and `SOAK_DIR`
//! (a scratch directory, removed with the run; named, it is kept) and `SOAK_LOG_DIR` (the log's directory
//! apart from `SOAK_DIR`, always kept) set the run, and
//! `LOAD_ACCOUNTS`, `LOAD_TRADERS`, `LOAD_SUBSCRIBERS`, `LOAD_CRYPTO_RATE`, `LOAD_RWA_MOVES` and
//! `LOAD_SOURCE_DELAY_MS` size the load as they do the load scenario's.

mod alerts;
mod config;
mod faults;
mod hours;
mod stored;
mod summary;
mod support;
mod timeline;

use std::io::Write;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ChildStdin;
use std::time::Duration;

use engine_core::command::Command;
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Instant, MissedTickBehavior, interval, interval_at, sleep_until, timeout};

use crate::clock::now_ms;
use crate::common::launch_book::{NamedCounts, build};
use crate::feeds::venue::Timing;
use crate::load_scenario::archived::Archived;
use crate::load_scenario::durable::Durable;
use crate::load_scenario::node::{Boot, Node};
use crate::load_scenario::plan::{Leg, Line, Plan, requests_per_second};
use crate::load_scenario::report::{Snapshot, judge, print, unmeasured};
use crate::load_scenario::soak::alerts::Alerts;
use crate::load_scenario::soak::config::Config;
use crate::load_scenario::soak::faults::{Done, Rig, priced_within};
use crate::load_scenario::soak::hours::{Disk, Interval, Restarted, Standing, Win, workload_gaps};
use crate::load_scenario::soak::stored::Stored;
use crate::load_scenario::soak::support::{Reaper, memory_mib, rebind};
use crate::load_scenario::soak::timeline::{Timeline, WINDOW_SETTLE, restart_alerts, start_alerts};
use crate::load_scenario::venues::{self, Venues};
use crate::load_scenario::{
    ACCOUNTS, ACCOUNTS_PER_CLIENT, Generator, LINE_WITHIN, QUOTING_PER_CENT, book_mix, env, moves, spawn_generator,
    stress, wait_priced,
};
use crate::sequencer::Request;
use crate::sequencer::recovery_scale::Base;
use crate::test_support::{Alert, capture_alerts_everywhere, scratch_dir};

/// How long a restart may keep the node down before the generator's clients give up: the drain, the
/// history owner's last cache and the recovery each have a couple of minutes.
const RESTART_DOWN: Duration = Duration::from_secs(7 * 60);

#[test]
#[ignore = "the soak, run by scripts/dev-soak.sh"]
fn dev_scenario_soak() {
    crate::common::rerun::runner_permille();
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("the node's runtime");
    runtime.block_on(run());
}

/// The run.
async fn run() {
    let began = Instant::now();
    let alerts = capture_alerts_everywhere();
    let config = Config::from(&|name| std::env::var(name).ok());
    let mix = book_mix(now_ms(), env("LOAD_ACCOUNTS", ACCOUNTS));
    let traders = env("LOAD_TRADERS", crate::load_scenario::TRADERS);
    let counts = NamedCounts { adders: 1, openers: traders as usize, closeouts: 0 };
    let mut book = build(&mix, counts);
    println!(
        "SOAK book accounts={} positions={} orders={} assembled_s={:.0}; {config:?}",
        book.h.engine.accounts().len(),
        book.positions,
        book.orders,
        book.assembled_in.as_secs_f64()
    );
    // The log starts from the book at seq 0, where a history index and a catalog start.
    let base = Base::new(&std::mem::take(&mut book.h.engine)).expect("the book encodes");
    drop(book);

    let source_delay = Duration::from_millis(env("LOAD_SOURCE_DELAY_MS", crate::load_scenario::SOURCE_DELAY_MS));
    let venues = venues::start(moves(), source_delay, true).await;
    let (dir, kept) = match std::env::var("SOAK_DIR") {
        Ok(dir) => (PathBuf::from(dir), true),
        Err(_) => (scratch_dir("soak"), false),
    };
    let mut durable = Durable::new(&dir, config.snapshot);
    if let Some(log) = &config.log_dir {
        durable.cfg.node.log_dir = log.join("log");
    }
    if config.storage {
        // Ranges of production's size, not a test build's six.
        crate::archive::set_range_seqs(10_000);
        // A stopped node's snapshot writer finishes before its stop returns, as a process that ends leaves none to
        // race the next node's, within the stop's drain (`node::DRAIN`).
        crate::daily_snapshots::wait_on_drop(Duration::from_secs(100));
        if config.day != Duration::from_secs(86_400) {
            crate::daily_snapshots::shorten_day(config.day.as_millis() as i64);
        }
        durable = durable.launching(&dir, Archived::start(config.day).await);
    }
    let (listener, api) = crate::load_scenario::node::listen().await;
    let node =
        Node::start(Boot::Book(Box::new(base.engine())), &durable.cfg.node.log_dir, listener, &venues, Some(&durable))
            .await;
    wait_priced(&node.tx).await;
    let plan = Plan {
        api,
        first_key: 2,
        traders,
        accounts_per_client: ACCOUNTS_PER_CLIENT,
        subscribers: env("LOAD_SUBSCRIBERS", node.api.max_subscribers.get() as u32),
        quoting_per_cent: QUOTING_PER_CENT,
        quote_markets: node.api.quote_max_markets.get(),
        legs: config.legs(),
        ramp: None,
        windows: config.windows_in(config.report.as_secs()),
        screening_bound_ms: u64::try_from(venues.access.max_age.as_millis()).expect("a bound in range"),
        connect_secs: RESTART_DOWN.as_secs(),
        // The soak judges no window by the runner's storage.
        storage_probe: false,
    };
    println!("SOAK data in {} ({})", dir.display(), if kept { "kept" } else { "removed with the run" });
    let timing = Timing::for_venues(&venues::template_venues());
    let mut soak = Soak::new(config, durable, venues, (node, api), (timing, alerts, began));
    soak.drive(&plan).await;
    let (signer, log_dir) = soak.finish().await;
    // A log whose older segments left the host is read from the bucket first, which puts them back.
    if let Some(stored) = &soak.stored {
        stored.archived.read_plain(&mut soak.durable.cfg);
        let put = crate::archive::restore_whole(&soak.durable.cfg, &log_dir).await;
        println!("SOAK replay: segments put back from the bucket: {put:?}");
        put.expect("the segments the host lacks are read back from the bucket");
    }
    let replayed = crate::sequencer::replay::replay_path_from(base.engine(), &log_dir, signer, Default::default())
        .expect("the log replays");
    println!(
        "SOAK replay commands={} checkpoints={}: the run's log replays to its roots from the book",
        replayed.commands, replayed.checkpoints
    );
    soak.conclude();
}

/// What the run holds as it goes.
struct Soak {
    config: Config,
    durable: Durable,
    venues: Venues,
    api: SocketAddr,
    node: Option<Node>,
    timing: Timing,
    timeline: Timeline,
    alerts: Alerts,
    interval: Interval,
    /// The legs the generator runs, each with the windows it is judged in.
    legs: Vec<Leg>,
    /// The commands the nodes before the last restart sequenced since the interval began.
    carried: f64,
    /// What the node's metrics read at the interval's start and at the last window's end.
    start: Snapshot,
    window: Snapshot,
    leg_began: Instant,
    /// Resident memory and the stats worker's lag, sampled through the interval.
    samples: Vec<(u64, u64)>,
    first: Option<Standing>,
    disk: Disk,
    /// Restarts so far; each second one is a crash.
    restarts: u32,
    /// The start's place in the timeline until the first leg begins.
    starting: Option<usize>,
    /// Every interval's failures, by the interval.
    failed: Vec<(String, Vec<String>)>,
    /// The node's storage, when it runs the launch configuration (`SOAK_STORAGE`).
    stored: Option<Stored>,
    /// How many bytes the stand-in bucket held when the memory baseline was set.
    held_at_first: u64,
    /// When the generator began.
    driven: Instant,
}

impl Soak {
    fn new(
        config: Config,
        durable: Durable,
        venues: Venues,
        (node, api): (Node, SocketAddr),
        (timing, alerts, began): (Timing, mpsc::UnboundedReceiver<Alert>, Instant),
    ) -> Self {
        let known = config.known_alerts.clone();
        let stored = durable.archived().map(|archived| {
            let daily = durable.daily_snapshots().expect("[daily_snapshots]").to_owned();
            let history = crate::config::HistorySection {
                dir: durable.cfg.history.dir.clone(),
                retention: durable.cfg.history.retention,
                marks: None,
            };
            Stored::new(archived.clone(), durable.cfg.node.log_dir.clone(), daily, history)
        });
        let (start, window) = (Snapshot::take(&node.metrics), Snapshot::take(&node.metrics));
        let mut timeline = Timeline::default();
        // The start lasts until the generator's first leg: the book's own alerts and the warm-up are its.
        let starting = timeline.begin_at("start", began, &start_alerts());
        Soak {
            config,
            durable,
            venues,
            api,
            node: Some(node),
            timing,
            timeline,
            alerts: Alerts::new(alerts, known),
            interval: Interval::default(),
            legs: Vec::new(),
            carried: 0.0,
            start,
            window,
            leg_began: Instant::now(),
            samples: Vec::new(),
            first: None,
            disk: Disk::default(),
            restarts: 0,
            starting: Some(starting),
            failed: Vec::new(),
            stored,
            held_at_first: 0,
            driven: Instant::now(),
        }
    }

    /// The leg the generator was planned to run under `name`.
    fn planned(&self, name: &str) -> &Leg {
        self.legs.iter().find(|planned| planned.name == name).expect("a planned leg")
    }

    fn node(&self) -> &Node {
        self.node.as_ref().expect("the node is running")
    }

    /// Drives the generator's legs, injecting a fault each `fault_every` and restarting the node each
    /// `restart_every`, until the last leg ends.
    async fn drive(&mut self, plan: &Plan) {
        let Generator { child, mut answers, mut lines } = spawn_generator(plan);
        let mut child = Reaper(child);
        self.legs = plan.legs.clone();
        let began = Instant::now();
        self.driven = began;
        let mut fault_ticks = interval_at(began + self.config.fault_every, self.config.fault_every);
        fault_ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut samples = interval(self.config.sample_every());
        samples.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut restart_at = self.config.restart_every.map(|every| began + every);
        let mut last_line = Instant::now();
        let (done_tx, mut done_rx) = mpsc::unbounded_channel::<Done>();
        let (mut running, mut turn) = (None, 0);
        loop {
            let restart_due = async {
                match restart_at {
                    Some(at) if running.is_none() => sleep_until(at).await,
                    _ => std::future::pending().await,
                }
            };
            tokio::select! {
                line = lines.recv() => match line {
                    Some(line) => {
                        last_line = Instant::now();
                        self.line(line, &mut answers).await;
                    }
                    None => break,
                },
                () = sleep_until(last_line + LINE_WITHIN) => {
                    panic!("the load generator sent nothing for {LINE_WITHIN:?}");
                }
                Some(alert) = self.alerts.recv() => self.alerts.note(&alert),
                _ = samples.tick() => self.sample(),
                _ = fault_ticks.tick() => {
                    if running.is_some() {
                        println!("SOAK fault {} skipped: the last is still running", faults::nth(turn).name());
                    } else {
                        let fault = faults::nth(turn);
                        turn += 1;
                        // Alerts queued behind this tick, a restart's, are stamped before the fault's.
                        self.alerts.drain();
                        running = Some(self.timeline.begin(fault.name(), &fault.explains()));
                        let rig = self.rig();
                        let done = done_tx.clone();
                        println!("SOAK fault {} begins, for {:?}", fault.name(), fault.lasting());
                        tokio::spawn(async move { let _ = done.send(rig.inject(fault).await); });
                    }
                }
                Some(done) = done_rx.recv() => {
                    self.timeline.end(running.take().expect("a fault was running"));
                    self.fault_done(done);
                }
                () = restart_due => {
                    self.restart().await;
                    restart_at = self.config.restart_every.map(|every| Instant::now() + every);
                }
            }
        }
        // A fault still running holds a sender of the node's queue, which its stop waits on.
        if let Some(place) = running.take() {
            let done = done_rx.recv().await.expect("the running fault ends");
            self.timeline.end(place);
            self.fault_done(done);
        }
        let status = child.0.wait().expect("the generator ends");
        assert!(status.success(), "the load generator failed: {status}");
    }

    /// What a fault is injected through, on the node now.
    fn rig(&self) -> Rig {
        let node = self.node();
        Rig {
            links: self.venues.links.clone(),
            quiet: self.venues.quiet.clone(),
            time_out: node.time_out.clone(),
            tx: node.tx.clone(),
            timing: self.timing,
        }
    }

    /// Records a fault that has run and whether it raised the alert it must.
    fn fault_done(&mut self, done: Done) {
        self.alerts.drain();
        let name = done.fault.name();
        let priced =
            done.priced_again.as_ref().map_or_else(|why| format!("NOT PRICED: {why}"), |took| format!("{took:?}"));
        println!("SOAK fault {name} ended; every market priced again after {priced}");
        if let Some(alert) = done.fault.alert() {
            let raised = self.alerts.raised_since(done.began, alert);
            if !raised {
                self.interval.missing_alerts.push(format!("{name} (alert {alert})"));
            }
        }
        self.interval.faults.push(done);
    }

    /// Takes a sample of memory and of how far the stats worker is behind.
    fn sample(&mut self) {
        let behind = self.node.as_ref().map_or(0, |node| node.stats.behind());
        self.samples.push((memory_mib().0, behind));
    }

    /// Stops the node and starts it again from its newest cache, or its newest daily snapshot, on the same API address. A node that does
    /// not stop cleanly ends the run: nothing may start beside what could still write the log.
    async fn restart(&mut self) {
        self.alerts.drain();
        let place = self.timeline.begin("restart", &restart_alerts());
        let old = self.node.take().expect("a node");
        // Each second restart is a crash.
        let abrupt = self.restarts % 2 == 1;
        self.restarts += 1;
        self.carried += Snapshot::take(&old.metrics).since(&self.start).commands;
        let off_host = self.stored.as_ref().map_or(0, |stored| stored.figures().off_host);
        if let Some(stored) = self.stored.as_mut() {
            stored.restarted(self.driven.elapsed(), off_host);
        }
        let stopping = Instant::now();
        let stopped = if abrupt { old.kill().await } else { old.stop(true).await };
        if let Err(error) = stopped {
            panic!("the node did not stop for its restart, so the soak ends rather than start beside it: {error:#}");
        }
        let down = stopping.elapsed();
        let listener = rebind(self.api).await;
        let starting = Instant::now();
        let dir = self.durable.cfg.node.log_dir.clone();
        let node = Node::start(Boot::Recover, &dir, listener, &self.venues, Some(&self.durable)).await;
        // Recovered once the node has stamped a command of its own.
        let (reply, stamped) = oneshot::channel();
        node.tx.send(Request::Submit { cmd: Command::Tick, reply: Some(reply) }).await.expect("the sequencer");
        timeout(RESTART_DOWN, stamped).await.expect("the restarted node stamps a command").expect("an answer");
        let recovered = starting.elapsed();
        let wait = self.timing.reconnect_max + faults::DEADLINE;
        let priced = priced_within(&node.tx, wait).await;
        (self.start, self.window) = (Snapshot::take(&node.metrics), Snapshot::take(&node.metrics));
        self.node = Some(node);
        self.alerts.drain();
        self.timeline.end(place);
        println!(
            "SOAK restart ({}): stopped in {down:?}, recovered to its first command from the newest {} in \
             {recovered:?}, with {off_host} segments of the log off the host, every market priced {}",
            if abrupt { "crash, no drain" } else { "drained" },
            if self.stored.is_some() { "daily snapshot" } else { "cache" },
            priced.as_ref().map_or_else(|why| format!("NOT: {why}"), |took| format!("{took:?} after"))
        );
        self.interval.restarts.push(Restarted { abrupt, stopped: down, recovered, priced });
    }

    /// One line from the generator.
    async fn line(&mut self, line: Line, answers: &mut ChildStdin) {
        match line {
            Line::Ready { traders, subscribers } => {
                println!("SOAK generator ready traders={traders} subscribers={subscribers}")
            }
            Line::Start { leg, multiple } => {
                if let Some(starting) = self.starting.take() {
                    self.timeline.end(starting);
                }
                println!("SOAK {leg} begins at {multiple}x launch flow");
                (self.start, self.window) =
                    (Snapshot::take(&self.node().metrics), Snapshot::take(&self.node().metrics));
                self.leg_began = Instant::now();
                self.carried = 0.0;
                self.samples.clear();
            }
            Line::Window { leg, index, figures, .. } => {
                let now = Snapshot::take(&self.node().metrics);
                let breaches = judge(&figures, &now.since(&self.window));
                // The generator's own arithmetic: a leg's windows are equal parts of its length.
                let planned = self.planned(&leg);
                let length = Duration::from_secs(planned.secs) / planned.windows.max(1);
                let from = self.leg_began + length * index;
                let excused = self.timeline.excuse(from, from + length, WINDOW_SETTLE).map(str::to_owned);
                let gaps = unmeasured(&figures, &now.since(&self.window));
                println!("SOAK {leg} window={index} excused={excused:?} breaches={breaches:?} gaps={gaps:?}");
                self.window = now;
                self.interval.windows.push(Win { index, breaches, gaps, excused });
            }
            Line::End { leg, multiple, figures } => {
                let node_figures = Snapshot::take(&self.node().metrics).since(&self.start);
                let breaches = judge(&figures, &node_figures);
                let expected = requests_per_second(multiple) * self.planned(&leg).secs as f64;
                self.interval.workload = workload_gaps(&figures, expected);
                let peak = self.samples.iter().map(|(rss, _)| *rss).max().unwrap_or(0);
                println!("SOAK {leg} figures, the injected faults included, the node's since its last restart:");
                print(&leg, multiple, &figures, &node_figures, peak, &breaches);
                println!("LOAD leg={leg} stress_levels {:?}", stress(&self.node().tx).await);
                self.report(&leg, self.carried + node_figures.commands).await;
                writeln!(answers, "next").expect("the generator reads");
            }
        }
    }
}
