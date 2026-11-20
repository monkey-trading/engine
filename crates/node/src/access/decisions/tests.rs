use super::*;

use std::collections::BTreeSet;

use engine_core::access::{Jurisdiction, SanctionsStatus};
use engine_core::eip712::{B256, Signature};
use k256::ecdsa::SigningKey;

/// The time the tests start at, and the bound the config records: 30 s, so a decision is attached
/// until 27 s after it was observed. With a 2 s timeout and 1 s turns, one observed as it arrives is
/// due a refresh 21 s after it arrived, two attempts before it stops being attached, and a failed one
/// is tried again a turn later.
const T0: Millis = 1_792_504_800_000;
const MAX_AGE: Millis = 30_000;
const USABLE: Millis = 27_000;
const REFRESH: Millis = 21_000;
const TURN: Millis = 1_000;

#[test]
fn a_kept_decision_backs_only_its_own_account_and_address() {
    let decisions = decisions(8);
    let (a, b, x, y) = (account(1), account(2), peer(1), peer(2));
    assert!(decisions.hit(a, x, T0).is_none(), "nothing kept before the first answer");
    assert!(decisions.store(a, x, &decision(a, T0), &domain(), T0, true));
    assert_eq!(decisions.hit(a, x, T0).map(|screening| screening.account), Some(Some(a)));
    assert!(decisions.hit(b, x, T0).is_none(), "another account at the address");
    assert!(decisions.hit(a, y, T0).is_none(), "the account at another address");
    // An answer about another account is never kept for the pair.
    assert!(!decisions.store(b, y, &decision(a, T0), &domain(), T0, true));
    assert!(decisions.hit(b, y, T0).is_none());
}

#[test]
fn a_decision_is_attached_while_a_tenth_of_its_bound_is_left_and_never_after() {
    let decisions = decisions(8);
    let (a, x) = (account(1), peer(1));
    assert!(decisions.store(a, x, &decision(a, T0), &domain(), T0, true));
    assert!(decisions.hit(a, x, T0 + USABLE).is_some());
    assert!(decisions.hit(a, x, T0 + USABLE + 1).is_none());
    assert!(decisions.hit(a, x, T0 + MAX_AGE).is_none(), "not even inside the engine's bound");
    // An answer observed too long ago to attach is not kept.
    let now = T0 + 2 * MAX_AGE;
    assert!(!decisions.store(a, x, &decision(a, now - USABLE - 1), &domain(), now, true));
    assert!(decisions.store(a, x, &decision(a, now - USABLE), &domain(), now, true));
    assert!(decisions.hit(a, x, now).is_some());
    assert_eq!(decisions.len(), 1, "the expired decision is forgotten");
}

#[test]
fn only_a_decision_that_verifies_as_the_engine_checks_it_is_kept() {
    let (a, x) = (account(1), peer(1));
    let earlier = SigningKey::from_slice(&[0x78; 32]).unwrap();
    let cases: [(&str, Screening); 7] = [
        ("signed by an earlier source key", signed_by(&earlier, unsigned(a, T0))),
        ("naming an earlier source", signed_by(&earlier, Screening { source: address(&earlier), ..unsigned(a, T0) })),
        ("naming a request", signed(Screening { request_hash: Some(B256::repeat_byte(1)), ..unsigned(a, T0) })),
        ("naming a payer", signed(Screening { payer: Some(a), ..unsigned(a, T0) })),
        ("about no account", signed(Screening { account: None, ..unsigned(a, T0) })),
        ("stamped past the lead", signed(unsigned(a, T0 + MAX_EVIDENCE_LEAD_MS + 1))),
        ("unsigned", unsigned(a, T0)),
    ];
    for (case, screening) in cases {
        let decisions = decisions(8);
        assert!(!decisions.store(a, x, &screening, &domain(), T0, true), "{case}");
        assert!(decisions.hit(a, x, T0).is_none(), "{case}");
    }
    let decisions = decisions(8);
    assert!(decisions.store(a, x, &decision(a, T0 + MAX_EVIDENCE_LEAD_MS), &domain(), T0, true));
}

#[test]
fn a_refused_decision_is_kept_and_reused_like_a_clear_one() {
    let decisions = decisions(8);
    let (a, x) = (account(1), peer(1));
    let refused = signed(Screening {
        jurisdiction: Jurisdiction::UnitedStates,
        account_status: SanctionsStatus::Sanctioned,
        ..unsigned(a, T0)
    });
    assert!(decisions.store(a, x, &refused, &domain(), T0, true));
    for now in [T0, T0 + 1_000, T0 + USABLE] {
        assert_eq!(decisions.hit(a, x, now), Some(refused.clone()));
    }
}

#[test]
fn a_newer_decision_replaces_an_older_one_and_never_the_reverse() {
    let decisions = decisions(8);
    let (a, x) = (account(1), peer(1));
    let newer = decision(a, T0 + 1_000);
    assert!(decisions.store(a, x, &newer, &domain(), T0 + 1_000, true));
    // A refresh that left before the miss that kept `newer` lands after it.
    let refused = signed(Screening { account_status: SanctionsStatus::Sanctioned, ..unsigned(a, T0) });
    assert!(!decisions.store(a, x, &refused, &domain(), T0 + 1_100, false));
    assert_eq!(decisions.hit(a, x, T0 + 1_200), Some(newer));
    let newest = decision(a, T0 + 2_000);
    assert!(decisions.store(a, x, &newest, &domain(), T0 + 2_000, false));
    assert_eq!(decisions.hit(a, x, T0 + 2_000), Some(newest));
}

#[test]
fn the_pairs_kept_are_bounded_and_an_expired_one_makes_room() {
    let decisions = decisions(2);
    let pairs = [(account(1), peer(1)), (account(1), peer(2)), (account(2), peer(1))];
    for (index, (account, peer)) in pairs.into_iter().enumerate() {
        assert_eq!(decisions.store(account, peer, &decision(account, T0), &domain(), T0, true), index < 2);
    }
    assert!(decisions.hit(account(2), peer(1), T0).is_none(), "a pair past the bound is screened per request");
    // A kept pair's newer decision replaces its own in place.
    assert!(decisions.store(account(1), peer(1), &decision(account(1), T0 + 1), &domain(), T0 + 1, true));
    assert_eq!(decisions.len(), 2);
    // Once the second pair's decision runs out, the third pair is kept in its place.
    let later = T0 + USABLE + 1;
    assert!(decisions.store(account(2), peer(1), &decision(account(2), later), &domain(), later, true));
    assert_eq!(decisions.len(), 2);
    assert!(decisions.hit(account(1), peer(2), later).is_none());
}

#[test]
fn an_active_pair_is_refreshed_before_its_decision_stops_and_an_idle_one_waits_for_its_next_request() {
    let decisions = decisions(8);
    let (a, b) = ((account(1), peer(1)), (account(2), peer(2)));
    for (account, peer) in [a, b] {
        assert!(decisions.store(account, peer, &decision(account, T0), &domain(), T0, true));
    }
    assert!(decisions.due(T0 + REFRESH - 1, 8).is_empty());
    assert_eq!(set(decisions.due(T0 + REFRESH, 8)), set(vec![a, b]));
    assert!(decisions.due(T0 + REFRESH, 8).is_empty(), "a refresh in flight is not due again");
    let refreshed = T0 + REFRESH;
    for (account, peer) in [a, b] {
        assert!(decisions.store(account, peer, &decision(account, refreshed), &domain(), refreshed, false));
        decisions.refreshed((account, peer), refreshed);
    }
    // `a` sends a request; `b` sent its last at T0, more than a bound before the next refresh is due.
    assert!(decisions.hit(a.0, a.1, T0 + MAX_AGE).is_some());
    assert_eq!(decisions.due(refreshed + REFRESH, 8), vec![a]);
    // Its next request schedules `b` again, from its decision's own time.
    assert!(decisions.hit(b.0, b.1, refreshed + REFRESH + 1).is_some());
    assert_eq!(decisions.due(refreshed + REFRESH + 1, 8), vec![b]);
}

#[test]
fn a_turn_takes_at_most_its_limit_and_leaves_the_rest_due() {
    let decisions = decisions(8);
    let pairs: Vec<Pair> = (1..=3).map(|n| (account(n), peer(n))).collect();
    for (account, peer) in &pairs {
        assert!(decisions.store(*account, *peer, &decision(*account, T0), &domain(), T0, true));
    }
    let first = decisions.due(T0 + REFRESH, 2);
    assert_eq!(first.len(), 2);
    let rest = decisions.due(T0 + REFRESH, 2);
    assert_eq!(set([first, rest].concat()), set(pairs));
}

#[test]
fn a_failed_refresh_tries_again_on_the_next_turn_while_the_decision_lasts() {
    let decisions = decisions(8);
    let (a, x) = (account(1), peer(1));
    assert!(decisions.store(a, x, &decision(a, T0), &domain(), T0, true));
    assert_eq!(decisions.due(T0 + REFRESH, 8), vec![(a, x)]);
    decisions.refreshed((a, x), T0 + REFRESH);
    assert!(decisions.due(T0 + REFRESH + TURN - 1, 8).is_empty());
    assert_eq!(decisions.due(T0 + REFRESH + TURN, 8), vec![(a, x)]);
    decisions.refreshed((a, x), T0 + REFRESH + TURN);
    assert_eq!(decisions.due(T0 + REFRESH + 2 * TURN, 8), vec![(a, x)], "and again a turn later");
    decisions.refreshed((a, x), T0 + REFRESH + 2 * TURN);
    // Once the decision runs out it is forgotten, and nothing is due for it.
    assert!(decisions.due(T0 + USABLE + 1, 8).is_empty());
    assert_eq!(decisions.len(), 0);
}

#[test]
fn a_decision_observed_long_before_it_arrived_is_refreshed_before_it_stops_being_attached() {
    let decisions = decisions(8);
    let (a, x) = (account(1), peer(1));
    // Observed 20 s before it arrives, it is attached for 7 s more, and due a refresh halfway through.
    assert!(decisions.store(a, x, &decision(a, T0 - 20_000), &domain(), T0, true));
    assert!(decisions.due(T0 + 3_499, 8).is_empty());
    assert_eq!(decisions.due(T0 + 3_500, 8), vec![(a, x)]);
    assert!(decisions.hit(a, x, T0 + 7_000).is_some(), "still attached while the refresh is in flight");
}

#[test]
fn a_refresh_racing_an_expiry_never_attaches_the_expired_decision() {
    let decisions = decisions(8);
    let (a, x) = (account(1), peer(1));
    assert!(decisions.store(a, x, &decision(a, T0), &domain(), T0, true));
    assert_eq!(decisions.due(T0 + REFRESH, 8), vec![(a, x)]);
    // The source is slow: the decision runs out before the refresh lands.
    assert!(decisions.hit(a, x, T0 + USABLE + 1).is_none());
    let late = T0 + USABLE + 500;
    let refreshed = decision(a, T0 + REFRESH + 1_000);
    assert!(decisions.store(a, x, &refreshed, &domain(), late, false));
    decisions.refreshed((a, x), late);
    assert_eq!(decisions.hit(a, x, late), Some(refreshed));
    assert!(decisions.hit(a, x, T0 + REFRESH + 1_000 + USABLE + 1).is_none());
}

#[test]
fn a_refresh_landing_after_the_old_decision_expired_keeps_the_pair_active() {
    let decisions = decisions(8);
    let (a, x) = (account(1), peer(1));
    assert!(decisions.store(a, x, &decision(a, T0), &domain(), T0, true));
    assert_eq!(decisions.due(T0 + REFRESH, 8), vec![(a, x)]);
    assert!(decisions.hit(a, x, T0 + 26_500).is_some());
    // The refresh returns a decision observed after the old one's attachment deadline.
    let landed = T0 + 27_500;
    assert!(decisions.store(a, x, &decision(a, landed), &domain(), landed, false));
    decisions.refreshed((a, x), landed);
    assert!(decisions.lock().lapsed.is_empty(), "the lapsed activity is dropped once the refresh ends");
    // Its refresh time is 16.5 s after the pair's last request, so it is refreshed with no request between.
    let next = landed + USABLE - 2 * (2_000 + TURN);
    assert_eq!(decisions.due(next, 8), vec![(a, x)]);
}

#[test]
fn a_refresh_that_fails_after_the_decision_expired_leaves_no_activity_behind() {
    let decisions = decisions(8);
    let (a, x) = (account(1), peer(1));
    assert!(decisions.store(a, x, &decision(a, T0), &domain(), T0, true));
    assert_eq!(decisions.due(T0 + REFRESH, 8), vec![(a, x)]);
    let expired = T0 + USABLE + 1;
    assert!(decisions.due(expired, 8).is_empty());
    assert_eq!(decisions.lock().lapsed.len(), 1, "kept while the refresh is in flight");
    decisions.refreshed((a, x), expired);
    assert!(decisions.lock().lapsed.is_empty());
}

// ── Helpers ──

/// Decisions checked against `source()` and a 30 s bound, keeping at most `capacity` pairs.
fn decisions(capacity: usize) -> Decisions {
    let config: AccessSection = toml::from_str(&format!(
        "source='{}'\nendpoint='http://127.0.0.1:9'\ntimeout='2s'\nmax_age='30s'\nmax_response_bytes=4096\n\
         max_in_flight=8\nmax_in_flight_per_account=2\nmax_in_flight_per_client=4\nreserved_in_flight=2",
        address(&source()),
    ))
    .unwrap();
    config.validate().unwrap();
    Decisions::with_capacity(&config, capacity)
}

fn source() -> SigningKey {
    SigningKey::from_slice(&[0x77; 32]).unwrap()
}

fn address(key: &SigningKey) -> AccountId {
    eip712::address(key.verifying_key())
}

fn domain() -> Eip712Domain {
    eip712::domain(31_337, AccountId([0xaa; 20]))
}

fn account(n: u8) -> AccountId {
    AccountId([n; 20])
}

fn peer(n: u8) -> IpAddr {
    IpAddr::from([203, 0, 113, n])
}

/// The source's allowed and clear decision for `account` observed at `observed_at`, unsigned.
fn unsigned(account: AccountId, observed_at: Millis) -> Screening {
    Screening {
        source: address(&source()),
        request_hash: None,
        observed_at,
        account: Some(account),
        payer: None,
        jurisdiction: Jurisdiction::Allowed,
        account_status: SanctionsStatus::Clear,
        payer_status: SanctionsStatus::Clear,
        signature: Signature::ZERO,
    }
}

/// `unsigned(account, observed_at)` signed by the source.
fn decision(account: AccountId, observed_at: Millis) -> Screening {
    signed(unsigned(account, observed_at))
}

fn signed(screening: Screening) -> Screening {
    signed_by(&source(), screening)
}

fn signed_by(key: &SigningKey, mut screening: Screening) -> Screening {
    screening.signature = eip712::sign(key, &eip712::screening_hash(&domain(), &screening));
    screening
}

fn set(pairs: Vec<Pair>) -> BTreeSet<Pair> {
    pairs.into_iter().collect()
}
