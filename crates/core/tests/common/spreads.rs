//! Shared harness for the `V52` spread tests: every fill pays its size spread in tenths of a bp, and
//! listings move their bands only after notice.

use engine_core::Fixed;
use engine_core::auth::ClientAuth;
use engine_core::command::AdminCommand;
use engine_core::event::Event;
use engine_core::listing_bounds::ListingNotice;
use engine_core::market::MarketSpec;
use engine_core::protocol::ProtocolVersion;
use engine_core::session::Schedule;
use engine_core::spread_tier::SpreadTier;
use engine_core::types::{AssetClass, MarketId, Millis};

use crate::common::close_spread::{NOTICE_MS, fund, share_params};
use crate::common::session_keys::funded_at;
use crate::common::{AAPL, BTC, CHAIN_ID, Client, ETH, Harness, VAULT, market, operator, us_equities};

/// The access-screened harness at `version`, with client 1 declared and holding `amount` and BTC
/// priced at 100000. From `V52` the genesis markets' bands read 1bp to 1m, 2bp to 5m and 5bp above,
/// the first raised from 0 to the crypto and equity floor at activation.
pub fn funded(version: ProtocolVersion, amount: &str) -> (Harness, Client) {
    let (mut h, client, _) = funded_at(version);
    fund(&mut h, &client, amount);
    (h, client)
}

/// Spread bands in tenths of a bp, as `(up_to, rate)` with whole-unit bounds.
pub fn tenths(bands: &[(Option<i64>, i64)]) -> Vec<SpreadTier> {
    bands.iter().map(|&(up_to, rate)| SpreadTier::tenths(up_to.map(Fixed::from_int), rate)).collect()
}

/// `market`'s listing as it stands, with `spread_tiers` in place of its bands.
pub fn with_spreads(h: &Harness, market: MarketId, spread_tiers: Vec<SpreadTier>) -> MarketSpec {
    MarketSpec { spread_tiers, ..h.engine.markets()[&market].spec.clone() }
}

/// A listing notice of `market`'s new spread bands, from seven days on.
pub fn spread_notice(h: &Harness, market: MarketId, spread_tiers: Vec<SpreadTier>) -> (AdminCommand, Millis) {
    let effective_at = h.ts + NOTICE_MS;
    let notice = ListingNotice { spread_tiers: Some(spread_tiers), margin_tiers: None, markup_bps: None, effective_at };
    (AdminCommand::AnnounceListing { market, notice }, effective_at)
}

/// The reason the last event gives for refusing an admin command.
pub fn refused(events: &[Event]) -> &str {
    match events.last() {
        Some(Event::AdminRejected { reason }) => reason,
        other => panic!("expected an admin rejection, got {other:?}"),
    }
}

/// Whether the admin command applied.
pub fn applied(events: &[Event]) -> bool {
    events.last() == Some(&Event::AdminApplied)
}

/// BTC's spread bands as the harness lists them from `V52`.
pub fn btc_bands(h: &Harness) -> Vec<SpreadTier> {
    h.engine.markets()[&BTC].spec.spread_tiers.clone()
}

/// A log that activates `V52` before its genesis, as a fresh node's does: client auth, the operator,
/// `V52`, the harness parameters with divergence shares, and BTC, ETH and AAPL at 1bp to 1m, 2bp to 5m and 5bp above. No
/// ceiling registry yet.
pub fn genesis_v52() -> Harness {
    genesis_at(ProtocolVersion::V52)
}

/// `genesis_v52` activating `version` before the genesis instead, from `V54` with the parameters and
/// listings in the form it takes.
pub fn genesis_at(version: ProtocolVersion) -> Harness {
    let mut h = Harness::empty();
    h.admin(AdminCommand::SetClientAuth(ClientAuth::Eip712 { chain_id: CHAIN_ID, vault: VAULT }));
    h.admin(AdminCommand::SetOperator(operator()));
    h.admin(AdminCommand::ActivateProtocol { version });
    let mut params = share_params();
    if version >= ProtocolVersion::V54 {
        params.convert_to_score();
    }
    h.admin(AdminCommand::SetParams(params));
    for (id, symbol, class, schedule, margin) in [
        (BTC, "BTC-USD", AssetClass::Crypto, Schedule::AlwaysOpen, 1_000),
        (ETH, "ETH-USD", AssetClass::Crypto, Schedule::AlwaysOpen, 1_000),
        (AAPL, "AAPL", AssetClass::Equity, us_equities(), 2_000),
    ] {
        let spread_tiers = tenths(&[(Some(1_000_000), 10), (Some(5_000_000), 20), (None, 50)]);
        let mut spec = MarketSpec { spread_tiers, ..market(id, symbol, class, schedule, margin) };
        if version >= ProtocolVersion::V54 {
            spec.convert_to_floored();
        }
        h.admin(AdminCommand::UpsertMarket(spec));
    }
    h
}
