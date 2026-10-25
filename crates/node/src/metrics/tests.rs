use super::*;

use std::collections::BTreeSet;

use engine_core::Engine;
use engine_core::command::{AdminCommand, Command, Sequenced};
use engine_core::deposits::DepositBeneficiary;
use engine_core::eip712::B256;
use engine_core::event::{CancelReason, Fill};
use engine_core::price::SourceQuote;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{AccountId, Millis, Side};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::NodeConfig;
use crate::sequencer::genesis_commands;
use crate::test_support::every_event;

#[test]
fn fills_count_by_market_and_rejections_by_event_and_reason() {
    let metrics = Metrics::new();
    metrics.record(&Event::Fill(fill(1)));
    metrics.record(&Event::Fill(fill(1)));
    metrics.record(&Event::Fill(fill(2)));
    let margin = Reject::InsufficientMargin { equity: Fixed::ZERO, required: Fixed::ONE };
    metrics.record(&Event::OrderRejected { account: ACCOUNT, client_id: 1, reason: margin });
    metrics.record(&Event::WithdrawalRejected {
        account: ACCOUNT,
        amount: Fixed::ONE,
        reason: Reject::InsufficientFunds { available: Fixed::ZERO },
    });
    metrics.record(&Event::PricesRejected { market: 1, reason: "malformed quote from mock-a".into() });
    // Not a fill or a rejection.
    metrics.record(&Event::OrderCancelled { account: ACCOUNT, order_id: 1, reason: CancelReason::Unfillable });

    let text = metrics.encode();
    assert!(text.contains("engine_fills_total{market=\"1\"} 2\n"), "{text}");
    assert!(text.contains("engine_fills_total{market=\"2\"} 1\n"), "{text}");
    assert!(text.contains("engine_rejections_total{event=\"order_rejected\",reason=\"insufficient_margin\"} 1\n"));
    assert!(text.contains("engine_rejections_total{event=\"withdrawal_rejected\",reason=\"insufficient_funds\"} 1\n"));
    assert!(text.contains("engine_rejections_total{event=\"prices_rejected\",reason=\"malformed_quote\"} 1\n"));
    assert_eq!(counted(&text, "engine_rejections_total{"), 3, "{text}");
}

#[test]
fn recorded_events_put_no_account_in_any_label_or_value() {
    let metrics = Metrics::new();
    metrics.record(&Event::Fill(fill(1)));
    metrics.record(&Event::OrderRejected { account: ACCOUNT, client_id: 7, reason: Reject::HouseCapacity });
    metrics.record(&Event::DepositRejected {
        account: ACCOUNT,
        amount: Fixed::ONE,
        chain_ref: "0xdead:1".into(),
        reason: Reject::DuplicateReference,
    });
    let text = metrics.encode();
    assert!(!text.contains(&ACCOUNT.to_string()), "{text}");
    assert!(!text.contains("account"), "{text}");
    assert!(!text.contains("0xdead"), "{text}");
}

#[test]
fn custody_rejections_precreate_and_count_every_public_reason_exactly_once() {
    let metrics = Metrics::new();
    let receipt = every_event()
        .into_iter()
        .find_map(|event| match event {
            Event::CustodyDepositRejected { receipt, .. } => Some(receipt),
            _ => None,
        })
        .expect("custody rejection fixture");
    let reasons = [
        DepositReject::ProtocolNotActive,
        DepositReject::InvalidReceipt,
        DepositReject::InvalidAmount,
        DepositReject::ArithmeticOverflow,
        DepositReject::UnknownReceipt,
        DepositReject::ConflictingReceipt,
        DepositReject::SettledReceipt,
    ];
    let kinds: BTreeSet<_> = reasons.iter().map(deposit_reject_kind).collect();
    assert_eq!(kinds, BTreeSet::from(DEPOSIT_REJECT_KINDS));
    let initial = metrics.encode();
    for reason in reasons {
        let kind = deposit_reject_kind(&reason);
        assert_eq!(serde_json::to_string(&reason).unwrap(), format!("{{\"kind\":\"{kind}\"}}"));
        let series = format!("engine_rejections_total{{event=\"custody_deposit_rejected\",reason=\"{kind}\"}}");
        assert!(initial.contains(&format!("{series} 0\n")), "{initial}");
        let event = Event::CustodyDepositRejected { receipt: receipt.clone(), reason };
        metrics.record(&event);
        assert!(metrics.encode().contains(&format!("{series} 1\n")));
        metrics.record(&event);
        assert!(metrics.encode().contains(&format!("{series} 2\n")));
    }
    assert_eq!(counted(&metrics.encode(), "engine_rejections_total{"), 7, "only the seven custody series moved");
}

#[test]
fn custody_receipt_identity_never_expands_rejection_label_cardinality() {
    let metrics = Metrics::new();
    let receipt = every_event()
        .into_iter()
        .find_map(|event| match event {
            Event::CustodyDepositRejected { receipt, .. } => Some(receipt),
            _ => None,
        })
        .expect("custody rejection fixture");
    let labels = |text: &str| -> BTreeSet<String> {
        text.lines()
            .filter_map(|line| line.strip_prefix("engine_rejections_total{"))
            .map(|line| line.split_once('}').unwrap().0.to_owned())
            .collect()
    };
    let initial = labels(&metrics.encode());
    for byte in 1..=32 {
        let account = AccountId([byte; 20]);
        let hash = B256::repeat_byte(byte);
        let event = Event::CustodyDepositRejected {
            receipt: engine_core::deposits::DepositReceipt {
                beneficiary: DepositBeneficiary::Trader { account },
                tx_hash: hash,
                log_index: u64::from(byte),
                ..receipt.clone()
            },
            reason: DepositReject::ConflictingReceipt,
        };
        metrics.record(&event);
        let text = metrics.encode();
        assert!(!text.contains(&account.to_string()), "{text}");
        assert!(!text.contains(&hash.to_string()), "{text}");
    }
    let text = metrics.encode();
    assert_eq!(labels(&text), initial);
    assert!(!text.contains("account"), "{text}");
    assert!(!text.contains("chain_ref"), "{text}");
    assert!(
        text.contains(
            "engine_rejections_total{event=\"custody_deposit_rejected\",reason=\"conflicting_receipt\"} 32\n"
        )
    );
    assert_eq!(counted(&text, "engine_rejections_total{"), 1, "every receipt shares one series");
}

#[test]
fn every_rejection_reason_has_the_kind_the_public_log_uses() {
    let reasons: Vec<Reject> = every_event()
        .into_iter()
        .filter_map(|event| match event {
            Event::OrderRejected { reason, .. } => Some(reason),
            _ => None,
        })
        .collect();
    for reason in &reasons {
        let logged = serde_json::to_string(reason).unwrap();
        // The variant's own `kind` comes first in its serialization.
        assert!(logged.starts_with(&format!("{{\"kind\":\"{}\"", reject_kind(reason))), "{logged}");
    }
    let kinds: BTreeSet<&str> = reasons.iter().map(reject_kind).collect();
    assert_eq!(kinds, BTreeSet::from(REJECT_KINDS), "every kind, and only those, is pre-created");
}

#[tokio::test]
async fn the_operator_listener_serves_openmetrics_text() {
    let metrics = Arc::new(Metrics::new());
    metrics.commands.inc();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(axum::serve(listener, router(metrics)).into_future());

    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(b"GET /metrics HTTP/1.1\r\nHost: node\r\nConnection: close\r\n\r\n").await.unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    assert!(response.contains(&format!("content-type: {CONTENT_TYPE_OPENMETRICS}\r\n")), "{response}");
    assert!(response.contains("engine_commands_total 1\n"), "{response}");
    assert!(response.ends_with("# EOF\n"), "{response}");
}

#[test]
fn to_f64_keeps_the_decimal_value() {
    assert_eq!(to_f64("1234.5".parse().unwrap()), 1234.5);
    assert_eq!(to_f64("-0.000000001".parse().unwrap()), -1e-9);
}

#[test]
fn price_rejections_count_by_the_reason_the_engine_gives() {
    let mut engine = dev_engine();
    let quote = |mid: &str, ts| SourceQuote { source: "venue-a".into(), mid: mid.parse().unwrap(), ts, contract: None };
    let for_contract = SourceQuote { contract: Some("BTCZ6".into()), ..quote("100", T0) };
    let commands = [
        Command::Prices { market: 99, quotes: vec![quote("100", T0)] },
        Command::Prices { market: 1, quotes: vec![quote("0", T0)] },
        Command::Prices { market: 1, quotes: vec![quote("100", T0 + 5_000)] },
        // Stamped at 05:00 in New York, before Apple's open.
        Command::Prices { market: 40, quotes: vec![quote("230", T0 - 5 * 3_600_000)] },
        Command::Prices { market: 1, quotes: vec![for_contract] },
    ];
    let metrics = Metrics::new();
    for cmd in commands {
        let seq = engine.last_seq().map_or(0, |s| s + 1);
        engine.apply(&Sequenced { seq, ts: T0, cmd }).unwrap().iter().for_each(|event| metrics.record(event));
    }
    let text = metrics.encode();
    for reason in ["unknown_market", "malformed_quote", "quote_ahead", "market_closed", "contract_not_taken"] {
        let line = format!("engine_rejections_total{{event=\"prices_rejected\",reason=\"{reason}\"}} 1\n");
        assert!(text.contains(&line), "{text}");
    }
    assert!(text.contains("engine_rejections_total{event=\"prices_rejected\",reason=\"other\"} 0\n"));
}

#[test]
fn every_rejection_and_delivery_series_starts_at_zero() {
    let text = Metrics::new().encode();
    for line in [
        "engine_rejections_total{event=\"order_rejected\",reason=\"insufficient_margin\"} 0\n",
        "engine_rejections_total{event=\"deposit_rejected\",reason=\"duplicate_reference\"} 0\n",
        "engine_rejections_total{event=\"prices_rejected\",reason=\"quote_ahead\"} 0\n",
        "engine_rejections_total{event=\"admin_rejected\",reason=\"other\"} 0\n",
        "engine_rejections_total{event=\"corporate_action_failed\",reason=\"other\"} 0\n",
        "engine_alert_deliveries_total{severity=\"critical\",outcome=\"sent\"} 0\n",
        "engine_alert_deliveries_total{severity=\"warning\",outcome=\"dropped\"} 0\n",
    ] {
        assert!(text.contains(line), "{line}: {text}");
    }
    assert_eq!(
        text.matches("engine_rejections_total{").count(),
        3 * REJECT_KINDS.len() + DEPOSIT_REJECT_KINDS.len() + PRICES_REJECT_REASONS.len() + 2
    );
    assert_eq!(text.matches("engine_alert_deliveries_total{").count(), 2 * 3);
    assert!(text.contains("engine_alert_attempts_failed_total{severity=\"warning\"} 0\n"), "{text}");
}

#[test]
fn corporate_action_failures_increment_the_counter_without_action_id_labels() {
    let metrics = Metrics::new();
    metrics.record(&Event::CorporateActionFailed {
        id: "private-action-identity".into(),
        market: 40,
        at: 2000,
        reason: "precision".into(),
    });
    let text = metrics.encode();
    assert!(text.contains("engine_rejections_total{event=\"corporate_action_failed\",reason=\"other\"} 1\n"), "{text}");
    assert!(!text.contains("private-action-identity"));
}

/// The dashboard and the alerting rules read only metrics this node exports, and every `up` they
/// read is the engine job's, so neither can show or page on another service's series.
#[test]
fn ops_queries_read_only_this_nodes_metrics_and_the_engine_jobs_up() {
    let metrics = Metrics::new();
    // A family is exported once it has a series; these get theirs from live markets.
    let market = MarketLabel { market: 1 };
    let _ = metrics.fills.get_or_create(&market);
    let _ = metrics.house_net_exposure.get_or_create(&market);
    let _ = metrics.market_open.get_or_create(&market);
    let _ = metrics.market_priced.get_or_create(&market);
    let _ = metrics.quote_timestamp_seconds.get_or_create(&QuoteLabels { market: 1, venue: "venue-a".into() });
    let exported = metrics.encode();
    let ops = [
        ("dashboard", include_str!("../../../../ops/grafana/engine-node.json")),
        ("rules", include_str!("../../../../ops/prometheus/engine-node-rules.yml")),
    ];
    for (file, text) in ops {
        let text = text.replace('\\', "");
        let names = metric_names(&text);
        assert!(!names.is_empty(), "{file}");
        for name in names {
            let family = ["_total", "_bucket", "_count", "_sum"]
                .iter()
                .fold(name.as_str(), |n, s| n.strip_suffix(s).unwrap_or(n));
            assert!(
                exported.contains(&format!("# TYPE {family} ")),
                "{file} reads {name}, which the node does not export"
            );
        }
        let ups: Vec<&str> = text
            .match_indices("up{")
            .filter(|(at, _)| !text[..*at].ends_with(|c: char| c.is_ascii_alphanumeric() || c == '_'))
            .map(|(at, _)| &text[at..at + text[at..].find('}').unwrap()])
            .collect();
        assert!(!ups.is_empty(), "{file}");
        for up in ups {
            assert!(up.contains("job=\"engine\""), "{file}: {up}");
        }
    }
}

// ── Helpers ──

/// Every `engine_…` metric name in `text`.
fn metric_names(text: &str) -> BTreeSet<String> {
    text.match_indices("engine_")
        .map(|(at, _)| text[at..].chars().take_while(|c| c.is_ascii_lowercase() || *c == '_').collect())
        .collect()
}

/// Tue 20 Oct 2026 14:00 UTC.
const T0: Millis = 1_792_504_800_000;

/// How many samples of the counter `prefix` names are above zero.
fn counted(text: &str, prefix: &str) -> usize {
    text.lines().filter(|line| line.starts_with(prefix) && !line.ends_with(" 0")).count()
}

/// An engine after the dev config's genesis, at `T0`.
/// An engine after the dev config's protocol activation and genesis, as a node sequences them, at
/// `T0`.
fn dev_engine() -> Engine {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../config/dev.toml");
    let cfg = NodeConfig::load(std::path::Path::new(path)).unwrap();
    let activation = [
        AdminCommand::SetClientAuth(cfg.client_auth().unwrap()),
        AdminCommand::SetOperator(cfg.operator().unwrap()),
        AdminCommand::ActivateProtocol { version: ProtocolVersion::LATEST },
    ];
    let commands: Vec<Command> =
        activation.into_iter().map(Command::Admin).chain(genesis_commands(&cfg.genesis)).collect();
    let mut engine = Engine::new();
    for (seq, cmd) in commands.into_iter().enumerate() {
        engine.apply(&Sequenced { seq: seq as u64, ts: T0, cmd }).unwrap();
    }
    engine
}

const ACCOUNT: AccountId = AccountId([0xaa; 20]);

fn fill(market: MarketId) -> Fill {
    Fill {
        account: ACCOUNT,
        order_id: 1,
        market,
        side: Side::Buy,
        qty: Fixed::ONE,
        price: Fixed::from_int(100),
        index: Fixed::from_int(100),
        spread_bps: 0,
        realised_pnl: Fixed::ZERO,
        fx_index: None,
        profit_fee: Fixed::ZERO,
        position: Fixed::ONE,
        liquidation: false,
    }
}
