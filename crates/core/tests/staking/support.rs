//! What the `V75` staking tests share: an access-screened staking harness at a chosen version, with
//! the house 1,000 USDC above its floor and rotation on, and the staking mirror's frontier.

use alloy_primitives::B256;
use engine_core::Fixed;
use engine_core::access::AccessPolicy;
use engine_core::command::{AdminCommand, Command};
use engine_core::election::RegistryElection;
use engine_core::emissions::TokenUnits;
use engine_core::event::Event;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{AccountId, Millis};

use crate::common::close_spread::{ceilings, install_expect, source_key};
use crate::common::{Client, HOUSE_CAPITAL, Harness};

pub const DAY_MS: Millis = 24 * 60 * 60 * 1000;
pub const HOUR_MS: Millis = 60 * 60 * 1000;
/// One USDC base unit in `Fixed`'s raw units.
pub const USDC: i128 = 1_000;
/// What the house holds above the liquidity floor, in USDC base units: 1,000 USDC.
pub const EXCESS: u128 = 1_000_000_000;

/// Staking at `version` with one-day epochs and a three-day cooldown, the house 1,000 USDC above its
/// floor, access screening on, and rotation on, so the operator in office earns a fifth of each close.
pub fn harness(version: ProtocolVersion) -> Harness {
    harness_with(version, DAY_MS)
}

/// `harness` with epochs of `epoch_ms`.
pub fn harness_with(version: ProtocolVersion, epoch_ms: Millis) -> Harness {
    let floor = Fixed::from_int(HOUSE_CAPITAL) - Fixed::from_raw(EXCESS as i128 * USDC);
    let mut h = Harness::staking(floor, epoch_ms, 3 * DAY_MS);
    install_expect(&mut h, ceilings());
    h.admin(AdminCommand::SetAccessPolicy(AccessPolicy { source: source_key(), max_age_ms: 1_000 }));
    h.admin(AdminCommand::ActivateProtocol { version });
    let (block, chain_ref) = (h.log.len() as u64, format!("0xswitch:{}", h.log.len()));
    h.cmd(Command::SwitchRotation { enabled: true, election_cancelled: false, block, chain_ref });
    h
}

/// Mirrors an election of `candidate` under the admin key of `key_seed`, in effect from `effective_at`,
/// which must be a notice away.
pub fn elect(h: &mut Harness, candidate: AccountId, key_seed: u8, effective_at: Millis) {
    let (block, chain_ref) = (h.log.len() as u64, format!("0xelect:{}", h.log.len()));
    let admin_key = Client::new(key_seed).account;
    let election = RegistryElection { epoch: 1, candidate, admin_key, effective_at, block, chain_ref };
    let events = h.cmd(Command::ElectOperator(election));
    assert!(matches!(events.last(), Some(Event::OperatorElected { .. })), "{events:?}");
}

/// When `epoch` ends.
pub fn end_of(h: &Harness, epoch: u64) -> Millis {
    h.engine.staking().expect("staking is initialised").config.epoch_end(epoch)
}

/// Mirrors `account`'s stake of `amount` in block `block`.
pub fn stake_in(h: &mut Harness, account: AccountId, amount: u128, block: u64) {
    let chain_ref = format!("0xstake:{}", h.log.len());
    let events = h.cmd(Command::Stake { account, amount: TokenUnits(amount), block, chain_ref });
    assert!(matches!(events[..], [Event::Staked { .. }]), "{events:?}");
}

/// Mirrors `account`'s stake of `amount` in a block numbered by the log's length.
pub fn stake(h: &mut Harness, account: AccountId, amount: u128) {
    stake_in(h, account, amount, h.log.len() as u64);
}

/// The staking mirror completes a scan through `block`, stamped `time` on chain.
pub fn progress(h: &mut Harness, block: u64, time: Millis) -> Vec<Event> {
    let block_hash = B256::repeat_byte(9);
    let events = h.cmd(Command::StakingProgress { block, block_hash, block_time: Some(time), named_turn: None });
    assert!(!events.iter().any(|event| matches!(event, Event::AdminRejected { .. })), "{events:?}");
    events
}

/// `progress` through a block numbered by the log's length.
pub fn frontier(h: &mut Harness, time: Millis) -> Vec<Event> {
    progress(h, h.log.len() as u64, time)
}

/// At `end`: the mirror passes it, then a tick. Returns both commands' events, which hold the close of
/// the epoch ending at `end` under every version.
pub fn close_at(h: &mut Harness, end: Millis) -> Vec<Event> {
    h.advance_to(end);
    let mut events = frontier(h, end);
    events.extend(h.cmd(Command::Tick));
    events
}

/// What the close among `events` measured and allocated, in USDC base units, if one closed.
pub fn closed(events: &[Event]) -> Option<(u128, u128)> {
    events.iter().find_map(|event| match event {
        Event::DistributionEpochClosed { excess, allocated, .. } => Some((excess.0, allocated.0)),
        _ => None,
    })
}

/// `account`'s cumulative distribution in USDC base units.
pub fn paid(h: &Harness, account: AccountId) -> u128 {
    h.engine.distribution(account).0
}
