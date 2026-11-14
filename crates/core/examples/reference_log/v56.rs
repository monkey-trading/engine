//! The session's `V56` part: parameter changes by stake-weighted operator vote. The node records the
//! vote contract; a passed normal margin raise waits its seven days and applies, a fast-path stress
//! floor and close-out level apply at once and lapse at their lifetime, a fast-path margin cut is
//! refused, the turn's key's governed commands are refused while rotation is on, a voted relisting
//! is held to the listing bounds without a listing notice, and voted score tiers and fee curve to
//! the toxicity and holding-fee bounds without a parameter notice.

use engine_core::Fixed;
use engine_core::command::{AdminCommand, Command};
use engine_core::event::Event;
use engine_core::governance::{GovernanceConfig, PassedProposal, VotedChange, command_hash};
use engine_core::market::{MarketSpec, Tier};
use engine_core::operator_bounds::NOTICE_MS;
use engine_core::protocol::ProtocolVersion;
use engine_core::spread_tier::SpreadTier;
use engine_core::types::{AccountId, Millis};

use crate::common::{BTC, ETH, Harness};

/// The seed of the turn's key `V55`'s part leaves in effect.
const TURN_SEED: u8 = 0x5c;
/// A restriction's lifetime: a day.
const TTL_MS: Millis = 24 * 60 * 60 * 1000;

/// `V56` activates with rotation on and a turn in effect, and records the vote contract. A margin
/// raise passes and applies seven days after it is logged; a fast-path stress floor and close-out
/// level apply at once and lapse a day later, the turn's key clearing neither; a fast-path margin
/// cut and a proposal whose bytes are no command are refused; the turn's key's own SetParams is
/// refused. BTC's spread bands at twice their launch rates apply by a normal vote with no listing
/// notice, while three times them are refused at application, and score tiers at one and a half times
/// their launch scores, with the fee curve's second point raised 50 bps, apply by a normal vote with
/// no parameter notice.
pub(crate) fn session(h: &mut Harness) {
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V56 });
    h.admin(AdminCommand::SetGovernance(GovernanceConfig { vote: AccountId([0x52; 20]), restriction_ttl_ms: TTL_MS }));

    let raise = margin(h, ETH, 1_500);
    let logged = h.ts;
    let events = pass(h, 1, false, Some(raise.clone()));
    assert!(matches!(events.last(), Some(Event::ProposalScheduled { proposal: 1, .. })), "{events:?}");

    let floor = AdminCommand::SetStressFloor { market: Some(BTC), level: 2 };
    let events = pass(h, 2, true, Some(floor));
    assert!(events.iter().any(|event| matches!(event, Event::RestrictionApplied { proposal: 2, .. })), "{events:?}");
    let mut closeout = h.engine.baseline_params().expect("set");
    closeout.closeout_level_bps += 500;
    let events = pass(h, 3, true, Some(AdminCommand::SetParams(closeout.clone())));
    assert!(events.iter().any(|event| matches!(event, Event::RestrictionApplied { proposal: 3, .. })), "{events:?}");
    let cut = margin(h, BTC, 1);
    let events = pass(h, 4, true, Some(cut));
    assert!(matches!(events.last(), Some(Event::ProposalRefused { proposal: 4, .. })), "{events:?}");
    let events = pass(h, 5, false, None);
    assert!(matches!(events.last(), Some(Event::ProposalRefused { proposal: 5, .. })), "{events:?}");
    let (wider, too_wide, spreads_logged) = (spreads(h, BTC, 2), spreads(h, BTC, 3), h.ts);
    let mut tiers = h.engine.baseline_params().expect("set");
    let launch = h.engine.operator_bounds().launch_score_tiers.clone().expect("recorded at V54");
    for (tier, launched) in tiers.toxicity.score_tiers.iter_mut().zip(&launch) {
        tier.min_score = Fixed::from_raw(launched.min_score.raw() * 3 / 2);
    }
    // From `V55` the same vote raises the fee curve's second point, at or above its launch curve.
    if let [first, second, ..] = &mut tiers.profit_fee_curve[..]
        && second.fee_bps + 50 <= first.fee_bps
    {
        second.fee_bps += 50;
    }
    let tiers = AdminCommand::SetParams(tiers);
    for (proposal, command) in [(6, wider.clone()), (7, too_wide.clone()), (8, tiers.clone())] {
        let events = pass(h, proposal, false, Some(command));
        assert!(matches!(events.last(), Some(Event::ProposalScheduled { .. })), "{events:?}");
    }

    let lever = h.operate_as(TURN_SEED, AdminCommand::SetStressFloor { market: Some(BTC), level: 0 });
    assert_eq!(lever.last(), Some(&Event::AdminApplied));
    assert_eq!(h.engine.markets()[&BTC].stress.level, 2);
    let mut fee = h.engine.baseline_params().expect("set");
    fee.profit_fee_bps += 100;
    let refused = h.operate_as(TURN_SEED, AdminCommand::SetParams(fee));
    assert!(matches!(refused.last(), Some(Event::AdminRejected { .. })), "{refused:?}");

    h.advance(TTL_MS);
    let events = h.cmd(Command::Tick);
    assert!(events.contains(&Event::RestrictionLapsed { proposal: 2 }), "{events:?}");
    assert!(events.contains(&Event::RestrictionLapsed { proposal: 3 }), "{events:?}");

    h.advance_to(logged + NOTICE_MS);
    let applied = h.cmd(Command::ApplyProposal(Box::new(VotedChange { proposal: 1, command: raise })));
    assert_eq!(applied.last(), Some(&Event::AdminApplied));
    assert_eq!(h.engine.markets()[&ETH].spec.margin_tiers[0].bps, 1_500);

    h.advance_to(spreads_logged + NOTICE_MS);
    let refused = h.cmd(Command::ApplyProposal(Box::new(VotedChange { proposal: 7, command: too_wide })));
    assert!(matches!(refused.last(), Some(Event::AdminRejected { .. })), "{refused:?}");
    let applied = h.cmd(Command::ApplyProposal(Box::new(VotedChange { proposal: 6, command: wider })));
    assert_eq!(applied.last(), Some(&Event::AdminApplied));
    let applied = h.cmd(Command::ApplyProposal(Box::new(VotedChange { proposal: 8, command: tiers })));
    assert_eq!(applied.last(), Some(&Event::AdminApplied));
}

/// The baseline spec of `market` with every margin band at `bps`.
fn margin(h: &Harness, market: u32, bps: i64) -> AdminCommand {
    let spec = h.engine.baseline_spec(market).expect("listed");
    let margin_tiers = spec.margin_tiers.iter().map(|tier| Tier { bps, ..*tier }).collect();
    AdminCommand::UpsertMarket(MarketSpec { margin_tiers, ..spec })
}

/// The baseline listing of `market` with each launch spread band's rate times `factor`.
fn spreads(h: &Harness, market: u32, factor: i64) -> AdminCommand {
    let launch = &h.engine.operator_bounds().launch_bands[&market].spread_tiers;
    let spread_tiers = launch.iter().map(|band| SpreadTier::tenths(band.up_to, band.tenth_bps() * factor)).collect();
    AdminCommand::UpsertMarket(MarketSpec { spread_tiers, ..h.engine.baseline_spec(market).expect("listed") })
}

/// Mirrors proposal `proposal` passing on `fast` with `command`, or with bytes that are no command.
pub(crate) fn pass(h: &mut Harness, proposal: u64, fast: bool, command: Option<AdminCommand>) -> Vec<Event> {
    let command_hash = command.as_ref().map(command_hash).unwrap_or_default();
    let (block, chain_ref) = (52_000_000 + proposal, format!("0xvote:52:{proposal}"));
    h.cmd(Command::PassProposal(Box::new(PassedProposal { proposal, fast, command_hash, command, block, chain_ref })))
}
