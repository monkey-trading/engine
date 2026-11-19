//! `V84`'s new risk and withdrawals under portfolio margin, through `Engine::apply` (#913, D4, D5, D7, D8,
//! D10, D14): the initial requirement is the portfolio requirement at initial bands, so a hedged book opens
//! and withdraws past what per-position margin admits; a group with a member that cannot trade counts at
//! per-position margin; each band keeps its tier's floor, so a lone position keeps its tier's leverage while
//! a hedged book may pass it gross. A log at `V84` whose parameters name no grid keeps the rule before it.
//! Closing one leg is in `closes`, the preview and the quotes in `preview`, and a close-out's tier in
//! `closeout_tier`.

mod common;

use common::close_spread::trade;
use common::events::{fills, only_fill, rejection};
use common::portfolio::{genesis_with_grid, pairs};
use common::weekend::{client, withdraw};
use common::{BTC, Client, ETH, Harness, f};
use engine_core::Fixed;
use engine_core::event::{Event, Reject};
use engine_core::portfolio::PortfolioGrid;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::Side;

const V83: ProtocolVersion = ProtocolVersion::V83;
const V84: ProtocolVersion = ProtocolVersion::V84;

#[test]
fn a_hedged_pair_opens_past_what_per_position_margin_admits() {
    // $150k opens $1M of BTC long at its 10% bands; ETH's $1M short would need $200k at per-position margin,
    // and its portfolio requirement is about $32k.
    let (mut h, a) = long_btc(V84, pairs(100), "150000");
    only_fill(&trade(&mut h, &a, ETH, Side::Sell, "250", "4000"));
    let standing = h.engine.margin_standing(a.account).expect("credited");
    let portfolio = h.engine.portfolio_standing(a.account, &pairs(100)).unwrap().expect("credited");
    assert_eq!(standing.initial_margin, portfolio.initial_margin);
    assert!(standing.initial_margin < f("40000"), "{standing:?}");
    // At `V83` new risk reads per-position margin, and refuses the short naming it.
    let (mut h, a) = long_btc(V83, pairs(100), "150000");
    let events = trade(&mut h, &a, ETH, Side::Sell, "250", "4000");
    let Reject::InsufficientMargin { required, .. } = rejection(&events) else { panic!("{events:?}") };
    assert_eq!(*required, f("200000"));
}

#[test]
fn a_refusal_names_the_portfolio_requirement() {
    // A short of $9M of ETH over-hedges the pair: its $1M of bands would need $1M at per-position margin, and
    // the portfolio requirement, less the credit on the matched part, is the one the refusal names.
    let (mut h, a) = pair(V84, pairs(100), "150000");
    let preview = h.engine.preview_order(a.account, ETH, Side::Sell, f("2000"), false).expect("credited");
    let events = trade(&mut h, &a, ETH, Side::Sell, "2000", "4000");
    let Reject::InsufficientMargin { equity, required } = rejection(&events) else { panic!("{events:?}") };
    assert_eq!(*required, preview.initial_margin);
    assert!(*equity < *required && *required < f("1000000"), "{events:?}");
}

#[test]
fn a_withdrawal_takes_the_equity_above_the_portfolio_requirement_and_no_more() {
    let (mut h, a) = pair(V84, pairs(100), "150000");
    let (equity, initial) = equity_and_initial(&h, &a);
    let withdrawable = h.engine.withdrawable(a.account);
    assert_eq!(withdrawable, equity - initial);
    let whole = Fixed::from_int(i64::try_from(withdrawable.raw() / Fixed::ONE.raw()).expect("in range"));
    let over = whole + Fixed::from_int(1);
    let events = withdraw(&mut h, &a, &over.to_string());
    let refused =
        matches!(events.last(), Some(Event::WithdrawalRejected { reason: Reject::InsufficientFunds { .. }, .. }));
    assert!(refused, "{events:?}");
    let events = withdraw(&mut h, &a, &whole.to_string());
    assert!(events.iter().any(|e| matches!(e, Event::WithdrawalQueued { .. })), "{events:?}");
    // At `V83` the same pair withdraws only above its per-position requirement.
    let (h, a) = pair(V83, pairs(100), "250000");
    let (equity, _) = h.equity_and_margin(a.account);
    assert_eq!(h.engine.withdrawable(a.account), equity - f("200000"));
}

#[test]
fn while_a_member_cannot_trade_its_group_counts_at_per_position_margin() {
    let (mut h, a) = pair(V84, pairs(100), "150000");
    let before = h.engine.margin_standing(a.account).expect("credited").initial_margin;
    // ETH's quotes go stale; BTC still prices.
    h.advance(2_000);
    h.price(BTC, "100000");
    let (equity, initial) = equity_and_initial(&h, &a);
    assert_eq!(initial, f("200000"), "the pair at its bands while ETH cannot trade, against {before}");
    assert_eq!(h.engine.withdrawable(a.account), Fixed::ZERO, "{equity} against {initial}");
    // Adding to BTC is checked with the pair at per-position margin, and refused.
    let events = trade(&mut h, &a, BTC, Side::Buy, "0.1", "100000");
    let Reject::InsufficientMargin { required, .. } = rejection(&events) else { panic!("{events:?}") };
    assert!(*required > f("200000"), "{events:?}");
    // Once ETH prices again, the credit returns.
    h.price(ETH, "4000");
    assert_eq!(h.engine.margin_standing(a.account).expect("credited").initial_margin, before);
    only_fill(&trade(&mut h, &a, BTC, Side::Buy, "0.1", "100000"));
}

#[test]
fn with_no_grid_new_risk_reads_per_position_margin() {
    let (mut h, a) = long_btc(V84, PortfolioGrid::default(), "150000");
    let events = trade(&mut h, &a, ETH, Side::Sell, "250", "4000");
    let Reject::InsufficientMargin { required, .. } = rejection(&events) else { panic!("{events:?}") };
    assert_eq!(*required, f("200000"));
}

// ── Helpers ──

/// A `version` log under `grid`: client 1 holding `amount` and $1M of BTC long at 100,000, ETH at 4,000.
pub(crate) fn long_btc(version: ProtocolVersion, grid: PortfolioGrid, amount: &str) -> (Harness, Client) {
    let mut h = genesis_with_grid(version, grid, 750, 0);
    let a = client(&mut h, 1, amount);
    h.price(BTC, "100000");
    h.price(ETH, "4000");
    only_fill(&trade(&mut h, &a, BTC, Side::Buy, "10", "100000"));
    (h, a)
}

/// `long_btc` with $1M of ETH short too.
pub(crate) fn pair(version: ProtocolVersion, grid: PortfolioGrid, amount: &str) -> (Harness, Client) {
    let (mut h, a) = long_btc(version, grid, amount);
    only_fill(&trade(&mut h, &a, ETH, Side::Sell, "250", "4000"));
    (h, a)
}

/// `client`'s equity and initial requirement at the marks.
pub(crate) fn equity_and_initial(h: &Harness, client: &Client) -> (Fixed, Fixed) {
    let (equity, _) = h.equity_and_margin(client.account);
    (equity, h.engine.margin_standing(client.account).expect("credited").initial_margin)
}

#[path = "portfolio_new_risk/closeout_tier.rs"]
mod closeout_tier;
#[path = "portfolio_new_risk/closes.rs"]
mod closes;
#[path = "portfolio_new_risk/preview.rs"]
mod preview;
#[path = "portfolio_new_risk/tiers.rs"]
mod tiers;
