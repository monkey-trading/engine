//! The operator's share of distributions from `V47`, through `Engine::apply`: a fifth of each
//! epoch's distribution while rotation is on, credited to the operator in effect when the close
//! runs, in the same cumulative leaf a staker's share goes to, and nothing with rotation off or an
//! epoch that distributes nothing.

mod common;

use common::close_spread::{attest, ceilings, fund, install_expect, source_key, trade};
use common::{BTC, Client, HOUSE_CAPITAL, Harness, operator};
use engine_core::Fixed;
use engine_core::access::AccessPolicy;
use engine_core::command::{AdminCommand, Command};
use engine_core::election::RegistryElection;
use engine_core::emissions::TokenUnits;
use engine_core::event::Event;
use engine_core::operator_bounds::NOTICE_MS;
use engine_core::protocol::ProtocolVersion;
use engine_core::staking::UsdcUnits;
use engine_core::types::{AccountId, Millis, Side};

const DAY_MS: Millis = 24 * 60 * 60 * 1000;
/// One USDC base unit in `Fixed`'s raw units (9 decimals against USDC's 6).
const USDC: i128 = 1_000;
/// The seed of the elected candidate's admin key.
const KEY_SEED: u8 = 0x66;
/// The seed of the client whose losses the house earns from.
const TRADER_SEED: u8 = 70;
const CANDIDATE: AccountId = AccountId([0xc1; 20]);
const SECOND: AccountId = AccountId([0xc2; 20]);
const STAKER_A: AccountId = AccountId([0xa1; 20]);
const STAKER_B: AccountId = AccountId([0xa2; 20]);

fn key() -> AccountId {
    Client::new(KEY_SEED).account
}

/// A house whose close finds `excess` USDC base units above the floor, under `version`.
fn at(version: ProtocolVersion, excess: u128) -> Harness {
    let floor = Fixed::from_int(HOUSE_CAPITAL) - Fixed::from_raw(excess as i128 * USDC);
    let mut h = Harness::staking(floor, DAY_MS, 3 * DAY_MS);
    install_expect(&mut h, ceilings());
    h.admin(AdminCommand::SetAccessPolicy(AccessPolicy { source: source_key(), max_age_ms: 1_000 }));
    h.admin(AdminCommand::ActivateProtocol { version });
    let trader = Client::new(TRADER_SEED);
    attest(&mut h, &trader);
    fund(&mut h, &trader, "10000000");
    h
}

/// `at` under `V47` with rotation on.
fn rotating(excess: u128) -> Harness {
    let mut h = at(ProtocolVersion::V47, excess);
    switch(&mut h, true);
    h
}

/// Mirrors the registry's rotation switch, cancelling no election.
fn switch(h: &mut Harness, enabled: bool) -> Vec<Event> {
    let chain_ref = format!("0xswitch:{}", h.log.len());
    let block = h.log.len() as u64;
    h.cmd(Command::SwitchRotation { enabled, election_cancelled: false, block, chain_ref })
}

/// Mirrors an election of `candidate` under `admin_key`, taking effect at `effective_at` on chain.
fn elect(h: &mut Harness, epoch: u64, candidate: AccountId, admin_key: AccountId, effective_at: Millis) {
    let (block, chain_ref) = (h.log.len() as u64, format!("0xelect:{}", h.log.len()));
    let events =
        h.cmd(Command::ElectOperator(RegistryElection { epoch, candidate, admin_key, effective_at, block, chain_ref }));
    assert!(matches!(events.last(), Some(Event::OperatorElected { .. })), "{events:?}");
}

/// Records the staking mirror's finalised frontier at the next block, stamped `time` on chain.
/// Takes effect after the command's own handover check, so a handover it allows applies on the next
/// command.
fn finalised(h: &mut Harness, time: Millis) {
    let block = h.log.len() as u64;
    let block_hash = alloy_primitives::B256::repeat_byte(9);
    let events = h.cmd(Command::StakingProgress { block, block_hash, block_time: Some(time) });
    assert!(!events.iter().any(|event| matches!(event, Event::AdminRejected { .. })), "{events:?}");
}

/// Elects `candidate` under `admin_key` and runs the clock past its effective time and the mirror's
/// frontier to it, so the first command after this applies the handover.
fn seat(h: &mut Harness, epoch: u64, candidate: AccountId, admin_key: AccountId) {
    let effective_at = h.ts + NOTICE_MS;
    elect(h, epoch, candidate, admin_key, effective_at);
    h.advance_to(effective_at);
    finalised(h, effective_at);
}

fn stake(h: &mut Harness, account: AccountId, amount: u128) {
    let (block, chain_ref) = (h.log.len() as u64, format!("0xstake:{}", h.log.len()));
    let events = h.cmd(Command::Stake { account, amount: TokenUnits(amount), block, chain_ref });
    assert!(matches!(events[..], [Event::Staked { .. }]), "{events:?}");
}

/// Ticks at the end of the open epoch, closing it, and returns the tick's events.
fn close(h: &mut Harness) -> Vec<Event> {
    let staking = h.engine.staking().expect("staking is initialised");
    let end = staking.config.epoch_end(staking.config.epochs_elapsed(h.ts));
    h.advance_to(end);
    h.cmd(Command::Tick)
}

/// The operator's share among `events`.
fn share(events: &[Event]) -> Option<(AccountId, UsdcUnits)> {
    let mut shares = events.iter().filter_map(|event| match event {
        Event::OperatorShareAllocated { account, amount, .. } => Some((*account, *amount)),
        _ => None,
    });
    let found = shares.next();
    assert_eq!(shares.next(), None, "one share a close: {events:?}");
    found
}

/// The house earns at least $1,000: a client loses that much on a round trip, and pays its spreads.
fn earn(h: &mut Harness) {
    let trader = Client::new(TRADER_SEED);
    for (side, mid) in [(Side::Buy, "100000"), (Side::Sell, "99900")] {
        h.price(BTC, mid);
        let events = trade(h, &trader, BTC, side, "10", mid);
        assert!(events.iter().any(|event| matches!(event, Event::Fill(_))), "{events:?}");
    }
}

/// Closes the open epoch, which must pay `account` a fifth of its excess, and returns the share.
fn pays(h: &mut Harness, account: AccountId) -> u128 {
    let events = close(h);
    let excess = events.iter().find_map(|event| match event {
        Event::DistributionEpochClosed { excess, .. } => Some(excess.0),
        _ => None,
    });
    let fifth = excess.expect("an epoch closed") / 5;
    assert!(fifth > 0, "{events:?}");
    assert_eq!(share(&events), Some((account, UsdcUnits(fifth))), "{events:?}");
    fifth
}

/// What a close allocated, by the close's own event.
fn allocated(events: &[Event]) -> UsdcUnits {
    events
        .iter()
        .find_map(|event| match event {
            Event::DistributionEpochClosed { allocated, .. } => Some(*allocated),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no epoch closed: {events:?}"))
}

fn leaf(h: &Harness, account: AccountId) -> u128 {
    h.engine.distribution(account).0
}

#[test]
fn an_elected_candidate_earns_a_fifth_and_stakers_share_the_rest_pro_rata() {
    let mut h = rotating(1_000_000_000);
    seat(&mut h, 1, CANDIDATE, key());
    stake(&mut h, STAKER_A, 1);
    stake(&mut h, STAKER_B, 3);
    assert_eq!(h.engine.operator().map(|operator| operator.address), Some(key()));

    let cash = h.engine.house_cash();
    let events = close(&mut h);
    assert_eq!(share(&events), Some((CANDIDATE, UsdcUnits(200_000_000))));
    assert_eq!([leaf(&h, CANDIDATE), leaf(&h, STAKER_A), leaf(&h, STAKER_B)], [200_000_000, 200_000_000, 600_000_000]);
    assert_eq!(allocated(&events), UsdcUnits(1_000_000_000));
    // The leaf is the candidate's, not its admin key's, and cash leaves the house once.
    assert_eq!(leaf(&h, key()), 0);
    assert_eq!(h.engine.house_cash(), cash - Fixed::from_raw(1_000_000_000 * USDC));
    assert_eq!(h.engine.staking().unwrap().distributed, UsdcUnits(1_000_000_000));
    h.assert_conserved(Fixed::from_raw(1_000_000_000 * USDC));
}

#[test]
fn rotation_off_pays_the_operator_nothing_even_with_a_candidate_in_effect() {
    let mut h = rotating(1_000_000_000);
    seat(&mut h, 1, CANDIDATE, key());
    stake(&mut h, STAKER_A, 1);
    assert!(matches!(switch(&mut h, false)[..], [Event::RotationSwitched { enabled: false, .. }]));
    let events = close(&mut h);
    assert_eq!(share(&events), None);
    assert_eq!((leaf(&h, CANDIDATE), leaf(&h, STAKER_A)), (0, 1_000_000_000));
}

#[test]
fn rotation_never_on_pays_stakers_alone() {
    let mut h = at(ProtocolVersion::V47, 1_000_000_000);
    stake(&mut h, STAKER_A, 1);
    let events = close(&mut h);
    assert_eq!(share(&events), None);
    assert_eq!(leaf(&h, STAKER_A), 1_000_000_000);
    assert_eq!(leaf(&h, operator().address), 0);
}

#[test]
fn before_v47_stakers_keep_everything_and_the_activation_starts_the_share() {
    let mut h = at(ProtocolVersion::V46, 1_000_000_000);
    switch(&mut h, true);
    seat(&mut h, 1, CANDIDATE, key());
    stake(&mut h, STAKER_A, 1);
    assert_eq!(close(&mut h).iter().filter(|e| matches!(e, Event::OperatorShareAllocated { .. })).count(), 0);
    assert_eq!((leaf(&h, CANDIDATE), leaf(&h, STAKER_A)), (0, 1_000_000_000));
    // Nothing the older rules ran recorded the candidate, so their encoding stands.
    assert_eq!(h.engine.operator_candidate(), None);
    assert_eq!(h.engine.operator().map(|operator| operator.address), Some(key()));

    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V47 });
    assert_eq!(h.engine.operator_candidate(), None);
    // The first command under the new rules records the election already in effect, and the next
    // close pays its candidate.
    earn(&mut h);
    assert_eq!(h.engine.operator_candidate(), Some(CANDIDATE));
    pays(&mut h, CANDIDATE);
}

#[test]
fn an_epoch_that_distributes_nothing_credits_the_operator_nothing() {
    // No excess: the house sits exactly at the floor.
    let mut h = rotating(0);
    seat(&mut h, 1, CANDIDATE, key());
    stake(&mut h, STAKER_A, 1);
    let events = close(&mut h);
    assert_eq!((share(&events), allocated(&events)), (None, UsdcUnits(0)));
    assert_eq!(leaf(&h, CANDIDATE), 0);
    h.engine.state_root();
    assert!(h.engine.distribution_proof(CANDIDATE).is_none());

    // An excess with nothing staked distributes nothing either, and stays in the house.
    let mut h = rotating(1_000_000_000);
    seat(&mut h, 1, CANDIDATE, key());
    let cash = h.engine.house_cash();
    let events = close(&mut h);
    assert_eq!((share(&events), allocated(&events)), (None, UsdcUnits(0)));
    assert_eq!(leaf(&h, CANDIDATE), 0);
    assert_eq!(h.engine.house_cash(), cash);
}

#[test]
fn a_close_that_sweeps_a_built_up_excess_pays_the_operator_a_fifth_of_the_sweep() {
    // Nothing staked across several closes: the excess stays in the house. The first stake, however
    // small and however late, shares the sweep with the operator, and takes nothing more than a
    // staker's part of it.
    let mut h = rotating(1_000_000_000);
    seat(&mut h, 1, CANDIDATE, key());
    for _ in 0..3 {
        assert_eq!(allocated(&close(&mut h)), UsdcUnits(0));
    }
    stake(&mut h, STAKER_A, 1);
    let events = close(&mut h);
    assert_eq!(share(&events), Some((CANDIDATE, UsdcUnits(200_000_000))));
    assert_eq!(leaf(&h, STAKER_A), 800_000_000);
    assert_eq!(allocated(&events), UsdcUnits(1_000_000_000));

    // The sweep emptied the excess, so the next close pays neither.
    let events = close(&mut h);
    assert_eq!((share(&events), allocated(&events)), (None, UsdcUnits(0)));
}

#[test]
fn an_operator_that_also_stakes_gets_both_parts_in_one_leaf() {
    let mut h = rotating(1_000_000_000);
    seat(&mut h, 1, CANDIDATE, key());
    stake(&mut h, CANDIDATE, 1);
    stake(&mut h, STAKER_A, 3);
    let events = close(&mut h);
    assert_eq!(share(&events), Some((CANDIDATE, UsdcUnits(200_000_000))));
    assert!(events.contains(&Event::DistributionAllocated {
        account: CANDIDATE,
        epoch: h.engine.staking().unwrap().epoch - 1,
        amount: UsdcUnits(200_000_000),
    }));
    assert_eq!((leaf(&h, CANDIDATE), leaf(&h, STAKER_A)), (400_000_000, 600_000_000));

    // The only staker takes the whole distribution through the two parts.
    let mut h = rotating(1_000_000_003);
    seat(&mut h, 1, CANDIDATE, key());
    stake(&mut h, CANDIDATE, 5);
    let events = close(&mut h);
    assert_eq!(allocated(&events), UsdcUnits(1_000_000_003));
    assert_eq!(leaf(&h, CANDIDATE), 1_000_000_003);
}

#[test]
fn with_no_election_the_key_in_effect_earns_in_its_own_address() {
    // The registry takes the initial operator's candidate identity to be its key's address.
    let mut h = rotating(1_000_000_000);
    stake(&mut h, STAKER_A, 1);
    let events = close(&mut h);
    assert_eq!(share(&events), Some((operator().address, UsdcUnits(200_000_000))));
    assert_eq!(leaf(&h, STAKER_A), 800_000_000);
}

#[test]
fn the_operators_leaf_proves_against_the_root_for_a_claim_and_an_exit() {
    // The candidate has no trading account and no stake: the leaf is all it has in the state.
    let mut h = rotating(1_000_000_000);
    seat(&mut h, 1, CANDIDATE, key());
    stake(&mut h, STAKER_A, 1);
    close(&mut h);

    let root = h.engine.state_root();
    let proof = h.engine.distribution_proof(CANDIDATE).expect("the operator holds a leaf");
    assert_eq!(proof.entitled, UsdcUnits(200_000_000));
    assert!(proof.verify(root.into()), "the claim's proof holds against the state root");

    let exit = h.engine.exit_proof(CANDIDATE).expect("the exit proof exists under V38");
    assert!(exit.account.is_none(), "no trading account");
    assert_eq!(exit.distribution, Some(proof), "the exit registers the same leaf the claim pays");
}

#[test]
fn a_replay_reaches_the_same_root_and_candidate() {
    let mut h = rotating(1_000_000_000);
    seat(&mut h, 1, CANDIDATE, key());
    stake(&mut h, CANDIDATE, 2);
    stake(&mut h, STAKER_A, 3);
    close(&mut h);
    let root = h.engine.state_root();
    let mut replayed = common::batch::replayed(&h);
    assert_eq!(replayed.state_root(), root);
    assert_eq!(replayed.operator_election(), h.engine.operator_election());
    assert_eq!(replayed.operator_candidate(), Some(CANDIDATE));
    assert_eq!(replayed.distribution(CANDIDATE), h.engine.distribution(CANDIDATE));
}

#[path = "operator_share/elections.rs"]
mod elections;

#[path = "operator_share/rounding.rs"]
mod rounding;

#[path = "operator_share/time_in_office.rs"]
mod time_in_office;

#[path = "operator_share/daily_turns.rs"]
mod daily_turns;
