//! The session's `V87` part: a futures market's contracts and roll, while votes govern. A vote activates the
//! version, and another, passed beside it, relists US 500 with June's roll a day later, a change to the contracts
//! from the front on, which applies with no listing notice, a vote's seven days being its notice. US 500's front,
//! March, has not quoted since the `V21` part, and no gap to June was recorded. Seven days after the engine first
//! sees March without a price, June quotes and the market rolls onto it from its mark, so its index takes up where
//! it stopped. The operator's own relistings meet the notice only while votes do not govern, which
//! `tests/futures_contract_bounds.rs` and `tests/price_rule_bounds.rs` cover.

use chrono::Duration;
use engine_core::command::{AdminCommand, Command};
use engine_core::event::Event;
use engine_core::futures::MARK_ROLL_WAIT_MS;
use engine_core::operator_bounds::NOTICE_MS;
use engine_core::price::SourceQuote;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{MarketId, Millis};

use crate::common::{Harness, quote};
use crate::v81::{applied, mark, scheduled};

/// US 500, which the `V21` part lists on E-mini futures.
const US500: MarketId = 30;
/// The first of this part's proposal ids.
const PROPOSAL: u64 = 290;

/// Activates `V87` by vote, with the voted relisting beside it; then the roll above.
pub(crate) fn session(h: &mut Harness) {
    let activation = AdminCommand::ActivateProtocol { version: ProtocolVersion::V87 };
    scheduled(h, PROPOSAL, &activation);
    let futures = h.engine.markets()[&US500].futures.clone().expect("US 500 prices from futures");
    assert_eq!((futures.contract.as_str(), futures.gap), ("ESH7", None), "{futures:?}");
    let mut spec = h.engine.baseline_spec(US500).expect("US 500 is listed");
    let june = spec.contracts.last_mut().expect("US 500 lists contracts");
    assert_eq!(june.contract, "ESM7");
    june.from += Duration::days(1);
    let relisting = AdminCommand::UpsertMarket(spec.clone());
    scheduled(h, PROPOSAL + 1, &relisting);
    h.advance(NOTICE_MS);
    applied(h, PROPOSAL, activation);
    assert_eq!(h.engine.protocol_version(), ProtocolVersion::V87);
    // An activation leaves the rollovers its notice spanned to the next command.
    h.cmd(Command::Tick);
    applied(h, PROPOSAL + 1, relisting);
    assert_eq!(h.engine.markets()[&US500].spec.contracts, spec.contracts);

    // March stopped quoting with no gap to June. Seven days after the engine first sees it without a price, June
    // quotes, and the market rolls onto it from its mark.
    let futures = h.engine.markets()[&US500].futures.clone().expect("US 500 prices from futures");
    let since = futures.front_unpriced_since.expect("March has no price");
    h.advance_to(since + MARK_ROLL_WAIT_MS);
    h.cmd(Command::Tick);
    let last = mark(h, US500);
    assert!(h.engine.markets()[&US500].index(h.ts).is_err(), "US 500 has no price");
    let events = h.cmd(june_prices(h.ts, &["6100", "6101"]));
    assert!(events.iter().any(|e| matches!(e, Event::MarketRolled { market: US500, .. })), "{events:?}");
    assert_eq!(h.engine.markets()[&US500].index(h.ts).map(|index| index.mid), Ok(last));
}

/// One venue per mid quoting June for US 500, stamped `ts`.
fn june_prices(ts: Millis, mids: &[&str]) -> Command {
    let quotes = mids
        .iter()
        .enumerate()
        .map(|(i, mid)| SourceQuote { contract: Some("ESM7".into()), ..quote(&format!("venue{i}"), mid, ts) })
        .collect();
    Command::Prices { market: US500, quotes }
}
