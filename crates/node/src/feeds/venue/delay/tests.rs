use super::*;

const MS: Duration = Duration::from_millis(1);

/// A probe with one ping sent at `at` for each offset in `pings`, and their payloads.
fn pinged(at: Instant, pings: &[u32]) -> (DelayProbe, Vec<Vec<u8>>) {
    let mut probe = DelayProbe::default();
    let payloads = pings.iter().map(|ms| probe.sent(at + MS * *ms, 1_000 + Millis::from(*ms))).collect();
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
fn a_pong_reports_the_ping_it_newly_answers() {
    let at = Instant::now();
    let (mut probe, payloads) = pinged(at, &[0, 100, 200]);
    assert_eq!(probe.next_ping(), 3);
    assert_eq!(probe.answered(&payloads[1]), Some(Ping { ping: 1, sent: at + MS * 100, sent_ms: 1_100 }));
    assert_eq!(probe.newest_answered(), Some(at + MS * 100));
    // A late answer to an older ping, a repeat, or one for no ping answers nothing new.
    assert_eq!(probe.answered(&payloads[0]), None);
    assert_eq!(probe.answered(&payloads[1]), None);
    assert_eq!(probe.answered(&[1, 2, 3]), None);
    assert_eq!(probe.answered(&payloads[2]), Some(Ping { ping: 2, sent: at + MS * 200, sent_ms: 1_200 }));
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

#[test]
fn each_quote_keeps_the_bound_of_the_ping_answered_when_it_was_read() {
    let at = Instant::now();
    let (mut probe, payloads) = pinged(at, &[0, 100]);
    let max_delay = MS * 2_000;
    probe.answered(&payloads[0]);
    probe.note_read("BTC", 1_000, max_delay);
    probe.note_read("BTC", 1_001, max_delay);
    probe.answered(&payloads[1]);
    // A quote read after the later answer, while the earlier ones still wait, as one ahead of the
    // host clock does.
    probe.note_read("BTC", 1_002, max_delay);
    assert_eq!(probe.take_not_after("BTC", 1_001), (at + max_delay).into_std());
    assert_eq!(probe.take_not_after("BTC", 1_002), (at + MS * 100 + max_delay).into_std());
}

#[test]
fn a_repeated_stamp_keeps_the_stricter_bound() {
    let at = Instant::now();
    let (mut probe, payloads) = pinged(at, &[0, 100]);
    let max_delay = MS * 2_000;
    probe.answered(&payloads[0]);
    probe.note_read("BTC", 1_000, max_delay);
    probe.answered(&payloads[1]);
    probe.note_read("BTC", 1_000, max_delay);
    assert_eq!(probe.take_not_after("BTC", 1_000), (at + max_delay).into_std());
}

#[test]
fn peeking_a_deadline_leaves_it_to_be_taken_when_the_quote_goes() {
    let at = Instant::now();
    let (mut probe, payloads) = pinged(at, &[0]);
    let max_delay = MS * 2_000;
    probe.answered(&payloads[0]);
    probe.note_read("BTC", 1_000, max_delay);
    let bound = (at + max_delay).into_std();
    assert_eq!(probe.peek_not_after("BTC", 1_000), Some(bound));
    assert_eq!(probe.peek_not_after("BTC", 1_001), None, "a stamp no quote carries has no bound");
    assert_eq!(probe.peek_not_after("ETH", 1_000), None, "another symbol's is untouched");
    // The read did not consume it: the quote that waited on it still carries it when it goes.
    assert_eq!(probe.take_not_after("BTC", 1_000), bound);
    assert_eq!(probe.peek_not_after("BTC", 1_000), None, "taken at last");
}

#[test]
fn taking_a_quote_forgets_the_older_and_keeps_the_newer() {
    let at = Instant::now();
    let (mut probe, payloads) = pinged(at, &[0]);
    let max_delay = MS * 2_000;
    probe.answered(&payloads[0]);
    for ts in [1_000, 1_001, 1_002] {
        probe.note_read("BTC", ts, max_delay);
    }
    probe.note_read("ETH", 1_000, max_delay);
    assert_eq!(probe.take_not_after("BTC", 1_001), (at + max_delay).into_std());
    assert_eq!(probe.not_after["BTC"].keys().copied().collect::<Vec<_>>(), [1_002]);
    assert_eq!(probe.not_after["ETH"].len(), 1, "another symbol's is untouched");
}

#[test]
fn a_quote_with_no_bound_noted_is_already_past_it() {
    let (mut probe, _) = pinged(Instant::now(), &[0]);
    // Nothing answered, and nothing noted: the bound is no later than now.
    let before = std::time::Instant::now();
    probe.note_read("BTC", 1_000, MS * 2_000);
    assert!(probe.take_not_after("BTC", 1_000) <= std::time::Instant::now());
    assert!(probe.take_not_after("BTC", 999) >= before);
}
