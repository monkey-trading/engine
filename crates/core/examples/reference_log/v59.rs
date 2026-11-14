//! The session's `V59` part: the stress trigger on the house's marked loss over the trailing day. With
//! rotation on, a passed vote activates it, giving level 2 a threshold of zero; a client's gain past
//! what the house earned since raises BTC to level 2, where its next fill pays the level's spread; the
//! house back ahead clears the trigger, and BTC steps down after the cooldown; a vote raising the
//! threshold is refused and one lowering it applies.

use engine_core::Fixed;
use engine_core::command::{AdminCommand, Command};
use engine_core::event::Event;
use engine_core::governance::VotedChange;
use engine_core::house_loss::LossTrigger;
use engine_core::operator_bounds::NOTICE_MS;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::Side;

use crate::common::close_spread::{attest, fund, reduce, trade};
use crate::common::{BTC, Client, Harness, f};
use crate::v56::pass;

/// The seed of the key this part trades through.
const CLIENT_SEED: u8 = 0x66;
/// The stress cooldown in force.
const COOLDOWN_MS: i64 = 300_000;

/// Activates `V59` by vote; a gain raises BTC to level 2 and the house back ahead lets it step down;
/// the threshold moves only down.
pub(crate) fn session(h: &mut Harness) {
    let activation = AdminCommand::ActivateProtocol { version: ProtocolVersion::V59 };
    assert!(matches!(pass(h, 14, false, Some(activation.clone())).last(), Some(Event::ProposalScheduled { .. })));
    h.advance(NOTICE_MS);
    // The week's rollovers run first: an activation runs none.
    h.cmd(Command::Tick);
    assert_eq!(apply(h, 14, activation).last(), Some(&Event::AdminApplied));
    let launch = vec![LossTrigger { level: 2, house_loss: Fixed::ZERO }];
    assert_eq!(h.engine.operator_bounds().launch_loss_triggers, Some(launch));

    // The client's buy leaves the clients at least half a BTC long, so a 1% rise is a loss to the house,
    // within the house's cap on BTC at its NAV here.
    let client = Client { expires_at: h.ts + 60_000, ..Client::new(CLIENT_SEED) };
    attest(h, &client);
    fund(h, &client, "1000000");
    // Priced again at its mark, BTC moves no position.
    let mid = h.engine.markets()[&BTC].mark.expect("BTC has traded");
    h.price(BTC, &mid.to_string());
    let net = h.engine.markets()[&BTC].client_qty;
    let qty = f("0.5") + (-net).max(Fixed::ZERO);
    only_fill(&trade(h, &client, BTC, Side::Buy, &qty.to_string(), &mid.to_string()));
    let up = mid + mid.mul_bps(100);
    let events = h.price(BTC, &up.to_string());
    assert!(events.iter().any(|e| matches!(e, Event::HouseLossChanged { level: 2, .. })), "{events:?}");
    assert_eq!(h.engine.markets()[&BTC].stress.level, 2);
    let add = only_fill(&trade(h, &client, BTC, Side::Buy, "0.1", &up.to_string()));
    let level_two = h.engine.params().expect("set").stress_levels[2].min_spread_bps;
    assert!(add.spread_bps >= level_two, "{add:?}");

    // Back at the mid the house is ahead again, and after the cooldown BTC steps down.
    let events = h.price(BTC, &mid.to_string());
    assert!(events.iter().any(|e| matches!(e, Event::HouseLossChanged { level: 0, .. })), "{events:?}");
    h.advance(COOLDOWN_MS);
    h.price(BTC, &mid.to_string());
    assert!(h.engine.markets()[&BTC].stress.level < 2);
    let held = (qty + f("0.1")).to_string();
    only_fill(&reduce(h, &client, BTC, Side::Sell, &held, &mid.to_string()));

    // A vote may lower the threshold but not raise it.
    let (raised, lowered) = (threshold(h, "1"), threshold(h, "-1"));
    for (proposal, command) in [(15, raised.clone()), (16, lowered.clone())] {
        assert!(matches!(pass(h, proposal, false, Some(command)).last(), Some(Event::ProposalScheduled { .. })));
    }
    h.advance(NOTICE_MS);
    assert!(matches!(apply(h, 15, raised).last(), Some(Event::AdminRejected { .. })));
    assert_eq!(apply(h, 16, lowered).last(), Some(&Event::AdminApplied));
}

/// The baseline parameters with level 2's loss threshold at `amount`.
fn threshold(h: &Harness, amount: &str) -> AdminCommand {
    let mut params = h.engine.baseline_params().expect("set");
    params.stress_levels[2].trigger.get_or_insert_with(Default::default).house_loss = Some(amount.parse().unwrap());
    AdminCommand::SetParams(params)
}

/// Sequences the node's `ApplyProposal` for `proposal` with `command`.
fn apply(h: &mut Harness, proposal: u64, command: AdminCommand) -> Vec<Event> {
    h.cmd(Command::ApplyProposal(Box::new(VotedChange { proposal, command })))
}

/// The one fill among `events`.
fn only_fill(events: &[Event]) -> engine_core::event::Fill {
    crate::common::events::only_fill(events).clone()
}
