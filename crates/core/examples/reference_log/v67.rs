//! The session's `V66` part: a weekend close-out on a market with no index pays the stress level the
//! house's loss sets. With rotation on, a vote activates the version and another puts level 2's loss
//! threshold back at zero, so every market steps down to level 0 while the house is not behind. A
//! client's AAPL long cannot be held over the weekend; a second client's BTC long gains until the house
//! is behind on the day, which raises BTC to level 2 while AAPL, whose quote has gone stale, stays at
//! level 0; when the gap starts the close-out fills at AAPL's mark at level 2's spread. A staking
//! frontier then names a drawn turn that has yet to start.

use engine_core::Fixed;
use engine_core::command::{AdminCommand, Command};
use engine_core::event::Event;
use engine_core::governance::VotedChange;
use engine_core::operator_bounds::NOTICE_MS;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{Millis, Side};

use crate::common::close_spread::{attest, fund, trade};
use crate::common::events::only_fill;
use crate::common::{AAPL, BTC, Client, Harness, f};
use crate::v56::pass;

/// The seeds of the two keys this part trades through.
const AAPL_SEED: u8 = 0x6a;
const BTC_SEED: u8 = 0x6b;
/// The stress cooldown in force.
const COOLDOWN_MS: Millis = 300_000;
const HOUR_MS: Millis = 60 * 60 * 1000;

/// Activates `V66` and puts the threshold at zero, both by vote; then the close-out described above.
pub(crate) fn session(h: &mut Harness) {
    let activation = AdminCommand::ActivateProtocol { version: ProtocolVersion::V66 };
    let mut params = h.engine.baseline_params().expect("set");
    params.stress_levels[2].trigger.get_or_insert_with(Default::default).house_loss = Some(Fixed::ZERO);
    let zero = AdminCommand::SetParams(params);
    for (proposal, command) in [(70, &activation), (71, &zero)] {
        let events = pass(h, proposal, false, Some(command.clone()));
        assert!(matches!(events.last(), Some(Event::ProposalScheduled { .. })), "{events:?}");
    }
    h.advance(NOTICE_MS);
    for (proposal, command) in [(70, activation), (71, zero)] {
        let events = h.cmd(Command::ApplyProposal(Box::new(VotedChange { proposal, command })));
        assert_eq!(events.last(), Some(&Event::AdminApplied), "{events:?}");
    }
    assert_eq!(h.engine.protocol_version(), ProtocolVersion::V66);

    // The next weekend gap: two clients open three hours before it, and the house is not behind.
    let close = h.engine.markets()[&AAPL].weekend.expect("AAPL keeps its next gap").close;
    h.advance_to(close - 3 * HOUR_MS);
    h.cmd(Command::Tick);
    let (stock, coin) = (Client::new(AAPL_SEED), Client::new(BTC_SEED));
    for (client, funds) in [(&stock, "10000"), (&coin, "1000000")] {
        attest(h, client);
        fund(h, client, funds);
    }
    let btc = h.engine.markets()[&BTC].mark.expect("BTC has traded");
    h.price(AAPL, "250");
    h.price(BTC, &btc.to_string());
    only_fill(&trade(h, &stock, AAPL, Side::Buy, "190", "250"));
    only_fill(&trade(h, &coin, BTC, Side::Buy, "5", &btc.to_string()));
    h.advance(COOLDOWN_MS);
    h.price(AAPL, "250");
    h.price(BTC, &btc.to_string());
    assert_eq!(h.engine.markets()[&AAPL].stress.level, 0);

    // AAPL is priced flat just before the close; with its quote stale, BTC rises until the house is behind.
    h.advance_to(close - 5_000);
    h.price(AAPL, "250");
    h.advance_to(close - 2_000);
    let up = btc + btc.mul_bps(400);
    h.price(BTC, &up.to_string());
    assert!(h.engine.house_loss().expect("V66").loss.is_positive());
    let levels = (h.engine.markets()[&AAPL].stress.level, h.engine.markets()[&BTC].stress.level);
    assert_eq!(levels, (0, 2));

    // The gap starts: the close-out pays level 2's spread though AAPL's own level is still 0.
    h.advance_to(close + HOUR_MS);
    let closed = only_fill(&h.cmd(Command::Tick)).clone();
    assert!(closed.liquidation && closed.account == stock.account && closed.index == f("250"), "{closed:?}");
    let level_two = h.engine.params().expect("set").stress_levels[2].min_spread_bps;
    assert!(closed.spread_bps >= level_two, "{closed:?}");
    assert_eq!(h.engine.markets()[&AAPL].stress.level, 0);

    // The registry has drawn a turn that begins an hour on: the frontier names it before its boundary.
    let progress = Command::StakingProgress {
        block: 66_000_000,
        block_hash: alloy_primitives::B256::repeat_byte(66),
        block_time: Some(h.ts),
        named_turn: Some(Some(h.ts + HOUR_MS)),
    };
    assert_eq!(h.cmd(progress), []);
    assert_eq!(h.engine.governance().turns_from, Some(h.ts + HOUR_MS));
}
