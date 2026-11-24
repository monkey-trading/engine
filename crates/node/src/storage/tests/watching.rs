//! The watcher's judgement of volumes and stores: thresholds, projections, and the episodes a run leaves open.

use super::*;

#[test]
fn a_free_share_below_each_threshold_raises_its_alert_and_clears_with_a_margin() {
    let (mut watch, metrics) = (primed_watch(), Metrics::new());
    let t0 = Instant::now();
    let mut at = |free_percent: u64| {
        let reading = reading_with_free(free_percent);
        capture(|| watch.observe(t0, &reading, &metrics))
    };
    assert!(at(40).is_empty());
    let [low] = at(14).try_into().unwrap();
    assert_eq!((low.key(), low.severity), ("disk_space_low{volume=log}".to_owned(), Some(Severity::Warning)));
    assert!(low.summary.starts_with("volume log has 14.0 GiB free of 100.0 GiB (14.0%)"), "{}", low.summary);
    assert!(low.summary.ends_with("holds log 30.0 GiB"), "{}", low.summary);
    // Still low, and 16% is inside the 16.5% a held Warning needs to clear: no new line.
    assert!(at(13).is_empty());
    assert!(at(16).is_empty());
    let lines = at(4);
    let [critical] = lines.as_slice() else { panic!("{lines:?}") };
    assert_eq!(
        (critical.key(), critical.severity),
        ("disk_space_critical{volume=log}".to_owned(), Some(Severity::Critical))
    );
    // Critical clears while the Warning stays open, then the Warning clears.
    assert!(at(5).is_empty());
    let [resolved] = at(8).try_into().unwrap();
    assert_eq!((resolved.key(), resolved.severity), (critical.key(), None));
    let [resolved] = at(60).try_into().unwrap();
    assert_eq!((resolved.key(), resolved.severity), (low.key(), None));
    assert!(at(60).is_empty());
}

#[test]
fn a_volume_that_jumps_from_clear_to_critical_raises_both_levels() {
    let (mut watch, metrics) = (primed_watch(), Metrics::new());
    let reading = reading_with_free(2);
    let keys: Vec<String> =
        capture(|| watch.observe(Instant::now(), &reading, &metrics)).iter().map(|line| line.key()).collect();
    assert_eq!(keys, ["disk_space_low{volume=log}", "disk_space_critical{volume=log}"]);
}

#[test]
fn a_volume_resolves_the_alerts_it_is_not_at_once_its_growth_is_known_for_an_episode_a_run_before_left_open() {
    let metrics = Metrics::new();
    let t0 = Instant::now();
    let window = config().rate_window;
    let lines_at = |free_percent| {
        let (mut watch, reading) = (Watch::new(config()), reading_with_free(free_percent));
        let disk = |lines: Vec<crate::alerts::AlertLine>| {
            lines.iter().filter(|line| line.name.starts_with("disk_space")).map(|l| (l.key(), l.severity)).collect()
        };
        let first: Vec<_> = disk(capture(|| watch.observe(t0, &reading, &metrics)));
        let later: Vec<_> = disk(capture(|| watch.observe(t0 + window, &reading, &metrics)));
        (first, later)
    };
    let resolved = |name: &str| (format!("{name}{{volume=log}}"), None);
    let raised = |name: &str, severity| (format!("{name}{{volume=log}}"), Some(severity));
    // Healthy: nothing at first, since the projection that ended an earlier fill-time episode is not yet known; then
    // both episodes an earlier run left open end.
    assert_eq!(lines_at(60), (vec![], vec![resolved("disk_space_low"), resolved("disk_space_critical")]));
    // Low on space: the Warning is raised again, which pages nothing within its quiet period; the Critical ends later.
    assert_eq!(
        lines_at(10),
        (vec![raised("disk_space_low", Severity::Warning)], vec![resolved("disk_space_critical")])
    );
    assert_eq!(
        lines_at(2),
        (vec![raised("disk_space_low", Severity::Warning), raised("disk_space_critical", Severity::Critical)], vec![])
    );
}

#[test]
fn a_restart_does_not_end_a_fill_time_episode_it_cannot_yet_judge() {
    let (mut watch, metrics) = (primed_watch(), Metrics::new());
    let t0 = Instant::now();
    let mut at = |hours: u32, used_gib: u64| capture(|| watch.observe(t0 + HOUR * hours, &filling(used_gib), &metrics));
    // Free space is plentiful and no growth is known: an episode raised on a projected fill is still open at the
    // receiver, and nothing here ends it.
    for hours in 0..6 {
        assert!(at(hours, 100 + 3 * u64::from(hours)).is_empty(), "hour {hours}");
    }
    // The window fills with the same 3 GiB an hour, and the Warning is raised again.
    let [low, critical] = at(6, 118).try_into().unwrap();
    assert_eq!((low.key(), low.severity), ("disk_space_low{volume=log}".to_owned(), Some(Severity::Warning)));
    assert_eq!(critical.severity, None);
}

#[test]
fn a_volume_filling_within_the_warning_time_alerts_alone_and_clears_with_a_margin() {
    let (mut watch, metrics) = (primed_watch(), Metrics::new());
    let t0 = Instant::now();
    let mut at = |hours: u32, used_gib: u64| capture(|| watch.observe(t0 + HOUR * hours, &filling(used_gib), &metrics));
    // The window is six hours: nothing is projected until it is sampled, whatever the first hours wrote.
    assert!(at(0, 100).is_empty());
    // 3 GiB an hour leaves 882 GiB free, about 12 days: inside the 14-day warning, past the 3-day critical.
    // The Critical an earlier run might have left open ends in the same sample, as the growth becomes known.
    let [low, critical] = at(6, 118).try_into().unwrap();
    assert_eq!((low.key(), low.severity), ("disk_space_low{volume=log}".to_owned(), Some(Severity::Warning)));
    assert_eq!((critical.key(), critical.severity), ("disk_space_critical{volume=log}".to_owned(), None));
    // 2.5 GiB an hour, 867 GiB free: 14.5 days is past the warning time and inside the 15.4 days a held one clears at.
    assert!(at(12, 133).is_empty());
    // 2.17 GiB an hour, 854 GiB free: 16.4 days.
    let [resolved] = at(18, 146).try_into().unwrap();
    assert_eq!((resolved.key(), resolved.severity), (low.key(), None));
}

#[test]
fn a_volume_filling_within_the_critical_time_raises_critical_and_exports_its_projection() {
    let (mut watch, metrics) = (primed_watch(), Metrics::new());
    let t0 = Instant::now();
    let mut at = |hours: u32, used_gib: u64| capture(|| watch.observe(t0 + HOUR * hours, &filling(used_gib), &metrics));
    assert!(at(0, 100).is_empty());
    // 100 GiB an hour leaves 300 GiB free, three hours.
    let lines = at(6, 700);
    let [low, critical] = lines.as_slice() else { panic!("{lines:?}") };
    assert_eq!((low.name.as_str(), critical.name.as_str()), ("disk_space_low", "disk_space_critical"));
    assert!(critical.summary.contains("growing 2400.0 GiB a day, full in 0.1 days"), "{}", critical.summary);
    metrics_of(&metrics, |text| {
        let growth = value(text, "engine_volume_growth_bytes_per_second{volume=\"log\"}");
        assert!((growth - 100.0 * GIB as f64 / 3600.0).abs() < 1.0, "{text}");
        let fill = value(text, "engine_volume_fill_seconds{volume=\"log\"}");
        assert!((fill - 10_800.0).abs() < 1.0, "{text}");
    });
    // Growth stops: the window slides past it, the projection goes, and both alerts resolve.
    let resolved = at(12, 700);
    assert!(resolved.iter().all(|line| line.severity.is_none()) && resolved.len() == 2, "{resolved:?}");
    metrics_of(&metrics, |text| {
        assert!(!text.contains("engine_volume_fill_seconds{"), "{text}");
        assert_eq!(value(text, "engine_volume_growth_bytes_per_second{volume=\"log\"}"), 0.0);
    });
}

#[test]
fn a_steady_growth_keeps_its_projection_through_a_window_of_uneven_reads() {
    let (mut watch, metrics) = (primed_watch(), Metrics::new());
    let t0 = Instant::now();
    let (mut seconds, mut raised, mut resolved) = (0u64, 0, 0);
    // Reads land 55 to 65 seconds apart, as a read's own time moves each one, for seven hours of 3 GiB an hour.
    for step in 0..450u64 {
        let used = 100 * GIB + 3 * GIB * seconds / 3600;
        let reading =
            Reading { volumes: vec![volume(&[Store::Log], 1000 * GIB, 1000 * GIB - used)], ..Reading::default() };
        let lines = capture(|| watch.observe(t0 + Duration::from_secs(seconds), &reading, &metrics));
        raised += lines.iter().filter(|line| line.severity.is_some()).count();
        resolved += lines.iter().filter(|line| line.severity.is_none()).count();
        if seconds >= 6 * 3600 {
            let projected = metrics_of(&metrics, |text| text.contains("engine_volume_fill_seconds{"));
            assert!(projected, "no projection at {seconds} s");
        }
        seconds += 55 + step * 7 % 11;
    }
    assert!(seconds > 6 * 3600 + 3600, "the loop ran only {seconds} s");
    // The 12-day projection raises the Warning once, when the window fills, and never flaps; the Critical a run before
    // might have left open ends in that same sample.
    assert_eq!((raised, resolved), (1, 1));
}

#[test]
fn a_burst_in_the_windows_first_hour_projects_nothing_until_the_window_is_full() {
    let (mut watch, metrics) = (primed_watch(), Metrics::new());
    let t0 = Instant::now();
    // A 1.92 TB volume whose index catches up 23 GiB in the first hour of a start, then stops: read at the
    // hour's endpoints that is 23 GiB an hour, a fill in under four days.
    let mut at = |minutes: u64, used_gib: u64| {
        let reading =
            Reading { volumes: vec![volume(&[Store::Log], 1920 * GIB, (1920 - used_gib) * GIB)], ..Reading::default() };
        capture(|| watch.observe(t0 + Duration::from_secs(60 * minutes), &reading, &metrics))
    };
    assert!(at(0, 100).is_empty());
    assert!(at(60, 123).is_empty());
    metrics_of(&metrics, |text| assert!(!text.contains("engine_volume_growth"), "{text}"));
    // Over the whole window it is 3.8 GiB an hour, about 20 days to fill: no alert is raised, and the ones a run before
    // might have left open end.
    let lines = at(360, 123);
    assert!(lines.len() == 2 && lines.iter().all(|line| line.severity.is_none()), "{lines:?}");
    metrics_of(&metrics, |text| {
        let growth = value(text, "engine_volume_growth_bytes_per_second{volume=\"log\"}");
        assert!((growth - 23.0 * GIB as f64 / 21_600.0).abs() < 1.0, "{text}");
    });
}

#[test]
fn an_unreadable_store_pages_once_and_resolves_when_it_reads_again() {
    let (mut watch, metrics) = (primed_watch(), Metrics::new());
    let t0 = Instant::now();
    let failing =
        Reading { failures: vec![(Store::DailySnapshots, "reading /d: denied".to_owned())], ..Reading::default() };
    let healthy = Reading { stores: vec![(Store::DailySnapshots, 0)], ..Reading::default() };
    let mut observe = |reading: &Reading| capture(|| watch.observe(t0, reading, &metrics));
    let [alert] = observe(&failing).try_into().unwrap();
    assert_eq!(
        (alert.key(), alert.severity),
        ("storage_unreadable{store=daily_snapshots}".to_owned(), Some(Severity::Warning))
    );
    assert_eq!(alert.summary, "cannot read daily_snapshots's size: reading /d: denied");
    assert!(observe(&failing).is_empty());
    let [resolved] = observe(&healthy).try_into().unwrap();
    assert_eq!((resolved.key(), resolved.severity), (alert.key(), None));
}

#[test]
fn a_first_healthy_read_resolves_each_stores_unreadable_episode_a_run_before_left_open() {
    let mut watch = Watch::new(config());
    let reading = Reading { stores: vec![(Store::Log, 0), (Store::AlertOutbox, 0)], ..Reading::default() };
    let lines = capture(|| watch.observe(Instant::now(), &reading, &Metrics::new()));
    let keys: Vec<String> = lines.iter().map(|line| line.key()).collect();
    assert_eq!(
        keys,
        ["storage_unreadable{store=log}", "storage_unreadable{store=alert_outbox}", "storage_unreadable{store=all}"]
    );
    assert!(lines.iter().all(|line| line.severity.is_none()));
}

#[test]
fn a_read_that_does_not_finish_pages_all_and_leaves_each_stores_own_episode() {
    let (mut watch, metrics) = (primed_watch(), Metrics::new());
    let failing =
        Reading { failures: vec![(Store::DailySnapshots, "reading /d: denied".to_owned())], ..Reading::default() };
    capture(|| watch.observe(Instant::now(), &failing, &metrics));
    let [alert] = capture(|| watch.unread("the read has not finished in 30s")).try_into().unwrap();
    assert_eq!(alert.key(), "storage_unreadable{store=all}");
    // The next read that finishes ends `all`, and the store's own episode with it only when it reads.
    let healthy = Reading { stores: vec![(Store::DailySnapshots, 0)], ..Reading::default() };
    let keys: Vec<String> =
        capture(|| watch.observe(Instant::now(), &healthy, &metrics)).iter().map(|line| line.key()).collect();
    assert_eq!(keys, ["storage_unreadable{store=daily_snapshots}", "storage_unreadable{store=all}"]);
}
