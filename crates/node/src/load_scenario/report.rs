//! The node's side of each leg's figures: its metrics read before and after, as an operator's
//! Prometheus reads them, the CPU and resident memory of the process, which holds the node, the venue
//! stand-ins and the screening source, and the leg judged against #904's launch bar together with
//! what the generator saw.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::load_scenario::plan::{
    BUSY_SHARE, Figures, ORDER_TO_FILL_P50_GUARD_US, ORDER_TO_FILL_US, PRICE_TO_STAMP_S, Quantiles, STREAM_LAG_P99_MS,
};
use crate::metrics::Metrics;

/// The histograms the report reads, by their exported names.
const PRICE_TO_STAMP: &str = "engine_price_to_stamp_seconds";
const REQUEST: &str = "engine_api_request_seconds";
const STREAM_LAG: &str = "engine_stream_lag_seconds";
const BATCH: &str = "engine_sequencer_batch_seconds";
/// Every budget of the launch bar `judge` holds a leg to, by name and quantile.
pub(super) const BUDGETS: [(&str, &str); 8] = [
    ("order_to_fill_us", "p50"),
    ("order_to_fill_us", "p99"),
    ("order_to_fill_us", "p999"),
    ("price_to_stamp_us", "p50"),
    ("price_to_stamp_us", "p99"),
    ("price_to_stamp_us", "p999"),
    ("stream_lag_ms", "p99"),
    ("sequencer_busy_share", "mean"),
];
/// The counters it reads.
const BUSY: &str = "engine_sequencer_busy_seconds_total";
const COMMANDS: &str = "engine_commands_total";

/// A histogram's cumulative counts at each bucket bound, its sum and its count.
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct Histogram {
    pub buckets: Vec<(f64, u64)>,
    pub sum: f64,
    pub count: u64,
}

/// The node's metrics, the process's CPU time and the time they were read at.
#[derive(Clone, Debug)]
pub(super) struct Snapshot {
    at: Instant,
    histograms: BTreeMap<String, Histogram>,
    counters: BTreeMap<String, f64>,
    cpu_s: f64,
}

/// What the node did between two snapshots.
#[derive(Clone, Debug)]
pub(super) struct Delta {
    pub secs: f64,
    pub price_to_stamp: Histogram,
    pub request: Histogram,
    pub stream_lag: Histogram,
    pub batch: Histogram,
    pub busy_share: f64,
    pub cpu_cores: f64,
    pub commands: f64,
}

/// A budget a leg broke: what, at which quantile, the figure and the bar.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Breach {
    pub budget: &'static str,
    pub quantile: &'static str,
    pub figure: f64,
    pub bar: f64,
}

impl Histogram {
    /// The observations between `earlier` and this.
    fn since(&self, earlier: &Histogram) -> Histogram {
        let buckets = self
            .buckets
            .iter()
            .map(|(bound, count)| {
                let before = earlier.buckets.iter().find(|(b, _)| b == bound).map_or(0, |(_, c)| *c);
                (*bound, count.saturating_sub(before))
            })
            .collect();
        Histogram { buckets, sum: self.sum - earlier.sum, count: self.count.saturating_sub(earlier.count) }
    }

    /// The `q` quantile, interpolated within its bucket as Prometheus's `histogram_quantile` does,
    /// in seconds; the highest finite bound for one past it, and 0 with no observations.
    pub(super) fn quantile(&self, q: f64) -> f64 {
        let wanted = q * self.count as f64;
        let mut below = (0.0, 0u64);
        for (bound, count) in &self.buckets {
            if *count as f64 >= wanted && wanted > 0.0 {
                if bound.is_infinite() {
                    return below.0;
                }
                let within = (count - below.1) as f64;
                let share = if within == 0.0 { 1.0 } else { (wanted - below.1 as f64) / within };
                return below.0 + (bound - below.0) * share;
            }
            below = (*bound, *count);
        }
        0.0
    }

    /// Whether at least the `q` share of observations are within `bound`, one of the buckets'
    /// bounds: exactly whether the `q` quantile is within it.
    pub(super) fn within(&self, q: f64, bound: f64) -> bool {
        let at = self.buckets.iter().find(|(b, _)| (b - bound).abs() < bound * 1e-9).map(|(_, count)| *count);
        let at = at.unwrap_or_else(|| panic!("{bound} is no bucket bound"));
        at as f64 >= (q * self.count as f64).ceil()
    }
}

impl Snapshot {
    /// The node's metrics now, from their text, and the process's CPU time.
    pub(super) fn take(metrics: &Metrics) -> Self {
        let (histograms, counters) = parse(&metrics.encode());
        Snapshot { at: Instant::now(), histograms, counters, cpu_s: process().0 }
    }

    /// What the node did from `earlier` to this.
    pub(super) fn since(&self, earlier: &Snapshot) -> Delta {
        let secs = self.at.duration_since(earlier.at).as_secs_f64().max(1e-9);
        let histogram = |name: &str| {
            let (now, then) = (self.histograms.get(name).cloned().unwrap_or_default(), earlier.histograms.get(name));
            now.since(then.unwrap_or(&Histogram::default()))
        };
        let counter = |name: &str| {
            self.counters.get(name).copied().unwrap_or(0.0) - earlier.counters.get(name).copied().unwrap_or(0.0)
        };
        Delta {
            secs,
            price_to_stamp: histogram(PRICE_TO_STAMP),
            request: histogram(REQUEST),
            stream_lag: histogram(STREAM_LAG),
            batch: histogram(BATCH),
            busy_share: counter(BUSY) / secs,
            cpu_cores: (self.cpu_s - earlier.cpu_s) / secs,
            commands: counter(COMMANDS),
        }
    }
}

/// Every histogram and every counter in `text`, the registry's encoding, each counter summed over
/// its labels. A histogram is a family its `# TYPE` line names one, whose sum and count come before
/// its buckets.
fn parse(text: &str) -> (BTreeMap<String, Histogram>, BTreeMap<String, f64>) {
    let (mut histograms, mut counters) = (BTreeMap::<String, Histogram>::new(), BTreeMap::new());
    for line in text.lines() {
        if let Some(family) = line.strip_prefix("# TYPE ").and_then(|rest| rest.strip_suffix(" histogram")) {
            histograms.insert(family.to_owned(), Histogram::default());
            continue;
        }
        let Some((series, value)) = line.rsplit_once(' ').filter(|_| !line.starts_with('#')) else { continue };
        let Ok(value) = value.parse::<f64>() else { continue };
        let (name, labels) = series.split_once('{').unwrap_or((series, ""));
        let part = |suffix: &str| name.strip_suffix(suffix).filter(|family| histograms.contains_key(*family));
        if let Some(family) = part("_bucket") {
            let bound = labels.split("le=\"").nth(1).and_then(|rest| rest.split('"').next());
            let bound = match bound {
                Some("+Inf") => f64::INFINITY,
                Some(bound) => bound.parse().unwrap_or(f64::NAN),
                None => continue,
            };
            histograms.get_mut(family).expect("a histogram").buckets.push((bound, value as u64));
        } else if let Some(family) = part("_sum") {
            histograms.get_mut(family).expect("a histogram").sum = value;
        } else if let Some(family) = part("_count") {
            histograms.get_mut(family).expect("a histogram").count = value as u64;
        } else if name.ends_with("_total") {
            *counters.entry(name.to_owned()).or_default() += value;
        }
    }
    (histograms, counters)
}

/// The process's CPU time in seconds and its resident memory in MiB, as `ps` reports them.
pub(super) fn process() -> (f64, u64) {
    let out = std::process::Command::new("ps")
        .args(["-o", "time=,rss=", "-p", &std::process::id().to_string()])
        .output()
        .expect("ps runs");
    let text = String::from_utf8_lossy(&out.stdout);
    let mut fields = text.split_whitespace();
    let cpu = fields.next().map_or(0.0, cpu_seconds);
    let rss_kib: u64 = fields.next().and_then(|rss| rss.parse().ok()).unwrap_or(0);
    (cpu, rss_kib >> 10)
}

/// `[[dd-]hh:]mm:ss[.ss]` as seconds.
fn cpu_seconds(time: &str) -> f64 {
    let (days, clock) = time.split_once('-').map_or((0.0, time), |(days, clock)| (days.parse().unwrap_or(0.0), clock));
    let clock = clock.split(':').fold(0.0, |total, part| total * 60.0 + part.parse::<f64>().unwrap_or(0.0));
    days * 86_400.0 + clock
}

/// The process's highest resident memory in MiB since `reset`, sampled every second.
pub(super) struct Resident {
    peak: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
}

impl Resident {
    /// Starts sampling.
    pub(super) fn start() -> Self {
        let (peak, stop) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicBool::new(false)));
        let (into, stopped) = (peak.clone(), stop.clone());
        std::thread::spawn(move || {
            while !stopped.load(Ordering::Relaxed) {
                into.fetch_max(process().1, Ordering::Relaxed);
                std::thread::sleep(Duration::from_secs(1));
            }
        });
        Resident { peak, stop }
    }

    /// The highest sampled since the last reset; the next second's sample starts the next peak.
    pub(super) fn reset(&self) -> u64 {
        self.peak.swap(0, Ordering::Relaxed)
    }
}

impl Drop for Resident {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// The budgets `figures` and `node` break, of #904's launch bar.
pub(super) fn judge(figures: &Figures, node: &Delta) -> Vec<Breach> {
    let mut breaches = Vec::new();
    let quantiles = ["p50", "p99", "p999"];
    let fill = &figures.order_to_fill_us;
    let fills = [fill.p50, fill.p99, fill.p999];
    for ((quantile, figure), bar) in quantiles.iter().zip(fills).zip(ORDER_TO_FILL_US) {
        if figure > bar {
            breaches.push(Breach { budget: "order_to_fill_us", quantile, figure: figure as f64, bar: bar as f64 });
        }
    }
    for ((quantile, q), bar) in quantiles.iter().zip([0.5, 0.99, 0.999]).zip(PRICE_TO_STAMP_S) {
        if !node.price_to_stamp.within(q, bar) {
            let figure = node.price_to_stamp.quantile(q) * 1e6;
            breaches.push(Breach { budget: "price_to_stamp_us", quantile, figure, bar: bar * 1e6 });
        }
    }
    // The bar holds every subscriber: a frame reaches each within it, at its own p99.
    if figures.slowest_subscriber_p99_ms > STREAM_LAG_P99_MS {
        let (figure, bar) = (figures.slowest_subscriber_p99_ms as f64, STREAM_LAG_P99_MS as f64);
        breaches.push(Breach { budget: "stream_lag_ms", quantile: "p99", figure, bar });
    }
    if node.busy_share > BUSY_SHARE {
        breaches.push(Breach {
            budget: "sequencer_busy_share",
            quantile: "mean",
            figure: node.busy_share,
            bar: BUSY_SHARE,
        });
    }
    breaches
}

/// What keeps a leg from measuring what it claims, which no budget can show: a request refused,
/// rejected or unanswered, no market order filled, a subscriber that took no frame, lost its
/// connection or had not taken the leg's last commands by the end of its drain, or no price
/// stamped.
pub(super) fn unmeasured(figures: &Figures, node: &Delta) -> Vec<String> {
    let gaps = [
        (figures.refused > 0, format!("{} requests refused, rejected or failed", figures.refused)),
        (figures.unanswered > 0, format!("{} requests unanswered at the drain's end", figures.unanswered)),
        (figures.order_to_fill_us.count == 0, "no market order filled".to_owned()),
        (figures.silent_subscribers > 0, format!("{} subscribers took no frame", figures.silent_subscribers)),
        (figures.disconnects > 0, format!("{} subscriber connections ended", figures.disconnects)),
        (
            figures.behind_subscribers > 0,
            format!(
                "{} subscribers had not taken the leg's last commands by its drain's end",
                figures.behind_subscribers
            ),
        ),
        (node.price_to_stamp.count == 0, "no price stamped".to_owned()),
    ];
    gaps.into_iter().filter(|(gap, _)| *gap).map(|(_, why)| why).collect()
}

/// The breaches a measured leg fails on: every one of the bar's, but an order-to-fill p50 within its
/// regression guard (`ORDER_TO_FILL_P50_GUARD_US`), which the bar's breaches still show.
pub(super) fn guarded(windows: &[Vec<Breach>]) -> Vec<Vec<Breach>> {
    let within_guard = |breach: &&Breach| {
        (breach.budget, breach.quantile) == ("order_to_fill_us", "p50")
            && breach.figure <= ORDER_TO_FILL_P50_GUARD_US as f64
    };
    windows.iter().map(|breaches| breaches.iter().filter(|b| !within_guard(b)).cloned().collect()).collect()
}

/// A loaded runner's tolerance, as `sequencer::wait_bench::price_wait_budget` grants one: a budget at
/// the median must hold in every window, and one at a tail, or the busy share, in all but `misses`
/// of them. Returns what failed it.
pub(super) fn tolerate(windows: &[Vec<Breach>], misses: usize) -> Vec<String> {
    let mut failed = Vec::new();
    let mut tails: BTreeMap<(&'static str, &'static str), usize> = BTreeMap::new();
    for (index, breaches) in windows.iter().enumerate() {
        for breach in breaches {
            if breach.quantile == "p50" {
                failed
                    .push(format!("window {index}: {} p50 {:.0} over {:.0}", breach.budget, breach.figure, breach.bar));
            } else {
                *tails.entry((breach.budget, breach.quantile)).or_default() += 1;
            }
        }
    }
    for ((budget, quantile), count) in tails {
        if count > misses {
            failed.push(format!("{budget} {quantile} over its bar in {count} of {} windows", windows.len()));
        }
    }
    failed
}

/// Prints a leg's figures, and the budgets it broke.
pub(super) fn print(leg: &str, multiple: f64, figures: &Figures, node: &Delta, rss_mib: u64, breaches: &[Breach]) {
    let us = |q: &Quantiles| format!("p50={} p99={} p999={} max={} count={}", q.p50, q.p99, q.p999, q.max, q.count);
    let node_us = |h: &Histogram| {
        let at = |q| (h.quantile(q) * 1e6).round() as u64;
        format!("p50={} p99={} p999={} count={}", at(0.5), at(0.99), at(0.999), h.count)
    };
    let replies: Vec<String> = figures.replies.iter().map(|(kind, count)| format!("{kind}={count}")).collect();
    println!(
        "LOAD leg={leg} multiple={multiple} secs={:.0} requests={} fills={} late={} refused={} unanswered={} \
         commands_per_second={:.0} replies[{}]",
        node.secs,
        figures.sent.values().sum::<u64>(),
        figures.fills,
        figures.late,
        figures.refused,
        figures.unanswered,
        node.commands / node.secs,
        replies.join(" ")
    );
    println!("LOAD leg={leg} order_to_fill_us {} bar={ORDER_TO_FILL_US:?}", us(&figures.order_to_fill_us));
    println!("LOAD leg={leg} request_us(client) {}", us(&figures.request_us));
    println!("LOAD leg={leg} api_request_us(node) {}", node_us(&node.request));
    let bar: Vec<u64> = PRICE_TO_STAMP_S.iter().map(|s| (s * 1e6) as u64).collect();
    println!("LOAD leg={leg} price_to_stamp_us {} bar={bar:?}", node_us(&node.price_to_stamp));
    println!(
        "LOAD leg={leg} stream_lag_ms(client) {} slowest_subscriber_p99={} silent={} frames={} quote_frames={} \
         disconnects={} bar_p99={STREAM_LAG_P99_MS}",
        us(&figures.stream_lag_ms),
        figures.slowest_subscriber_p99_ms,
        figures.silent_subscribers,
        figures.frames,
        figures.quote_frames,
        figures.disconnects
    );
    println!("LOAD leg={leg} stream_send_us(node) {}", node_us(&node.stream_lag));
    println!("LOAD leg={leg} sequencer_batch_us {}", node_us(&node.batch));
    println!(
        "LOAD leg={leg} sequencer_busy_share={:.3} bar={BUSY_SHARE} process_cpu_cores={:.2} process_resident_mib={rss_mib}",
        node.busy_share, node.cpu_cores
    );
    let verdict = if breaches.is_empty() { "within".to_owned() } else { format!("OVER {breaches:?}") };
    println!("LOAD bar leg={leg} multiple={multiple} {verdict}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_histogram_is_read_from_the_nodes_encoding_and_judged_at_its_bounds() {
        let metrics = Metrics::new();
        let waited = &metrics.latency.price_to_stamp;
        (0..50).for_each(|_| waited.observe(0.000_5));
        (0..49).for_each(|_| waited.observe(0.005));
        waited.observe(0.5);
        metrics.fills.get_or_create(&crate::metrics::MarketLabel { market: 1 }).inc_by(3);
        metrics.fills.get_or_create(&crate::metrics::MarketLabel { market: 2 }).inc_by(4);
        let (histograms, counters) = parse(&metrics.encode());
        let h = &histograms[PRICE_TO_STAMP];
        assert_eq!((h.count, h.buckets.len()), (100, 17), "{h:?}");
        assert!((h.sum - 0.770).abs() < 1e-9, "{}", h.sum);
        assert_eq!(counters["engine_fills_total"], 7.0, "a counter sums over its labels");
        assert!(h.within(0.5, 0.001), "50 of 100 within a millisecond holds the median there");
        assert!(!h.within(0.51, 0.001));
        assert!(h.within(0.99, 0.01) && !h.within(0.999, 0.01));
        // The 50th of 100 observations is the last in the bucket up to half a millisecond.
        assert!((h.quantile(0.5) - 0.000_5).abs() < 1e-12, "{}", h.quantile(0.5));
        assert!((h.quantile(0.995) - 0.375).abs() < 1e-9, "halfway into the bucket from 0.25 s to 0.5 s");
        let earlier = h.clone();
        waited.observe(0.000_5);
        let (histograms, _) = parse(&metrics.encode());
        let since = histograms[PRICE_TO_STAMP].since(&earlier);
        assert_eq!((since.count, since.buckets[4].1, since.buckets[3].1), (1, 1, 1), "{since:?}");
    }

    #[test]
    fn a_leg_that_measured_nothing_or_was_refused_is_named_unmeasured() {
        let metrics = Metrics::new();
        let before = Snapshot::take(&metrics);
        metrics.latency.price_to_stamp.observe(0.000_1);
        let node = Snapshot::take(&metrics).since(&before);
        let filled = Quantiles { count: 1, p50: 1, p99: 1, p999: 1, max: 1 };
        let clean = Figures { order_to_fill_us: filled, ..Figures::default() };
        assert!(unmeasured(&clean, &node).is_empty());
        for (gap, figures) in [
            ("refused", Figures { refused: 1, ..clean.clone() }),
            ("unanswered", Figures { unanswered: 1, ..clean.clone() }),
            ("filled", Figures { order_to_fill_us: Quantiles::default(), ..clean.clone() }),
            ("took no frame", Figures { silent_subscribers: 1, ..clean.clone() }),
            ("ended", Figures { disconnects: 1, ..clean.clone() }),
            ("last commands", Figures { behind_subscribers: 1, ..clean.clone() }),
        ] {
            let gaps = unmeasured(&figures, &node);
            assert!(gaps.len() == 1 && gaps[0].contains(gap), "{gap}: {gaps:?}");
        }
        let idle = Snapshot::take(&metrics);
        assert_eq!(unmeasured(&clean, &idle.since(&idle)), ["no price stamped"]);
    }

    #[test]
    fn cpu_time_reads_both_platforms_formats() {
        assert_eq!(cpu_seconds("1:02.50"), 62.5);
        assert_eq!(cpu_seconds("01:01:02"), 3_662.0);
        assert_eq!(cpu_seconds("1-00:00:01"), 86_401.0);
    }

    #[test]
    fn a_loaded_runner_may_miss_a_tail_but_never_the_median() {
        let tail = Breach { budget: "order_to_fill_us", quantile: "p99", figure: 30_000.0, bar: 25_000.0 };
        let median = Breach { budget: "price_to_stamp_us", quantile: "p50", figure: 2_000.0, bar: 1_000.0 };
        let windows = vec![vec![tail.clone()], vec![tail.clone()], vec![], vec![], vec![]];
        assert!(tolerate(&windows, 2).is_empty());
        let windows = vec![vec![tail.clone()], vec![tail.clone()], vec![tail.clone()], vec![], vec![]];
        assert_eq!(tolerate(&windows, 2).len(), 1);
        let windows = vec![vec![median], vec![], vec![], vec![], vec![]];
        assert_eq!(tolerate(&windows, 2).len(), 1, "a median over its bar in one window fails");
    }

    #[test]
    fn only_order_to_fills_median_is_held_to_its_guard_in_place_of_the_bar() {
        let fill = |figure| Breach { budget: "order_to_fill_us", quantile: "p50", figure, bar: 5_000.0 };
        let stamp = Breach { budget: "price_to_stamp_us", quantile: "p50", figure: 2_000.0, bar: 1_000.0 };
        let tail = Breach { budget: "order_to_fill_us", quantile: "p99", figure: 30_000.0, bar: 25_000.0 };
        let guard = ORDER_TO_FILL_P50_GUARD_US as f64;
        let windows = vec![vec![fill(5_800.0)], vec![fill(guard)], vec![], vec![], vec![]];
        assert!(tolerate(&guarded(&windows), 2).is_empty(), "within the guard, over the bar");
        let windows = vec![vec![fill(guard + 1.0)], vec![], vec![], vec![], vec![]];
        assert_eq!(tolerate(&guarded(&windows), 2).len(), 1, "over the guard");
        // Every other budget is still held to the bar.
        let windows = vec![vec![stamp], vec![tail.clone()], vec![tail.clone()], vec![tail], vec![]];
        assert_eq!(tolerate(&guarded(&windows), 2).len(), 2);
    }
}
