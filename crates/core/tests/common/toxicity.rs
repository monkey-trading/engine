//! Toxicity score test harness: the access-screened `V54` setup, with BTC-USD and ETH-USD relisted
//! with a markout floor, and helpers that trade and read an account's score.

use engine_core::auth::ClientAuth;
use engine_core::command::AdminCommand;
use engine_core::event::Fill;
use engine_core::market::MarketSpec;
use engine_core::params::ProtocolParams;
use engine_core::protocol::ProtocolVersion;
use engine_core::session::Schedule;
use engine_core::toxicity::{HORIZON_MS, ToxicityStanding, launch_score_tiers};
use engine_core::types::{AccountId, AssetClass, MarketId, Side};

use crate::common::batch::onboard;
use crate::common::close_spread::{share_params, trade};
use crate::common::events::only_fill;
use crate::common::session_keys::funded_at;
use crate::common::{BTC, Client, ETH, Harness, f, market, operator};

/// The markout floor the harness lists BTC-USD and ETH-USD with: 1 bp.
pub const FLOOR_BPS: &str = "1";
/// The spread a fill in a harness crypto market's first band pays at the calm stress level from `V52`:
/// the crypto floor of its first band, 1 bp, at 1x. A fill's `spread_bps` is this and its add-on.
pub const BAND_BPS: i64 = 1;
/// Half the sampling grid: a price stamped here leaves the next grid point unpriced, since the
/// harness's quotes are fresh for a second, so no change is counted and the spread is the floor.
pub const OFF_GRID_MS: i64 = 2_500;

/// `V54` activated over the access-screened harness, with client 1 declared and holding 100000,
/// BTC-USD and ETH-USD relisted with `FLOOR_BPS`, and BTC priced at 100000 off the sampling grid.
pub fn scored() -> (Harness, Client) {
    let (mut h, client, _) = funded_at(ProtocolVersion::V54);
    for (id, symbol) in [(BTC, "BTC-USD"), (ETH, "ETH-USD")] {
        h.operator_admin(AdminCommand::UpsertMarket(floored(crypto(id, symbol), FLOOR_BPS)));
    }
    h.advance(OFF_GRID_MS);
    h.price(BTC, "100000");
    (h, client)
}

/// `funded_at(V54)`, with `listings` relisted before the activation, so each keeps the floor and
/// bands it names in place of what the activation gives, and its client.
pub fn relisted_v54<const N: usize>(listings: [MarketSpec; N]) -> (Harness, Client) {
    let (mut h, client, _) = funded_at(ProtocolVersion::V51);
    for spec in listings {
        h.operator_admin(AdminCommand::UpsertMarket(spec));
    }
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V54 });
    (h, client)
}

/// A log that activates `V54` before its genesis, as a node's fresh log does: `fresh_at` with
/// `v54_params`, and BTC-USD listed with `FLOOR_BPS`; no ceilings installed yet.
pub fn fresh_v54() -> Harness {
    let mut h = fresh_at(ProtocolVersion::V54, v54_params());
    h.admin(AdminCommand::UpsertMarket(floored(crypto(BTC, "BTC-USD"), FLOOR_BPS)));
    h
}

/// A log that activates `version` with unsigned requests and the harness operator, then sets
/// `params`; nothing listed.
pub fn fresh_at(version: ProtocolVersion, params: ProtocolParams) -> Harness {
    let mut h = Harness::empty();
    h.admin(AdminCommand::SetClientAuth(ClientAuth::Unsigned));
    h.admin(AdminCommand::SetOperator(operator()));
    h.admin(AdminCommand::ActivateProtocol { version });
    h.admin(AdminCommand::SetParams(params));
    h
}

/// `common::params` in the form `V54` takes: divergence triggers as shares, no earlier tiers, the
/// protocol's horizon, and the launch score tiers.
pub fn v54_params() -> ProtocolParams {
    let mut params = share_params();
    params.toxicity.tiers.clear();
    params.toxicity.horizon_ms = HORIZON_MS;
    params.toxicity.score_tiers = launch_score_tiers();
    params
}

/// A 24/7 crypto listing as the harness's genesis lists it, 10% margin, with its spread bands in the
/// tenths of a bp `V52` writes them in.
pub fn crypto(id: MarketId, symbol: &str) -> MarketSpec {
    let mut spec = market(id, symbol, AssetClass::Crypto, Schedule::AlwaysOpen, 1_000);
    spec.convert_spread_tiers();
    spec
}

/// `spec` with a markout floor of `bps`.
pub fn floored(spec: MarketSpec, bps: &str) -> MarketSpec {
    MarketSpec { markout_floor_bps: Some(f(bps)), ..spec }
}

/// A second client, from `seed`, declared and holding `amount`.
pub fn another(h: &mut Harness, seed: u8, amount: &str) -> Client {
    let client = Client::new(seed);
    onboard(h, &client, amount);
    client
}

/// The client's screened market order in BTC, which must fill.
pub fn fill(h: &mut Harness, client: &Client, side: Side, qty: &str, quoted: &str) -> Fill {
    only_fill(&trade(h, client, BTC, side, qty, quoted)).clone()
}

/// A horizon on, BTC priced at `mid`: every markout from a horizon before falls due and scores.
pub fn mark(h: &mut Harness, mid: &str) {
    h.advance(HORIZON_MS);
    h.price(BTC, mid);
}

/// The account's score as of the sequenced time.
pub fn standing(h: &Harness, account: AccountId) -> ToxicityStanding {
    h.engine.toxicity_standing(account).expect("V54 scores every account")
}
