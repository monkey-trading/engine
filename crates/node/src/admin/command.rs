//! The admin command each `engine admin` action sends, built from its arguments and the TOML files
//! they name, checked as the node would check them before anything is signed.

use std::path::Path;

use anyhow::{Context, bail};
use engine_core::command::AdminCommand;
use engine_core::corporate::CorporateAction;
use engine_core::emissions::EmissionSchedule;
use engine_core::listing_bounds::ListingNotice;
use engine_core::market::MarketSpec;
use engine_core::operator_bounds::{ParamsNotice, StressSettings};
use engine_core::params::ProtocolParams;
use engine_core::protocol::ProtocolVersion;
use engine_core::spread_tier::check_spread_tiers;

use crate::admin::{AdminAction, input};

impl AdminAction {
    /// The admin command this action sends.
    /// Error cases:
    /// - a spec or parameters file cannot be read, does not parse, or fails validation
    pub(super) fn command(self) -> anyhow::Result<AdminCommand> {
        Ok(match self {
            AdminAction::ScheduleCorporateAction { file } => {
                let action: CorporateAction = input::read_toml(&file)?;
                action.validate().map_err(anyhow::Error::msg)?;
                AdminCommand::ScheduleCorporateAction(action)
            }
            AdminAction::UpsertMarket { file } => AdminCommand::UpsertMarket(read_spec(&file)?),
            AdminAction::SetParams { file } => AdminCommand::SetParams(read_params(&file)?),
            AdminAction::SetBenchmarkRate { benchmark, annual_bps } => {
                AdminCommand::SetBenchmarkRate { benchmark, annual_bps }
            }
            AdminAction::SyncBenchmarks { .. } => bail!("publisher sync is asynchronous"),
            AdminAction::DeliverSecrets(_) | AdminAction::SecretsStatus(_) => bail!("secrets take no admin command"),
            AdminAction::SetStressFloor { level, market } => AdminCommand::SetStressFloor { market, level },
            AdminAction::SetReduceOnly { reduce_only, market } => AdminCommand::SetReduceOnly { market, reduce_only },
            AdminAction::AnnounceParams {
                profit_fee_bps,
                closeout_level_bps,
                stress_from,
                score_tiers_from,
                effective_at,
            } => {
                let stress = stress_from.map(|file| read_params(&file).map(|params| StressSettings::of(&params)));
                let tiers = score_tiers_from.map(|file| read_params(&file).map(|params| params.toxicity.score_tiers));
                AdminCommand::AnnounceParams(ParamsNotice {
                    profit_fee_bps,
                    closeout_level_bps,
                    stress: stress.transpose()?,
                    score_tiers: tiers.transpose()?,
                    effective_at: effective_at.timestamp_millis(),
                })
            }
            AdminAction::AnnounceMarkup { market, markup_bps, effective_at } => {
                AdminCommand::AnnounceMarkup { market, markup_bps, effective_at: effective_at.timestamp_millis() }
            }
            AdminAction::AnnounceListing { market, spreads_from, margins_from, markup_bps, effective_at } => {
                let notice = ListingNotice {
                    spread_tiers: spreads_from
                        .map(|file| read_spec(&file).map(|spec| spec.spread_tiers))
                        .transpose()?,
                    margin_tiers: margins_from
                        .map(|file| read_spec(&file).map(|spec| spec.margin_tiers))
                        .transpose()?,
                    markup_bps,
                    effective_at: effective_at.timestamp_millis(),
                };
                AdminCommand::AnnounceListing { market, notice }
            }
            AdminAction::RotateOperator { address } => AdminCommand::RotateOperator { address },
            AdminAction::InitEmissions { start, period_budget } => {
                AdminCommand::InitEmissions(EmissionSchedule { start_ms: start.timestamp_millis(), period_budget })
            }
            AdminAction::InitStaking { start, liquidity_floor, epoch_ms, cooldown_ms } => {
                AdminCommand::InitStaking(engine_core::staking::StakingConfig {
                    liquidity_floor,
                    epoch_ms,
                    start_ms: start.timestamp_millis(),
                    cooldown_ms,
                })
            }
            AdminAction::AdoptCloseSpreadCeiling { market, max_close_spread_bps, effective_at, listing } => {
                AdminCommand::AdoptCloseSpreadCeiling {
                    market,
                    max_close_spread_bps,
                    effective_at: effective_at.timestamp_millis(),
                    listing: listing.map(|file| read_spec(&file)).transpose()?,
                }
            }
        })
    }
}

/// The market spec in `file`, checked as the node, which runs the latest version, would list it.
/// Error cases:
/// - a file that cannot be read or parsed
/// - a spec that fails `MarketSpec::validate` or `check_spread_tiers`
fn read_spec(file: &Path) -> anyhow::Result<MarketSpec> {
    let spec: MarketSpec = input::read_toml(file)?;
    spec.validate()
        .and_then(|()| check_spread_tiers(&spec.spread_tiers, spec.asset_class, ProtocolVersion::LATEST))
        .map_err(anyhow::Error::msg)
        .with_context(|| file.display().to_string())?;
    Ok(spec)
}

/// The parameters in `file`, checked as the node, which runs the latest version, would apply them.
/// Error cases:
/// - a file that cannot be read or parsed
/// - parameters that fail `validate_for`, `check_closing_spread` or `check_notional_weighting`
fn read_params(file: &Path) -> anyhow::Result<ProtocolParams> {
    let params: ProtocolParams = input::read_toml(file)?;
    params
        .validate_for(ProtocolVersion::LATEST)
        .and_then(|()| params.check_closing_spread())
        .and_then(|()| params.check_notional_weighting())
        .map_err(anyhow::Error::msg)
        .with_context(|| file.display().to_string())?;
    Ok(params)
}
