//! The session's `V59` part: volume tiers. A vote activates the version, which gives the parameters the
//! launch table; a fresh wallet trades at the best price only up to a quarter of BTC's first band,
//! churn and a round trip count nothing, an open that crosses midnight counts on the day it matures,
//! and $500k counted on each of two days reaches tier 1, whose first band ends at half the bound. A
//! voted table that widens a share is refused, and so is one that raises a threshold, as BTC's voted
//! bands would charge an account it demotes more than twice its launch spread.

use engine_core::command::{AdminCommand, Command};
use engine_core::event::{Event, Fill};
use engine_core::governance::{PassedProposal, VotedChange, command_hash};
use engine_core::market::MarketSpec;
use engine_core::operator_bounds::NOTICE_MS;
use engine_core::protocol::ProtocolVersion;
use engine_core::toxicity::HORIZON_MS;
use engine_core::types::Side;
use engine_core::volume_tier::{DAY_MS, MIN_HOLD_MS, day_of, launch_volume_tiers};

use crate::common::close_spread::{attest, fund, trade};
use crate::common::{BTC, Client, Harness, f};

/// The seed of the wallet this part trades through.
const CLIENT_SEED: u8 = 0x66;

/// `V59` activates by vote. The wallet churns and makes a round trip five minutes before midnight,
/// opens 6 BTC 30 s before it, which counts on the next day, closes them a day later, and once that
/// close's horizon has passed trades at tier 1. A voted share widening and a voted threshold raise are
/// refused at application.
pub(crate) fn session(h: &mut Harness) {
    let activate = AdminCommand::ActivateProtocol { version: ProtocolVersion::V59 };
    // Staking's epochs sweep the house's NAV down to its liquidity floor, so a $5M floor under BTC's
    // house cap lets the house take the wallet's $600k.
    let floored = MarketSpec { house_cap_floor: f("5000000"), ..h.engine.baseline_spec(BTC).expect("listed") };
    let floored = AdminCommand::UpsertMarket(floored);
    let logged = h.ts;
    scheduled(&pass(h, 20, floored.clone()));
    scheduled(&pass(h, 21, activate.clone()));
    h.advance_to(logged + NOTICE_MS);
    applied(&apply(h, 20, floored));
    applied(&apply(h, 21, activate));
    assert_eq!(h.engine.params().expect("set").volume_tiers, launch_volume_tiers());
    // An activation leaves the rollovers its notice spanned to the next command.
    h.cmd(Command::Tick);

    let client = Client { expires_at: h.ts + 60_000, ..Client::new(CLIENT_SEED) };
    attest(h, &client);
    fund(h, &client, "2000000");
    let midnight = (day_of(h.ts) + 1) * DAY_MS;
    h.advance_to(midnight - 300_000);
    h.price(BTC, "100000");
    only_fill(&trade(h, &client, BTC, Side::Buy, "1", "100000"));
    h.advance(1_000);
    h.price(BTC, "100000");
    only_fill(&trade(h, &client, BTC, Side::Sell, "1", "100000"));
    h.advance(30_000);
    h.price(BTC, "100100");
    only_fill(&trade(h, &client, BTC, Side::Buy, "1", "100100"));
    h.advance(30_000);
    h.price(BTC, "100000");
    only_fill(&trade(h, &client, BTC, Side::Sell, "1", "100000"));

    h.advance_to(midnight - 30_000);
    h.price(BTC, "100000");
    // BTC's bands read 2bp to $1M after `V56`'s vote; tier 0's ends at $250k, and 4bp follow to $5M.
    let opened = only_fill(&trade(h, &client, BTC, Side::Buy, "6", "100000"));
    assert_eq!(opened.price, f("100031.666666667"), "{opened:?}");
    h.advance_to(midnight + MIN_HOLD_MS);
    h.cmd(Command::Tick);
    let standing = h.engine.volume_standing(client.account).expect("from V59");
    assert_eq!((standing.tier, standing.matured), (0, opened.price.mul(f("6"))));

    h.advance(DAY_MS);
    h.price(BTC, "100000");
    only_fill(&trade(h, &client, BTC, Side::Sell, "6", "100000"));
    assert_eq!(h.engine.volume_standing(client.account).expect("from V59").tier, 0);
    h.advance(HORIZON_MS);
    h.price(BTC, "100000");
    assert_eq!(h.engine.volume_standing(client.account).expect("from V59").tier, 1);
    // Tier 1's first band ends at $500k: 2bp on it and 4bp on the $100k above.
    let tiered = only_fill(&trade(h, &client, BTC, Side::Buy, "6", "100000"));
    assert_eq!(tiered.price, f("100023.333333334"), "{tiered:?}");

    let mut raised = h.engine.baseline_params().expect("set");
    raised.volume_tiers[1].threshold = f("2000000");
    let mut wider = raised.clone();
    wider.volume_tiers[3].best_price_share_bps = 30_000;
    let (raised, wider) = (AdminCommand::SetParams(raised), AdminCommand::SetParams(wider));
    let logged = h.ts;
    scheduled(&pass(h, 22, wider.clone()));
    scheduled(&pass(h, 23, raised.clone()));
    h.advance_to(logged + NOTICE_MS);
    refused(&apply(h, 22, wider), "above its launch value");
    // An account at tier 1 at launch, dropped to tier 0, would pay BTC's voted 4bp from $250k, where
    // it paid 1bp to $500k.
    refused(&apply(h, 23, raised), "market 1 at volume tier 0, for an account at tier 1 at launch");
}

/// Mirrors normal proposal `proposal` passing with `command`.
fn pass(h: &mut Harness, proposal: u64, command: AdminCommand) -> Vec<Event> {
    let command_hash = command_hash(&command);
    let (block, chain_ref) = (59_000_000 + proposal, format!("0xvote:59:{proposal}"));
    let passed = PassedProposal { proposal, fast: false, command_hash, command: Some(command), block, chain_ref };
    h.cmd(Command::PassProposal(Box::new(passed)))
}

/// Sequences the node's `ApplyProposal` for `proposal` with `command`.
fn apply(h: &mut Harness, proposal: u64, command: AdminCommand) -> Vec<Event> {
    h.cmd(Command::ApplyProposal(Box::new(VotedChange { proposal, command })))
}

fn only_fill(events: &[Event]) -> Fill {
    crate::common::events::only_fill(events).clone()
}

fn scheduled(events: &[Event]) {
    assert!(matches!(events.last(), Some(Event::ProposalScheduled { .. })), "{events:?}");
}

fn applied(events: &[Event]) {
    assert_eq!(events.last(), Some(&Event::AdminApplied), "{events:?}");
}

fn refused(events: &[Event], reason: &str) {
    assert!(matches!(events.last(), Some(Event::AdminRejected { reason: r }) if r.contains(reason)), "{events:?}");
}
