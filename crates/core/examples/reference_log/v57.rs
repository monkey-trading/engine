//! The session's `V57` part: an add's averaged age is clamped at the launch curve's last hold, whatever
//! curve is in force, a voted one included.

use engine_core::command::{AdminCommand, Command};
use engine_core::event::{Event, Fill};
use engine_core::governance::VotedChange;
use engine_core::holding_fee::{CurvePoint, DEFAULT_PROFIT_FEE_CURVE};
use engine_core::operator_bounds::NOTICE_MS;
use engine_core::params::ProtocolParams;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::Side;

use crate::common::close_spread::{attest, fund, reduce, trade};
use crate::common::{BTC, Client, Harness};
use crate::v56::pass;

/// The seed of the key this part trades through.
const CLIENT_SEED: u8 = 0x65;

/// Under `V56`, with rotation on, a BTC long opens, and votes pass for `V57` and for the launch curve
/// extended to 30 minutes with a flat 10% tail, replacing the raised curve of the earlier parts. Both
/// apply a week later: `V57` keeps the long's open time, and a ninefold add to it counts the units held
/// as the launch curve's 15 minutes old, not the voted curve's 30: the open time moves to 90 s back,
/// where the close straight after pays the 56.28% the curve charges there.
pub(crate) fn session(h: &mut Harness) {
    let client = Client { expires_at: h.ts + 60_000, ..Client::new(CLIENT_SEED) };
    attest(h, &client);
    fund(h, &client, "1000000");
    h.price(BTC, "80000");
    let opened = h.ts;
    only_fill(&trade(h, &client, BTC, Side::Buy, "0.03", "80000"));

    let activation = AdminCommand::ActivateProtocol { version: ProtocolVersion::V57 };
    let mut extended = DEFAULT_PROFIT_FEE_CURVE.to_vec();
    extended.push(CurvePoint { hold_ms: 30 * 60_000, fee_bps: 1_000 });
    let baseline = h.engine.baseline_params().expect("parameters are set");
    let curve = AdminCommand::SetParams(ProtocolParams { profit_fee_curve: extended, ..baseline });
    for (proposal, command) in [(9, &activation), (10, &curve)] {
        let events = pass(h, proposal, false, Some(command.clone()));
        assert!(matches!(events.last(), Some(Event::ProposalScheduled { .. })), "{events:?}");
    }
    h.advance(NOTICE_MS);
    for (proposal, command) in [(9, activation), (10, curve)] {
        let events = h.cmd(Command::ApplyProposal(Box::new(VotedChange { proposal, command })));
        assert_eq!(events.last(), Some(&Event::AdminApplied), "{events:?}");
    }
    assert_eq!(h.engine.protocol_version(), ProtocolVersion::V57);
    assert_eq!(h.engine.accounts()[&client.account].opened_at.get(&BTC), Some(&opened));

    h.price(BTC, "80000");
    only_fill(&trade(h, &client, BTC, Side::Buy, "0.27", "80000"));
    h.price(BTC, "81000");
    let close = only_fill(&reduce(h, &client, BTC, Side::Sell, "0.27", "81000"));
    assert_eq!((close.hold_ms, close.profit_fee_bps), (Some(90_000), Some(5_628)), "{close:?}");
}

fn only_fill(events: &[Event]) -> Fill {
    crate::common::events::only_fill(events).clone()
}
