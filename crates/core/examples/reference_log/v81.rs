//! The session's `V81` part: close-outs under portfolio margin, while votes govern. A vote names BTC and
//! ETH as one offset group in the parameters, at a 1% floor share, and a second activates the version,
//! which takes that grid up as its launch grid. A client holds BTC long against ETH short: ETH tripling
//! closes the pair whole, BTC before ETH. The client reopens the pair, and ETH rises until the pair stays
//! above the close-out level of its portfolio requirement while ETH alone would not: a sell limit on BTC
//! under the mid fills as it rests, and the check after the command's fills closes ETH. Votes then relist
//! BTC and ETH at once, raise ETH's residual shock within its bound and refuse one below its launch value,
//! and the fast path raises the group's floor share at once.

use engine_core::Fixed;
use engine_core::command::{AdminCommand, Command, OrderKind};
use engine_core::event::Event;
use engine_core::fixed::Rounding;
use engine_core::governance::VotedChange;
use engine_core::market::MarketSpec;
use engine_core::operator_bounds::NOTICE_MS;
use engine_core::params::ProtocolParams;
use engine_core::portfolio::{GroupMember, OffsetGroup, PortfolioGrid};
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{MarketId, Side};

use crate::common::close_spread::{attest, fund, rest, trade};
use crate::common::events::fills;
use crate::common::{BTC, Client, ETH, Harness, f};
use crate::v56::pass;

/// The seed of the key this part trades through.
const CLIENT_SEED: u8 = 0x81;

/// Takes the grid up by vote, then the close-outs, the check after a command's fills and the votes above.
pub(crate) fn session(h: &mut Harness) {
    let grid = crypto(100, 20);
    let named = AdminCommand::SetParams(ProtocolParams { portfolio_grid: grid.clone(), ..baseline(h) });
    let activation = AdminCommand::ActivateProtocol { version: ProtocolVersion::V81 };
    scheduled(h, 230, &named);
    scheduled(h, 231, &activation);
    h.advance(NOTICE_MS);
    applied(h, 230, named);
    applied(h, 231, activation);
    assert_eq!(h.engine.protocol_version(), ProtocolVersion::V81);
    assert_eq!(h.engine.operator_bounds().launch_grid.as_ref(), Some(&grid));
    // An activation leaves the rollovers its notice spanned to the next command.
    h.cmd(Command::Tick);
    let (btc, eth) = (mark(h, BTC), mark(h, ETH));
    h.price(BTC, &btc.to_string());
    h.price(ETH, &eth.to_string());

    // A pair closed out whole: ETH tripling leaves nothing to restore the level but the group.
    let client = Client { expires_at: h.ts + 60_000, ..Client::new(CLIENT_SEED) };
    attest(h, &client);
    fund(h, &client, "200000");
    open_pair(h, &client, btc, eth);
    // Earlier parts' holders of ETH close out too; the client's close-out is its pair.
    let events = h.price(ETH, &eth.mul_int(3).to_string());
    let own = fills(&events).into_iter().filter(|fill| fill.account == client.account && fill.liquidation);
    let closed: Vec<MarketId> = own.map(|fill| fill.market).collect();
    assert_eq!(closed, vec![BTC, ETH], "{events:?}");
    h.price(ETH, &eth.to_string());

    // The pair again, on fresh cash, with ETH up until it stays above its level and ETH alone would not.
    fund(h, &client, "200000");
    open_pair(h, &client, btc, eth);
    let between = within_window(h, &client, eth);
    assert!(fills(&h.price(ETH, &between.to_string())).is_empty());
    let under = btc - btc.mul_bps(100);
    let events = rest(h, &client, BTC, Side::Sell, &qty(btc).to_string(), OrderKind::Limit { price: under });
    let closed: Vec<(MarketId, bool)> = fills(&events).iter().map(|fill| (fill.market, fill.liquidation)).collect();
    assert_eq!(closed, vec![(BTC, false), (ETH, true)], "{events:?}");
    h.price(ETH, &eth.to_string());

    // Votes: BTC and ETH relisted at once; ETH's shock raised within its bound, and lowered below launch.
    let relisted = |h: &Harness, id: MarketId| MarketSpec {
        max_slippage_bps: h.engine.markets()[&id].spec.max_slippage_bps - 1,
        ..h.engine.baseline_spec(id).expect("listed")
    };
    let relisting = AdminCommand::RelistMarkets { specs: vec![relisted(h, BTC), relisted(h, ETH)] };
    let raised = AdminCommand::SetParams(ProtocolParams { portfolio_grid: crypto(100, 60), ..baseline(h) });
    let lowered = AdminCommand::SetParams(ProtocolParams { portfolio_grid: crypto(100, 10), ..baseline(h) });
    scheduled(h, 232, &relisting);
    scheduled(h, 233, &lowered);
    h.advance(NOTICE_MS);
    applied(h, 232, relisting);
    let events = h.cmd(Command::ApplyProposal(Box::new(VotedChange { proposal: 233, command: lowered })));
    assert!(matches!(events.last(), Some(Event::AdminRejected { .. })), "{events:?}");
    scheduled(h, 234, &raised);
    h.advance(NOTICE_MS);
    applied(h, 234, raised);

    // The fast path raises the floor share at once.
    let restricted = AdminCommand::SetParams(ProtocolParams { portfolio_grid: crypto(500, 60), ..baseline(h) });
    let events = pass(h, 235, true, Some(restricted));
    assert!(events.iter().any(|e| matches!(e, Event::RestrictionApplied { proposal: 235, .. })), "{events:?}");
    let in_force = h.engine.params().expect("set").portfolio_grid.groups[0].floor_share_bps;
    assert_eq!(in_force, 500);
}

/// BTC and ETH as one group at `floor_share_bps`, ETH loading 1.25, each at `residual_bps`, which a vote
/// may raise to 80 bps, and the floor share to all of it.
fn crypto(floor_share_bps: i64, residual_bps: i64) -> PortfolioGrid {
    let member = |market, loading: &str| GroupMember {
        market,
        loading: f(loading),
        residual_shock_bps: residual_bps,
        max_residual_shock_bps: 80,
    };
    let group = OffsetGroup {
        name: "crypto".into(),
        floor_share_bps,
        max_floor_share_bps: 10_000,
        members: vec![member(BTC, "1"), member(ETH, "1.25")],
    };
    PortfolioGrid { groups: vec![group] }
}

/// The baseline parameters.
fn baseline(h: &Harness) -> ProtocolParams {
    h.engine.baseline_params().expect("set")
}

/// Passes `proposal` carrying `command`, scheduled seven days on.
pub(crate) fn scheduled(h: &mut Harness, proposal: u64, command: &AdminCommand) {
    let events = pass(h, proposal, false, Some(command.clone()));
    assert!(matches!(events.last(), Some(Event::ProposalScheduled { .. })), "{events:?}");
}

/// Applies `proposal`'s `command`, which must apply.
pub(crate) fn applied(h: &mut Harness, proposal: u64, command: AdminCommand) {
    let events = h.cmd(Command::ApplyProposal(Box::new(VotedChange { proposal, command })));
    assert_eq!(events.last(), Some(&Event::AdminApplied), "{events:?}");
}

/// `market`'s mark.
pub(crate) fn mark(h: &Harness, market: MarketId) -> Fixed {
    h.engine.markets()[&market].mark.expect("traded before")
}

/// $200k of quantity at `price`, to the nearest thousandth below.
pub(crate) fn qty(price: Fixed) -> Fixed {
    let units = Fixed::from_int(200_000).div_rounded(price, Rounding::Down);
    units - Fixed::from_raw(units.raw() % 1_000_000)
}

/// $200k of BTC long and $200k of ETH short for `client` at `btc` and `eth`.
pub(crate) fn open_pair(h: &mut Harness, client: &Client, btc: Fixed, eth: Fixed) {
    let (btc_qty, eth_qty) = (qty(btc).to_string(), qty(eth).to_string());
    let events = trade(h, client, BTC, Side::Buy, &btc_qty, &btc.to_string());
    assert_eq!(fills(&events).len(), 1, "{events:?}");
    let events = trade(h, client, ETH, Side::Sell, &eth_qty, &eth.to_string());
    assert_eq!(fills(&events).len(), 1, "{events:?}");
}

/// The lowest ETH price above `eth`, on a grid of 0.1%, where `client` stays above the close-out level of
/// its portfolio requirement by at least a thousandth of ETH's notional, and its ETH position alone would
/// be short of half its bands by as much, with every other position at its mark.
fn within_window(h: &Harness, client: &Client, eth: Fixed) -> Fixed {
    let acct = &h.engine.accounts()[&client.account];
    let moved = h.engine.portfolio_move(acct, ETH).expect("a hedged pair");
    let (equity, _) = h.engine.equity_and_closeout_requirement(acct);
    let held = acct.position(ETH).qty;
    let spec = &h.engine.markets()[&ETH].spec;
    (1..2_000)
        .map(|step| eth + eth.mul_bps(10 * step))
        .find(|price| {
            let notional = held.abs().mul(*price);
            let buffer = notional.mul_bps(10);
            let equity_there = equity + held.mul(*price - eth);
            let alone = spec.margin_for(notional, Rounding::Up).mul_bps(5_000);
            moved.slack_at(*price) > buffer && equity_there < alone - buffer
        })
        .expect("the pair's credit exceeds BTC's bands, so such a price exists")
}
