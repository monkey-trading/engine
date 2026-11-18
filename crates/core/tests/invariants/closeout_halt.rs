//! The close-out index halts where the full check halts: an account whose equity a price takes past
//! `Fixed`'s range stops the engine at that price's command under the index as under today's full
//! check, and the command before it applies alike under both.

use std::panic::{AssertUnwindSafe, catch_unwind};

use engine_core::access::AccessPolicy;
use engine_core::command::{AdminCommand, Sequenced};
use engine_core::event::Event;
use engine_core::types::Side;
use engine_core::{CloseoutOracle, Engine, Fixed};
use serde_json::json;

use crate::common::close_spread::{attest, fund, source_key, trade};
use crate::common::launch_book::{price_command, production_harness};
use crate::common::session_keys::HOUR_MS;
use crate::common::{BTC, Client, Harness};

#[test]
fn an_account_past_the_range_halts_both_checks_at_the_same_command() {
    let mut h = production_harness();
    h.admin(AdminCommand::SetAccessPolicy(AccessPolicy { source: source_key(), max_age_ms: 10 * HOUR_MS }));
    let cmd = price_command(&h, BTC, "50000", h.ts);
    h.cmd(cmd);
    let client = Client::new(0x61);
    attest(&mut h, &client);
    fund(&mut h, &client, "20000");
    let events = trade(&mut h, &client, BTC, Side::Buy, "0.2", "50000");
    assert!(events.iter().any(|e| matches!(e, Event::Fill(_))), "{events:?}");
    // The account's cash a thousand units short of `Fixed`'s range, which a gain of a dollar passes.
    let mut state = serde_json::to_value(&h.engine).unwrap();
    let account = serde_json::to_value(client.account).unwrap();
    let cash = Fixed::from_raw(i128::MAX - 1_000);
    state["accounts"][account.as_str().unwrap()]["balance"] = json!(cash);
    let text = state.to_string();
    let engine = |full_check: bool| {
        let mut engine: Engine = serde_json::from_str(&text).unwrap();
        engine.restore_indexes();
        engine.set_closeout_oracle(CloseoutOracle { full_check, ..CloseoutOracle::default() });
        engine
    };
    let (mut indexed, mut full) = (engine(false), engine(true));
    // The same price: the position's loss keeps its equity in range.
    h.advance(100);
    let same = price(&h, "50000", 0);
    let (first, second) = (apply(&mut indexed, &same), apply(&mut full, &same));
    assert!(first.is_some(), "the index's check halted on a price that leaves equity in range");
    assert_eq!(first, second, "the two checks differ on a price that leaves equity in range");
    // A rise of a hundred dollars: the full check's equity leaves the range, and so the index's must.
    h.advance(100);
    let rise = price(&h, "50100", 1);
    assert!(apply(&mut full, &rise).is_none(), "the full check did not halt");
    assert!(apply(&mut indexed, &rise).is_none(), "the index's check did not halt where the full check does");
}

/// The command quoting BTC-USD at `mid` now, `step` commands past the harness's log.
fn price(h: &Harness, mid: &str, step: u64) -> Sequenced {
    Sequenced { seq: h.log.len() as u64 + step, ts: h.ts, cmd: price_command(h, BTC, mid, h.ts) }
}

/// `sequenced`'s events on `engine`, or `None` when the engine halted on it.
fn apply(engine: &mut Engine, sequenced: &Sequenced) -> Option<Vec<Event>> {
    catch_unwind(AssertUnwindSafe(|| engine.apply(sequenced).expect("in sequence"))).ok()
}
