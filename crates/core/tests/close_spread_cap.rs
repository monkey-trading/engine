//! Close-spread ceilings under `V39`: the registry's adoption, the governance path, and the
//! listing guards. Ceilings are unequal test values at or above the harness launch close charge
//! (8bps: a 5bps stress minimum plus the 3bps top toxicity add-on); the pricing-side behaviour —
//! the clamp, the reversal refusal, and the triggered order's closing slice — is in
//! `close_spread_pricing.rs`.

use engine_core::auth::ClientAuth;
use engine_core::close_spread::MAX_CLOSE_SPREAD_BPS;
use engine_core::command::{AdminCommand, Command};
use engine_core::event::Event;
use engine_core::market::QuoteCurrency;
use engine_core::protocol::ProtocolVersion;
use engine_core::session::Schedule;
use engine_core::types::AssetClass;
use engine_core::Fixed;

mod common;
use common::close_spread::*;
use common::events::only_fill;
use common::{ETH, acct, market};

//! Close-spread ceilings under `V39`: the registry's adoption, the governance path, and the
//! listing guards. Ceilings are unequal test values at or above the harness launch close charge
//! (8bps: a 5bps stress minimum plus the 3bps top toxicity add-on); the pricing-side behaviour —
//! the clamp, the reversal refusal, and the triggered order's closing slice — is in
//! `close_spread_pricing.rs`.

use engine_core::auth::ClientAuth;
use engine_core::close_spread::MAX_CLOSE_SPREAD_BPS;
use engine_core::command::{AdminCommand, Command};
use engine_core::event::Event;
use engine_core::protocol::ProtocolVersion;
use engine_core::Fixed;

mod common;
use common::close_spread::*;

#[test]
fn install_adopts_every_listed_market_once_and_refuses_a_flawed_registry() {
    let mut h = Harness::v8(ClientAuth::Unsigned);
    let ok = ceilings();
    // A listed market with no entry, an entry for an unlisted market, a duplicate, an out-of-range
    // value, and a nonzero value below the launch close charge: each refuses, the registry stays
    // uninstalled.
    let cases: Vec<(Vec<CloseSpreadCeiling>, &str)> = vec![
        (ok[..ok.len() - 1].to_vec(), "listed without"),
        (with(&ok, 77, 20), "not listed"),
        (duplicating(&ok, ETH, 25), "twice"),
        (with(&ok, BTC, MAX_CLOSE_SPREAD_BPS + 1), "0..=9999"),
        (with(&ok, BTC, 7), "largest close charge"),
    ];
    for (ceilings, why) in cases {
        let events = install(&mut h, ceilings);
        assert!(rejected(&events).contains(why), "{why}: {events:?}");
        assert!(h.engine.close_spread_ceilings().is_none());
    }
    // Zero is a legal ceiling: free closes.
    install_expect(&mut h, with(&ok, BTC, 0));
    assert_eq!(h.engine.close_spread_ceilings().unwrap()[&BTC], 0);
    // A log records the registry once.
    let events = install(&mut h, ok);
    assert!(rejected(&events).contains("already installed"));
}

