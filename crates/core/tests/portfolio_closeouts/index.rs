//! The close-out index under the portfolio requirement (#912, #639): its keys bound the per-position
//! requirement, which is never below the portfolio one, so each stays sound; its exact check reads the
//! portfolio requirement; and a raised shock or floor share alone, with no price, close-out level or
//! band moved, re-assesses every unkeyed account. Each test runs with the oracle checking the index
//! against a full assessment at every refresh and check site.

use super::*;
use common::portfolio::{grid_notice, with_grid};
use common::spreads::applied;
use engine_core::CloseoutOracle;
use engine_core::command::{AdminCommand, Command};
use engine_core::operator_bounds::NOTICE_MS;

/// The pair holder of `pairs(floor_share_bps)`, with the oracle verifying the index, after `notice`, if
/// any, is announced and its seven days have run.
fn verified(floor_share_bps: i64, notice: Option<PortfolioGrid>) -> (Harness, Client) {
    let mut h = genesis_with_grid(V81, pairs(floor_share_bps), 750, 0);
    h.engine.set_closeout_oracle(CloseoutOracle { verify: true, ..CloseoutOracle::default() });
    if let Some(grid) = notice {
        assert!(applied(&h.operate(grid_notice(grid, h.ts + NOTICE_MS))));
        h.advance(NOTICE_MS);
        h.cmd(Command::Tick);
    }
    let a = client(&mut h, 1, "210000");
    open_pair(&mut h, &a);
    (h, a)
}

/// Moves BTC until `client`'s slack is about `slack` dollars, each dollar of BTC moving it by `per_dollar`.
fn move_btc_to_slack(h: &mut Harness, client: &Client, slack: i64, per_dollar: &str) {
    let (equity, level) = standing(h, client);
    let btc = f("100000")
        + (f(&slack.to_string()) - (equity - level)).div_rounded(f(per_dollar), engine_core::fixed::Rounding::Down);
    assert!(fills(&h.price(BTC, &btc.to_string())).is_empty());
}

#[test]
fn a_price_bringing_a_hedged_account_to_its_level_closes_it_out() {
    let (mut h, a) = verified(5_000, None);
    // Between its two levels the account is unkeyed: its keys would bound per-position margin.
    assert!(fills(&h.price(ETH, "4200")).is_empty());
    assert!(fills(&h.price(BTC, "92000")).is_empty());
    let events = h.price(ETH, "4400");
    assert_eq!(liquidations(&events), vec![BTC, ETH], "{events:?}");
    assert!(h.engine.accounts()[&a.account].positions.is_empty());
}

#[test]
fn a_raised_shock_alone_closes_out_an_account_it_brings_below_its_level() {
    // At a 1% floor share the credit is the matched bands less the grid charge, so a shock raised from
    // 0.2% to 0.8% on each leg raises the requirement by about $10.8k, half of it at the level.
    let mut raised = pairs(100);
    for member in &mut raised.groups[0].members {
        member.residual_shock_bps = 80;
    }
    let (mut h, a) = verified(100, Some(raised.clone()));
    move_btc_to_slack(&mut h, &a, 2_700, "10.338");
    let (equity, level) = standing(&h, &a);
    assert!(equity > level && equity - level < f("5400"), "{equity} against {level}");
    let events = h.operate(AdminCommand::SetParams(with_grid(&h, raised)));
    assert_eq!(liquidations(&events), vec![BTC, ETH], "{events:?}");
    assert_eq!(events.last(), Some(&Event::AdminApplied), "the outcome stays the command's last event");
    assert!(h.engine.closeout_coverage().regrids > 0);
}

#[test]
fn a_raised_floor_share_alone_closes_out_an_account_it_brings_below_its_level() {
    // From a 1% floor share to 20%: the credit falls from the matched bands less their grid charge, about
    // 93% of the matched bands, to 80% of them, about $23k less, raising the level by about $11.5k.
    let (mut h, a) = verified(100, Some(pairs(2_000)));
    move_btc_to_slack(&mut h, &a, 4_000, "10.338");
    let (equity, level) = standing(&h, &a);
    assert!(equity > level && equity - level < f("8000"), "{equity} against {level}");
    let events = h.operate(AdminCommand::SetParams(with_grid(&h, pairs(2_000))));
    assert_eq!(liquidations(&events), vec![BTC, ETH], "{events:?}");
    assert!(h.engine.closeout_coverage().regrids > 0);
}
