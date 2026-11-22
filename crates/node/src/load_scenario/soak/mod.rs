//! The soak (#909): the node load scenario run for hours at ten times launch flow, on a node that
//! takes what a long run gives it. Run by `scripts/dev-soak.sh`; `SOAK_HOURS` sets its length.
//!
//! The node is the load scenario's, run as a node with `[snapshots]` is (`durable`): its history owner
//! publishes a recovery cache every `SOAK_SNAPSHOT_SECS` of sequenced time, and the generator drives
//! it through its API and stream as the load scenario's does, in intervals of `SOAK_REPORT_MINUTES`,
//! each a leg of the generator's, judged in windows of `SOAK_WINDOW_SECS`. While it runs:
//! - every `SOAK_FAULT_MINUTES` one of #867's faults is injected, in turn (`faults`): a stalled
//!   venue, a dropped one, a slow link, a quiet product, both venues down, and trusted time down;
//! - every `SOAK_RESTART_HOURS` the node is stopped and started again from its newest cache on the
//!   same API address, the generator's connections and subscribers reconnecting as clients do. The
//!   first stop drains, which leaves its newest cache at the final checkpoint; the next drops the node
//!   as a crash does (`Node::kill`: no drain, no final checkpoint), so the start replays the log past
//!   its newest cache; and so on in turn.
//!
//! At the end of each interval it prints the figures the load scenario prints, then the interval's
//! report (`SOAK`): the windows judged against #904's launch bar, those a fault, a restart or the start
//! excuses apart (`timeline`); resident memory against the first interval's, allowed the growth of the
//! newest cache, which the state's size sets; the bytes the log and the caches hold on disk and the
//! log's growth a command; the stats worker's lag; each fault's recovery and each restart's; and the
//! alerts no event excused. It passes if every interval does (`hours`) and the log replays to its roots
//! from the book at the end, as the load scenario's does. It does not run the marks cache or the
//! alert outbox, so it holds no figure for their disk.
//!
//! `SOAK_HOURS` (8), `SOAK_REPORT_MINUTES` (60), `SOAK_FAULT_MINUTES` (20), `SOAK_RESTART_HOURS` (2,
//! 0 for none), `SOAK_WINDOW_SECS` (120), `SOAK_SNAPSHOT_SECS` (300, the template's), `SOAK_MULTIPLE`
//! (10), `SOAK_SHORT` (1 for a run that tests the harness, as CI's does: an interval with every window
//! excused passes and memory is not bounded), `SOAK_KNOWN_ALERTS` (alerts to expect, by name) and `SOAK_DIR`
//! (a scratch directory, removed with the run; named, it is kept) set the run, and
//! `LOAD_ACCOUNTS`, `LOAD_TRADERS`, `LOAD_SUBSCRIBERS`, `LOAD_CRYPTO_RATE`, `LOAD_RWA_MOVES` and
//! `LOAD_SOURCE_DELAY_MS` size the load as they do the load scenario's.

mod alerts;
mod config;
mod faults;
mod hours;
mod support;
mod timeline;

use std::io::Write;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ChildStdin;
use std::time::Duration;

use engine_core::command::Command;
use engine_core::types::AccountId;
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Instant, MissedTickBehavior, interval, interval_at, sleep_until, timeout};

use crate::clock::now_ms;
use crate::common::launch_book::{NamedCounts, build};
use crate::feeds::venue::Timing;
use crate::load_scenario::durable::Durable;
use crate::load_scenario::node::{Boot, Node};
use crate::load_scenario::plan::{Leg, Line, Plan, requests_per_second};
use crate::load_scenario::report::{Snapshot, judge, print, process, unmeasured};
use crate::load_scenario::soak::alerts::Alerts;
use crate::load_scenario::soak::config::Config;
use crate::load_scenario::soak::faults::{Done, Rig, priced_within};
use crate::load_scenario::soak::hours::{
    Disk, Interval, Restarted, Standing, Win, baseline, dir_bytes, failures, median, memory_failure, workload_gaps,
};
use crate::load_scenario::soak::support::{Reaper, log_quiet, newest_cache, rebind};
use crate::load_scenario::soak::timeline::{FEED_ALERTS, Timeline, WINDOW_SETTLE, restart_alerts, start_alerts};
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
    let durable = Durable::new(&dir, config.snapshot);
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
    };
    println!("SOAK data in {} ({})", dir.display(), if kept { "kept" } else { "removed with the run" });
    let timing = Timing::for_venues(&venues::template_venues());
    let mut soak = Soak::new(config, durable, venues, (node, api), (timing, alerts, began));
    soak.drive(&plan).await;
    let (signer, log_dir) = soak.finish().await;
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

    /// Takes a sample of resident memory and of how far the stats worker is behind.
    fn sample(&mut self) {
        let behind = self.node.as_ref().map_or(0, |node| node.stats.behind());
        self.samples.push((process().1, behind));
    }

    /// Stops the node and starts it again from its newest cache, on the same API address. A node that does
    /// not stop cleanly ends the run: nothing may start beside what could still write the log.
    async fn restart(&mut self) {
        self.alerts.drain();
        let place = self.timeline.begin("restart", &restart_alerts());
        let old = self.node.take().expect("a node");
        // Each second restart is a crash.
        let abrupt = self.restarts % 2 == 1;
        self.restarts += 1;
        self.carried += Snapshot::take(&old.metrics).since(&self.start).commands;
        let stopping = Instant::now();
        let stopped = if abrupt { old.kill().await } else { old.stop(true).await };
        if let Err(error) = stopped {
            panic!("the node did not stop for its restart, so the soak ends rather than start beside it: {error:#}");
        }
        if let Err(error) = log_quiet(&self.durable.cfg.node.log_dir).await {
            panic!("the stopped node's log did not settle, so the soak ends rather than start beside it: {error:#}");
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
            "SOAK restart ({}): stopped in {down:?}, recovered to its first command from the newest cache in \
             {recovered:?}, every market priced {}",
            if abrupt { "crash, no drain" } else { "drained" },
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
            Line::Window { leg, index, figures } => {
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

    /// Judges and prints the interval that has ended.
    async fn report(&mut self, leg: &str, commands: f64) {
        self.alerts.drain();
        let mut interval = std::mem::take(&mut self.interval);
        let (unexplained, counts) = self.alerts.report(&self.timeline);
        interval.unexpected_alerts = unexplained;
        let mut failed = failures(&interval, !self.config.short);
        let disk =
            Disk { log: dir_bytes(&self.durable.cfg.node.log_dir), snapshots: dir_bytes(self.durable.snapshots()) };
        let rss: Vec<u64> = self.samples.iter().map(|(rss, _)| *rss).collect();
        let standing = Standing {
            rss_mib: if rss.is_empty() { process().1 } else { median(&rss) },
            state_bytes: newest_cache(&self.durable),
        };
        self.first = baseline(self.first, standing);
        let first = self.first.unwrap_or(standing);
        if !self.config.short && self.first.is_some() {
            failed.extend(memory_failure(first, standing));
        }
        if !self.node().history_runs() {
            failed.push("the history owner stopped, so the node takes no more recovery caches".to_owned());
        }
        let (excused, judged) =
            interval.windows.iter().fold((0, 0), |(excused, judged), window| match window.excused {
                Some(_) => (excused + 1, judged),
                None => (excused, judged + 1),
            });
        let behind = self.samples.iter().map(|(_, behind)| *behind).max().unwrap_or(0);
        let grown = disk.log.saturating_sub(self.disk.log);
        println!("SOAK {leg} windows={} judged={judged} excused={excused}", interval.windows.len());
        println!(
            "SOAK {leg} memory resident_mib median={} max={} first_interval_median={} newest_cache_bytes={}",
            standing.rss_mib,
            self.samples.iter().map(|(rss, _)| *rss).max().unwrap_or(0),
            first.rss_mib,
            standing.state_bytes
        );
        println!(
            "SOAK {leg} disk_bytes log={} caches_and_history_index={} marks_cache=not run alert_outbox=not run; \
             log_growth_bytes={grown} commands={commands:.0} log_bytes_per_command={:.0}",
            disk.log,
            disk.snapshots,
            grown as f64 / commands.max(1.0)
        );
        println!("SOAK {leg} stats_worker_behind_max_commands={behind}");
        println!(
            "SOAK {leg} history_owner takes_captures={} index_behind_commands={:?}",
            self.node().history_runs(),
            self.node().history_behind().await
        );
        println!("SOAK {leg} alerts raised: {counts:?}");
        let verdict = if failed.is_empty() { "PASS".to_owned() } else { format!("FAIL {failed:?}") };
        println!("SOAK {leg} verdict {verdict}");
        self.disk = disk;
        self.samples.clear();
        self.failed.push((leg.to_owned(), failed));
    }

    /// Stops the node, and returns its signer and log, to replay.
    async fn finish(&mut self) -> (AccountId, PathBuf) {
        let place = self.timeline.begin("stop", &FEED_ALERTS);
        let node = self.node.take().expect("a node");
        let (signer, log_dir) = (node.signer, node.log_dir.clone());
        node.stop(false).await.expect("the node drains");
        self.alerts.drain();
        self.timeline.end(place);
        // What ended after the last report: a fault, a restart or an alert no interval has judged.
        let mut tail = std::mem::take(&mut self.interval);
        (tail.unexpected_alerts, _) = self.alerts.report(&self.timeline);
        self.failed.push(("after the last interval".to_owned(), failures(&tail, false)));
        (signer, log_dir)
    }

    /// Fails the run if any interval failed.
    fn conclude(&self) {
        let failed: Vec<_> = self.failed.iter().filter(|(_, why)| !why.is_empty()).collect();
        println!("SOAK {} of {} intervals failed", failed.len(), self.failed.len());
        assert!(failed.is_empty(), "the soak failed: {failed:?}");
    }
}
