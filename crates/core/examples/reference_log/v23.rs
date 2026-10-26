//! Signed prior-close cash entitlements, rounded splits with cash in lieu, quote fencing and an
//! atomic split failure under V23.

use chrono::{DateTime, Duration, Utc};
use engine_core::command::{AdminCommand, Command, OrderKind};
use engine_core::corporate::{Adjustment, CorporateAction};
use engine_core::event::{CancelReason, Event};
use engine_core::params::Rollover;
use engine_core::protocol::ProtocolVersion;
use engine_core::session::Schedule;
use engine_core::types::{AssetClass, Side};

use crate::common::{Harness, f, market};

/// Appends a signed dividend, a four-for-one split, a reverse split that rounds and settles
/// fractions, and a split that fails.
pub(crate) fn session(h: &mut Harness) {
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V23 });
    h.operator_admin(AdminCommand::SetReduceOnly { market: None, reduce_only: false });
    h.operator_admin(AdminCommand::SetStressFloor { market: None, level: 0 });
    let date = DateTime::<Utc>::from_timestamp_millis(h.ts).unwrap().date_naive() + Duration::days(1);
    let start = date.and_hms_opt(14, 0, 0).unwrap().and_utc().timestamp_millis();
    h.advance_to(start);
    h.cmd(Command::Tick);
    let mut params = h.engine.params().unwrap().clone();
    params.stress_levels.truncate(1);
    params.toxicity.tiers.clear();
    params.toxicity.horizon_ms = 3_600_000;
    params.house_total_cap_floor = f("1000000000");
    params.rollover = Rollover { tz: chrono_tz::UTC, hour: 22, minute: 0 };
    h.operator_admin(AdminCommand::SetParams(params));
    let schedule: Schedule = serde_json::from_value(serde_json::json!({
        "kind": "weekly", "tz": "UTC",
        "windows": [{"open": format!("{} 14:00", date.format("%a")), "close": format!("{} 14:01", date.format("%a"))}],
        "special_sessions": [
            {"from": format!("{date}T14:02:00"), "to": format!("{date}T14:03:00")},
            {"from": format!("{date}T14:04:00"), "to": format!("{date}T14:05:00")},
            {"from": format!("{date}T14:06:00"), "to": format!("{date}T14:07:00")},
            {"from": format!("{date}T14:08:00"), "to": format!("{date}T14:09:00")}
        ]
    }))
    .unwrap();
    let stock = 34_000;
    let mut spec = market(stock, "CORPORATE-REFERENCE", AssetClass::Equity, schedule, 2000);
    spec.price_rules.max_age_ms = 600_000;
    spec.house_cap_floor = f("1000000000");
    h.operator_admin(AdminCommand::UpsertMarket(spec));
    let long = h.signer(70);
    let short = h.signer(71);
    h.deposit(long, "10000");
    h.deposit(short, "10000");
    h.price(stock, "80");
    for (account, side, quantity) in [(long, Side::Buy, "2"), (short, Side::Sell, "3")] {
        let events = h.market_order(account, stock, side, quantity, "80");
        assert!(events.iter().any(|e| matches!(e, Event::Fill(_))), "{events:?}");
    }
    h.order(long, stock, Side::Buy, "1", OrderKind::Limit { price: f("64") });
    let dividend = CorporateAction {
        id: "v23-dividend".into(),
        market: stock,
        previous_close: start + 60_000,
        ex_open: start + 120_000,
        adjustment: Adjustment::Dividend { gross_per_unit: f("4"), withholding_rate: f("0.25") },
    };
    h.operator_admin(AdminCommand::ScheduleCorporateAction(dividend.clone()));
    let duplicate = h.operate(AdminCommand::ScheduleCorporateAction(dividend));
    assert!(duplicate.iter().any(|e| matches!(e, Event::AdminRejected { reason } if reason.contains("duplicate"))));
    h.operator_admin(AdminCommand::ScheduleCorporateAction(CorporateAction {
        id: "v23-split".into(),
        market: stock,
        previous_close: start + 180_000,
        ex_open: start + 240_000,
        adjustment: Adjustment::Split { new_units: 4, old_units: 1 },
    }));
    h.advance_to(start + 120_000);
    let dividend = h.cmd(Command::Tick);
    assert!(dividend.iter().any(
        |e| matches!(e, Event::CorporateDividend { account, amount, .. } if *account == long && *amount == f("6"))
    ));
    assert!(dividend.iter().any(
        |e| matches!(e, Event::CorporateDividend { account, amount, .. } if *account == short && *amount == f("-12"))
    ));
    h.advance_to(start + 240_000);
    let split = h.cmd(Command::Tick);
    assert!(split.iter().any(|e| matches!(e, Event::CorporateSplit { new_units: 4, old_units: 1, .. })));
    assert_eq!(h.engine.accounts()[&long].position(stock).qty, f("8"));
    let order = h.engine.accounts()[&long].orders.values().find(|o| o.market == stock).unwrap();
    assert_eq!((order.qty, order.kind), (f("4"), OrderKind::Limit { price: f("16") }));
    let events = h.market_order(long, stock, Side::Buy, "1", "20");
    assert!(events.iter().any(|e| matches!(e, Event::Fill(fill) if fill.price == f("20"))), "{events:?}");
    let events = h.market_order(long, stock, Side::Buy, "0.5", "20");
    assert!(events.iter().any(|e| matches!(e, Event::Fill(_))), "{events:?}");
    h.order(long, stock, Side::Buy, "0.000000001", OrderKind::Limit { price: f("16") });
    h.price(stock, "21");
    h.operator_admin(AdminCommand::ScheduleCorporateAction(CorporateAction {
        id: "v23-reverse".into(),
        market: stock,
        previous_close: start + 300_000,
        ex_open: start + 360_000,
        adjustment: Adjustment::Split { new_units: 1, old_units: 3 },
    }));
    h.advance_to(start + 360_000);
    let reverse = h.cmd(Command::Tick);
    // 9.5 / 3 leaves 2/3 of a nano-unit, closed at the mark 63 against the entry 60.
    assert!(reverse.iter().any(|e| matches!(e, Event::CorporateCashInLieu { account, amount, .. }
        if *account == long && *amount == f("0.000000002"))));
    assert!(reverse.iter().any(|e| matches!(e, Event::OrderCancelled { reason: CancelReason::Split, .. })));
    assert_eq!(h.engine.accounts()[&long].position(stock).qty, f("3.166666666"));
    assert_eq!(h.engine.accounts()[&short].position(stock).qty, f("-4"));
    // Scaled four-for-one, this mark rounds to zero, so the split fails and blocks the market.
    h.price(stock, "0.000000003");
    let held = h.engine.accounts()[&long].position(stock);
    h.operator_admin(AdminCommand::ScheduleCorporateAction(CorporateAction {
        id: "v23-zero-price".into(),
        market: stock,
        previous_close: start + 420_000,
        ex_open: start + 480_000,
        adjustment: Adjustment::Split { new_units: 4, old_units: 1 },
    }));
    h.advance_to(start + 480_000);
    let refused = h.cmd(Command::Tick);
    assert!(refused.iter().any(|e| matches!(e, Event::CorporateActionFailed { .. })));
    assert!(h.engine.markets()[&stock].corporate_action_blocked);
    assert_eq!(h.engine.accounts()[&long].position(stock), held);
}
