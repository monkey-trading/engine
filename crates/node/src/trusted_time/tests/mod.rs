//! How replies make a sample: agreement among the servers, the quorum, the drift bound and back-off.
//! The task's rounds, the gate, the alert keys and the offline dev scenario are in the modules below.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};

use tokio::task::JoinHandle;

use super::*;
use crate::clock::test_clock::TestClock;
use crate::roughtime::testing::TestServer;
use crate::roughtime::{VERSION_1, VERSION_DRAFT};
use crate::trusted_time::tests::logs::Logs;

mod alerts;
mod gate;
pub(crate) mod logs;
mod sampler;
mod scenario;

const MAX_WIDTH_MS: Millis = 10_000;

#[test]
fn a_reply_bounds_true_time_by_its_radius_and_round_trip() {
    let received = Instant::now();
    let reading = Reading { midpoint_s: 1_792_502_815, radius_s: 3, version: VERSION_1 };
    let reply = Reply::new(reading, Duration::from_micros(1_500), received).unwrap();
    assert_eq!(reply, Reply { received, earliest_ms: 1_792_502_812_000, latest_ms: 1_792_502_818_002 });
}

#[test]
fn a_midpoint_beyond_millisecond_range_is_refused() {
    let reading = Reading { midpoint_s: u64::MAX / 1000, radius_s: 3, version: VERSION_1 };
    assert!(Reply::new(reading, Duration::ZERO, Instant::now()).is_err());
}

#[test]
fn with_every_server_answering_a_sample_spans_what_all_but_the_tolerated_liars_cover() {
    let at = Instant::now();
    let replies = [reply(at, 100, 200), reply(at, 150, 250), reply(at, 120, 180)];
    // Three servers tolerate one liar: instants two replies cover.
    assert_eq!(agree(&replies, 3, MAX_WIDTH_MS).unwrap(), Sample { at, earliest_ms: 120, latest_ms: 200 });
    // Five tolerate two: with three replies, instants one covers.
    assert_eq!(agree(&replies, 5, MAX_WIDTH_MS).unwrap(), Sample { at, earliest_ms: 100, latest_ms: 250 });
}

#[test]
fn a_reply_outside_the_quorum_does_not_move_the_sample() {
    let at = Instant::now();
    let replies = [reply(at, 0, 6_000), reply(at, 500, 6_500), reply(at, 60_000, 66_000)];
    assert_eq!(agree(&replies, 3, MAX_WIDTH_MS).unwrap(), Sample { at, earliest_ms: 500, latest_ms: 6_000 });
}

#[test]
fn with_a_server_missing_a_liar_widens_the_sample_rather_than_moving_it_off_true_time() {
    let at = Instant::now();
    let (honest, liar) = (reply(at, 0, 6_000), reply(at, 5_000, 11_000));
    // Either reply may be the liar, so the sample spans both and true time stays inside it.
    let sample = agree(&[honest, liar], 3, 12_000).unwrap();
    assert_eq!((sample.earliest_ms, sample.latest_ms), (0, 11_000));
    // Too wide for a 10s bound: no sample.
    let err = agree(&[honest, liar], 3, MAX_WIDTH_MS).unwrap_err();
    assert!(format!("{err:#}").contains("wider than the 10000ms bound"), "{err:#}");
}

#[test]
fn with_a_server_missing_two_replies_sharing_no_instant_make_no_sample() {
    let at = Instant::now();
    // Their union would fit the bound, but the two servers do not agree.
    let err = agree(&[reply(at, 0, 6_000), reply(at, 7_000, 13_000)], 3, 15_000).unwrap_err();
    assert!(format!("{err:#}").contains("no two replies agree on an instant"), "{err:#}");
}

#[test]
fn replies_received_earlier_are_moved_on_to_the_last_arrival() {
    let at = Instant::now() + Duration::from_secs(2);
    let replies = [reply(at, 10_000, 16_000), reply(at - Duration::from_secs(2), 8_000, 14_000)];
    assert_eq!(agree(&replies, 2, MAX_WIDTH_MS).unwrap(), Sample { at, earliest_ms: 10_000, latest_ms: 16_000 });
}

#[test]
fn replies_that_disagree_or_fall_short_of_a_quorum_make_no_sample() {
    let at = Instant::now();
    // Two servers tolerate no liar: both replies must cover the same instant.
    let err = agree(&[reply(at, 0, 1_000), reply(at, 5_000, 6_000)], 2, MAX_WIDTH_MS).unwrap_err();
    assert!(format!("{err:#}").contains("no two replies agree on an instant"), "{err:#}");
    let err = agree(&[reply(at, 0, 1_000)], 3, MAX_WIDTH_MS).unwrap_err();
    assert!(format!("{err:#}").contains("only 1 replies, 2 needed"), "{err:#}");
    assert!(agree(&[], 3, MAX_WIDTH_MS).is_err());
}

#[test]
fn a_sample_wider_than_the_bound_is_refused() {
    let at = Instant::now();
    let fits = [reply(at, 0, MAX_WIDTH_MS), reply(at, 0, MAX_WIDTH_MS)];
    assert!(agree(&fits, 2, MAX_WIDTH_MS).is_ok());
    // Two pairs that agree with the wide reply but not with each other leave true time anywhere
    // between them.
    let split = [reply(at, 0, MAX_WIDTH_MS + 1), reply(at, 0, 1_000), reply(at, MAX_WIDTH_MS, MAX_WIDTH_MS + 1)];
    let err = agree(&split, 3, MAX_WIDTH_MS).unwrap_err();
    assert!(format!("{err:#}").contains("wider than the 10000ms bound"), "{err:#}");
}

#[test]
fn every_sample_holds_true_time_while_no_more_servers_lie_than_a_quorum_tolerates() {
    let at = Instant::now();
    let grid = grid(at);
    let honest: Vec<Reply> =
        grid.iter().copied().filter(|r| (r.earliest_ms..=r.latest_ms).contains(&GRID_TRUE)).collect();
    let mut samples = 0;
    for servers in [2, 3] {
        let tolerated = servers - quorum(servers);
        for replies in quorum(servers)..=servers {
            for liars in 0..=tolerated.min(replies) {
                let sets: Vec<&[Reply]> =
                    (0..replies).map(|i| if i < liars { grid.as_slice() } else { honest.as_slice() }).collect();
                each_combination(&sets, &mut Vec::new(), &mut |combination| {
                    if let Ok(sample) = agree(combination, servers, 1_000) {
                        samples += 1;
                        let holds = (sample.earliest_ms..=sample.latest_ms).contains(&GRID_TRUE);
                        assert!(holds, "{sample:?} from {combination:?} of {servers} servers");
                    }
                });
            }
        }
    }
    assert!(samples > 0);
}

#[test]
fn replies_of_which_no_two_share_an_instant_never_make_a_sample() {
    let at = Instant::now();
    let grid = grid(at);
    let apart = |a: &Reply, b: &Reply| a.latest_ms < b.earliest_ms || b.latest_ms < a.earliest_ms;
    let mut checked = 0;
    each_combination(&[&grid, &grid, &grid], &mut Vec::new(), &mut |three| {
        let pairs = [(0, 1), (0, 2), (1, 2)];
        if pairs.iter().all(|&(i, j)| apart(&three[i], &three[j])) {
            checked += 1;
            assert!(agree(three, 3, 1_000).is_err(), "{three:?}");
            assert!(agree(&three[..2], 3, 1_000).is_err(), "{three:?}");
            assert!(agree(&three[..2], 2, 1_000).is_err(), "{three:?}");
        }
    });
    assert!(checked > 0);
}

#[test]
fn a_quorum_is_a_strict_majority_of_the_servers() {
    assert_eq!([2, 3, 4, 5].map(quorum), [2, 2, 3, 3]);
}

#[test]
fn retries_back_off_by_half_again_up_to_the_cap() {
    let cap = Duration::from_secs(40);
    let delays: Vec<u64> = [1, 2, 3, 4].map(|n| retry_delay(n, cap).as_millis() as u64).to_vec();
    assert_eq!(delays, [1_000, 1_500, 2_250, 3_375]);
    assert!(retry_delay(10, cap) < cap);
    assert_eq!(retry_delay(11, cap), cap);
    assert_eq!(retry_delay(u32::MAX, cap), cap);
}

#[test]
fn for_every_valid_config_a_recovered_server_is_back_in_a_sample_within_the_grace() {
    let secs = Duration::from_secs;
    for (interval, max_drift) in (1..60).flat_map(|i| (1..60).map(move |d| (secs(i), secs(d)))) {
        // Config validation refuses the rest.
        if interval + 2 * max_drift >= NO_SAMPLE_GRACE {
            continue;
        }
        let cap = max_retry_delay(interval, max_drift);
        // After its last failure a server waits out its back-off, then up to one round, which a
        // silent server stretches to the drift bound, then up to the drift bound for its reply.
        let round = interval.max(max_drift);
        for failures in 1..40 {
            let back = retry_delay(failures, cap) + round + max_drift;
            assert!(back <= NO_SAMPLE_GRACE || cap == secs(1), "{interval:?} {max_drift:?} {failures}");
        }
    }
    assert_eq!(max_retry_delay(secs(30), secs(30)), secs(1));
    assert_eq!(max_retry_delay(Duration::MAX, secs(10)), secs(1));
}

#[test]
fn durations_round_to_whole_milliseconds_down_or_up() {
    let ns = Duration::from_nanos;
    assert_eq!([floor_millis(ns(1_999_999)), floor_millis(ns(2_000_000))], [1, 2]);
    assert_eq!([ceil_millis(ns(0)), ceil_millis(ns(1_000_000)), ceil_millis(ns(1_000_001))], [0, 1, 2]);
}

// ── Helpers ──

/// True time on the agreement tests' grid.
const GRID_TRUE: Millis = 50;

/// A reply received at `at` for every interval with ends on a 10ms grid from 0 to 100.
fn grid(at: Instant) -> Vec<Reply> {
    (0..=100).step_by(10).flat_map(|lo| (lo..=100).step_by(10).map(move |hi| reply(at, lo, hi))).collect()
}

/// Calls `f` with every way of picking one reply from each of `sets`.
fn each_combination(sets: &[&[Reply]], picked: &mut Vec<Reply>, f: &mut impl FnMut(&[Reply])) {
    let Some((first, rest)) = sets.split_first() else {
        return f(picked);
    };
    for reply in *first {
        picked.push(*reply);
        each_combination(rest, picked, f);
        picked.pop();
    }
}

/// A reply received at `received`, bounding true time to `earliest_ms..=latest_ms`.
fn reply(received: Instant, earliest_ms: Millis, latest_ms: Millis) -> Reply {
    Reply { received, earliest_ms, latest_ms }
}

/// Three honest servers in both versions, with radii of one and two seconds.
async fn honest_servers(clock: &Arc<TestClock>) -> [FakeServer; 3] {
    [
        FakeServer::start(1, VERSION_1, 1, clock.clone()).await,
        FakeServer::start(2, VERSION_DRAFT, 1, clock.clone()).await,
        FakeServer::start(3, VERSION_1, 2, clock.clone()).await,
    ]
}

/// A loopback address nothing listens on.
async fn closed_port() -> String {
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    socket.local_addr().unwrap().to_string()
}

/// The config entry for `server`.
fn server_config(server: &FakeServer) -> RoughtimeServer {
    RoughtimeServer { name: server.address.clone(), address: server.address.clone(), public_key: server.key }
}

/// A Roughtime server on loopback that reports `clock`'s true time, shifted by `lie_ms`. While
/// `answering` is clear it stays silent, and while `garbled` is set its replies fail to verify.
struct FakeServer {
    address: String,
    key: VerifyingKey,
    answering: Arc<AtomicBool>,
    garbled: Arc<AtomicBool>,
    lie_ms: Arc<AtomicI64>,
    requests: Arc<AtomicUsize>,
    task: JoinHandle<()>,
}

impl Drop for FakeServer {
    /// Stops serving.
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl FakeServer {
    /// Serves replies signed by keys from `seed`, as `version`, with `radius_s`.
    async fn start(seed: u8, version: u32, radius_s: u32, clock: Arc<TestClock>) -> Self {
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let address = socket.local_addr().unwrap().to_string();
        let server = TestServer::new(seed, version);
        let key = server.public_key();
        let (answering, garbled) = (Arc::new(AtomicBool::new(true)), Arc::new(AtomicBool::new(false)));
        let (lie_ms, requests) = (Arc::new(AtomicI64::new(0)), Arc::new(AtomicUsize::new(0)));
        let (answer, garble, lie, count) = (answering.clone(), garbled.clone(), lie_ms.clone(), requests.clone());
        let task = tokio::spawn(async move {
            let mut buf = [0; 2048];
            loop {
                let (len, from) = socket.recv_from(&mut buf).await.unwrap();
                count.fetch_add(1, Ordering::SeqCst);
                if answer.load(Ordering::SeqCst) {
                    let midpoint_s = ((clock.true_ms() + lie.load(Ordering::SeqCst)) / 1000) as u64;
                    let mut reply = server.reply(&buf[..len], midpoint_s, radius_s);
                    if garble.load(Ordering::SeqCst) {
                        // The last byte is the Merkle index, so the reply no longer proves the request.
                        let at = reply.len() - 1;
                        reply[at] ^= 1;
                    }
                    socket.send_to(&reply, from).await.unwrap();
                }
            }
        });
        Self { address, key, answering, garbled, lie_ms, requests, task }
    }
}
