use super::*;

use crate::alerts::{AlertLine, Status};
use crate::test_support::scratch_dir;

#[test]
fn committed_pages_reopen_in_route_order_and_only_acknowledged_pages_disappear() {
    let cfg = config("reopen");
    let mut store = open(cfg.clone());
    for (market, severity) in [(1, Severity::Critical), (2, Severity::Warning), (3, Severity::Critical)] {
        assert!(store.add(page(market, severity)).unwrap());
    }
    let first = store.next(Severity::Critical).unwrap().unwrap();
    assert_eq!(first.page.key, "outage{market=1}");
    drop(store);
    let mut store = open(cfg.clone());
    let repeated = store.next(Severity::Critical).unwrap().unwrap();
    assert_eq!(repeated.page, first.page);
    store.remove(repeated.id).unwrap();
    drop(store);
    let store = open(cfg);
    assert_eq!(store.next(Severity::Critical).unwrap().unwrap().page.key, "outage{market=3}");
    assert_eq!(store.next(Severity::Warning).unwrap().unwrap().page.key, "outage{market=2}");
    assert_eq!(store.size().unwrap().0, 2);
}

#[test]
fn open_episodes_survive_delivery_and_reopening_until_a_resolve_is_queued() {
    let cfg = config("open-episodes");
    let mut store = open(cfg.clone());
    for market in [1, 2, 3] {
        assert!(store.add(page(market, Severity::Critical)).unwrap());
    }
    let mut resolved = page(2, Severity::Critical);
    resolved.status = Status::Resolved;
    assert!(store.add(resolved).unwrap());
    // Delivering and removing every page does not close the episodes they opened.
    while let Some(pending) = store.next(Severity::Critical).unwrap() {
        store.remove(pending.id).unwrap();
    }
    drop(store);
    let keys = |store: &Store| store.open_episodes().unwrap().into_iter().map(|(key, _)| key).collect::<Vec<_>>();
    assert_eq!(keys(&open(cfg)), ["outage{market=1}", "outage{market=3}"]);
}

#[test]
fn a_refused_firing_page_opens_no_episode_and_an_old_outbox_gains_the_table() {
    let cfg = OutboxSection { max_pages: 1, ..config("refused-episode") };
    let mut store = open(cfg.clone());
    assert!(store.add(page(1, Severity::Critical)).unwrap());
    assert!(!store.add(page(2, Severity::Critical)).unwrap());
    assert_eq!(store.open_episodes().unwrap().len(), 1);
    store.conn.execute_batch("DROP TABLE episodes").unwrap();
    drop(store);
    assert!(open(cfg).open_episodes().unwrap().is_empty());
}

#[test]
fn page_count_byte_and_payload_bounds_refuse_without_removing_pending_pages() {
    for (name, pages, bytes, large) in
        [("count", 1, 10000, false), ("bytes", 10, 400, false), ("payload", 10, 200000, true)]
    {
        let mut store = open(OutboxSection { max_pages: pages, max_bytes: bytes, ..config(name) });
        assert!(store.add(page(1, Severity::Critical)).unwrap());
        let mut next = page(2, Severity::Critical);
        if large {
            next.summary = "x".repeat(MAX_PAGE_BYTES);
        }
        assert!(!store.add(next).unwrap());
        assert_eq!(store.metrics.alert_outbox_overflows.get(), 1);
        assert_eq!(store.size().unwrap().0, 1);
        assert_eq!(store.next(Severity::Critical).unwrap().unwrap().page.key, "outage{market=1}");
    }
}

#[test]
fn expired_pages_reopen_with_no_retry_budget_and_are_removed_only_on_permanent_failure() {
    let cfg = config("expired");
    let mut store = open(cfg.clone());
    store.add(page(1, Severity::Critical)).unwrap();
    let pending = store.next(Severity::Critical).unwrap().unwrap();
    let body =
        serde_json::to_string(&Pending { id: 0, created_ms: now_ms() - 2000, expires_ms: now_ms() - 1000, ..pending })
            .unwrap();
    store.conn.execute("UPDATE pages SET body=?", [body]).unwrap();
    drop(store);
    let mut store = open(cfg);
    let pending = store.next(Severity::Critical).unwrap().unwrap();
    assert_eq!(pending.remaining(), Duration::ZERO);
    assert_eq!(store.size().unwrap().0, 1);
    store.remove(pending.id).unwrap();
    assert!(store.next(Severity::Critical).unwrap().is_none());
}

#[test]
fn corrupt_schema_rows_lifetimes_and_missing_routes_fail_closed_without_deleting_pages() {
    for (name, update) in [
        ("schema", "CREATE TABLE surprise (value TEXT)"),
        ("body", "UPDATE pages SET body='not json'"),
        ("route", "UPDATE pages SET route='unknown'"),
        ("key", "UPDATE pages SET body=json_set(body,'$.page.key','forged')"),
        (
            "overflow",
            "UPDATE pages SET body=json_set(body,'$.created_ms',-9223372036854775808,'$.expires_ms',9223372036854775807)",
        ),
    ] {
        let cfg = config(name);
        let mut store = open(cfg.clone());
        store.add(page(1, Severity::Critical)).unwrap();
        store.conn.execute_batch(update).unwrap();
        drop(store);
        assert!(Store::open(cfg.clone(), [true, true], Arc::new(Metrics::new())).is_err(), "{name}");
        let conn = Connection::open(cfg.dir.join("outbox.sqlite")).unwrap();
        assert_eq!(conn.query_row("SELECT count(*) FROM pages", [], |r| r.get::<_, i64>(0)).unwrap(), 1);
    }
    let cfg = config("no-route");
    let mut store = open(cfg.clone());
    store.add(page(1, Severity::Critical)).unwrap();
    drop(store);
    assert!(Store::open(cfg, [false, true], Arc::new(Metrics::new())).is_err());
}

#[tokio::test]
async fn an_owner_error_closes_the_client_and_is_observed_by_its_supervisor() {
    let mut tasks = JoinSet::new();
    let outbox =
        Outbox::start(config("owner-error"), [true, false], Arc::new(Metrics::new()), &mut tasks).await.unwrap();
    assert!(outbox.remove(999).await.is_err());
    assert!(tokio::time::timeout(Duration::from_secs(1), tasks.join_next()).await.unwrap().unwrap().is_ok());
    assert!(outbox.next(Severity::Critical).await.is_err());
}

#[test]
fn disk_failure_does_not_make_a_page_eligible_for_delivery() {
    let mut store = open(config("disk-failure"));
    store.conn.pragma_update(None, "query_only", true).unwrap();
    assert!(store.add(page(1, Severity::Critical)).is_err());
    assert!(store.next(Severity::Critical).unwrap().is_none());
    assert_eq!(store.metrics.alert_pages_pending.get_or_create(&SeverityLabel { severity: "critical" }).get(), 0);
}

// ── Helpers ──

fn config(name: &str) -> OutboxSection {
    OutboxSection {
        dir: scratch_dir(&format!("outbox-{name}")),
        max_pages: 10,
        max_bytes: 10000,
        max_age: Duration::from_secs(60),
    }
}

fn open(cfg: OutboxSection) -> Store {
    Store::open(cfg, [true, true], Arc::new(Metrics::new())).unwrap()
}

fn page(market: u64, severity: Severity) -> Page {
    let line = AlertLine {
        name: "outage".into(),
        severity: Some(severity),
        fields: vec![("market".into(), market.to_string())],
        summary: "unreachable".into(),
        context: "command{id=7}".into(),
        ts_ms: now_ms(),
    };
    Page::new(line.clone(), line.key(), Status::Firing, severity)
}

#[tokio::test]
async fn an_expired_page_is_permanently_failed_without_any_network_attempt() {
    let cfg = config("delivery-expired");
    let mut store = open(cfg.clone());
    store.add(page(1, Severity::Critical)).unwrap();
    let pending = store.next(Severity::Critical).unwrap().unwrap();
    let body =
        serde_json::to_string(&Pending { id: 0, created_ms: now_ms() - 2000, expires_ms: now_ms() - 1000, ..pending })
            .unwrap();
    store.conn.execute("UPDATE pages SET body=?", [body]).unwrap();
    drop(store);
    let metrics = Arc::new(Metrics::new());
    let mut tasks = JoinSet::new();
    let outbox = Outbox::start(cfg.clone(), [true, false], metrics.clone(), &mut tasks).await.unwrap();
    let (tx, rx) = mpsc::channel(1);
    drop(tx);
    let url = crate::config::WebhookUrl::from_env("HOOK", |_| Some("http://127.0.0.1:9/".into())).unwrap();
    crate::alerts::webhook::deliver(
        rx,
        outbox,
        Severity::Critical,
        crate::alerts::webhook::client().unwrap(),
        url,
        metrics.clone(),
        crate::alerts::webhook::RETRY,
    )
    .await
    .unwrap();
    while tasks.join_next().await.is_some() {}
    assert_eq!(metrics.alert_attempts_failed.get_or_create(&SeverityLabel { severity: "critical" }).get(), 0);
    assert_eq!(
        metrics
            .alert_deliveries
            .get_or_create(&crate::metrics::DeliveryLabels { severity: "critical", outcome: "failed" })
            .get(),
        1
    );
    assert!(open(cfg).next(Severity::Critical).unwrap().is_none());
}

#[test]
fn sqlite_file_growth_is_capped_even_for_an_oversized_direct_write() {
    let cfg = config("physical-bound");
    let store = open(cfg.clone());
    let error =
        store.conn.execute("INSERT INTO pages(route,body) VALUES('critical',zeroblob(1000000))", []).unwrap_err();
    assert_eq!(error.sqlite_error_code(), Some(rusqlite::ErrorCode::DiskFull));
    let page_size: i64 = store.conn.pragma_query_value(None, "page_size", |r| r.get(0)).unwrap();
    let bound = (cfg.max_bytes.div_ceil(page_size as u64) + cfg.max_pages * 8 + 16) * page_size as u64;
    assert!(std::fs::metadata(cfg.dir.join("outbox.sqlite")).unwrap().len() <= bound);
}
