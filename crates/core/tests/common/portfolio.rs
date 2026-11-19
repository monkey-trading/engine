//! Shared harness for `V81`'s portfolio margin close-outs: the weekend harness's log with the parameters
//! naming an offset grid from genesis. BTC and ETH trade around the clock; oil and gold share CME Globex
//! hours, oil with its weekend margin and gold with none.

use engine_core::command::AdminCommand;
use engine_core::operator_bounds::ParamsNotice;
use engine_core::params::ProtocolParams;
use engine_core::portfolio::{GroupMember, OffsetGroup, PortfolioGrid};
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{MarketId, Millis};
use engine_core::volume_tier::launch_volume_tiers;

use crate::common::toxicity::v54_params;
use crate::common::weekend::{GOLD, OIL, genesis_with_params, globex};
use crate::common::{BTC, ETH, Harness, f};

/// A member at `residual` bps, which a notice or a vote may raise to four times that.
pub fn member(market: MarketId, loading: &str, residual: i64) -> GroupMember {
    GroupMember { market, loading: f(loading), residual_shock_bps: residual, max_residual_shock_bps: residual * 4 }
}

/// A group of `members` at `floor_share_bps`, which may rise to all of it.
pub fn group(name: &str, floor_share_bps: i64, members: Vec<GroupMember>) -> OffsetGroup {
    OffsetGroup { name: name.into(), floor_share_bps, max_floor_share_bps: 10_000, members }
}

/// BTC with ETH, loading 1.25, and oil with gold, loading 0.9, at 0.2% each; every floor share at
/// `floor_share_bps`.
pub fn pairs(floor_share_bps: i64) -> PortfolioGrid {
    PortfolioGrid {
        groups: vec![
            group("crypto", floor_share_bps, vec![member(BTC, "1", 20), member(ETH, "1.25", 20)]),
            group("commodities", floor_share_bps, vec![member(OIL, "1", 20), member(GOLD, "0.9", 20)]),
        ],
    }
}

/// The parameters the harness genesis sets, at the launch volume tier table, naming `grid`.
pub fn params_with(grid: PortfolioGrid) -> ProtocolParams {
    ProtocolParams { volume_tiers: launch_volume_tiers(), portfolio_grid: grid, ..v54_params() }
}

/// A log that activates `version` before a genesis whose parameters name `grid`, with oil on Globex hours
/// at `oil_weekend_bps` and gold at `gold_weekend_bps`.
pub fn genesis_with_grid(
    version: ProtocolVersion,
    grid: PortfolioGrid,
    oil_weekend_bps: i64,
    gold_weekend_bps: i64,
) -> Harness {
    genesis_with_params(version, globex(vec![]), oil_weekend_bps, gold_weekend_bps, params_with(grid))
}

/// The parameters in force with `grid`.
pub fn with_grid(h: &Harness, grid: PortfolioGrid) -> ProtocolParams {
    ProtocolParams { portfolio_grid: grid, ..h.engine.params().expect("genesis sets parameters").clone() }
}

/// A notice naming `grid` from `effective_at`.
pub fn grid_notice(grid: PortfolioGrid, effective_at: Millis) -> AdminCommand {
    AdminCommand::AnnounceParams(ParamsNotice {
        profit_fee_bps: None,
        closeout_level_bps: None,
        stress: None,
        score_tiers: None,
        profit_fee_curve: None,
        volume_tiers: None,
        rollover: None,
        corporate_bounds: None,
        portfolio_grid: Some(grid),
        effective_at,
    })
}

/// The grid in force.
pub fn grid_in_force(h: &Harness) -> PortfolioGrid {
    h.engine.params().expect("genesis sets parameters").portfolio_grid.clone()
}
