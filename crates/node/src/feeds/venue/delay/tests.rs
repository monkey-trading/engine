use super::*;

const MS: Duration = Duration::from_millis(1);

/// A probe with one ping sent at `at` for each offset in `pings`, and their payloads.
fn pinged(at: Instant, pings: &[u32]) -> (DelayProbe, Vec<Vec<u8>>) {
    let mut probe = DelayProbe::default();
    let payloads = pings.iter().map(|ms| probe.sent(at + MS * *ms)).collect();
    (probe, payloads)
}

#[test]
fn nothing_bounds_the_stream_before_a_ping_is_answered() {
    let at = Instant::now();
    let (probe, _) = pinged(at, &[0]);
    assert_eq!(probe.behind(at + MS * 5), None);
    assert_eq!(probe.waiting(at + MS * 5), Some(MS * 5));
}

#[test]
fn the_stream_may_be_as_far_behind_as_the_newest_answered_ping_is_old() {
    let at = Instant::now();
    let (mut probe, payloads) = pinged(at, &[0, 100]);
    probe.answered(&payloads[0]);
    assert_eq!(probe.behind(at + MS * 150), Some(MS * 150));
    assert_eq!(probe.waiting(at + MS * 150), Some(MS * 50));
    probe.answered(&payloads[1]);
    assert_eq!(probe.behind(at + MS * 150), Some(MS * 50));
    assert_eq!(probe.waiting(at + MS * 150), None);
}

#[test]
fn a_pong_answers_the_pings_before_it() {
    let at = Instant::now();
    let (mut probe, payloads) = pinged(at, &[0, 100, 200]);
    probe.answered(&payloads[1]);
    assert_eq!(probe.behind(at + MS * 100), Some(Duration::ZERO));
    assert_eq!(probe.waiting(at + MS * 200), Some(Duration::ZERO), "only the third still waits");
}

#[test]
fn a_late_answer_to_an_older_ping_does_not_move_the_bound_back() {
    let at = Instant::now();
    let (mut probe, payloads) = pinged(at, &[0, 100]);
    probe.answered(&payloads[1]);
    probe.answered(&payloads[0]);
    assert_eq!(probe.behind(at + MS * 100), Some(Duration::ZERO));
}

#[test]
fn a_pong_for_no_ping_sent_bounds_nothing() {
    let at = Instant::now();
    let (mut probe, _) = pinged(at, &[0]);
    // Unsolicited: empty, the wrong length, or naming a ping not sent yet.
    for payload in [&[][..], &[1, 2, 3][..], &u64::MAX.to_be_bytes()[..], &1u64.to_be_bytes()[..]] {
        probe.answered(payload);
        assert_eq!(probe.behind(at), None, "{payload:?}");
    }
    assert_eq!(probe.waiting(at), Some(Duration::ZERO), "the ping still waits");
}
