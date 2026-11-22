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
fn a_window_prints_its_batch_tail_busy_share_and_host_waits() {
    let metrics = Metrics::new();
    let earlier = Snapshot::take(&metrics);
    metrics.batch_seconds.observe(0.25);
    let window = Snapshot::take(&metrics).since(&earlier);
    let window = Delta { busy_share: 0.5, host: Waited { io_ms: Some(12.0), ..Waited::default() }, ..window };
    // Each quantile is interpolated within the bucket from 0.2048 s to 0.4096 s, as Prometheus does.
    assert_eq!(
        window_figures(&window),
        "batch_p99_us=407552 batch_p999_us=409395 busy_share=0.500 host_io_wait_ms=12 host_cpu_wait_ms=n/a \
         host_disk_io=n/a"
    );
}

/// A launch-flow window of some 3,000 batches with one 250 ms sync stall, which breaks the 25 ms
/// order-to-fill p99 bar with the one order it catches: the 32 batches formed while the sync is held
/// wait from 250 ms down to nothing. The p999 names the stall, and the p99 stays under the bar.
#[test]
fn a_windows_batch_p999_names_one_sync_stall_its_p99_misses() {
    let metrics = Metrics::new();
    let earlier = Snapshot::take(&metrics);
    (0..3_000).for_each(|_| metrics.batch_seconds.observe(0.000_5));
    (0..32).for_each(|formed| metrics.batch_seconds.observe(0.25 * f64::from(32 - formed) / 32.0));
    let window = Snapshot::take(&metrics).since(&earlier);
    let printed = |name: &str| -> f64 {
        let line = window_figures(&window);
        let field = line.split(' ').find_map(|field| field.strip_prefix(name)).expect("printed");
        field.parse().expect("a figure")
    };
    let bar_us = ORDER_TO_FILL_US[1] as f64;
    assert!(printed("batch_p99_us=") < bar_us, "p99 {} misses the stall", printed("batch_p99_us="));
    assert!(printed("batch_p999_us=") > 200_000.0, "p999 {} names the stall", printed("batch_p999_us="));
}

#[test]
fn a_snapshot_delta_carries_what_the_hosts_waits_grew_by() {
    let earlier = Snapshot {
        waits: Waits { io_us: Some(1_000), cpu_us: Some(2_000), disk_io: Some((10, 4_096)) },
        ..Snapshot::take(&Metrics::new())
    };
    let later = Snapshot {
        waits: Waits { io_us: Some(51_000), cpu_us: Some(2_500), disk_io: Some((15, 8_192)) },
        ..earlier.clone()
    };
    let waited = Waited { io_ms: Some(50.0), cpu_ms: Some(0.5), disk_io: Some((5, 4_096)) };
    assert_eq!(later.since(&earlier).host, waited);
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
