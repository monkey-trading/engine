//! The session's `V89` part: bug bounty awards. A vote activates the version. While votes govern, a High
//! award to a researcher and a cap of 10% of the excess and 100,000 USDC then pass, beside an award for
//! another amount than its severity's and a fast-path award, both refused when mirrored, and the key in
//! office's own award is refused. A second vote for the same award passes too. Seven days on the award and
//! the cap apply, and the second vote's award is refused, its advisory already awarded. A client loses a BTC
//! round trip to the house, and the staking mirror passes the end of the oldest epoch not yet closed: its
//! close pays the award a tenth of its excess first, into the researcher's distribution leaf, before the
//! operator's share and the stakers, and the rest of the award stays queued.

use alloy_primitives::B256;
use engine_core::awards::{AwardApproval, AwardCap, Severity};
use engine_core::command::{AdminCommand, Command};
use engine_core::event::Event;
use engine_core::governance::VotedChange;
use engine_core::operator_bounds::NOTICE_MS;
use engine_core::protocol::ProtocolVersion;
use engine_core::staking::UsdcUnits;
use engine_core::types::{AccountId, Side};

use crate::common::close_spread::{attest, fund, trade};
use crate::common::events::only_fill;
use crate::common::{BTC, Client, Harness};
use crate::v56::pass;
use crate::v76::office_seed;
use crate::v81::{applied, mark, qty, scheduled};

/// The seed of the key this part trades through.
const CLIENT_SEED: u8 = 0x89;
/// The first of this part's proposal ids.
const PROPOSAL: u64 = 294;
/// The researcher the award is credited to.
const RESEARCHER: AccountId = AccountId([0x89; 20]);

/// Activates `V89` by vote; then the awards described above.
pub(crate) fn session(h: &mut Harness) {
    let activation = AdminCommand::ActivateProtocol { version: ProtocolVersion::V89 };
    scheduled(h, PROPOSAL, &activation);
    h.advance(NOTICE_MS);
    applied(h, PROPOSAL, activation);
    assert_eq!(h.engine.protocol_version(), ProtocolVersion::V89);
    // An activation leaves the rollovers its notice spanned to the next command.
    h.cmd(Command::Tick);

    let high = AwardApproval {
        recipient: RESEARCHER,
        severity: Severity::High,
        advisory: "GHSA-4v2c-9xq8-jm3w".into(),
        amount: Severity::High.amount(),
    };
    let award = AdminCommand::ApproveAward(high.clone());
    scheduled(h, PROPOSAL + 1, &award);
    let cap = AwardCap { excess_bps: 1_000, max: UsdcUnits(100_000_000_000) };
    scheduled(h, PROPOSAL + 2, &AdminCommand::SetAwardCap(cap));
    let critical_amount = AdminCommand::ApproveAward(AwardApproval { amount: Severity::Critical.amount(), ..high });
    let events = pass(h, PROPOSAL + 3, false, Some(critical_amount));
    assert!(matches!(events.last(), Some(Event::ProposalRefused { .. })), "{events:?}");
    let events = pass(h, PROPOSAL + 4, true, Some(award.clone()));
    assert!(matches!(events.last(), Some(Event::ProposalRefused { .. })), "{events:?}");
    let events = h.operate_as(office_seed(h), award.clone());
    assert!(matches!(events.last(), Some(Event::AdminRejected { .. })), "{events:?}");
    scheduled(h, PROPOSAL + 5, &award);

    h.advance(NOTICE_MS);
    applied(h, PROPOSAL + 1, award.clone());
    applied(h, PROPOSAL + 2, AdminCommand::SetAwardCap(cap));
    let again = VotedChange { proposal: PROPOSAL + 5, command: award };
    let events = h.cmd(Command::ApplyProposal(Box::new(again)));
    let refused =
        matches!(events.last(), Some(Event::AdminRejected { reason }) if reason.contains("already has award 0"));
    assert!(refused, "{events:?}");
    assert_eq!((h.engine.awards().len(), h.engine.award_cap()), (1, cap));

    // A client loses a BTC round trip to the house, which the next close distributes.
    let btc = mark(h, BTC);
    h.price(BTC, &btc.to_string());
    let client = Client { expires_at: h.ts + 60_000, ..Client::new(CLIENT_SEED) };
    attest(h, &client);
    fund(h, &client, "200000");
    let size = qty(btc).to_string();
    only_fill(&trade(h, &client, BTC, Side::Buy, &size, &btc.to_string()));
    let lower = btc - btc.mul_bps(100);
    h.price(BTC, &lower.to_string());
    only_fill(&trade(h, &client, BTC, Side::Sell, &size, &lower.to_string()));

    // The mirror passes the end of the oldest epoch not yet closed, alone, and the close pays the award.
    let staking = h.engine.staking().expect("the session staked");
    let end = staking.config.epoch_end(staking.epoch);
    let (block, block_hash) = (89_000_000 + h.log.len() as u64, B256::repeat_byte(0x89));
    let progress = Command::StakingProgress { block, block_hash, block_time: Some(end), named_turn: None };
    let events = h.cmd(progress);
    assert!(!events.iter().any(|event| matches!(event, Event::AdminRejected { .. })), "{events:?}");
    let events = h.cmd(Command::Tick);
    let closes: Vec<(u128, u128)> = events
        .iter()
        .filter_map(|event| match event {
            Event::DistributionEpochClosed { excess, awarded, .. } => Some((excess.0, awarded.0)),
            _ => None,
        })
        .collect();
    let [(excess, awarded)] = closes[..] else { panic!("one epoch closes: {events:?}") };
    assert_eq!(awarded, cap.of(excess), "{events:?}");
    assert!(awarded > 0 && awarded < Severity::High.amount().0, "{events:?}");
    assert_eq!(h.engine.distribution(RESEARCHER), UsdcUnits(awarded));
    assert_eq!(h.engine.awards()[&0].owed(), Severity::High.amount().0 - awarded);
}
