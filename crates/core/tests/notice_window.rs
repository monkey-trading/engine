//! From `V88`, through `Engine::apply`: a notice is used from its time to seven days after it, and the
//! change it names needs a new notice after that, so no notice stays armed for the operator to apply at a
//! moment of its choosing; and a noticed price-rule change must still price a market that holds positions,
//! reduce-only or not, since a market with no index sets no mark and its close-outs wait.

mod common;

use common::close_spread::trade;
use common::events::only_fill;
use common::portfolio::{genesis_with_grid, grid_notice, pairs, with_grid};
use common::weekend::{NIKKEI, client, genesis_at, globex};
use common::{BTC, Harness, USDJPY, f};
use engine_core::Fixed;
use engine_core::command::{AdminCommand, Command};
use engine_core::corporate::CorporateBounds;
use engine_core::event::Event;
use engine_core::holding_fee::CurvePoint;
use engine_core::listing_bounds::ListingNotice;
use engine_core::market::{MarketSpec, PriceRules};
use engine_core::operator_bounds::{NOTICE_MS, NOTICE_USE_MS, ParamsNotice, StressSettings};
use engine_core::params::{ProtocolParams, Rollover};
use engine_core::protocol::ProtocolVersion;
use engine_core::toxicity::ScoreTier;
use engine_core::types::{Millis, Side};
use engine_core::volume_tier::launch_volume_tiers;

const DAY_MS: Millis = 86_400_000;
const V87: ProtocolVersion = ProtocolVersion::V87;
const V88: ProtocolVersion = ProtocolVersion::V88;

#[test]
fn an_armed_price_rule_notice_cannot_freeze_a_held_market_a_month_after_its_time() {
    let mut h = genesis_at(V88, globex(vec![]), 0, 0);
    let launch = h.engine.markets()[&BTC].spec.price_rules;
    let unmeetable = PriceRules { min_sources: 9, ..launch };
    let armed_at = h.ts + NOTICE_MS;
    assert!(applied(&h.operate(rules_notice(unmeetable, armed_at))));
    h.advance_to(armed_at + 30 * DAY_MS);
    h.price(BTC, "100000");
    h.cmd(Command::Tick);
    let long = client(&mut h, 2, "12000");
    let short = client(&mut h, 3, "12000");
    h.price(BTC, "100000");
    only_fill(&trade(&mut h, &long, BTC, Side::Buy, "1", "100000"));
    only_fill(&trade(&mut h, &short, BTC, Side::Sell, "1", "100000"));
    // Reduce-only, then the noticed rules. The notice has lapsed, and the rules would leave BTC, which holds
    // positions, with no price, which refuses them first.
    h.operator_admin(AdminCommand::SetReduceOnly { market: Some(BTC), reduce_only: true });
    let freeze = h.operate(btc_with(&h, unmeetable));
    assert!(!applied(&freeze), "an armed notice froze a held market: {freeze:?}");
    assert!(reason(&freeze).contains("would have no price"), "{freeze:?}");
    let events = walk_down(&mut h, 75_000);
    assert!(events.iter().any(|e| matches!(e, Event::CloseOut { .. })), "the long closes out as BTC falls");
    assert_eq!(bad_debt(&events), Fixed::ZERO);
}

#[test]
fn a_noticed_price_rule_no_quote_meets_is_refused_on_a_held_market_even_reduce_only() {
    for (version, refused) in [(V87, false), (V88, true)] {
        let mut h = genesis_at(version, globex(vec![]), 0, 0);
        let launch = h.engine.markets()[&BTC].spec.price_rules;
        let unmeetable = PriceRules { min_sources: 9, ..launch };
        let effective_at = h.ts + NOTICE_MS;
        assert!(applied(&h.operate(rules_notice(unmeetable, effective_at))));
        h.advance_to(effective_at + DAY_MS);
        h.price(BTC, "100000");
        h.cmd(Command::Tick);
        let long = client(&mut h, 2, "12000");
        h.price(BTC, "100000");
        only_fill(&trade(&mut h, &long, BTC, Side::Buy, "1", "100000"));
        h.operator_admin(AdminCommand::SetReduceOnly { market: Some(BTC), reduce_only: true });
        let relisted = h.operate(btc_with(&h, unmeetable));
        assert_eq!(applied(&relisted), !refused, "{version:?}: {relisted:?}");
        if refused {
            assert!(reason(&relisted).contains("would have no price"), "{relisted:?}");
        }
    }
}

/// A noticed USD-JPY change no venue set meets is refused from `V88` while Japan 225, quoted in yen and held,
/// would have no price, though Japan 225 is reduce-only; before `V88` reduce-only let it through.
#[test]
fn noticed_rules_that_unprice_a_held_reduce_only_market_converting_at_them_are_refused() {
    for (version, refused) in [(V87, false), (V88, true)] {
        let mut h = genesis_at(version, globex(vec![]), 0, 0);
        let long = client(&mut h, 2, "12000");
        h.price(USDJPY, "150");
        h.price(NIKKEI, "38000");
        only_fill(&trade(&mut h, &long, NIKKEI, Side::Buy, "10", "38000"));
        let launch = h.engine.markets()[&USDJPY].spec.price_rules;
        let unmeetable = PriceRules { min_sources: 9, ..launch };
        let notice = ListingNotice {
            markup_bps: None,
            price_rules: Some(unmeetable),
            ..ListingNotice::markup(0, h.ts + NOTICE_MS)
        };
        assert!(applied(&h.operate(AdminCommand::AnnounceListing { market: USDJPY, notice })));
        h.operator_admin(AdminCommand::SetReduceOnly { market: Some(NIKKEI), reduce_only: true });
        h.advance_to(h.ts + NOTICE_MS);
        let spec = MarketSpec { price_rules: unmeetable, ..h.engine.markets()[&USDJPY].spec.clone() };
        let relisted = h.operate(AdminCommand::UpsertMarket(spec));
        assert_eq!(applied(&relisted), !refused, "{version:?}: {relisted:?}");
        if refused {
            let named = format!(
                "market {NIKKEI} has open positions and would have no price under the price rules of market {USDJPY}"
            );
            assert_eq!(reason(&relisted), named);
        }
    }
}

/// A notice for each site that checks one, listing and parameter alike, used at the moment it lapses and a
/// millisecond after: from `V88` the late use needs a new notice; before it, it applies. The listing changes
/// left out share the price rules' check, and the base profit fee the close-out level's.
#[test]
fn a_notice_is_used_within_seven_days_of_its_time_and_needs_renewing_after() {
    const KINDS: [&str; 10] = [
        "markup",
        "price rules",
        "close-out level",
        "stress",
        "score tiers",
        "profit fee curve",
        "volume tiers",
        "rollover",
        "corporate bounds",
        "portfolio grid",
    ];
    for (version, late) in [(V87, NOTICE_USE_MS + DAY_MS), (V88, NOTICE_USE_MS), (V88, NOTICE_USE_MS + 1)] {
        let lapses = version >= V88 && late > NOTICE_USE_MS;
        for kind in KINDS {
            let mut h = match kind {
                "portfolio grid" => genesis_with_grid(version, pairs(5_000), 750, 0),
                _ => genesis_at(version, globex(vec![]), 0, 0),
            };
            let effective_at = h.ts + NOTICE_MS;
            let (notice, change) = noticed(&h, kind, effective_at);
            assert!(applied(&h.operate(notice)), "{version:?} {kind}");
            h.advance_to(effective_at + late);
            let events = h.operate(change);
            assert_eq!(applied(&events), !lapses, "{version:?} {kind} {late}: {events:?}");
            if lapses {
                assert!(reason(&events).contains("needs a new notice: its notice lapsed at"), "{kind}: {events:?}");
            }
        }
    }
}

// ── Helpers ──

/// The operator's notice of a change of `kind` from `effective_at`, and the command that makes it.
fn noticed(h: &Harness, kind: &str, effective_at: Millis) -> (AdminCommand, AdminCommand) {
    let spec = h.engine.markets()[&BTC].spec.clone();
    let params = h.engine.params().cloned().expect("genesis set the parameters");
    let none = params_notice(effective_at);
    let set = |notice: ParamsNotice, set: ProtocolParams| {
        (AdminCommand::AnnounceParams(notice), AdminCommand::SetParams(set))
    };
    match kind {
        "markup" => {
            let markup = spec.financing.markup_bps + 1;
            let mut relisted = spec.clone();
            relisted.financing.markup_bps = markup;
            let notice = ListingNotice::markup(markup, effective_at);
            (AdminCommand::AnnounceListing { market: BTC, notice }, AdminCommand::UpsertMarket(relisted))
        }
        // Tighter, so within the launch rules a notice may name.
        "price rules" => {
            let rules = PriceRules { max_age_ms: spec.price_rules.max_age_ms - 1, ..spec.price_rules };
            (rules_notice(rules, effective_at), btc_with(h, rules))
        }
        "close-out level" => {
            let level = params.closeout_level_bps + 100;
            set(
                ParamsNotice { closeout_level_bps: Some(level), ..none },
                ProtocolParams { closeout_level_bps: level, ..params },
            )
        }
        "stress" => {
            let mut wider = params;
            let top = wider.stress_levels.len() - 1;
            wider.stress_levels[top].min_spread_bps += 1;
            set(ParamsNotice { stress: Some(StressSettings::of(&wider)), ..none }, wider)
        }
        "score tiers" => {
            let tiers = vec![
                ScoreTier { min_score: f("6"), extra_spread_bps: 3 },
                ScoreTier { min_score: f("12"), extra_spread_bps: 7 },
            ];
            let mut raised = params;
            raised.toxicity.score_tiers = tiers.clone();
            set(ParamsNotice { score_tiers: Some(tiers), ..none }, raised)
        }
        // The full fee held to a minute and the base reached at 30 minutes, above launch at every hold.
        "profit fee curve" => {
            let curve = vec![
                CurvePoint { hold_ms: 60_000, fee_bps: 10_000 },
                CurvePoint { hold_ms: 1_800_000, fee_bps: 1_000 },
            ];
            set(
                ParamsNotice { profit_fee_curve: Some(curve.clone()), ..none },
                ProtocolParams { profit_fee_curve: curve, ..params },
            )
        }
        "volume tiers" => {
            let mut tiers = launch_volume_tiers();
            tiers[1].threshold = f("2000000");
            set(
                ParamsNotice { volume_tiers: Some(tiers.clone()), ..none },
                ProtocolParams { volume_tiers: tiers, ..params },
            )
        }
        "rollover" => {
            let rollover = Rollover { tz: chrono_tz::Europe::London, hour: 21, minute: 0 };
            set(ParamsNotice { rollover: Some(rollover.clone()), ..none }, ProtocolParams { rollover, ..params })
        }
        "corporate bounds" => {
            let bounds = CorporateBounds { max_dividend_bps: 500, max_split_ratio: 10, min_trading_days: 3 };
            set(
                ParamsNotice { corporate_bounds: Some(bounds), ..none },
                ProtocolParams { corporate_bounds: bounds, ..params },
            )
        }
        _ => {
            let mut raised = pairs(5_000);
            raised.groups[0].members[1].residual_shock_bps = 40;
            raised.groups[1].floor_share_bps = 7_000;
            (grid_notice(raised.clone(), effective_at), AdminCommand::SetParams(with_grid(h, raised)))
        }
    }
}

fn applied(events: &[Event]) -> bool {
    events.contains(&Event::AdminApplied)
}

fn reason(events: &[Event]) -> String {
    events
        .iter()
        .find_map(|event| match event {
            Event::AdminRejected { reason } => Some(reason.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

fn bad_debt(events: &[Event]) -> Fixed {
    events
        .iter()
        .filter_map(|event| match event {
            Event::BadDebt { amount, .. } => Some(*amount),
            _ => None,
        })
        .fold(Fixed::ZERO, |sum, amount| sum + amount)
}

fn btc_with(h: &Harness, rules: PriceRules) -> AdminCommand {
    AdminCommand::UpsertMarket(MarketSpec { price_rules: rules, ..h.engine.markets()[&BTC].spec.clone() })
}

/// BTC from 100,000 down to `to` a thousand at a time.
fn walk_down(h: &mut Harness, to: i64) -> Vec<Event> {
    let mut all = Vec::new();
    let mut mid = 100_000;
    while mid > to {
        mid -= 1_000;
        h.advance(200);
        all.extend(h.price(BTC, &mid.to_string()));
    }
    all
}

/// The operator's notice of BTC's price rules `rules` from `effective_at`.
fn rules_notice(rules: PriceRules, effective_at: Millis) -> AdminCommand {
    let notice = ListingNotice { markup_bps: None, price_rules: Some(rules), ..ListingNotice::markup(0, effective_at) };
    AdminCommand::AnnounceListing { market: BTC, notice }
}

/// A parameter notice from `effective_at` naming nothing.
fn params_notice(effective_at: Millis) -> ParamsNotice {
    ParamsNotice {
        profit_fee_bps: None,
        closeout_level_bps: None,
        stress: None,
        score_tiers: None,
        profit_fee_curve: None,
        volume_tiers: None,
        rollover: None,
        corporate_bounds: None,
        portfolio_grid: None,
        effective_at,
    }
}
