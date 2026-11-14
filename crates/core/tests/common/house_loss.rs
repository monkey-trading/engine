//! Shared harness for the `V59` house loss trigger tests: a third stress level for the trigger to
//! raise markets to, and readings of the loss and of each market's level.

use engine_core::Fixed;
use engine_core::access::AccessPolicy;
use engine_core::auth::ClientAuth;
use engine_core::command::AdminCommand;
use engine_core::market::MarketSpec;
use engine_core::params::StressLevel;
use engine_core::protocol::ProtocolVersion;
use engine_core::session::Schedule;
use engine_core::types::{AssetClass, MarketId};

use crate::common::close_spread::{attest, ceilings, fund, install_expect, source_key, widen_slippage};
use crate::common::session_keys::HOUR_MS;
use crate::common::{BTC, CHAIN_ID, Client, ETH, Harness, VAULT, market, params};

/// The level the trigger raises markets to: a 6 bp minimum and four times the size spread, as wide as
/// the harness's AAPL ceiling of 15 bps admits with the top toxicity add-on.
pub fn level_two() -> StressLevel {
    StressLevel { size_spread_multiplier_bps: 40_000, min_spread_bps: 6, trigger: None }
}

/// The access-screened harness with `level_two` added to the harness parameters under `V8`, wide
/// slippage bounds, and BTC listed at `btc_margin_bps`; then `V59`, whose activation gives level 2 a
/// loss threshold of zero. BTC is priced at 100000 and ETH at 3000, and client 1 is declared and holds
/// `amount`.
pub fn harness(btc_margin_bps: i64, amount: &str) -> (Harness, Client) {
    let mut h = Harness::v8(ClientAuth::Eip712 { chain_id: CHAIN_ID, vault: VAULT });
    install_expect(&mut h, ceilings());
    h.admin(AdminCommand::SetAccessPolicy(AccessPolicy { source: source_key(), max_age_ms: 10 * HOUR_MS }));
    let mut parameters = params();
    parameters.stress_levels.push(level_two());
    h.operator_admin(AdminCommand::SetParams(parameters));
    widen_slippage(&mut h);
    let btc = market(BTC, "BTC-USD", AssetClass::Crypto, Schedule::AlwaysOpen, btc_margin_bps);
    h.operator_admin(AdminCommand::UpsertMarket(MarketSpec { max_slippage_bps: 10_000, ..btc }));
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V59 });
    let client = Client::new(1);
    attest(&mut h, &client);
    fund(&mut h, &client, amount);
    reprice(&mut h, "100000");
    (h, client)
}

/// Prices BTC at `btc` and ETH at 3000, so both markets are re-read.
pub fn reprice(h: &mut Harness, btc: &str) {
    h.price(BTC, btc);
    h.price(ETH, "3000");
}

/// A second client, declared and holding `amount`.
pub fn second_client(h: &mut Harness, seed: u8, amount: &str) -> Client {
    let client = Client::new(seed);
    attest(h, &client);
    fund(h, &client, amount);
    client
}

/// The house's marked loss over the trailing day.
pub fn loss(h: &Harness) -> Fixed {
    h.engine.house_loss().expect("V59 is active").loss
}

/// `market`'s stress level.
pub fn level(h: &Harness, market: MarketId) -> usize {
    h.engine.markets()[&market].stress.level
}
