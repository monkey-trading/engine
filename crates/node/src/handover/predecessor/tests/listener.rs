//! The listener's places: connections that send no request hold none of them from the turn's member,
//! however many are queued ahead of it or however often a peer renews them, one address holds only its
//! share and only until its request is due, a full listener, or a full share, gives the oldest silent
//! place there to a new connection, requests are served one at a time under one stop, and a request recorded on
//! one connection gets nothing on another.

use super::*;

use std::collections::VecDeque;

use tokio::net::TcpStream;
use tokio::time::timeout;

use crate::handover::predecessor::listener::{CONNECTIONS, PER_PEER, REQUEST_TIMEOUT};

/// How long a take may run here: a successor with its challenge at once takes the keys in moments.
const TAKE_BOUND: Duration = Duration::from_secs(15);

#[tokio::test]
async fn silent_connections_ahead_of_the_successor_leave_it_the_keys() {
    let mut exchange = upgrading("listener-silent").await;
    let _silent = (connect(&exchange.address).await, connect(&exchange.address).await);
    let started = std::time::Instant::now();
    let taken = timeout(TAKE_BOUND, exchange.take()).await.expect("the take hung");
    taken.expect("silent connections kept the successor from the keys");
    assert!(started.elapsed() < Duration::from_secs(10), "the take waited {:?} behind them", started.elapsed());
    timeout(Duration::from_secs(5), &mut exchange.run).await.expect("the predecessor did not end").unwrap().unwrap();
}

#[tokio::test]
async fn a_peer_renewing_silent_connections_holds_no_successor_off() {
    let exchange = upgrading("listener-renewed").await;
    let address = exchange.address.clone();
    // Faster than any request is due, and enough to keep every place taken.
    let attacker = tokio::spawn(async move {
        let mut held = VecDeque::new();
        loop {
            if let Ok(stream) = TcpStream::connect(&address).await {
                held.push_back(stream);
            }
            while held.len() > 2 * CONNECTIONS {
                held.pop_front();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    });
    tokio::time::sleep(Duration::from_secs(1)).await;
    let taken = timeout(TAKE_BOUND, exchange.take()).await.expect("the take hung");
    attacker.abort();
    taken.expect("a peer renewing silent connections kept the successor from the keys");
}

#[tokio::test]
async fn a_full_listener_gives_the_oldest_silent_place_to_a_new_connection() {
    let mut exchange = upgrading("listener-full").await;
    let (started, mut silent) = (std::time::Instant::now(), Vec::new());
    for _ in 0..CONNECTIONS {
        silent.push(challenged(&exchange.address).await);
    }
    assert!(started.elapsed() < REQUEST_TIMEOUT / 2, "the listener did not fill before its first requests were due");
    let _newer = challenged(&exchange.address).await;
    let oldest = timeout(Duration::from_secs(1), receive::<Reply>(&mut silent[0])).await;
    assert!(oldest.is_ok_and(|read| read.is_err()), "the oldest silent connection kept its place");
    let next = timeout(Duration::from_millis(100), receive::<Reply>(&mut silent[1])).await;
    assert!(next.is_err(), "a newer silent connection lost its place");
    assert!(!exchange.run.is_finished(), "the predecessor ended");
    timeout(TAKE_BOUND, exchange.take()).await.expect("the take hung").expect("a full listener refused the successor");
    timeout(Duration::from_secs(5), &mut exchange.run).await.expect("the predecessor did not end").unwrap().unwrap();
}

/// A second successor gets its challenge while the first holds the keys unconfirmed, waits for the first
/// to go, and is handed the keys under the same stop.
#[tokio::test]
async fn a_second_request_waits_for_the_first_and_reuses_its_stop() {
    let mut exchange = Exchange::start("listener-two-requests").await;
    exchange.make_upgrade_current();
    let head = write_log_tail(&exchange.successor_log, 3, &exchange.old.enclave_key().unwrap());
    let answer = exchange.answer_drain(Ok(head));
    let (mut first, _, request) = exchange.connect(exchange.successor.file_sha256).await;
    send(&mut first, &request).await.unwrap();
    assert!(matches!(receive(&mut first).await.unwrap(), Reply::Keys { .. }), "the first got no keys");
    let second = timeout(Duration::from_secs(5), exchange.connect(exchange.successor.file_sha256)).await;
    let (mut second, _, request) = second.expect("no challenge while another request was served");
    send(&mut second, &request).await.unwrap();
    let waiting = timeout(Duration::from_millis(500), receive::<Reply>(&mut second)).await;
    assert!(waiting.is_err(), "the second request was answered while the first was served");
    drop(first);
    let reply = timeout(TAKE_BOUND, receive::<Reply>(&mut second)).await.expect("no answer once the first left");
    assert!(matches!(reply.unwrap(), Reply::Keys { .. }), "the second got no keys");
    drop(second);
    timeout(TAKE_BOUND, exchange.take()).await.expect("the take hung").expect("the successor got no keys");
    timeout(Duration::from_secs(5), &mut exchange.run).await.expect("the predecessor did not end").unwrap().unwrap();
    assert_eq!(answer.await.unwrap(), 1, "a later request stopped sequencing again");
}

#[tokio::test]
async fn one_address_holds_at_most_its_share_and_only_until_its_request_is_due() {
    let exchange = Exchange::start("listener-per-peer").await;
    let (listener, _drains) = super::assignment::listener_over(&exchange, exchange.store.clone(), None).await;
    let address = listener.listener.local_addr().unwrap().to_string();
    let run = tokio::spawn(listener.run());
    let mut silent = Vec::new();
    for _ in 0..PER_PEER {
        silent.push(challenged(&address).await);
    }
    // A third connection from the address takes the place of its oldest silent one.
    let _crowded = challenged(&address).await;
    let oldest = timeout(Duration::from_secs(1), receive::<Reply>(&mut silent[0])).await;
    assert!(oldest.is_ok_and(|read| read.is_err()), "the address's oldest silent connection kept its place");
    let newer = timeout(REQUEST_TIMEOUT + Duration::from_secs(5), receive::<Reply>(&mut silent[1])).await;
    assert!(newer.is_ok_and(|read| read.is_err()), "a silent connection outlived its request's time");
    run.abort();
}

#[tokio::test]
async fn a_recorded_request_gets_nothing_on_another_connection() {
    let mut exchange = Exchange::start("handover-replay").await;
    exchange.make_upgrade_current();
    let head = write_log_tail(&exchange.successor_log, 3, &exchange.old.enclave_key().unwrap());
    let _answer = exchange.answer_drain(Ok(head));
    let (mut first, _, recorded) = exchange.connect(exchange.successor.file_sha256).await;
    send(&mut first, &recorded).await.unwrap();
    assert!(matches!(receive(&mut first).await.unwrap(), Reply::Keys { .. }));
    drop(first);

    // The host replays the request on a connection of its own, which carries another challenge.
    let mut replayed = TcpStream::connect(&exchange.address).await.unwrap();
    let _: Challenge = receive(&mut replayed).await.unwrap();
    send(&mut replayed, &recorded).await.unwrap();
    let reply = timeout(Duration::from_secs(5), receive(&mut replayed)).await.expect("no answer in time").unwrap();
    assert!(matches!(reply, Reply::Refused { .. }), "a replayed request got the keys");
    assert!(!exchange.run.is_finished(), "a replayed request ended the predecessor");
    exchange.take().await.unwrap();
}

// ── Helpers ──

/// An exchange whose vault names the successor's release, with a predecessor that stops at a final
/// checkpoint the successor's log already holds.
async fn upgrading(name: &str) -> Exchange {
    let mut exchange = Exchange::start(name).await;
    exchange.make_upgrade_current();
    let head = write_log_tail(&exchange.successor_log, 9, &exchange.old.enclave_key().unwrap());
    // Detached: the drain stays answered for as long as the predecessor runs.
    drop(exchange.answer_drain(Ok(head)));
    exchange
}

/// A connection to `address` that sends nothing.
async fn connect(address: &str) -> TcpStream {
    TcpStream::connect(address).await.expect("the handover endpoint is reachable")
}

/// A connection to `address` that has its challenge and sends nothing.
async fn challenged(address: &str) -> TcpStream {
    let mut stream = connect(address).await;
    let challenge = timeout(Duration::from_secs(5), receive::<Challenge>(&mut stream)).await;
    challenge.expect("a connection got no challenge in time").expect("the challenge is unreadable");
    stream
}
