//! A log's genesis as a fresh node sequences it under the latest rules.

use engine_core::auth::ClientAuth;
use engine_core::command::AdminCommand;
use engine_core::market::MarketSpec;
use engine_core::params::ProtocolParams;
use engine_core::protocol::ProtocolVersion;
use engine_core::session::Schedule;
use engine_core::types::{AssetClass, Millis};
use engine_core::volume_tier::launch_volume_tiers;

use crate::common::spreads::tenths;
use crate::common::toxicity::v54_params;
use crate::common::{
    AAPL, BTC, CHAIN_ID, CLOSE_SPREAD_CEILINGS, ETH, Harness, VAULT, f, market, operator, us_equities,
};

/// A log's genesis under `version` at `at`, as a fresh node sequences it before staking: client auth on `CHAIN_ID`
/// and `VAULT`, the operator, `version`, the parameters with the launch volume tiers, SOFR, and BTC, ETH and AAPL,
/// each with its close-spread ceiling as its minimum level distance.
pub fn node_genesis(version: ProtocolVersion, at: Millis) -> Harness {
    let mut h = Harness::empty();
    h.advance_to(at);
    h.admin(AdminCommand::SetClientAuth(ClientAuth::Eip712 { chain_id: CHAIN_ID, vault: VAULT }));
    h.admin(AdminCommand::SetOperator(operator()));
    h.admin(AdminCommand::ActivateProtocol { version });
    h.admin(AdminCommand::SetParams(ProtocolParams { volume_tiers: launch_volume_tiers(), ..v54_params() }));
    h.admin(AdminCommand::SetBenchmarkRate { benchmark: "SOFR".into(), annual_bps: 430 });
    for (id, symbol, class, schedule, margin) in [
        (BTC, "BTC-USD", AssetClass::Crypto, Schedule::AlwaysOpen, 1_000),
        (ETH, "ETH-USD", AssetClass::Crypto, Schedule::AlwaysOpen, 1_000),
        (AAPL, "AAPL", AssetClass::Equity, us_equities(), 2_000),
    ] {
        let ceiling = CLOSE_SPREAD_CEILINGS.iter().find(|(market, _)| *market == id).map(|(_, bps)| *bps);
        let spec = MarketSpec {
            spread_tiers: tenths(&[(Some(1_000_000), 10), (Some(5_000_000), 20), (None, 50)]),
            markout_floor_bps: Some(f("1")),
            min_level_distance_bps: ceiling,
            ..market(id, symbol, class, schedule, margin)
        };
        h.admin(AdminCommand::UpsertMarket(spec));
    }
    h
}
