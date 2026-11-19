//! `V70`: a proven operator fault. The mirrored `FaultProven` gives its member a haircut that starts at
//! the recorded penalty and falls linearly to nothing over its decay; each stretch of the member's time
//! in office after the mirror earns its part of the operator's share times `1 − h`, and the withheld
//! part goes to stakers pro rata.

use engine_core::operator_faults::{FaultProofConfig, MAX_DECAY_MS, Penalty, ProvenFault};
use engine_core::turns::{RegistryRotationSet, RegistryTurn, SetMember, TurnReason, boundary};

use super::*;

const HOUR_MS: Millis = 60 * 60 * 1000;
const CONTRACT: AccountId = AccountId([0xfa; 20]);
/// Half the share at the fault, back to the whole over ten days.
const PENALTY: Penalty = Penalty { initial_bps: 5_000, decay_ms: 10 * DAY_MS };

/// `at` under `V70` with the fault proof contract recorded under `penalty` and rotation on.
fn penalised(excess: u128, penalty: Penalty) -> Harness {
    let mut h = at(ProtocolVersion::V70, excess);
    h.admin(AdminCommand::SetFaultProof(FaultProofConfig { contract: CONTRACT, penalty }));
    switch(&mut h, true);
    h
}

/// Mirrors a `FaultProven` naming `member` for turn `turn`, under a chain reference of its own.
fn prove(h: &mut Harness, member: AccountId, turn: u64) -> Vec<Event> {
    let (block, chain_ref) = (h.log.len() as u64, format!("0xfault:{}", h.log.len()));
    h.cmd(Command::ProveFault(ProvenFault { member, turn, position: 0, signer_epoch: 1, seq: 42, block, chain_ref }))
}

/// Seats `CANDIDATE` and opens a distribution epoch at the next one's start, staked by `STAKER_A`,
/// whose first close pays the whole excess; returns when the epoch started.
fn seated_epoch(h: &mut Harness) -> Millis {
    seat(h, 1, CANDIDATE, key());
    h.cmd(Command::Tick);
    assert_eq!(h.engine.operator_candidate(), Some(CANDIDATE));
    let config = h.engine.staking().expect("staking is initialised").config;
    let start = config.epoch_end(config.epochs_elapsed(h.ts));
    h.advance_to(start);
    // Nothing is staked at this close, so it pays no one and starts the epoch's count.
    assert_eq!(allocated(&h.cmd(Command::Tick)), UsdcUnits(0));
    stake(h, STAKER_A, 1);
    start
}

/// Amounts by account, in event order.
type Parts = Vec<(AccountId, u128)>;

/// The operator's parts among `events`, and what the haircut withheld from each.
fn parts(events: &[Event]) -> (Parts, Parts) {
    let pick = |withheld: bool| {
        events
            .iter()
            .filter_map(|event| match event {
                Event::OperatorShareAllocated { account, amount, .. } if !withheld => Some((*account, amount.0)),
                Event::OperatorShareWithheld { account, amount, .. } if withheld => Some((*account, amount.0)),
                _ => None,
            })
            .collect()
    };
    (pick(false), pick(true))
}

/// The excess a close measured, in USDC base units.
fn excess_of(events: &[Event]) -> u128 {
    events
        .iter()
        .find_map(|event| match event {
            Event::DistributionEpochClosed { excess, .. } => Some(excess.0),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no epoch closed: {events:?}"))
}

#[test]
fn a_fault_at_the_epochs_start_cuts_the_members_share_by_the_haircuts_average() {
    let mut h = penalised(1_000_000_000, PENALTY);
    let start = seated_epoch(&mut h);
    let events = prove(&mut h, CANDIDATE, 7);
    assert_eq!(
        events.last(),
        Some(&Event::OperatorFaultProven {
            member: CANDIDATE,
            turn: 7,
            position: 0,
            signer_epoch: 1,
            seq: 42,
            decays_until: start + 10 * DAY_MS,
        })
    );
    assert_eq!(h.engine.haircut_bps(CANDIDATE, start), 5_000);
    assert_eq!(h.engine.haircut_bps(CANDIDATE, start + DAY_MS), 4_500);

    // Over the day the haircut falls from 50% to 45%: the candidate keeps 52.5% of the fifth, and the
    // staker takes the rest of the excess.
    let events = close(&mut h);
    assert_eq!(parts(&events), (vec![(CANDIDATE, 105_000_000)], vec![(CANDIDATE, 95_000_000)]));
    assert_eq!(leaf(&h, STAKER_A), 895_000_000);
    assert_eq!(allocated(&events), UsdcUnits(1_000_000_000));
}

#[test]
fn later_epochs_restore_the_share_linearly_and_the_spent_haircut_is_dropped() {
    let mut h = penalised(1_000_000_000, PENALTY);
    seated_epoch(&mut h);
    prove(&mut h, CANDIDATE, 7);
    // Day `k` of the decay averages a haircut of `5,000 · (21 − 2k) / 20` bps.
    for k in 1..=10u128 {
        let events = close(&mut h);
        let fifth = excess_of(&events) / 5;
        let kept = fifth * (10_000 - 250 * (21 - 2 * k)) / 10_000;
        let (allocated, withheld) = parts(&events);
        let [(account, amount)] = allocated[..] else { panic!("one part: {events:?}") };
        // Each command's part of the integral rounds up, so the kept part may fall one unit short.
        assert_eq!(account, CANDIDATE);
        assert!(amount <= kept && kept - amount <= 1, "day {k}: {amount} against {kept}");
        assert_eq!(withheld, vec![(CANDIDATE, fifth - amount)], "day {k}");
        earn(&mut h);
    }
    assert!(h.engine.operator_faults().haircuts.is_empty(), "the spent haircut is dropped at the close");
    assert_eq!(h.engine.haircut_bps(CANDIDATE, h.ts), 0);
    let events = close(&mut h);
    assert_eq!(parts(&events), (vec![(CANDIDATE, excess_of(&events) / 5)], vec![]));
}

#[test]
fn a_fault_mid_epoch_cuts_only_the_time_in_office_after_its_mirror() {
    let mut h = penalised(1_000_000_000, PENALTY);
    let start = seated_epoch(&mut h);
    h.advance_to(start + 12 * HOUR_MS);
    prove(&mut h, CANDIDATE, 7);
    // The first half is whole; over the second the haircut falls from 50% to 47.5%, withholding
    // 48.75% of it: the candidate keeps 75.625% of the fifth.
    let events = close(&mut h);
    assert_eq!(parts(&events), (vec![(CANDIDATE, 151_250_000)], vec![(CANDIDATE, 48_750_000)]));
}

#[test]
fn a_second_fault_restarts_the_decay_at_the_penalty() {
    let mut h = penalised(1_000_000_000, PENALTY);
    let start = seated_epoch(&mut h);
    prove(&mut h, CANDIDATE, 7);
    h.advance_to(start + 5 * DAY_MS);
    assert_eq!(h.engine.haircut_bps(CANDIDATE, h.ts), 2_500);
    let events = prove(&mut h, CANDIDATE, 12);
    assert!(
        matches!(events.last(), Some(Event::OperatorFaultProven { decays_until, .. }) if *decays_until == h.ts + 10 * DAY_MS)
    );
    assert_eq!(h.engine.haircut_bps(CANDIDATE, h.ts), 5_000);
    assert_eq!(h.engine.haircut_bps(CANDIDATE, h.ts + 10 * DAY_MS), 0);
}

#[test]
fn time_before_a_second_fault_is_withheld_under_the_first_haircut() {
    let mut h = penalised(1_000_000_000, PENALTY);
    let start = seated_epoch(&mut h);
    prove(&mut h, CANDIDATE, 7);
    h.advance_to(start + 12 * HOUR_MS);
    prove(&mut h, CANDIDATE, 12);
    // Each half withholds 48.75% of itself, the first under the first haircut and the second under the
    // restarted one: the candidate keeps 51.25% of the fifth.
    let events = close(&mut h);
    assert_eq!(parts(&events), (vec![(CANDIDATE, 102_500_000)], vec![(CANDIDATE, 97_500_000)]));
}

#[test]
fn the_haircut_scales_the_part_it_covers_from_none_to_all() {
    // A whole haircut keeps nothing at the fault, and the one at the decay's end nothing is cut by.
    for (initial_bps, kept) in [(1, 199_981_000), (2_500, 152_500_000), (10_000, 10_000_000)] {
        let mut h = penalised(1_000_000_000, Penalty { initial_bps, decay_ms: 10 * DAY_MS });
        seated_epoch(&mut h);
        prove(&mut h, CANDIDATE, 7);
        let events = close(&mut h);
        assert_eq!(parts(&events), (vec![(CANDIDATE, kept)], vec![(CANDIDATE, 200_000_000 - kept)]), "{initial_bps}");
        assert_eq!(leaf(&h, STAKER_A), 1_000_000_000 - kept, "{initial_bps}");
    }
    // Decaying within the day, a whole haircut leaves the candidate the hours after it reached nothing.
    let mut h = penalised(1_000_000_000, Penalty { initial_bps: 10_000, decay_ms: 6 * HOUR_MS });
    seated_epoch(&mut h);
    prove(&mut h, CANDIDATE, 7);
    let events = close(&mut h);
    // Six hours at an average of half, then eighteen whole: 21/24 of the fifth.
    assert_eq!(parts(&events), (vec![(CANDIDATE, 175_000_000)], vec![(CANDIDATE, 25_000_000)]));
}

#[test]
fn a_haircut_that_withholds_the_whole_part_allocates_the_member_nothing() {
    // A fifth of 200 units over one day of a ten-year decay from the whole share: the kept part,
    // under 0.014% of it, rounds to nothing, so the close allocates the candidate nothing.
    let mut h = penalised(1_000, Penalty { initial_bps: 10_000, decay_ms: MAX_DECAY_MS });
    seated_epoch(&mut h);
    prove(&mut h, CANDIDATE, 7);
    let events = close(&mut h);
    assert_eq!(parts(&events), (vec![], vec![(CANDIDATE, 200)]), "{events:?}");
    assert_eq!((leaf(&h, CANDIDATE), leaf(&h, STAKER_A)), (0, 1_000));
    assert_eq!(allocated(&events), UsdcUnits(1_000));
}

#[test]
fn a_member_under_a_daily_turn_is_cut_from_its_turn_taken() {
    let member = AccountId([0xd1; 20]);
    let mut h = penalised(1_000_000_000, PENALTY);
    let members = vec![SetMember { candidate: member, admin_key: key(), weight: TokenUnits(1) }];
    let effective_at = h.ts + NOTICE_MS;
    let (block, chain_ref) = (h.log.len() as u64, "0xset:1".to_owned());
    let adoption = RegistryRotationSet { set: 1, epoch: 1, effective_at, quorum: true, members, block, chain_ref };
    assert!(matches!(h.cmd(Command::AdoptRotationSet(adoption)).last(), Some(Event::RotationSetAdopted { .. })));
    let turn = (0..).find(|turn| boundary(*turn).unwrap() > h.ts + NOTICE_MS + DAY_MS).unwrap();
    let begins = boundary(turn).unwrap();
    let (block, chain_ref) = (h.log.len() as u64, format!("0xturn:{turn}"));
    let assignment = RegistryTurn {
        turn,
        set: 1,
        position: 0,
        reason: TurnReason::Draw,
        candidate: member,
        admin_key: key(),
        block,
        chain_ref,
    };
    assert!(matches!(h.cmd(Command::AssignTurn(assignment)).last(), Some(Event::TurnAssigned { .. })));
    // The close before the boundary opens the epoch the turn runs in, and pays no one.
    let config = h.engine.staking().unwrap().config;
    h.advance_to(config.epoch_end(config.epochs_elapsed(begins) - 1));
    h.cmd(Command::Tick);
    stake(&mut h, STAKER_A, 1);
    h.advance_to(begins);
    let taken = h.cmd(Command::TurnTaken { turn, release: alloy_primitives::B256::repeat_byte(7) });
    assert!(matches!(taken.last(), Some(Event::TurnTaken { .. })), "{taken:?}");
    prove(&mut h, member, turn);
    let events = close(&mut h);
    let served = config.epoch_end(config.epochs_elapsed(begins)) - begins;
    let (allocated_parts, withheld) = parts(&events);
    let whole = |account| {
        allocated_parts.iter().chain(&withheld).filter(|(a, _)| *a == account).map(|(_, amount)| amount).sum::<u128>()
    };
    // Only the member's part is cut; the key in effect's part before the boundary is whole.
    assert_eq!(withheld.iter().map(|(account, _)| *account).collect::<Vec<_>>(), vec![member]);
    assert_eq!(whole(member), 200_000_000 * served as u128 / DAY_MS as u128);
    assert!(allocated_parts.contains(&(member, whole(member) - withheld[0].1)), "{events:?}");
    assert!(withheld[0].1 > 0, "{events:?}");
}

#[test]
fn a_fault_before_v70_or_its_contract_is_credited_and_refused_and_holds_nothing() {
    let refused = |events: &[Event], reason: &str| {
        matches!(events.last(), Some(Event::OperatorFaultRefused { member, reason: r, .. })
            if *member == CANDIDATE && r.contains(reason))
    };
    // Before `V70`, and under it before the contract is recorded: credited, so the watcher moves on,
    // and nothing else changes.
    for (version, reason) in
        [(ProtocolVersion::V63, "fault proofs need protocol"), (ProtocolVersion::V70, "no fault proof contract")]
    {
        let mut h = at(version, 1_000_000_000);
        let chain_ref = format!("0xfault:{}", h.log.len());
        let events = prove(&mut h, CANDIDATE, 7);
        assert!(refused(&events, reason), "{version:?}: {events:?}");
        assert!(h.engine.is_staking_credited(&chain_ref), "{version:?}");
        assert!(h.engine.operator_faults().haircuts.is_empty(), "{version:?}");
    }
}

#[test]
fn a_mirror_the_log_contradicts_is_rejected_and_credits_nothing() {
    let rejected = |events: &[Event], reason: &str| matches!(events.last(), Some(Event::ElectionRejected { reason: r, .. }) if r.contains(reason));
    // A zero member, and a reference already credited.
    let mut h = penalised(1_000_000_000, PENALTY);
    let zero_ref = format!("0xfault:{}", h.log.len());
    let events = prove(&mut h, AccountId([0; 20]), 7);
    assert!(rejected(&events, "names no member"), "{events:?}");
    assert!(!h.engine.is_staking_credited(&zero_ref));
    let fault = |chain_ref: &str| ProvenFault {
        member: CANDIDATE,
        turn: 7,
        position: 0,
        signer_epoch: 1,
        seq: 42,
        block: 1,
        chain_ref: chain_ref.into(),
    };
    assert!(matches!(h.cmd(Command::ProveFault(fault("0xonce:1"))).last(), Some(Event::OperatorFaultProven { .. })));
    let since = h.engine.operator_faults().haircuts[&CANDIDATE];
    h.advance(DAY_MS);
    let events = h.cmd(Command::ProveFault(fault("0xonce:1")));
    assert!(rejected(&events, "already mirrored"), "{events:?}");
    assert_eq!(h.engine.operator_faults().haircuts[&CANDIDATE], since, "a rejected mirror restarts nothing");
}

#[test]
fn the_fault_proof_contract_is_recorded_once_within_its_bounds() {
    let record = |h: &mut Harness, contract: AccountId, penalty: Penalty| {
        h.cmd(Command::Admin(AdminCommand::SetFaultProof(FaultProofConfig { contract, penalty })))
    };
    let rejected = |events: &[Event]| matches!(events.last(), Some(Event::AdminRejected { .. }));
    let mut h = at(ProtocolVersion::V63, 1_000_000_000);
    assert!(rejected(&record(&mut h, CONTRACT, PENALTY)));
    let mut h = at(ProtocolVersion::V70, 1_000_000_000);
    for penalty in [
        Penalty { initial_bps: 0, ..PENALTY },
        Penalty { initial_bps: 10_001, ..PENALTY },
        Penalty { decay_ms: 0, ..PENALTY },
        Penalty { decay_ms: MAX_DECAY_MS + 1, ..PENALTY },
    ] {
        assert!(rejected(&record(&mut h, CONTRACT, penalty)), "{penalty:?}");
    }
    assert!(rejected(&record(&mut h, AccountId([0; 20]), PENALTY)));
    assert_eq!(h.engine.operator_faults().config, None);
    let bounds = Penalty { initial_bps: 10_000, decay_ms: MAX_DECAY_MS };
    assert_eq!(record(&mut h, CONTRACT, bounds).last(), Some(&Event::AdminApplied));
    assert!(rejected(&record(&mut h, CONTRACT, PENALTY)), "recorded once");
    // The operator cannot sign it: the node alone records it, from its config.
    assert!(
        !AdminCommand::SetFaultProof(FaultProofConfig { contract: CONTRACT, penalty: PENALTY }).is_operator_command()
    );
}

#[test]
fn a_replay_reaches_the_same_root_and_haircut() {
    let mut h = penalised(1_000_000_000, PENALTY);
    seated_epoch(&mut h);
    prove(&mut h, CANDIDATE, 7);
    h.advance(6 * HOUR_MS);
    earn(&mut h);
    let mut replayed = common::batch::replayed(&h);
    assert_eq!(replayed.state_root(), h.engine.state_root());
    assert_eq!(replayed.operator_faults(), h.engine.operator_faults());
    close(&mut h);
    let mut replayed = common::batch::replayed(&h);
    assert_eq!(replayed.state_root(), h.engine.state_root());
    assert_eq!(replayed.distribution(CANDIDATE), h.engine.distribution(CANDIDATE));
}

#[test]
fn from_v83_a_held_close_cuts_each_epoch_by_the_haircut_over_its_own_time() {
    let mut h = at(ProtocolVersion::V83, 1_000_000_000);
    h.admin(AdminCommand::SetFaultProof(FaultProofConfig { contract: CONTRACT, penalty: PENALTY }));
    switch(&mut h, true);
    seat(&mut h, 1, CANDIDATE, key());
    h.cmd(Command::Tick);
    assert_eq!(h.engine.operator_candidate(), Some(CANDIDATE));
    let config = h.engine.staking().expect("staking is initialised").config;
    let start = config.epoch_end(config.epochs_elapsed(h.ts));
    h.advance_to(start);
    finalised(&mut h, start);
    assert_eq!(allocated(&h.cmd(Command::Tick)), UsdcUnits(0));
    stake(&mut h, STAKER_A, 1);
    prove(&mut h, CANDIDATE, 7);
    // The candidate's host holds the close twelve hours into the next epoch, which those hours belong
    // to, under the haircut as it then stood.
    let end = start + DAY_MS;
    h.advance_to(end + 12 * HOUR_MS);
    h.cmd(Command::Tick);
    finalised(&mut h, end);
    let events = h.cmd(Command::Tick);
    // Over its own day the haircut falls from 50% to 45%: the candidate keeps 52.5% of the fifth.
    assert_eq!(parts(&events), (vec![(CANDIDATE, 105_000_000)], vec![(CANDIDATE, 95_000_000)]));
    earn(&mut h);
    let next = end + DAY_MS;
    h.advance_to(next);
    finalised(&mut h, next);
    let events = h.cmd(Command::Tick);
    // Over the next day, the held hours included, it falls from 45% to 40%: 57.5% of the fifth is kept.
    let fifth = excess_of(&events) / 5;
    let (allocated, withheld) = parts(&events);
    let [(account, amount)] = allocated[..] else { panic!("one part: {events:?}") };
    let kept = fifth * 5_750 / 10_000;
    assert_eq!(account, CANDIDATE);
    assert!(amount <= kept && kept - amount <= 1, "{amount} against {kept}");
    assert_eq!(withheld, vec![(CANDIDATE, fifth - amount)]);
}

#[test]
fn from_v83_a_stint_held_past_the_next_epochs_end_withholds_only_its_own_part_in_each() {
    let mut h = at(ProtocolVersion::V83, 1_000_000_000);
    h.admin(AdminCommand::SetFaultProof(FaultProofConfig { contract: CONTRACT, penalty: PENALTY }));
    switch(&mut h, true);
    seat(&mut h, 1, CANDIDATE, key());
    h.cmd(Command::Tick);
    let config = h.engine.staking().expect("staking is initialised").config;
    let start = config.epoch_end(config.epochs_elapsed(h.ts));
    h.advance_to(start);
    finalised(&mut h, start);
    assert_eq!(allocated(&h.cmd(Command::Tick)), UsdcUnits(0));
    stake(&mut h, STAKER_A, 1);
    prove(&mut h, CANDIDATE, 7);
    // The candidate's host holds the close a day and a half past the end, so one stint runs through the
    // whole next epoch and twelve hours into the one after; the mirror then passes one end at a time.
    let end = start + DAY_MS;
    h.advance_to(end + 36 * HOUR_MS);
    h.cmd(Command::Tick);
    finalised(&mut h, end);
    let events = h.cmd(Command::Tick);
    assert_eq!(parts(&events), (vec![(CANDIDATE, 105_000_000)], vec![(CANDIDATE, 95_000_000)]));
    earn(&mut h);
    finalised(&mut h, end + DAY_MS);
    let events = h.cmd(Command::Tick);
    // Over the next epoch's own day the haircut falls from 45% to 40%: 57.5% of the fifth is kept, and
    // the twelve hours after it withhold nothing from this close.
    let fifth = excess_of(&events) / 5;
    let (allocated, withheld) = parts(&events);
    let [(account, amount)] = allocated[..] else { panic!("one part: {events:?}") };
    let kept = fifth * 5_750 / 10_000;
    assert_eq!(account, CANDIDATE);
    assert!(amount <= kept && kept - amount <= 1, "{amount} against {kept}");
    assert_eq!(withheld, vec![(CANDIDATE, fifth - amount)]);
    // The twelve hours carried into the epoch after keep their haircut, which falls from 40% to 35% over
    // that epoch's day: 62.5% of the fifth is kept.
    earn(&mut h);
    let last = end + 2 * DAY_MS;
    h.advance_to(last);
    finalised(&mut h, last);
    let events = h.cmd(Command::Tick);
    let fifth = excess_of(&events) / 5;
    let (allocated, withheld) = parts(&events);
    let [(account, amount)] = allocated[..] else { panic!("one part: {events:?}") };
    let kept = fifth * 6_250 / 10_000;
    assert_eq!(account, CANDIDATE);
    assert!(amount <= kept && kept - amount <= 1, "{amount} against {kept}");
    assert_eq!(withheld, vec![(CANDIDATE, fifth - amount)]);
}
