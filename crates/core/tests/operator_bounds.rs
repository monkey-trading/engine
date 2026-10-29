//! The operator's powers from `V35`, through `Engine::apply`: rate changes within ±50% of the launch
//! rates and only after a seven-day notice, price rules that leave no price, and a key rotation that
//! takes effect after seven days.

mod common;

use common::{BTC, Client, ETH, Harness, acct, operator};
use engine_core::Engine;
use engine_core::auth::ClientAuth;
use engine_core::command::{AdminCommand, Command, Sequenced};
use engine_core::event::Event;
use engine_core::operator_bounds::{LaunchRates, NOTICE_MS, ParamsNotice, PendingRotation};
use engine_core::params::ProtocolParams;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{AccountId, Millis};

const DAY_MS: Millis = 24 * 60 * 60 * 1000;
/// The seed of the key the operator rotates to.
const NEXT_OPERATOR_SEED: u8 = 0x55;

fn rejected(reason: &str) -> Vec<Event> {
    vec![Event::AdminRejected { reason: reason.into() }]
}

/// The parameters a `v35` harness launches with: the genesis ones, with divergence triggers as shares.
fn base() -> ProtocolParams {
    Harness::v35().engine.params().cloned().expect("parameters are set")
}

fn rates(fee: i64, level: i64) -> AdminCommand {
    AdminCommand::SetParams(ProtocolParams { profit_fee_bps: fee, closeout_level_bps: level, ..base() })
}

/// The outcome of an admin command, leaving out the catch-up events a long advance adds.
fn op(h: &mut Harness, command: AdminCommand) -> Vec<Event> {
    h.operate(command).pop().into_iter().collect()
}

/// `op` signed by the key from `seed`.
fn op_as(h: &mut Harness, seed: u8, command: AdminCommand) -> Vec<Event> {
    h.operate_as(seed, command).pop().into_iter().collect()
}

fn notice(fee: Option<i64>, level: Option<i64>, effective_at: Millis) -> AdminCommand {
    AdminCommand::AnnounceParams(ParamsNotice { profit_fee_bps: fee, closeout_level_bps: level, effective_at })
}

fn next_operator() -> AccountId {
    Client::new(NEXT_OPERATOR_SEED).account
}

/// Announces `fee` and `level` for seven days from now, returning the time they apply from.
fn announce(h: &mut Harness, fee: Option<i64>, level: Option<i64>) -> Millis {
    let effective_at = h.ts + NOTICE_MS;
    h.operator_admin(notice(fee, level, effective_at));
    effective_at
}

/// An engine that has run `commands` unsigned, before any funds are credited: client auth and the
/// operator recorded, `V35` active, and then `commands`.
fn bare(commands: Vec<AdminCommand>) -> (Engine, Vec<Vec<Event>>) {
    let mut engine = Engine::new();
    let setup = [
        AdminCommand::SetClientAuth(ClientAuth::Unsigned),
        AdminCommand::SetOperator(operator()),
        AdminCommand::ActivateProtocol { version: ProtocolVersion::V35 },
    ];
    let mut outcomes = vec![];
    for (seq, command) in (0..).zip(setup.into_iter().chain(commands)) {
        let sequenced = Sequenced { seq, ts: common::start(), cmd: Command::Admin(command) };
        outcomes.push(engine.apply(&sequenced).expect("in sequence"));
    }
    (engine, outcomes.split_off(3))
}

#[test]
fn activation_records_the_rates_in_force_as_the_launch_rates() {
    let h = Harness::v35();
    assert_eq!(
        h.engine.operator_bounds().launch,
        Some(LaunchRates { profit_fee_bps: 1_000, closeout_level_bps: 5_000 })
    );
}

#[test]
fn parameters_first_set_under_v35_record_the_launch_rates() {
    let first = AdminCommand::SetParams(ProtocolParams { profit_fee_bps: 800, closeout_level_bps: 4_000, ..base() });
    let (mut engine, outcomes) = bare(vec![first]);
    assert_eq!(outcomes, [vec![Event::AdminApplied]]);
    assert_eq!(engine.operator_bounds().launch, Some(LaunchRates { profit_fee_bps: 800, closeout_level_bps: 4_000 }));
    // The bounds hold from the first parameters on.
    let sequenced = Sequenced { seq: 4, ts: common::start(), cmd: Command::Admin(rates(1_300, 4_000)) };
    assert_eq!(
        engine.apply(&sequenced).unwrap(),
        rejected("profit_fee_bps 1300 is outside ±50% of its launch value 800")
    );
}

#[test]
fn a_rate_change_without_a_notice_is_refused() {
    let mut h = Harness::v35();
    assert_eq!(
        op(&mut h, rates(1_200, 5_000)),
        rejected("a profit_fee_bps change to 1200 needs a notice of at least seven days")
    );
    assert_eq!(
        op(&mut h, rates(1_000, 4_000)),
        rejected("a closeout_level_bps change to 4000 needs a notice of at least seven days")
    );
    assert_eq!(h.engine.params(), Some(&base()));
}

#[test]
fn a_rate_outside_half_of_its_launch_value_is_refused_without_and_with_a_notice() {
    let mut h = Harness::v35();
    for fee in [499, 1_501, 0] {
        let reason = format!("profit_fee_bps {fee} is outside ±50% of its launch value 1000");
        assert_eq!(op(&mut h, rates(fee, 5_000)), rejected(&reason));
        let at = h.ts + NOTICE_MS;
        assert_eq!(op(&mut h, notice(Some(fee), None, at)), rejected(&reason));
    }
    for level in [2_499, 7_501] {
        let reason = format!("closeout_level_bps {level} is outside ±50% of its launch value 5000");
        assert_eq!(op(&mut h, rates(1_000, level)), rejected(&reason));
        let at = h.ts + NOTICE_MS;
        assert_eq!(op(&mut h, notice(None, Some(level), at)), rejected(&reason));
    }
    assert_eq!(h.engine.operator_bounds().notice, None);
}

#[test]
fn a_notice_may_name_each_end_of_the_range() {
    let mut h = Harness::v35();
    for (fee, level) in [(500, 2_500), (1_500, 7_500)] {
        announce(&mut h, Some(fee), Some(level));
    }
    assert_eq!(
        h.engine.operator_bounds().notice.map(|n| (n.profit_fee_bps, n.closeout_level_bps)),
        Some((Some(1_500), Some(7_500)))
    );
}

#[test]
fn a_notice_must_name_a_rate_and_give_seven_days() {
    let mut h = Harness::v35();
    let at = h.ts + NOTICE_MS;
    assert_eq!(op(&mut h, notice(None, None, at)), rejected("a parameter notice names no rate"));
    let soonest = h.ts + NOTICE_MS;
    assert_eq!(
        op(&mut h, notice(Some(1_200), None, soonest - 1)),
        rejected(&format!("a parameter notice takes effect no sooner than {soonest}"))
    );
    assert_eq!(
        op(&mut h, notice(Some(1_200), None, engine_core::types::MAX_STAMP_MS + 1)),
        rejected(&format!("a parameter notice takes effect no sooner than {soonest}"))
    );
    assert_eq!(op(&mut h, notice(Some(1_200), None, soonest)), vec![Event::AdminApplied]);
}

#[test]
fn a_noticed_change_applies_from_its_time_and_not_before() {
    let mut h = Harness::v35();
    let effective_at = announce(&mut h, Some(1_200), Some(6_000));
    h.advance(NOTICE_MS - 1);
    h.cmd(Command::Tick);
    assert_eq!(
        op(&mut h, rates(1_200, 5_000)),
        rejected(&format!("the profit_fee_bps change to 1200 takes effect from {effective_at}"))
    );
    assert_eq!(h.engine.params(), Some(&base()));
    h.advance(1);
    assert_eq!(op(&mut h, rates(1_200, 5_000)), vec![Event::AdminApplied,]);
    assert_eq!(h.engine.params().map(|p| (p.profit_fee_bps, p.closeout_level_bps)), Some((1_200, 5_000)));
}

#[test]
fn a_notice_is_spent_rate_by_rate() {
    let mut h = Harness::v35();
    let effective_at = announce(&mut h, Some(1_200), Some(6_000));
    h.advance_to(effective_at);
    h.operator_admin(rates(1_200, 5_000));
    let left = h.engine.operator_bounds().notice;
    assert_eq!(left.map(|n| (n.profit_fee_bps, n.closeout_level_bps)), Some((None, Some(6_000))));
    // The fee has moved, and moving it again needs a new notice.
    assert_eq!(
        op(&mut h, rates(1_300, 5_000)),
        rejected("a profit_fee_bps change to 1300 needs a notice of at least seven days")
    );
    h.operator_admin(rates(1_200, 6_000));
    assert_eq!(h.engine.operator_bounds().notice, None);
    assert_eq!(
        op(&mut h, rates(1_200, 5_000)),
        rejected("a closeout_level_bps change to 5000 needs a notice of at least seven days")
    );
}

#[test]
fn a_change_to_another_value_than_the_noticed_one_is_refused() {
    let mut h = Harness::v35();
    let effective_at = announce(&mut h, Some(1_200), None);
    h.advance_to(effective_at);
    assert_eq!(
        op(&mut h, rates(1_100, 5_000)),
        rejected("a profit_fee_bps change to 1100 needs a notice of at least seven days")
    );
    assert_eq!(h.engine.params(), Some(&base()));
}

#[test]
fn a_new_notice_replaces_the_pending_one() {
    let mut h = Harness::v35();
    let first = announce(&mut h, Some(1_200), None);
    h.advance(DAY_MS);
    let second = announce(&mut h, Some(1_400), None);
    h.advance_to(first);
    assert_eq!(
        op(&mut h, rates(1_200, 5_000)),
        rejected("a profit_fee_bps change to 1200 needs a notice of at least seven days")
    );
    assert_eq!(
        op(&mut h, rates(1_400, 5_000)),
        rejected(&format!("the profit_fee_bps change to 1400 takes effect from {second}"))
    );
    h.advance_to(second);
    assert_eq!(op(&mut h, rates(1_400, 5_000)), vec![Event::AdminApplied]);
}

#[test]
fn the_bounds_stay_around_the_launch_rates_however_far_the_rates_have_moved() {
    let mut h = Harness::v35();
    let effective_at = announce(&mut h, Some(1_500), None);
    h.advance_to(effective_at);
    h.operator_admin(rates(1_500, 5_000));
    // 50% above the rate now in force is 2250, past 50% above the launch rate.
    let at = h.ts + NOTICE_MS;
    assert_eq!(
        op(&mut h, notice(Some(2_250), None, at)),
        rejected("profit_fee_bps 2250 is outside ±50% of its launch value 1000")
    );
    let at = h.ts + NOTICE_MS;
    assert_eq!(op(&mut h, notice(Some(500), None, at)), vec![Event::AdminApplied]);
}

#[test]
fn parameters_that_leave_both_rates_alone_need_no_notice() {
    let mut h = Harness::v35();
    let cap = AdminCommand::SetParams(ProtocolParams { house_total_cap_bps: 15_000, ..base() });
    assert_eq!(op(&mut h, cap), vec![Event::AdminApplied]);
    assert_eq!(h.engine.params().map(|p| p.house_total_cap_bps), Some(15_000));
}

#[test]
fn a_pending_notice_survives_parameters_that_leave_its_rate_alone() {
    let mut h = Harness::v35();
    let effective_at = announce(&mut h, Some(1_200), None);
    h.advance_to(effective_at);
    h.operator_admin(AdminCommand::SetParams(ProtocolParams { house_total_cap_bps: 15_000, ..base() }));
    assert_eq!(h.engine.operator_bounds().notice.map(|n| n.profit_fee_bps), Some(Some(1_200)));
}

#[test]
fn before_v35_the_rates_move_without_notice_or_bounds() {
    let mut h = Harness::v33();
    assert_eq!(op(&mut h, rates(1_800, 9_000)), vec![Event::AdminApplied]);
    assert_eq!(h.engine.operator_bounds().launch, None);
    for command in
        [notice(Some(1_100), None, h.ts + NOTICE_MS), AdminCommand::RotateOperator { address: next_operator() }]
    {
        let reason = if matches!(command, AdminCommand::RotateOperator { .. }) {
            "an operator rotation"
        } else {
            "a parameter notice"
        };
        assert_eq!(op(&mut h, command), rejected(&format!("{reason} needs protocol V35")));
    }
}

#[test]
fn a_notice_and_a_rotation_are_refused_unsigned_and_before_parameters() {
    let late = notice(Some(1_200), None, common::start() + NOTICE_MS);
    let (_, outcomes) = bare(vec![late.clone(), AdminCommand::RotateOperator { address: next_operator() }]);
    for outcome in outcomes {
        assert_eq!(outcome, rejected("the operator signs a parameter notice or a rotation"));
    }
}

#[test]
fn a_signed_notice_with_no_parameters_set_is_refused() {
    let (mut engine, _) = bare(vec![]);
    let request = Client::operator().admin(0, notice(Some(1_200), None, common::start() + NOTICE_MS));
    let sequenced = Sequenced { seq: 3, ts: common::start(), cmd: request };
    assert_eq!(engine.apply(&sequenced).unwrap(), rejected("protocol params not set"));
}

#[test]
fn a_rotation_takes_effect_after_seven_days_and_the_old_key_signs_nothing_then() {
    let mut h = Harness::v35();
    let logged = h.ts;
    h.operator_admin(AdminCommand::RotateOperator { address: next_operator() });
    let effective_at = logged + NOTICE_MS;
    assert_eq!(h.engine.operator_bounds().rotation, Some(PendingRotation { address: next_operator(), effective_at }));
    // Until then the old key signs, and the new one does not.
    h.advance_to(effective_at - 1);
    assert_eq!(
        op_as(&mut h, NEXT_OPERATOR_SEED, AdminCommand::SetReduceOnly { market: Some(BTC), reduce_only: true }),
        rejected("invalid signature")
    );
    h.operator_admin(AdminCommand::SetReduceOnly { market: Some(BTC), reduce_only: true });
    assert_eq!(h.engine.operator().map(|o| o.address), Some(operator().address));
    // From the effective time the new key signs, and the old one does not.
    h.advance_to(effective_at);
    assert_eq!(
        op(&mut h, AdminCommand::SetReduceOnly { market: Some(BTC), reduce_only: false }),
        rejected("invalid signature")
    );
    assert!(h.engine.markets()[&BTC].reduce_only);
    let events =
        op_as(&mut h, NEXT_OPERATOR_SEED, AdminCommand::SetReduceOnly { market: Some(BTC), reduce_only: false });
    assert_eq!(events, vec![Event::AdminApplied]);
    assert!(!h.engine.markets()[&BTC].reduce_only);
    let operator_now = h.engine.operator().copied().unwrap();
    assert_eq!(
        (operator_now.address, operator_now.chain_id, operator_now.vault),
        (next_operator(), operator().chain_id, operator().vault)
    );
    assert_eq!(h.engine.operator_bounds().rotation, None);
    assert_eq!(h.engine.operator_bounds().rotations, 1);
    // The new key starts at its own nonce, and the old key's nonce moves no more.
    assert_eq!(h.engine.next_request_nonce(next_operator()), 1);
}

#[test]
fn a_rotation_takes_effect_before_the_command_at_its_time_is_authorised() {
    let mut h = Harness::v35();
    h.operator_admin(AdminCommand::RotateOperator { address: next_operator() });
    h.advance(NOTICE_MS);
    // No other command intervenes: the first command at the effective time is the new key's.
    let events =
        op_as(&mut h, NEXT_OPERATOR_SEED, AdminCommand::SetReduceOnly { market: Some(ETH), reduce_only: true });
    assert_eq!(events, vec![Event::AdminApplied]);
}

#[test]
fn a_new_rotation_replaces_the_pending_one_and_restarts_the_notice() {
    let mut h = Harness::v35();
    h.operator_admin(AdminCommand::RotateOperator { address: next_operator() });
    h.advance(3 * DAY_MS);
    let other = acct(0x77);
    h.operator_admin(AdminCommand::RotateOperator { address: other });
    assert_eq!(
        h.engine.operator_bounds().rotation,
        Some(PendingRotation { address: other, effective_at: h.ts + NOTICE_MS })
    );
    h.advance(4 * DAY_MS);
    // The first rotation's seven days have run, and the operator is still the original.
    assert_eq!(
        op_as(&mut h, NEXT_OPERATOR_SEED, AdminCommand::SetReduceOnly { market: Some(BTC), reduce_only: true }),
        rejected("invalid signature")
    );
    assert_eq!(h.engine.operator().map(|o| o.address), Some(operator().address));
}

#[test]
fn a_rotation_to_the_zero_address_or_the_operating_key_is_refused() {
    let mut h = Harness::v35();
    for address in [AccountId([0; 20]), operator().address] {
        let events = op(&mut h, AdminCommand::RotateOperator { address });
        assert_eq!(events, rejected("an operator rotation names a new, nonzero address"));
    }
    assert_eq!(h.engine.operator_bounds().rotation, None);
}

#[test]
fn rotated_operator_keeps_the_bounds_and_can_rotate_again() {
    let mut h = Harness::v35();
    h.operator_admin(AdminCommand::RotateOperator { address: next_operator() });
    h.advance(NOTICE_MS);
    let at = h.ts + NOTICE_MS;
    let first = op_as(&mut h, NEXT_OPERATOR_SEED, notice(Some(1_700), None, at));
    assert_eq!(first, rejected("profit_fee_bps 1700 is outside ±50% of its launch value 1000"));
    let events = op_as(&mut h, NEXT_OPERATOR_SEED, AdminCommand::RotateOperator { address: operator().address });
    assert_eq!(events, vec![Event::AdminApplied]);
}

#[test]
fn pending_notices_and_rotations_are_in_the_state_root_and_survive_a_snapshot() {
    let mut h = Harness::v35();
    // Engines that differ only in the pending notice, or the pending rotation, have different roots.
    let root_after = |fee, address| {
        let mut other = Harness::v35();
        announce(&mut other, Some(fee), None);
        other.operator_admin(AdminCommand::RotateOperator { address });
        other.engine.state_root()
    };
    assert_ne!(root_after(1_200, next_operator()), root_after(1_300, next_operator()));
    assert_ne!(root_after(1_200, next_operator()), root_after(1_200, acct(0x77)));
    assert_eq!(root_after(1_200, next_operator()), {
        announce(&mut h, Some(1_200), None);
        h.operator_admin(AdminCommand::RotateOperator { address: next_operator() });
        h.engine.state_root()
    });
    // A snapshot restores the pending state, and both engines go on to the same roots.
    let mut restored: Engine = serde_json::from_slice(&serde_json::to_vec(&h.engine).unwrap()).unwrap();
    assert_eq!(restored.operator_bounds(), h.engine.operator_bounds());
    restored.restore_indexes();
    h.advance(NOTICE_MS);
    let next = Sequenced {
        seq: h.log.len() as u64,
        ts: h.ts,
        cmd: Client { expires_at: h.ts + 60_000, ..Client::new(NEXT_OPERATOR_SEED) }
            .admin(0, AdminCommand::SetReduceOnly { market: Some(BTC), reduce_only: true }),
    };
    let from_restored = restored.apply(&next).unwrap();
    assert_eq!(from_restored, h.cmd(next.cmd));
    assert_eq!(from_restored.last(), Some(&Event::AdminApplied));
    assert_eq!(restored.state_root(), h.engine.state_root());
    assert_eq!(restored.operator_bounds().rotations, 1);
}
