//! A resolve pages only when it ends an open episode.

use super::*;

#[test]
fn past_the_cap_the_quietest_episode_is_forgotten_and_its_resolve_pages_nothing() {
    let mut episodes = Episodes::new(Duration::from_secs(10));
    let t0 = Instant::now();
    for market in 0..=1024 {
        episodes.on_line(alert("flood", market), t0 + Duration::from_millis(market)).unwrap();
    }
    assert_eq!(episodes.on_line(resolve("flood", 0), t0), None);
    assert!(episodes.on_line(resolve("flood", 1), t0).is_some());
}

#[test]
fn a_resolve_with_no_open_episode_pages_nothing_however_often_it_repeats() {
    let mut episodes = Episodes::new(Duration::from_secs(10));
    let t0 = Instant::now();
    for tick in 0..1000 {
        assert_eq!(episodes.on_line(resolve("collateral_mismatch", 0), t0 + Duration::from_millis(tick * 80)), None);
    }
    // A resolve already paged once ends its episode, so its repeats page nothing either.
    episodes.on_line(alert("collateral_mismatch", 0), t0).unwrap();
    assert_eq!(status(episodes.on_line(resolve("collateral_mismatch", 0), t0)), Some(Status::Resolved));
    assert_eq!(episodes.on_line(resolve("collateral_mismatch", 0), t0), None);
}

#[test]
fn a_genuine_alert_still_pages_after_many_healthy_ticks_and_its_resolve_follows() {
    let mut episodes = Episodes::new(Duration::from_secs(10));
    let t0 = Instant::now();
    for tick in 0..1000 {
        assert_eq!(episodes.on_line(resolve("collateral_mismatch", 0), t0 + Duration::from_millis(tick * 80)), None);
    }
    let at = t0 + Duration::from_secs(100);
    assert_eq!(status(episodes.on_line(alert("collateral_mismatch", 0), at)), Some(Status::Firing));
    assert_eq!(status(episodes.on_line(resolve("collateral_mismatch", 0), at)), Some(Status::Resolved));
}

#[tokio::test]
async fn untracked_resolves_page_no_route_and_leave_the_outbox_empty_for_a_genuine_alert() {
    let (critical, warning) = (Receiver::start(vec![]).await, Receiver::start(vec![]).await);
    let routes = routes(Some(&critical.url("/c")), Some(&warning.url("/w")));
    let metrics = Arc::new(Metrics::new());
    let (layer, router) = channel(routes, metrics.clone());
    let cfg = OutboxSection { max_pages: 4, ..outbox_config() };
    let routing = router.spawn(Duration::from_secs(3600), &cfg).await.unwrap();
    // Far more healthy ticks than the outbox holds.
    for _ in 0..50 {
        layer.lines.send(resolve("market_no_price", 1)).await.unwrap();
    }
    layer.lines.send(alert("market_no_price", 1)).await.unwrap();
    layer.lines.send(resolve("market_no_price", 1)).await.unwrap();
    routing.drain(Duration::from_secs(10)).await;
    let statuses = |receiver: &Receiver| receiver.bodies().iter().map(|b| b["status"].clone()).collect::<Vec<_>>();
    assert_eq!(statuses(&critical), [json!("firing"), json!("resolved")]);
    assert!(warning.bodies().is_empty());
    assert_eq!(metrics.alert_outbox_overflows.get(), 0);
}

#[tokio::test]
async fn a_restart_keeps_an_open_episode_so_its_resolve_still_pages() {
    let receiver = Receiver::start(vec![]).await;
    let cfg = outbox_config();
    // First run: the firing page is committed but its route is down, so it stays pending.
    let metrics = Arc::new(Metrics::new());
    let mut tasks = JoinSet::new();
    let outbox = Outbox::start(cfg.clone(), [true, false], metrics.clone(), &mut tasks).await.unwrap();
    let mut episodes = Episodes::new(Duration::from_secs(3600));
    let (queue, delivery) = mpsc::channel(1);
    drop(delivery);
    let queues = Queues { critical: Some(queue), warning: None, outbox };
    let page = episodes.on_line(alert("market_no_price", 1), Instant::now()).unwrap();
    enqueue(&queues, &mut episodes, page, &metrics).await.unwrap_err();
    drop(queues);
    while tasks.join_next().await.is_some() {}

    // Second run: the resolve ends the resumed episode, after the pending firing page.
    let routes = routes(Some(&receiver.url("/c")), None);
    let (layer, router) = channel(routes, Arc::new(Metrics::new()));
    let routing = router.spawn(Duration::from_secs(3600), &cfg).await.unwrap();
    layer.lines.send(resolve("market_no_price", 1)).await.unwrap();
    layer.lines.send(resolve("market_no_price", 1)).await.unwrap();
    routing.drain(Duration::from_secs(10)).await;
    let statuses: Vec<_> = receiver.bodies().iter().map(|b| b["status"].clone()).collect();
    assert_eq!(statuses, [json!("firing"), json!("resolved")]);
}

// A bundle can leave a route off that has pages waiting: the node starts, and they stay for a bundle
// that holds the route, as a config that dropped a route's variable does not.
#[tokio::test]
async fn a_route_a_bundle_left_off_keeps_its_pending_pages_and_the_node_starts() {
    let cfg = outbox_config();
    let metrics = Arc::new(Metrics::new());
    let mut tasks = JoinSet::new();
    let outbox = Outbox::start(cfg.clone(), [false, true], metrics.clone(), &mut tasks).await.unwrap();
    let mut waiting = page();
    waiting.severity = Severity::Warning;
    assert!(outbox.add(waiting).await.unwrap());
    drop(outbox);
    while tasks.join_next().await.is_some() {}

    let spawned = |routes: Routes| {
        let (_, router) = channel(routes, Arc::new(Metrics::new()));
        let cfg = cfg.clone();
        async move { router.spawn(Duration::from_secs(60), &cfg).await }
    };
    let err = spawned(Routes::default()).await.err().expect("a route a config dropped keeps refusing");
    assert!(format!("{err:#}").contains("pending pages need the warning webhook route"), "{err:#}");

    let alerts: AlertsSection = toml::from_str(
        "outbox = { dir = 'x', max_pages = 1, max_bytes = 1, max_age = '1h' }\nno_price_after = '5s'\n\
         episode_quiet = '15m'\n[warning]\nurl_secret = 'warning_url'\n",
    )
    .unwrap();
    let mut withheld = Routes::default();
    withheld.deliver(&alerts, &Default::default()).unwrap();
    assert!(!withheld.has(Severity::Warning), "no value, no route");
    spawned(withheld).await.expect("a route waiting for its secret stopped the node").drain(Duration::ZERO).await;

    let mut tasks = JoinSet::new();
    let outbox = Outbox::start(cfg, [false, true], Arc::new(Metrics::new()), &mut tasks).await.unwrap();
    assert!(outbox.next(Severity::Warning).await.unwrap().is_some(), "the page waiting for the route is gone");
}
