//! The node's side of each leg's figures: its metrics read before and after, as an operator's
//! Prometheus reads them, the process's CPU and resident memory, and the leg judged against #904's
//! launch bar together with what the generator saw.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::load_scenario::plan::{
    BUSY_SHARE, Figures, ORDER_TO_FILL_US, PRICE_TO_STAMP_S, Quantiles, STREAM_LAG_P99_MS,
};
use crate::metrics::Metrics;

/// The histograms the report reads, by their exported names.
const PRICE_TO_STAMP: &str = "engine_price_to_stamp_seconds";
const REQUEST: &str = "engine_api_request_seconds";
const STREAM_LAG: &str = "engine_stream_lag_seconds";
const BATCH: &str = "engine_sequencer_batch_seconds";
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
/// its labels.
fn parse(text: &str) -> (BTreeMap<String, Histogram>, BTreeMap<String, f64>) {
    let (mut histograms, mut counters) = (BTreeMap::<String, Histogram>::new(), BTreeMap::new());
    for line in text.lines().filter(|line| !line.starts_with('#')) {
        let Some((series, value)) = line.rsplit_once(' ') else { continue };
        let Ok(value) = value.parse::<f64>() else { continue };
        let (name, labels) = series.split_once('{').unwrap_or((series, ""));
        if let Some(family) = name.strip_suffix("_bucket") {
            let bound = labels.split("le=\"").nth(1).and_then(|rest| rest.split('"').next());
            let bound = match bound {
                Some("+Inf") => f64::INFINITY,
                Some(bound) => bound.parse().unwrap_or(f64::NAN),
                None => continue,
            };
            histograms.entry(family.to_owned()).or_default().buckets.push((bound, value as u64));
        } else if let Some(family) = name.strip_suffix("_sum").filter(|family| histograms.contains_key(*family)) {
            histograms.entry(family.to_owned()).or_default().sum = value;
        } else if let Some(family) = name.strip_suffix("_count").filter(|family| histograms.contains_key(*family)) {
            histograms.entry(family.to_owned()).or_default().count = value as u64;
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

    /// The highest sampled since the last reset, and a new start from now.
    pub(super) fn reset(&self) -> u64 {
        self.peak.swap(process().1, Ordering::Relaxed)
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
    if figures.stream_lag_ms.p99 > STREAM_LAG_P99_MS {
        let (figure, bar) = (figures.stream_lag_ms.p99 as f64, STREAM_LAG_P99_MS as f64);
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
        "LOAD leg={leg} multiple={multiple} secs={:.0} requests={} fills={} late={} commands_per_second={:.0} replies[{}]",
        node.secs,
        figures.sent.values().sum::<u64>(),
        figures.fills,
        figures.late,
        node.commands / node.secs,
        replies.join(" ")
    );
    println!("LOAD leg={leg} order_to_fill_us {} bar={ORDER_TO_FILL_US:?}", us(&figures.order_to_fill_us));
    println!("LOAD leg={leg} request_us(client) {}", us(&figures.request_us));
    println!("LOAD leg={leg} api_request_us(node) {}", node_us(&node.request));
    let bar: Vec<u64> = PRICE_TO_STAMP_S.iter().map(|s| (s * 1e6) as u64).collect();
    println!("LOAD leg={leg} price_to_stamp_us {} bar={bar:?}", node_us(&node.price_to_stamp));
    println!(
        "LOAD leg={leg} stream_lag_ms(client) {} frames={} quote_frames={} disconnects={} bar_p99={STREAM_LAG_P99_MS}",
        us(&figures.stream_lag_ms),
        figures.frames,
        figures.quote_frames,
        figures.disconnects
    );
    println!("LOAD leg={leg} stream_publish_us(node) {}", node_us(&node.stream_lag));
    println!("LOAD leg={leg} sequencer_batch_us {}", node_us(&node.batch));
    println!(
        "LOAD leg={leg} sequencer_busy_share={:.3} bar={BUSY_SHARE} cpu_cores={:.2} resident_mib={rss_mib}",
        node.busy_share, node.cpu_cores
    );
    let verdict = if breaches.is_empty() { "within".to_owned() } else { format!("OVER {breaches:?}") };
    println!("LOAD bar leg={leg} multiple={multiple} {verdict}");
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEXT: &str = "# TYPE engine_price_to_stamp_seconds histogram\n\
        engine_price_to_stamp_seconds_sum 0.5\n\
        engine_price_to_stamp_seconds_count 100\n\
        engine_price_to_stamp_seconds_bucket{le=\"0.001\"} 50\n\
        engine_price_to_stamp_seconds_bucket{le=\"0.01\"} 99\n\
        engine_price_to_stamp_seconds_bucket{le=\"+Inf\"} 100\n\
        # TYPE engine_fills counter\n\
        engine_fills_total{market=\"1\"} 3\n\
        engine_fills_total{market=\"2\"} 4\n";

    #[test]
    fn a_histogram_is_read_and_judged_at_its_bounds() {
        let (histograms, counters) = parse(TEXT);
        let h = &histograms[PRICE_TO_STAMP];
        assert_eq!((h.count, h.sum, h.buckets.len()), (100, 0.5, 3));
        assert_eq!(counters["engine_fills_total"], 7.0, "a counter sums over its labels");
        assert!(h.within(0.5, 0.001), "50 of 100 within a millisecond holds the median there");
        assert!(!h.within(0.51, 0.001));
        assert!(h.within(0.99, 0.01) && !h.within(0.999, 0.01));
        assert!((h.quantile(0.5) - 0.001).abs() < 1e-12, "{}", h.quantile(0.5));
        assert_eq!(h.quantile(0.999), 0.01, "past the last finite bound, the bound");
        let earlier = Histogram { buckets: vec![(0.001, 40), (0.01, 40), (f64::INFINITY, 40)], sum: 0.1, count: 40 };
        let since = h.since(&earlier);
        assert_eq!((since.count, since.buckets[0].1, since.buckets[1].1), (60, 10, 59));
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
}
