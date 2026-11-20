use super::*;
use engine_core::Fixed;
use engine_core::access::{AccessEvidence, EligibilityClass, SourceFailure};
use engine_core::event::CancelReason;
use engine_core::types::AccountId;

const ACCOUNT: AccountId = AccountId([1; 20]);

/// A carrier access command: health reads the events, not the command's kind.
fn command() -> Command {
    Command::RetryDepositScreening {
        chain_ref: "ref".into(),
        evidence: AccessEvidence::Failed { reason: SourceFailure::Unavailable },
    }
}

fn note(state: &mut AccessAlerts, events: &[Event]) {
    state.note(&Engine::default(), &command(), events);
}

fn refused(reason: AccessReject) -> Event {
    Event::WithdrawalRejected { account: ACCOUNT, amount: Fixed::ONE, reason: Reject::Access { reason } }
}

fn approved() -> [Event; 2] {
    [
        Event::WithdrawalApproved { account: ACCOUNT, amount: Fixed::ONE, nonce: 1 },
        Event::WithdrawalQueueUpdated { account: ACCOUNT, requested_seq: 1, remaining: Fixed::ZERO },
    ]
}

#[test]
fn source_failure_episodes_survive_unauthenticated_requests_and_resolve_on_verified_decisions() {
    let (logs, _guard) = crate::trusted_time::tests::logs::Logs::capture();
    // Every client action screened at the API: an order, a withdrawal and a declaration.
    let refusals: [fn(Reject) -> Event; 3] = [
        |reason| Event::OrderRejected { account: AccountId([1; 20]), client_id: 1, reason },
        |reason| Event::WithdrawalRejected { account: AccountId([1; 20]), amount: Fixed::ONE, reason },
        |reason| Event::EligibilityRejected { account: AccountId([1; 20]), reason },
    ];
    for (episode, rejected) in (1..).zip(refusals) {
        let mut state = AccessAlerts::default();
        note(&mut state, &[rejected(Reject::Access { reason: AccessReject::SourceUnavailable })]);
        assert_eq!(logs.count("alert=\"access_source_failed\""), episode);
        assert!(state.failed);
        note(&mut state, &[rejected(Reject::InvalidSignature)]);
        assert!(state.failed, "a forged claimant says nothing about source recovery");
        note(&mut state, &[rejected(Reject::Access { reason: AccessReject::AlreadyAttested })]);
        assert!(state.failed, "a repeat declaration says nothing about source recovery");
        assert_eq!(logs.count("alert_resolved=\"access_source_failed\""), episode - 1);
        note(&mut state, &[rejected(Reject::Access { reason: AccessReject::UnitedStates })]);
        assert!(!state.failed, "a verified excluding decision still proves the source recovered");
        note(&mut state, &[rejected(Reject::Access { reason: AccessReject::UnitedStates })]);
        assert!(!state.failed);
        assert_eq!(logs.count("alert_resolved=\"access_source_failed\""), episode);
    }
    let mut state = AccessAlerts { failed: true, since: 0 };
    note(&mut state, &[Event::EligibilityAttested { account: ACCOUNT, class: EligibilityClass::Professional }]);
    assert!(!state.failed, "a recorded declaration is a verified fresh decision");
}

#[test]
fn an_approval_of_an_older_queued_withdrawal_neither_resolves_nor_reopens_an_episode() {
    let (logs, _guard) = crate::trusted_time::tests::logs::Logs::capture();
    // The refused command also approves the FIFO head: its own result is the refusal.
    let mut state = AccessAlerts::default();
    let events = [vec![refused(AccessReject::SourceUnavailable)], approved().to_vec()].concat();
    note(&mut state, &events);
    assert!(state.failed, "the approval must not clear the outage the same command raised");
    assert_eq!(logs.count("alert_resolved=\"access_source_failed\""), 0);
    // A command with no screening result of its own changes nothing, in either state.
    note(&mut state, &approved());
    assert!(state.failed);
    let mut healthy = AccessAlerts::default();
    note(&mut healthy, &approved());
    assert!(!healthy.failed);
    // Its own screened request is the verified result, queued or approved at once.
    for queued in [
        vec![Event::WithdrawalQueued { account: ACCOUNT, amount: Fixed::ONE, requested_seq: 2 }],
        [vec![Event::WithdrawalQueued { account: ACCOUNT, amount: Fixed::ONE, requested_seq: 2 }], approved().to_vec()]
            .concat(),
    ] {
        let mut state = AccessAlerts { failed: true, since: 0 };
        note(&mut state, &queued);
        assert!(!state.failed);
    }
    assert_eq!(logs.count("alert_resolved=\"access_source_failed\""), 2);
}

#[test]
fn a_close_all_drives_the_source_episode_by_its_own_screening() {
    use engine_core::command::{CloseAllRequest, CloseEntry};
    let (logs, _guard) = crate::trusted_time::tests::logs::Logs::capture();
    let close_all = Command::AccessCloseAll {
        request: CloseAllRequest {
            account: ACCOUNT,
            closes: vec![CloseEntry { market: 1, quoted: Fixed::ONE }],
            max_slippage_bps: None,
            auth: None,
        },
        evidence: AccessEvidence::Failed { reason: SourceFailure::Unavailable },
    };
    let rejected = |reason| Event::OrderRejected { account: ACCOUNT, client_id: 0, reason };
    let note = |state: &mut AccessAlerts, cmd: &Command, events: &[Event]| state.note(&Engine::default(), cmd, events);

    let mut state = AccessAlerts::default();
    note(&mut state, &close_all, &[rejected(Reject::Access { reason: AccessReject::SourceUnavailable })]);
    assert!(state.failed, "a refused close-all opens the episode");
    assert_eq!(logs.count("alert=\"access_source_failed\""), 1);
    // Before `V44` the request reads no evidence, so it says nothing about the source.
    note(&mut state, &close_all, &[rejected(Reject::BatchUnsupported)]);
    assert!(state.failed);
    // Every outcome after a passed screening resolves it: an entry rejection, a refused list,
    // an acceptance.
    for (resolved, outcome) in (1..).zip([
        rejected(Reject::ReduceOnly),
        rejected(Reject::InvalidOrder { reason: "no markets to close".into() }),
        Event::OrderAccepted { account: ACCOUNT, client_id: 0, order_id: 1 },
    ]) {
        state.failed = true;
        note(&mut state, &close_all, &[outcome]);
        assert!(!state.failed);
        assert_eq!(logs.count("alert_resolved=\"access_source_failed\""), resolved);
    }
    // A cancel-all is not screened: its events say nothing about the source.
    let cancel_all = Command::CancelAll { account: ACCOUNT, auth: None };
    state.failed = true;
    note(
        &mut state,
        &cancel_all,
        &[Event::OrderCancelled { account: ACCOUNT, order_id: 1, reason: CancelReason::Client }],
    );
    assert!(state.failed);
}

#[test]
fn a_levels_request_drives_the_source_episode_by_its_own_screening() {
    use engine_core::command::PositionLevelsRequest;
    let (logs, _guard) = crate::trusted_time::tests::logs::Logs::capture();
    let levels = Command::AccessSetPositionLevels {
        request: PositionLevelsRequest {
            account: ACCOUNT,
            market: 1,
            take_profit: None,
            stop_loss: Some(Fixed::ONE),
            auth: None,
        },
        evidence: AccessEvidence::Failed { reason: SourceFailure::Unavailable },
    };
    let rejected = |reason| Event::OrderRejected { account: ACCOUNT, client_id: 0, reason };
    let note = |state: &mut AccessAlerts, events: &[Event]| state.note(&Engine::default(), &levels, events);

    let mut state = AccessAlerts::default();
    note(&mut state, &[rejected(Reject::Access { reason: AccessReject::SourceUnavailable })]);
    assert!(state.failed, "a refused levels request opens the episode");
    assert_eq!(logs.count("alert=\"access_source_failed\""), 1);
    // Before `V79` the request reads no evidence, so it says nothing about the source.
    note(&mut state, &[rejected(Reject::LevelsUnsupported)]);
    assert!(state.failed);
    // Every outcome after a passed screening resolves it: a level refused, and levels set.
    let too_close = Reject::LevelTooClose { level: Fixed::ONE, min_distance_bps: 30, reference: Fixed::ONE };
    let set = Event::PositionLevelsSet { account: ACCOUNT, market: 1, take_profit: None, stop_loss: None };
    for (resolved, outcome) in (1..).zip([rejected(too_close), set]) {
        state.failed = true;
        note(&mut state, &[outcome]);
        assert!(!state.failed);
        assert_eq!(logs.count("alert_resolved=\"access_source_failed\""), resolved);
    }
}

#[test]
fn a_failed_screening_in_a_reorganisation_outweighs_a_passed_one() {
    use alloy::primitives::B256;
    use engine_core::deposits::{DepositBeneficiary, DepositStatus, ReceiptMove};
    let receipt = |byte: u8| DepositReceipt {
        block: u64::from(byte),
        block_hash: B256::repeat_byte(byte),
        tx_hash: B256::repeat_byte(byte),
        log_index: 0,
        transaction_deposit_index: 0,
        deposit_id: u64::from(byte),
        beneficiary: DepositBeneficiary::Trader { account: ACCOUNT },
        amount: Fixed::ONE,
    };
    let moved = |byte: u8| ReceiptMove {
        receipt: receipt(byte),
        observed_at_seq: 0,
        replacement: receipt(byte + 1),
        replacement_reversed_at_seq: None,
        payer: None,
        evidence: None,
    };
    let reorganisation = Command::CustodyReorganisation { moves: vec![moved(1), moved(3)] };
    let credited = Event::CustodyDepositChanged {
        receipt: receipt(2),
        status: DepositStatus::Unfinalised,
        principal_delta: Fixed::ONE,
        uncredited: None,
    };
    let pending = Event::DepositPending {
        chain_ref: receipt(4).reference(),
        account: Some(ACCOUNT),
        payer: None,
        amount: Fixed::ONE,
        reason: AccessReject::SourceUnavailable,
    };
    for events in [[credited.clone(), pending.clone()], [pending, credited.clone()]] {
        let mut state = AccessAlerts::default();
        state.note(&Engine::default(), &reorganisation, &events);
        assert!(state.failed, "{events:?}");
    }
    let mut state = AccessAlerts { failed: true, since: 0 };
    state.note(&Engine::default(), &reorganisation, &[credited]);
    assert!(!state.failed, "a reorganisation whose screenings all passed resolves the episode");
}

#[test]
fn a_payer_less_hold_leaves_the_source_episode_as_it_was() {
    let hold = |reason| Event::DepositHeld {
        chain_ref: "legacy".into(),
        account: Some(ACCOUNT),
        payer: None,
        amount: Fixed::ONE,
        reason,
    };
    let held = hold(AccessReject::PayerMissing);
    for failed in [true, false] {
        let mut state = AccessAlerts { failed, since: 0 };
        note(&mut state, std::slice::from_ref(&held));
        assert_eq!(state.failed, failed, "a hold decided before any evidence is read");
    }
    let sanctioned = hold(AccessReject::PayerSanctioned);
    let mut state = AccessAlerts { failed: true, since: 0 };
    note(&mut state, &[sanctioned]);
    assert!(!state.failed, "a verified sanction hold proves the source answered");
}

#[test]
fn a_decision_observed_before_the_failure_resolves_nothing_and_a_later_one_does() {
    let (logs, _guard) = crate::trusted_time::tests::logs::Logs::capture();
    let mut engine = Engine::default();
    let tick = engine_core::command::Sequenced { seq: 0, ts: 10_000, cmd: Command::Tick };
    engine.apply(&tick).expect("in sequence");
    let mut state = AccessAlerts::default();
    state.note(&engine, &command(), &[refused(AccessReject::SourceUnavailable)]);
    assert!(state.failed);
    // An order on a decision the API kept from before the failure, then one observed after it.
    let accepted = [Event::OrderAccepted { account: ACCOUNT, client_id: 1, order_id: 1 }];
    for (observed_at, failed) in [(9_999, true), (10_000, false)] {
        state.note(&engine, &order_on(observed_at), &accepted);
        assert_eq!(state.failed, failed, "observed at {observed_at}");
    }
    assert_eq!(logs.count("alert_resolved=\"access_source_failed\""), 1);
}

/// An access order carrying a decision observed at `observed_at`.
fn order_on(observed_at: Millis) -> Command {
    let screening = engine_core::access::Screening {
        source: ACCOUNT,
        request_hash: None,
        observed_at,
        account: Some(ACCOUNT),
        payer: None,
        jurisdiction: engine_core::access::Jurisdiction::Allowed,
        account_status: engine_core::access::SanctionsStatus::Clear,
        payer_status: engine_core::access::SanctionsStatus::Clear,
        signature: engine_core::eip712::Signature::ZERO,
    };
    let request = engine_core::command::OrderRequest {
        account: ACCOUNT,
        client_id: 1,
        market: 1,
        side: engine_core::types::Side::Buy,
        qty: Fixed::ONE,
        kind: engine_core::command::OrderKind::Limit { price: Fixed::ONE },
        reduce_only: false,
        auth: None,
        take_profit: None,
        stop_loss: None,
    };
    Command::AccessOrder { request, evidence: AccessEvidence::Screened(screening) }
}
