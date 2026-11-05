//! The session's `V47` part: while rotation is on, a fifth of each epoch's distribution credits the
//! operator in effect in its candidate's leaf, and stakers share the rest.

use engine_core::command::{AdminCommand, Command};
use engine_core::election::RegistryElection;
use engine_core::emissions::TokenUnits;
use engine_core::event::Event;
use engine_core::operator_bounds::NOTICE_MS;
use engine_core::protocol::ProtocolVersion;
use engine_core::staking::UsdcUnits;
use engine_core::types::{AccountId, Millis, Side};

use crate::common::close_spread::{attest, fund, trade};
use crate::common::{Client, ETH, Harness};

const FIRST_STAKER: AccountId = AccountId([0x46; 20]);
const SECOND_STAKER: AccountId = AccountId([0x47; 20]);
/// The candidate this part elects under the admin key already in effect.
const NEXT_CANDIDATE: AccountId = AccountId([0x48; 20]);
/// The seed of the account key whose losses put the house above the floor.
const LOSER_SEED: u8 = 0x4e;
const HOUR_MS: Millis = 60 * 60 * 1000;

/// The trader loses on a round trip, so the house earns above the floor, then a tick at the end of
/// the open epoch closes it, returning what it credited the operator, its excess and its allocated
/// total.
pub(crate) fn close(h: &mut Harness) -> (Vec<(AccountId, UsdcUnits)>, UsdcUnits, UsdcUnits) {
    let loser = Client { expires_at: h.ts + 60_000, ..Client::new(LOSER_SEED) };
    for (side, price) in [(Side::Buy, "100"), (Side::Sell, "99.7")] {
        h.price(ETH, price);
        let events = trade(h, &loser, ETH, side, "1000", price);
        assert!(events.iter().any(|event| matches!(event, Event::Fill(_))), "{events:?}");
    }
    let staking = h.engine.staking().expect("the session staked");
    h.advance_to(staking.config.epoch_end(staking.config.epochs_elapsed(h.ts)));
    let events = h.cmd(Command::Tick);
    let shares = events
        .iter()
        .filter_map(|event| match event {
            Event::OperatorShareAllocated { account, amount, .. } => Some((*account, *amount)),
            _ => None,
        })
        .collect();
    let closed = events.iter().find_map(|event| match event {
        Event::DistributionEpochClosed { excess, allocated, .. } => Some((*excess, *allocated)),
        _ => None,
    });
    let (excess, allocated) = closed.unwrap_or_else(|| panic!("the epoch closes: {events:?}"));
    (shares, excess, allocated)
}

/// `V47` activates over an election already in effect, so the next command records its candidate.
/// Two stakers share what the operator's fifth leaves, and the house keeps their dust. An
/// election under the key already in effect moves no key, and moves the share when it takes effect.
/// Rotation turned off pays stakers alone.
pub(crate) fn session(h: &mut Harness) {
    let loser = Client { expires_at: h.ts + 60_000, ..Client::new(LOSER_SEED) };
    attest(h, &loser);
    fund(h, &loser, "1000000");
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V47 });
    let elected = h.engine.operator_election().last.expect("V37's part elected");
    assert_eq!(h.engine.operator_candidate(), None);
    for (account, amount, chain_ref) in
        [(FIRST_STAKER, 1_000_000, "0xstake:46:0"), (SECOND_STAKER, 2_000_000, "0xstake:46:1")]
    {
        let block = 46_000_000;
        let events = h.cmd(Command::Stake { account, amount: TokenUnits(amount), block, chain_ref: chain_ref.into() });
        assert!(matches!(events[..], [Event::Staked { .. }]), "{events:?}");
    }
    assert_eq!(h.engine.operator_candidate(), Some(elected.candidate));

    let (shares, excess, allocated) = close(h);
    assert_eq!(shares, vec![(elected.candidate, UsdcUnits(excess.0 / 5))]);
    let stakers = h.engine.staked_balances().len() as u128;
    assert!(allocated.0 <= excess.0 && excess.0 - allocated.0 < stakers, "{allocated:?} {excess:?}");

    // Under the key already in effect, the election moves no key and waits for its time.
    let key = h.engine.operator().expect("an operator is recorded").address;
    let effective_at = h.ts + NOTICE_MS + HOUR_MS;
    let events = h.cmd(Command::ElectOperator(RegistryElection {
        epoch: elected.epoch + 1,
        candidate: NEXT_CANDIDATE,
        admin_key: key,
        effective_at,
        block: 46_000_010,
        chain_ref: "0xelect:46:0".into(),
    }));
    assert!(matches!(events.last(), Some(Event::OperatorElected { .. })), "{events:?}");
    let (shares, _, _) = close(h);
    assert_eq!(shares.iter().map(|share| share.0).collect::<Vec<_>>(), [elected.candidate]);
    h.advance_to(effective_at);
    let frontier = Command::StakingProgress {
        block: 46_000_020,
        block_hash: alloy_primitives::B256::repeat_byte(46),
        block_time: Some(effective_at),
    };
    h.cmd(frontier);
    let (shares, excess, _) = close(h);
    assert_eq!(shares, vec![(NEXT_CANDIDATE, UsdcUnits(excess.0 / 5))]);
    assert_eq!(h.engine.operator().map(|operator| operator.address), Some(key));

    // With rotation off nothing goes to the operator, and turning it back on restores the share.
    let (enabled, block, chain_ref) = (false, 46_000_030, "0xrotation:46:0".to_owned());
    h.cmd(Command::SwitchRotation { enabled, election_cancelled: false, block, chain_ref });
    let (shares, excess, allocated) = close(h);
    assert!(shares.is_empty() && excess.0 > 0 && allocated.0 > 0, "{shares:?} {excess:?} {allocated:?}");
    let (enabled, block, chain_ref) = (true, 46_000_040, "0xrotation:46:1".to_owned());
    h.cmd(Command::SwitchRotation { enabled, election_cancelled: false, block, chain_ref });
    let (shares, excess, _) = close(h);
    assert_eq!(shares, vec![(NEXT_CANDIDATE, UsdcUnits(excess.0 / 5))]);
}
