//! Stress test helpers: a market with a divergence bound of its own, quotes a set number of bps
//! apart, and parameters raised only by divergence.

use engine_core::Fixed;
use engine_core::command::{AdminCommand, Command};
use engine_core::event::Event;
use engine_core::market::MarketSpec;
use engine_core::params::{ProtocolParams, StressTrigger};
use engine_core::price::SourceQuote;
use engine_core::session::{Period, Schedule};
use engine_core::types::{AssetClass, MarketId, Millis};

use crate::common::{Harness, f, market, params, us_equities};

/// A market the divergence tests list with a bound of their own.
pub const BOUNDED: MarketId = 20;
/// The divergence bound of every harness market.
pub const MAX_DIVERGENCE_BPS: i64 = 50;

/// Takes the house's 100m of genesis capital down to 1m, so each harness cap is 500k.
pub fn at_a_million_nav(h: &mut Harness) {
    let ev = h.cmd(Command::HouseCapital { amount: f("-99000000"), chain_ref: "to-a-million".into() });
    assert!(matches!(ev[..], [Event::HouseCapitalMoved { .. }, ..]), "{ev:?}");
    assert_eq!(h.engine.house_nav(), f("1000000"));
}

/// A venue quote.
pub fn quote(source: &str, mid: &str, ts: Millis) -> SourceQuote {
    SourceQuote { source: source.into(), mid: f(mid), ts, contract: None }
}

/// A `V13` harness with one stress level, raised only by a divergence of `share` of the bound and
/// never held by a cooldown, and the `BOUNDED` market listed with `bound`.
pub fn divergence_only(share: i64, bound: i64) -> Harness {
    let mut h = Harness::v13();
    h.operator_admin(AdminCommand::SetParams(divergence_params(share)));
    h.operator_admin(AdminCommand::UpsertMarket(bounded(bound)));
    h
}

/// The harness parameters with one stress level, raised only by a divergence of `share` of the
/// bound, and no cooldown.
pub fn divergence_params(share: i64) -> ProtocolParams {
    let mut p = ProtocolParams { stress_cooldown_ms: 0, ..with_divergence_shares(params()) };
    p.stress_levels[1].trigger = Some(StressTrigger { divergence_share_bps: Some(share), ..StressTrigger::default() });
    p
}

/// The `BOUNDED` market, 24/7, with a divergence bound of `bound`.
pub fn bounded(bound: i64) -> MarketSpec {
    let mut spec = market(BOUNDED, "BOUNDED", AssetClass::Crypto, Schedule::AlwaysOpen, 1_000);
    spec.price_rules.max_divergence_bps = bound;
    spec
}

/// Two venues `bps` apart around 10,000, so the index is 10,000 and diverges by exactly `bps`.
pub fn diverge(h: &mut Harness, bps: i64) {
    let half = Fixed::from_int(bps).div_int(2);
    let (low, high) = (Fixed::from_int(10_000) - half, Fixed::from_int(10_000) + half);
    let ev = h.quotes(BOUNDED, &[&low.to_string(), &high.to_string()]);
    assert!(!ev.iter().any(|e| matches!(e, Event::PricesRejected { .. })), "{ev:?}");
}

/// `p` with each absolute divergence trigger as the same threshold's share of the harness markets'
/// divergence bound, the form `V13` takes.
pub fn with_divergence_shares(mut p: ProtocolParams) -> ProtocolParams {
    p.share_divergence_triggers(MAX_DIVERGENCE_BPS);
    p
}

/// US equities hours with one dated closure, for the across-halt tests and the `V17` reference
/// part.
pub fn halted_us_equities(from: &str, to: &str) -> Schedule {
    let Schedule::Weekly { tz, windows, .. } = us_equities() else { unreachable!("US equities keep weekly hours") };
    let at = |local: &str| local.parse().expect("a local date and time");
    Schedule::Weekly {
        tz,
        windows,
        closed_dates: vec![],
        closures: vec![Period { from: at(from), to: at(to) }],
        special_sessions: vec![],
    }
}
