//! `market_no_price` counts from trusted time's first sample of the run, before which nothing is
//! sequenced, and through any later halt.

use super::*;

use crate::test_support::trusted_now;

/// A fed market unpriced from the start pages `after` from trusted time's first sample, whether that
/// comes at once or late, and again `after` from losing its price later, as through a trusted time
/// halt, which never takes the first sample back. An unfed market is named before the first sample.
#[test]
fn no_price_counts_from_trusted_times_first_sample() {
    let after = Duration::from_secs(5);
    let t0 = Instant::now();
    let stale = || Err(PriceError::Stale { fresh: 0, needed: 2 });
    for first_sample in [0, 61] {
        let mut no_price = NoPrice::new(BTreeSet::from([1]));
        let mut check = |price: Result<(), PriceError>, secs: u64| {
            let markets = sample(vec![market_sample(1, true, price), market_sample(2, true, stale())]);
            let sampled = secs >= first_sample;
            let lines = capture(|| no_price.check(&markets, t0 + Duration::from_secs(secs), after, sampled));
            lines.into_iter().map(move |line| (secs, line.key(), line.severity)).collect::<Vec<_>>()
        };
        let mut lines: Vec<_> = (0..first_sample + 20).flat_map(|secs| check(stale(), secs)).collect();
        // Priced, then unpriced again while trusted time halts sequencing.
        lines.extend(check(Ok(()), first_sample + 20));
        lines.extend((first_sample + 21..first_sample + 40).flat_map(|secs| check(stale(), secs)));
        let no_price = "market_no_price{market=1}".to_owned();
        let expected = vec![
            (0, "market_unfed{market=2}".to_owned(), Some(Severity::Warning)),
            (first_sample + 5, no_price.clone(), Some(Severity::Critical)),
            (first_sample + 20, no_price.clone(), None),
            (first_sample + 26, no_price, Some(Severity::Critical)),
        ];
        assert_eq!(lines, expected, "first sample at {first_sample}s");
    }
}

/// The monitor reads trusted time's samples: with the first late, no market pages before it, and a
/// market still unpriced pages `after` from it.
#[tokio::test(start_paused = true)]
async fn the_monitor_pages_no_price_only_from_trusted_times_first_sample() {
    crate::test_support::keep_callsites_interested();
    let metrics = Arc::new(Metrics::new());
    let (layer, mut router) = channel(Routes::default(), metrics.clone());
    let _logging = set_default(registry().with(layer.with_filter(filter_fn(is_alert_callsite))));
    let (tx, mut rx) = mpsc::channel(8);
    let (samples, samples_rx) = watch::channel(None);
    let sources = BTreeMap::from([(BTC, BTreeSet::from(["venue-a".to_owned(), "venue-b".to_owned()]))]);
    tokio::spawn(run(tx, metrics, Duration::from_secs(3), sources, samples_rx));
    // BTC has no quote, so every sample holds it unpriced.
    let engine = dev_engine();
    let mut asked = rx.recv().await;
    // Answers `count` samples and returns the `market_no_price` lines they raised: the monitor has
    // handled a sample once it asks for the next.
    let mut answer = async |count: usize| {
        for _ in 0..count {
            let Some(Request::Query(query)) = asked.take() else { panic!("expected a sample query") };
            query(&engine);
            asked = rx.recv().await;
        }
        router.take().into_iter().filter(|line| line.name == "market_no_price").collect::<Vec<_>>()
    };
    // Ten samples before trusted time's first, then that one and two seconds after it.
    assert!(answer(10).await.is_empty());
    samples.send_replace(Some(trusted_now()));
    assert!(answer(3).await.is_empty());
    let [alert] = answer(1).await.try_into().unwrap();
    assert_eq!((alert.key(), alert.severity), ("market_no_price{market=1}".to_owned(), Some(Severity::Critical)));
}
