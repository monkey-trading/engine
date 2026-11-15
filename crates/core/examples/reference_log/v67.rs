//! The session's `V67` part: request and order limits. A vote activates the version, which gives the
//! table's tiers resting-order limits of 25, 50, 100 and 250 and request rates of 2, 5, 10 and 20; a
//! fresh wallet rests 25 orders and the 26th is refused naming tier 0, and a voted table that raises
//! tier 0's limit above launch's is refused.

use engine_core::command::{AdminCommand, Command, OrderKind};
use engine_core::event::{Event, Reject};
use engine_core::governance::VotedChange;
use engine_core::operator_bounds::NOTICE_MS;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::Side;
use engine_core::volume_tier::{ACTIVATION_MAX_RESTING_ORDERS, ACTIVATION_REQUESTS_PER_SECOND};

use crate::common::close_spread::{attest, fund, rest};
use crate::common::events::rejection;
use crate::common::{Client, EURUSD, Harness, f};
use crate::v56::pass;

/// The seed of the wallet this part trades through.
const CLIENT_SEED: u8 = 0x7b;

/// `V67` activates by vote. A fresh wallet rests 25 buy limits in EUR-USD, and a 26th is refused as
/// tier 0's limit; a voted cut of nothing but a raised limit, 26 at tier 0, is refused.
pub(crate) fn session(h: &mut Harness) {
    let activate = AdminCommand::ActivateProtocol { version: ProtocolVersion::V67 };
    scheduled(&pass(h, 80, false, Some(activate.clone())));
    h.advance(NOTICE_MS);
    applied(&apply(h, 80, activate));
    let tiers = h.engine.params().expect("set").volume_tiers.clone();
    assert_eq!(tiers.iter().map(|t| t.max_resting_orders).collect::<Vec<_>>(), ACTIVATION_MAX_RESTING_ORDERS);
    assert_eq!(tiers.iter().map(|t| t.requests_per_second).collect::<Vec<_>>(), ACTIVATION_REQUESTS_PER_SECOND);
    // An activation leaves the rollovers its notice spanned to the next command.
    h.cmd(Command::Tick);

    let client = Client { expires_at: h.ts + 60_000, ..Client::new(CLIENT_SEED) };
    attest(h, &client);
    fund(h, &client, "10000");
    h.price(EURUSD, "1.1");
    let bid = |h: &mut Harness| rest(h, &client, EURUSD, Side::Buy, "10000", OrderKind::Limit { price: f("0.5") });
    for _ in 0..25 {
        let events = bid(h);
        assert!(events.iter().any(|event| matches!(event, Event::OrderAccepted { .. })), "{events:?}");
    }
    let refused = bid(h);
    assert_eq!(rejection(&refused), &Reject::TierOrderLimit { tier: 0, max: 25 });

    let mut raised = h.engine.baseline_params().expect("set");
    raised.volume_tiers[0].max_resting_orders = 26;
    let raised = AdminCommand::SetParams(raised);
    scheduled(&pass(h, 81, false, Some(raised.clone())));
    h.advance(NOTICE_MS);
    let events = apply(h, 81, raised);
    assert!(
        matches!(events.last(), Some(Event::AdminRejected { reason }) if reason.contains("at most its launch value 25")),
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
