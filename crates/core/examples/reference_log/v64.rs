//! The session's `V64` part: earned leverage. A vote activates the version, which gives the table's
//! tiers 0 and 1 margin floors of 5% and 2%; a fresh wallet's EUR-USD order at 26.4x is refused at the
//! floor and 19.8x fills, a withdrawal past the initial requirement is refused, and a voted table that
//! cuts tier 0's floor is refused.

use engine_core::command::{AdminCommand, Command};
use engine_core::event::{Event, Reject};
use engine_core::governance::VotedChange;
use engine_core::market::MarketSpec;
use engine_core::operator_bounds::NOTICE_MS;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::Side;

use crate::common::close_spread::{attest, fund, trade};
use crate::common::events::{only_fill, rejection};
use crate::common::weekend::withdraw;
use crate::common::{Client, EURUSD, Harness, f};
use crate::v56::pass;

/// The seed of the wallet this part trades through.
const CLIENT_SEED: u8 = 0x6a;

/// `V64` activates by vote. A fresh wallet with $10,000 is refused a $264k EUR-USD long, which EUR-USD's
/// 2% admits but tier 0's 5% floor does not, and opens $198k; a $100 withdrawal, past what its equity
/// holds above the initial requirement, is refused. A voted cut of tier 0's floor is refused.
pub(crate) fn session(h: &mut Harness) {
    let activate = AdminCommand::ActivateProtocol { version: ProtocolVersion::V64 };
    // Staking's epochs sweep the house's NAV down to its liquidity floor, so a $1M floor under EUR-USD's
    // house cap lets the house take the wallet's $198k.
    let floored = MarketSpec { house_cap_floor: f("1000000"), ..h.engine.baseline_spec(EURUSD).expect("listed") };
    let floored = AdminCommand::UpsertMarket(floored);
    scheduled(&pass(h, 50, false, Some(floored.clone())));
    scheduled(&pass(h, 51, false, Some(activate.clone())));
    h.advance(NOTICE_MS);
    applied(&apply(h, 50, floored));
    applied(&apply(h, 51, activate));
    let floors: Vec<i64> = h.engine.params().expect("set").volume_tiers.iter().map(|t| t.margin_floor_bps).collect();
    assert_eq!(floors, [500, 200, 0, 0]);
    // An activation leaves the rollovers its notice spanned to the next command.
    h.cmd(Command::Tick);

    let client = Client { expires_at: h.ts + 60_000, ..Client::new(CLIENT_SEED) };
    attest(h, &client);
    fund(h, &client, "10000");
    h.price(EURUSD, "1.1");
    let refused = rejection(&trade(h, &client, EURUSD, Side::Buy, "240000", "1.1")).clone();
    assert!(matches!(refused, Reject::InsufficientMargin { required, .. } if required == f("13200")), "{refused:?}");
    only_fill(&trade(h, &client, EURUSD, Side::Buy, "180000", "1.1"));
    let events = withdraw(h, &client, "100");
    assert!(matches!(events.last(), Some(Event::WithdrawalRejected { reason: Reject::InsufficientFunds { .. }, .. })));

    let mut cut = h.engine.baseline_params().expect("set");
    cut.volume_tiers[0].margin_floor_bps = 200;
    let cut = AdminCommand::SetParams(cut);
    scheduled(&pass(h, 52, false, Some(cut.clone())));
    h.advance(NOTICE_MS);
    let events = apply(h, 52, cut);
    assert!(
        matches!(events.last(), Some(Event::AdminRejected { reason }) if reason.contains("below its launch value")),
        "{events:?}"
    );
}

/// Sequences the node's `ApplyProposal` for `proposal` with `command`.
fn apply(h: &mut Harness, proposal: u64, command: AdminCommand) -> Vec<Event> {
    h.cmd(Command::ApplyProposal(Box::new(VotedChange { proposal, command })))
}

fn scheduled(events: &[Event]) {
    assert!(matches!(events.last(), Some(Event::ProposalScheduled { .. })), "{events:?}");
}

fn applied(events: &[Event]) {
    assert_eq!(events.last(), Some(&Event::AdminApplied), "{events:?}");
}
