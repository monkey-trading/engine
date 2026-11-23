//! How epoch closes pay the award queue: first, oldest award first, up to the cap in force, an award not
//! paid in full staying at the front; the operator's fifth and the stakers' shares from what is left; each
//! payment in the recipient's cumulative distribution leaf; and nothing at a close that distributes nothing.

use alloy_primitives::B256;
use engine_core::awards::{AwardCap, AwardPaid, Severity};
use engine_core::command::{AdminCommand, Command};
use engine_core::emissions::TokenUnits;
use engine_core::event::Event;
use engine_core::protocol::ProtocolVersion;
use engine_core::staking::UsdcUnits;
use engine_core::types::{AccountId, Side};
use engine_core::{Engine, Fixed};

use crate::common::close_spread::trade;
use crate::common::{BTC, Client, Harness};
use crate::{
    RESEARCHER, SECOND, STAKER_A, STAKER_B, TRADER_SEED, USDC, apply, award, governed, pass, rejected, vote_in,
};

/// What a close's events paid each award, the operator's parts, the stakers, and what it recorded as
/// awarded and allocated: the first close among `events` that measured an excess, or the last.
struct Close {
    awards: Vec<AwardPaid>,
    operator: u128,
    stakers: u128,
    awarded: u128,
    allocated: u128,
    excess: u128,
}

/// Moves to the end of the epoch now falls in, has the staking mirror pass it, and ticks, closing every
/// epoch due in turn.
fn close_due(h: &mut Harness) -> Vec<Event> {
    let config = h.engine.staking().expect("staking is initialised").config;
    let end = config.epoch_end(config.epochs_elapsed(h.ts));
    h.advance_to(end);
    let (block, block_hash) = (h.log.len() as u64, B256::repeat_byte(9));
    let progress = Command::StakingProgress { block, block_hash, block_time: Some(end), named_turn: None };
    let mut events = h.cmd(progress);
    assert!(!events.iter().any(|event| matches!(event, Event::AdminRejected { .. })), "{events:?}");
    events.extend(h.cmd(Command::Tick));
    events
}

/// The paying close among `events`: what came before its `DistributionEpochClosed`, from the previous one.
fn paying(events: &[Event]) -> Close {
    let mut close = Close { awards: Vec::new(), operator: 0, stakers: 0, awarded: 0, allocated: 0, excess: 0 };
    for event in events {
        match event {
            Event::AwardPaid(paid) => close.awards.push(*paid),
            Event::OperatorShareAllocated { amount, .. } => close.operator += amount.0,
            Event::DistributionAllocated { amount, .. } => close.stakers += amount.0,
            Event::DistributionEpochClosed { excess, allocated, awarded, .. } => {
                (close.excess, close.allocated, close.awarded) = (excess.0, allocated.0, awarded.0);
                if excess.0 > 0 {
                    return close;
                }
                close = Close { awards: Vec::new(), operator: 0, stakers: 0, awarded: 0, allocated: 0, excess: 0 };
            }
            _ => {}
        }
    }
    close
}

/// The trader loses at least `usd` to the house on a BTC round trip from 100,000 to 90,000, and pays its
/// spreads, so the next close finds that much more above the floor.
fn earn(h: &mut Harness, usd: u32) {
    let trader = Client::new(TRADER_SEED);
    let qty = (usd / 10_000).to_string();
    for (side, mid) in [(Side::Buy, "100000"), (Side::Sell, "90000")] {
        h.price(BTC, mid);
        let events = trade(h, &trader, BTC, side, &qty, mid);
        assert!(events.iter().any(|event| matches!(event, Event::Fill(_))), "{events:?}");
    }
}

/// What the launch cap lets a close whose excess is `excess` base units pay awards.
fn launch_cap(excess: u128) -> u128 {
    (excess * 500 / 10_000).min(50_000 * USDC)
}

fn owed(h: &Harness, id: u64) -> u128 {
    h.engine.awards()[&id].owed()
}

#[test]
fn a_close_pays_the_oldest_award_first_up_to_the_cap_and_the_rest_waits_at_the_front() {
    let mut h = governed(ProtocolVersion::V89, 400_000);
    vote_in(&mut h, 1, &award(RESEARCHER, Severity::High, "GHSA-1"));
    vote_in(&mut h, 2, &award(SECOND, Severity::Medium, "GHSA-2"));

    // 5% of 400,000 USDC is 20,000: all of it to the older award, before the operator's fifth and the
    // stakers' shares of what is left.
    let cash = h.engine.house_cash();
    let events = close_due(&mut h);
    let close = paying(&events);
    assert_eq!(close.excess, 400_000 * USDC, "{events:?}");
    let first = AwardPaid {
        award: 0,
        account: RESEARCHER,
        epoch: 0,
        amount: UsdcUnits(20_000 * USDC),
        owed: UsdcUnits(30_000 * USDC),
    };
    assert_eq!(close.awards, [first]);
    assert_eq!(close.awarded, 20_000 * USDC);
    assert_eq!(close.operator, (400_000 - 20_000) * USDC / 5);
    assert_eq!(close.allocated, close.awarded + close.operator + close.stakers);
    assert!(close.excess - close.allocated < 2, "only the stakers' rounding stays: {events:?}");
    assert_eq!(h.engine.distribution(RESEARCHER), UsdcUnits(20_000 * USDC));
    assert_eq!(h.engine.distribution(STAKER_A).0 + h.engine.distribution(STAKER_B).0, close.stakers);
    assert_eq!(cash - h.engine.house_cash(), Fixed::from_raw(close.allocated as i128 * 1_000));
    assert_eq!(h.engine.staking().unwrap().distributed.0, close.allocated, "the vault's claims count awards");
    // Award events come first, then the operator's share, then the stakers'.
    let position = |found: fn(&Event) -> bool| events.iter().position(found).unwrap();
    let paid_at = position(|e| matches!(e, Event::AwardPaid(_)));
    assert!(paid_at < position(|e| matches!(e, Event::OperatorShareAllocated { .. })));
    assert!(paid_at < position(|e| matches!(e, Event::DistributionAllocated { .. })));
    assert_eq!((owed(&h, 0), owed(&h, 1)), (30_000 * USDC, 10_000 * USDC));

    // The next close pays the front award again, its cap's worth, leaving the younger one waiting.
    earn(&mut h, 400_000);
    let close = paying(&close_due(&mut h));
    let second = launch_cap(close.excess);
    assert!(second > 20_000 * USDC && second < 25_000 * USDC, "the trader lost a little over 400,000");
    assert_eq!(close.awards.iter().map(|paid| (paid.award, paid.amount.0)).collect::<Vec<_>>(), [(0, second)]);
    assert_eq!(close.awarded, second);
    assert_eq!((owed(&h, 0), owed(&h, 1)), (30_000 * USDC - second, 10_000 * USDC));

    // The third, on a smaller excess, finishes the older award and pays the younger part of what it owes
    // from what the cap leaves.
    earn(&mut h, 300_000);
    let close = paying(&close_due(&mut h));
    let third = launch_cap(close.excess);
    let (rest, younger) = (30_000 * USDC - second, third - (30_000 * USDC - second));
    assert!(younger > 0 && younger < 10_000 * USDC, "{third}");
    let paid: Vec<_> = close.awards.iter().map(|paid| (paid.award, paid.account, paid.amount.0, paid.owed.0)).collect();
    assert_eq!(paid, [(0, RESEARCHER, rest, 0), (1, SECOND, younger, 10_000 * USDC - younger)]);
    assert_eq!(close.awarded, third);
    assert_eq!(h.engine.distribution(RESEARCHER), UsdcUnits(50_000 * USDC));
    assert_eq!(h.engine.distribution(SECOND), UsdcUnits(younger));

    // The fourth finishes the younger; with the queue paid, a close pays no award and its event leaves
    // the field out.
    earn(&mut h, 400_000);
    let close = paying(&close_due(&mut h));
    assert_eq!(close.awards.iter().map(|paid| (paid.award, paid.owed.0)).collect::<Vec<_>>(), [(1, 0)]);
    assert_eq!(h.engine.distribution(SECOND), UsdcUnits(10_000 * USDC));
    earn(&mut h, 400_000);
    let events = close_due(&mut h);
    let close = paying(&events);
    assert!(close.awards.is_empty() && close.awarded == 0, "{events:?}");
    assert_eq!(close.operator, close.excess / 5);
    let closed = events.iter().find(|e| matches!(e, Event::DistributionEpochClosed { .. })).unwrap();
    assert!(serde_json::to_value(closed).unwrap().get("awarded").is_none());
}

#[test]
fn the_cap_binds_at_its_limit_and_a_voted_cap_pays_more() {
    let mut h = governed(ProtocolVersion::V89, 2_000_000);
    vote_in(&mut h, 1, &award(RESEARCHER, Severity::Critical, "GHSA-1"));
    // 5% of 2,000,000 USDC is 100,000, over the 50,000 limit.
    let close = paying(&close_due(&mut h));
    assert_eq!(close.awarded, 50_000 * USDC);
    assert_eq!(owed(&h, 0), 200_000 * USDC);

    let widest = AwardCap { excess_bps: 2_000, max: UsdcUnits(250_000 * USDC) };
    vote_in(&mut h, 2, &AdminCommand::SetAwardCap(widest));
    earn(&mut h, 1_000_000);
    // 20% of the excess is a little over 200,000, which the 200,000 still owed takes up to.
    let close = paying(&close_due(&mut h));
    assert!(close.excess > 1_000_000 * USDC && close.excess / 5 > 200_000 * USDC, "{}", close.excess);
    assert_eq!(close.awarded, 200_000 * USDC);
    assert_eq!(close.operator, (close.excess - 200_000 * USDC) / 5);
    assert_eq!((owed(&h, 0), h.engine.distribution(RESEARCHER)), (0, UsdcUnits(250_000 * USDC)));
}

#[test]
fn a_close_at_or_below_the_floor_pays_no_award() {
    let mut h = governed(ProtocolVersion::V89, 0);
    vote_in(&mut h, 1, &award(RESEARCHER, Severity::Low, "GHSA-1"));
    let events = close_due(&mut h);
    let close = paying(&events);
    assert!(events.iter().any(|e| matches!(e, Event::DistributionEpochClosed { .. })), "{events:?}");
    assert_eq!((close.excess, close.awarded, close.allocated), (0, 0, 0));
    assert!(!events.iter().any(|e| matches!(e, Event::AwardPaid(_))), "{events:?}");
    assert_eq!((owed(&h, 0), h.engine.distribution(RESEARCHER)), (1_000 * USDC, UsdcUnits(0)));
}

#[test]
fn a_close_with_nobody_staked_pays_no_award() {
    let mut h = governed(ProtocolVersion::V89, 400_000);
    vote_in(&mut h, 1, &award(RESEARCHER, Severity::Low, "GHSA-1"));
    for (account, amount) in [(STAKER_A, 3), (STAKER_B, 1)] {
        let (block, chain_ref) = (h.log.len() as u64, format!("0xunstake:{}", h.log.len()));
        let events = h.cmd(Command::Unstake { account, amount: TokenUnits(amount), block, chain_ref });
        assert!(matches!(events[..], [Event::Unstaked { .. }]), "{events:?}");
    }
    let cash = h.engine.house_cash();
    let events = close_due(&mut h);
    let close = paying(&events);
    assert_eq!((close.excess, close.awarded, close.allocated), (400_000 * USDC, 0, 0), "{events:?}");
    assert!(!events.iter().any(|e| matches!(e, Event::AwardPaid(_))), "{events:?}");
    assert_eq!((owed(&h, 0), h.engine.house_cash()), (1_000 * USDC, cash));
}

#[test]
fn a_payment_is_in_the_recipients_distribution_leaf_beside_its_stake() {
    let mut h = governed(ProtocolVersion::V89, 400_000);
    vote_in(&mut h, 1, &award(STAKER_A, Severity::Medium, "GHSA-1"));
    let close = paying(&close_due(&mut h));
    let staked: u128 = (400_000 - 10_000) * USDC - close.operator;
    let entitled = h.engine.distribution(STAKER_A);
    // The award and three quarters of the stakers' share, rounded down, in one leaf.
    assert_eq!(entitled, UsdcUnits(10_000 * USDC + staked * 3 / 4));
    let root = h.engine.state_root().into();
    let proof = h.engine.distribution_proof(STAKER_A).expect("the leaf is in the root");
    assert_eq!(proof.entitled, entitled);
    assert!(proof.verify(root));
    assert!(h.engine.distribution_proof(AccountId([0x99; 20])).is_none());
}

#[test]
fn the_root_after_each_award_change_is_the_root_rebuilt_from_the_encoding() {
    // A leaf the incremental root misses after an approval, a payment or a cap move would part the two.
    let rebuilt_matches = |h: &mut Harness| {
        let root = h.engine.state_root();
        let mut rebuilt: Engine = serde_json::from_slice(&serde_json::to_vec(&h.engine).unwrap()).unwrap();
        assert_eq!(rebuilt.state_root(), root);
    };
    let mut h = governed(ProtocolVersion::V89, 400_000);
    rebuilt_matches(&mut h);
    vote_in(&mut h, 1, &award(RESEARCHER, Severity::High, "GHSA-1"));
    rebuilt_matches(&mut h);
    let close = paying(&close_due(&mut h));
    assert_eq!(close.awarded, 20_000 * USDC);
    rebuilt_matches(&mut h);
    vote_in(&mut h, 2, &AdminCommand::SetAwardCap(AwardCap { excess_bps: 2_000, max: UsdcUnits(250_000 * USDC) }));
    rebuilt_matches(&mut h);
}

#[test]
fn an_advisory_is_awarded_once_and_another_advisory_still_pays() {
    let mut h = governed(ProtocolVersion::V89, 400_000);
    let low = award(RESEARCHER, Severity::Low, "GHSA-4v2c-9xq8-jm3w");
    vote_in(&mut h, 1, &low);
    // A second vote naming the awarded advisory, for the same award, another recipient and severity, or in
    // other letter cases, passes, and applies nothing once its notice has run.
    let again = [
        low.clone(),
        award(SECOND, Severity::Critical, "GHSA-4v2c-9xq8-jm3w"),
        award(RESEARCHER, Severity::Low, "ghsa-4V2C-9XQ8-JM3W"),
    ];
    for (proposal, command) in (2..).zip(again) {
        let events = pass(&mut h, proposal, false, &command);
        assert!(matches!(events.last(), Some(Event::ProposalScheduled { .. })), "{events:?}");
        h.advance(engine_core::operator_bounds::NOTICE_MS);
        let events = apply(&mut h, proposal, &command);
        rejected(&events, "already has award 0");
        assert!(!events.iter().any(|e| matches!(e, Event::AwardApproved(_))), "{events:?}");
    }
    assert_eq!(h.engine.awards().len(), 1);
    assert!(h.engine.governance().pending.is_empty(), "each refused vote is spent");

    // Another advisory is awarded, and the close pays both, oldest first.
    vote_in(&mut h, 5, &award(SECOND, Severity::Medium, "GHSA-7rq2-c6jx-pm9f"));
    let close = paying(&close_due(&mut h));
    let paid: Vec<_> = close.awards.iter().map(|paid| (paid.award, paid.account, paid.amount.0, paid.owed.0)).collect();
    assert_eq!(paid, [(0, RESEARCHER, 1_000 * USDC, 0), (1, SECOND, 10_000 * USDC, 0)]);
    assert_eq!(h.engine.distribution(SECOND), UsdcUnits(10_000 * USDC));
}
