use engine_core::Fixed;
use engine_core::event::Event;
use engine_core::financing::{ChargesTree, FinancingRow};
use tokio::sync::watch;

use super::*;
use crate::api::tests::api_state;
use crate::history::derived::{Derived, DerivedRows};
use crate::log::Manifest;

#[tokio::test]
async fn an_accounts_charges_at_each_rollover_of_a_durable_command_prove_against_the_logged_roots() {
    let (first, second) = (rows(&[1, 2, 3]), rows(&[2, 3]));
    let events = [aggregate(&first), Event::AdminApplied, aggregate(&second)];
    let derived = Derived::of(9, &events, [first.clone(), second.clone()].concat()).unwrap().expect("rows");
    let (state, kept, flushed) = state();
    kept.send_modify(|kept| kept.keep(9, derived));
    flushed.send_modify(|manifest| manifest.flushed = Some(Offset::start_of(9)));

    let view = read(&state, account(2), 9).await.expect("kept and durable");
    assert_eq!(view.seq, 9);
    let roots = [ChargesTree::new(&first).root(), ChargesTree::new(&second).root()];
    for ((rollover, root), index) in view.rollovers.iter().zip(roots).zip([0, 2]) {
        assert_eq!((rollover.event, rollover.root), (Offset { seq: 9, index }, root));
        let proof = rollover.proof.as_ref().expect("the account is charged at both");
        assert!(proof.verify(rollover.root));
        assert_eq!(proof.charges.len(), 1);
    }
    // An account charged at the first rollover alone has no proof at the second.
    let view = read(&state, account(1), 9).await.expect("kept and durable");
    assert!(view.rollovers[0].proof.as_ref().is_some_and(|proof| proof.verify(view.rollovers[0].root)));
    assert!(view.rollovers[1].proof.is_none());
}

#[tokio::test]
async fn a_command_not_yet_durable_or_whose_rows_are_not_kept_is_not_found() {
    let rows = rows(&[1]);
    let (state, kept, flushed) = state();
    kept.send_modify(|kept| kept.keep(9, Derived::of(9, &[aggregate(&rows)], rows.clone()).unwrap().expect("rows")));
    flushed.send_modify(|manifest| manifest.flushed = Some(Offset::start_of(8)));
    assert!(matches!(read(&state, account(1), 9).await, Err(ApiError::NotFound)), "not durable yet");
    flushed.send_modify(|manifest| manifest.flushed = Some(Offset::start_of(10)));
    assert!(read(&state, account(1), 9).await.is_ok());
    assert!(matches!(read(&state, account(1), 10).await, Err(ApiError::NotFound)), "durable, but no rows kept");
}

#[tokio::test]
async fn a_read_waits_for_a_permit_the_logs_other_reads_share() {
    let rows = rows(&[1]);
    let (mut state, kept, flushed) = state();
    kept.send_modify(|kept| kept.keep(9, Derived::of(9, &[aggregate(&rows)], rows.clone()).unwrap().expect("rows")));
    flushed.send_modify(|manifest| manifest.flushed = Some(Offset::start_of(9)));
    state.backfills = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
    let waiting = tokio::time::timeout(std::time::Duration::from_millis(100), read(&state, account(1), 9)).await;
    assert!(waiting.is_err(), "no permit, no read");
    state.backfills.add_permits(1);
    assert!(read(&state, account(1), 9).await.is_ok());
    assert_eq!(state.backfills.available_permits(), 1, "the permit is back once the read is done");
}

// ── Helpers ──

/// An API state whose kept rows and published manifest the test sets.
fn state() -> (ApiState, watch::Sender<DerivedRows>, watch::Sender<Manifest>) {
    let (mut state, _rx) = api_state(true);
    let (kept, derived) = watch::channel(DerivedRows::default());
    let (flushed, manifest) = watch::channel(Manifest::default());
    state.history = state.history.with_derived(derived, None);
    state.manifest = manifest;
    (state, kept, flushed)
}

/// The read's answer for `account` at `seq`.
async fn read(state: &ApiState, account: AccountId, seq: u64) -> Result<FinancingView, ApiError> {
    financing(State(state.clone()), Path((format!("{account}"), seq))).await.map(|answer| answer.0)
}

fn account(seed: u8) -> AccountId {
    AccountId([seed; 20])
}

/// One rollover's rows: a charge to each of `seeds`, in ascending order.
fn rows(seeds: &[u8]) -> Vec<FinancingRow> {
    seeds
        .iter()
        .map(|seed| FinancingRow { account: account(*seed), market: 1, amount: Fixed::from_int(i64::from(*seed)) })
        .collect()
}

/// The aggregate a rollover over `rows` logs.
fn aggregate(rows: &[FinancingRow]) -> Event {
    let positions = rows.len() as u64;
    let root = ChargesTree::new(rows).root();
    Event::FinancingApplied { at: 0, span: 0, positions, accounts: positions, net: Fixed::ZERO, root }
}
