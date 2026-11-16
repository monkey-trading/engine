//! The member link: what a member's copy answers, and how an asker checks a status.

use std::sync::Arc;

use alloy::primitives::{Address, B256};
use engine_core::command::{Command, Sequenced};

use super::*;
use crate::liveness::fake::{ChainState, DOMAIN, FakeChain, account};
use crate::log::CommandEntry;
use crate::test_support::scratch_dir;

const MEMBER: AccountId = AccountId([0xb2; 20]);

/// An entries question names any offset a peer chooses: the largest ones answer nothing rather than
/// overflow, which would abort the node.
#[test]
fn an_entries_question_at_the_largest_offsets_answers_nothing() {
    let copy = Copy::default();
    for seq in 1..=3 {
        copy.appended(Offset::start_of(seq), &tick(seq));
    }
    assert!(copy.entries_after(Offset { seq: 1, index: u32::MAX }).is_empty());
    assert!(copy.entries_after(Offset { seq: u64::MAX, index: u32::MAX }).is_empty());
}

#[test]
fn entries_answer_from_right_after_the_asked_entry_and_never_across_a_gap() {
    let copy = Copy::default();
    for seq in 5..=9 {
        copy.appended(Offset::start_of(seq), &tick(seq));
    }
    let seqs = |entries: Vec<(Offset, LogEntry)>| entries.into_iter().map(|(offset, _)| offset.seq).collect::<Vec<_>>();
    assert_eq!(seqs(copy.entries_after(Offset::start_of(6))), vec![7, 8, 9], "joined at the entry asked after");
    assert_eq!(seqs(copy.entries_after(Offset::start_of(4))), vec![5, 6, 7, 8, 9], "the first retained follows it");
    assert!(copy.entries_after(Offset::start_of(3)).is_empty(), "seq 4 is not retained: a gap");
    assert!(copy.entries_after(Offset::start_of(9)).is_empty(), "nothing past the copy's tip");
}

#[test]
fn a_copy_keeps_the_newest_head_of_each_recent_epoch_and_the_newest_signed_head() {
    let copy = Copy::default();
    copy.saw(1, head(9));
    copy.saw(1, head(7));
    copy.durable(head(5), None);
    assert_eq!(copy.status(1).ok().map(|status| status.seen), Some(Some(head(9))), "an older head does not replace it");
    assert_eq!(copy.status(2).ok().map(|status| status.seen), Some(None), "nothing seen of epoch 2");
    for epoch in 2..=6 {
        copy.saw(epoch, head(epoch));
    }
    assert_eq!(copy.status(1).ok().map(|status| status.seen), Some(None), "only the latest epochs are kept");
    assert_eq!(copy.status(1).ok().and_then(|status| status.durable), Some(head(5)));
    let empty = Copy::default().status(1).expect("a copy holding no command answers");
    assert_eq!(empty.durable, None, "nothing durable: no durable head");
}

/// A copy seeded with the last command its log holds answers with it, and the key that sealed it, until
/// a follower links the copy, which then answers from its own replica: an operator that handed its keys
/// over answers its successor's start check while its replica builds.
#[test]
fn a_seeded_copy_answers_its_logs_last_command_until_a_follower_links_it() {
    let copy = Copy::default();
    copy.seed(head(9), AccountId([7; 20]));
    let status = copy.status(1).expect("a seeded copy answers");
    assert_eq!((status.durable, status.sealer), (Some(head(9)), AccountId([7; 20])));
    copy.rebuilt();
    assert_eq!(copy.status(1).ok().and_then(|status| status.durable), None, "a linked copy answers from its replica");
}

/// A copy set aside serves none of its entries; the heads seen and the conflicts are kept per key, so
/// another key reusing an epoch number after a reorg answers from its own, and the first key, put back,
/// finds its conflict standing; the newest signed head is the epoch in effect's.
#[test]
fn a_rebuilt_copy_serves_nothing_old_and_evidence_is_kept_per_key() {
    use crate::liveness::caller::tests::signed;

    let copy = Copy::default();
    for seq in 5..=9 {
        copy.appended(Offset::start_of(seq), &tick(seq));
    }
    copy.rebuilt();
    assert!(copy.entries_after(Offset::start_of(6)).is_empty(), "the set-aside copy's entries were served");
    let (first, second) = (AccountId([0x51; 20]), AccountId([0x52; 20]));
    copy.follows(first);
    copy.linked(5, B256::repeat_byte(0xaa), Some(first));
    copy.saw(2, Head { seq: 5, link: B256::repeat_byte(0xbb) });
    copy.saw(2, head(12));
    copy.signed_head(signed(2, 12));
    copy.durable(head(5), None);
    assert!(copy.status(2).is_err(), "a conflict under epoch 2");
    copy.follows(first);
    assert!(copy.status(2).is_err() && copy.signed().is_some(), "the same enclave keeps what it saw");
    copy.follows(second);
    let status = copy.status(2).expect("the last enclave's conflict stood under a reused epoch");
    assert_eq!(status.seen, None, "the last enclave's head was reported under a reused epoch");
    assert!(copy.signed().is_none(), "the last enclave's signed head is still relayed");
    copy.follows(first);
    assert!(copy.status(2).is_err(), "the first key, put back by a reorg, lost its conflict");
    copy.follows(second);
    copy.signed_head(signed(2, 20));
    copy.signed_head(signed(1, 21));
    assert_eq!(copy.signed().map(|held| held.epoch), Some(1), "a lower epoch in effect again was not taken");
}

/// A returning operator's own tail, sealed by its key, differs from its successor's heads: that is the
/// copy's divergence, not a second history on the successor's key, so once the copy is set aside and
/// rebuilt from the successor it confirms the successor's epoch; a link the head's own key sealed is a
/// conflict that a rebuild keeps.
#[test]
fn a_returning_operators_own_tail_diverges_until_the_copy_is_rebuilt() {
    let (outgoing, successor) = (AccountId([0x51; 20]), AccountId([0x52; 20]));
    let tail = |seq: u64| Head { seq, link: B256::repeat_byte(0xee) };
    let copy = Copy::default();
    copy.follows(successor);
    for seq in 5..=7 {
        copy.linked(seq, tail(seq).link, Some(outgoing));
    }
    copy.durable(tail(7), Some(outgoing));
    copy.saw(2, head(6));
    assert!(copy.status(2).is_err(), "the tail beside the successor's head confirmed it");
    copy.rebuilt();
    for seq in 5..=7 {
        copy.linked(seq, head(seq).link, Some(successor));
    }
    copy.durable(head(7), Some(successor));
    assert_eq!(copy.status(2).expect("the rebuilt copy confirms").durable, Some(head(7)));
    copy.linked(8, tail(8).link, Some(successor));
    copy.saw(2, head(8));
    copy.rebuilt();
    assert!(copy.status(2).is_err(), "a link the head's own key sealed is a second history a rebuild keeps");
}

#[test]
fn a_copy_confirms_nothing_under_an_epoch_whose_head_conflicts_with_its_links_now_or_later() {
    let forked = |seq: u64| Head { seq, link: B256::repeat_byte(0xee) };
    let key = AccountId([0x51; 20]);
    let copy_of = || {
        let copy = Copy::default();
        copy.follows(key);
        copy
    };
    for (name, before) in [("a head seen at a seq the copy holds", true), ("a head seen ahead of the copy", false)] {
        let copy = copy_of();
        let take = |copy: &Copy| {
            for seq in 1..=10 {
                copy.linked(seq, head(seq).link, Some(key));
            }
            copy.durable(head(10), None);
        };
        if before {
            take(&copy);
        }
        copy.saw(2, head(4));
        copy.saw(2, forked(7));
        copy.saw(3, head(8));
        if !before {
            take(&copy);
        }
        assert_eq!(copy.conflicted(2), Some(7), "{name}");
        let refused = copy.status(2).unwrap_err();
        assert!(format!("{refused:#}").contains("another link at seq 7"), "{name}: {refused:#}");
        let durable = copy.status(3).unwrap().durable;
        assert_eq!(durable, Some(head(10)), "{name}: another epoch's matching head still confirms");
        copy.rebuilt();
        assert!(copy.status(2).is_err(), "{name}: a rebuilt copy keeps the conflict");
    }
    let copy = copy_of();
    copy.linked(5, head(5).link, Some(key));
    copy.saw(1, forked(3));
    assert_eq!(copy.conflicted(1), None, "a seq older than the links retained is not judged");
    // Two heads of one epoch at one seq with other links are two histories, though a newer head moved
    // the epoch's newest on before the second came and the copy then holds the second's branch; another
    // epoch's head at that seq is not judged by them.
    let copy = copy_of();
    copy.linked(5, head(5).link, Some(key));
    copy.saw(2, head(7));
    copy.saw(2, head(8));
    copy.saw(3, forked(7));
    copy.saw(2, forked(7));
    assert_eq!(copy.conflicted(2), Some(7), "the conflicting heads ahead of the copy were not latched");
    assert_eq!(copy.conflicted(3), None, "another epoch's head at the seq was judged");
    for seq in 6..=8 {
        copy.linked(seq, forked(seq).link, Some(key));
    }
    copy.durable(forked(8), None);
    let refused = copy.status(2).unwrap_err();
    assert!(format!("{refused:#}").contains("another link at seq 7"), "{refused:#}");
}

#[test]
fn a_status_counts_only_from_the_members_standby_key_over_the_askers_nonce() {
    let key = crate::tee::EnclaveKey::generate();
    let status = Status { epoch: 3, durable: Some(head(10)), sealer: AccountId([6; 20]), seen: Some(head(11)) };
    let deployment = (42161, Address::repeat_byte(0x7a));
    let nonce = [9; 32];
    let signed = |member: AccountId, nonce: &[u8; 32]| {
        let digest = status.digest(deployment.0, deployment.1, member, nonce);
        Reply::Status { member, status, signature: key.sign_digest(digest).parse().unwrap() }
    };
    let address = AccountId(key.address());
    assert_eq!(verified_status(signed(MEMBER, &nonce), deployment, MEMBER, address, &nonce).unwrap(), status);
    let replayed = verified_status(signed(MEMBER, &[8; 32]), deployment, MEMBER, address, &nonce).unwrap_err();
    assert!(format!("{replayed:#}").contains("does not recover"), "{replayed:#}");
    let other = verified_status(signed(account(1), &nonce), deployment, MEMBER, address, &nonce).unwrap_err();
    assert!(format!("{other:#}").contains("answered as"), "{other:#}");
    let wrong_key = verified_status(signed(MEMBER, &nonce), deployment, MEMBER, account(2), &nonce).unwrap_err();
    assert!(format!("{wrong_key:#}").contains("does not recover"));
    let refused = Reply::Refused { reason: "not durable".into() };
    assert!(verified_status(refused, deployment, MEMBER, address, &nonce).is_err());
}

#[tokio::test]
async fn a_member_answers_its_status_and_entries_on_its_endpoint() {
    let dir = scratch_dir("member-link");
    let keys = Arc::new(StandbyKeys::for_test(&dir));
    let copy = Copy::default();
    for seq in 1..=3 {
        copy.appended(Offset::start_of(seq), &tick(seq));
    }
    copy.durable(head(3), None);
    let listen = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
    let clock: Arc<dyn crate::clock::Clock> = Arc::new(crate::clock::SystemClock);
    let server = Server {
        listen,
        deployment: (DOMAIN.standby.chain_id, DOMAIN.standby.vault),
        member: MEMBER,
        copy,
        standby: keys.clone(),
        provider: TeeProvider::Mock,
        platform: keys.platform(),
        trusted: TrustedClock::new(clock, tokio::sync::watch::channel(None).1, Duration::from_secs(10)),
        chain: Arc::new(FakeChain::new(ChainState::default())),
        domain: DOMAIN.standby,
        answering: None,
    };
    let task = tokio::spawn(server.run());
    let endpoint = listen.to_string();
    let nonce = B256::repeat_byte(4);
    let reply = loop {
        match Tcp.ask(&endpoint, &Request::Status { epoch: 1, nonce }).await {
            Ok(reply) => break reply,
            Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
        }
    };
    let deployment = (DOMAIN.standby.chain_id, DOMAIN.standby.vault);
    let status = verified_status(reply, deployment, MEMBER, keys.address(), &nonce.0).unwrap();
    assert_eq!((status.epoch, status.durable), (1, Some(head(3))));
    let Reply::Entries { entries } =
        Tcp.ask(&endpoint, &Request::Entries { after: Offset::start_of(1) }).await.unwrap()
    else {
        panic!("no entries")
    };
    assert_eq!(entries.iter().map(|(offset, _)| offset.seq).collect::<Vec<_>>(), vec![2, 3]);
    let vouch = Tcp.ask(&endpoint, &Request::Vouch { request: Box::new(vouch_request(&keys)) }).await.unwrap();
    assert!(matches!(vouch, Reply::Refused { .. }), "no trusted time sample: the vouch is refused, not answered");
    // One address holding silent connections takes only its share, and only until its question is due.
    let mut silent = Vec::new();
    for _ in 0..PER_PEER {
        silent.push(tokio::net::TcpStream::connect(&endpoint).await.unwrap());
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    let crowded = Tcp.ask(&endpoint, &Request::Status { epoch: 1, nonce }).await;
    assert!(crowded.is_err(), "a third connection from the address is dropped while two are held");
    tokio::time::sleep(QUESTION_TIMEOUT + Duration::from_millis(200)).await;
    let answered = Tcp.ask(&endpoint, &Request::Status { epoch: 1, nonce }).await.unwrap();
    assert!(matches!(answered, Reply::Status { .. }), "the silent connections were dropped at their question's time");
    drop(silent);
    task.abort();
}

/// A member answers `Attestation` with the latest vouch request quoted for the standby key it signs
/// with, and refuses one before any is quoted or when the latest is another key's.
#[tokio::test]
async fn a_member_attests_the_standby_key_it_signs_with() {
    let server = test_server("link-attestation");
    assert!(server.answer(&Request::Attestation).await.is_err(), "an attestation before any was quoted");
    let other = StandbyKeys::for_test(&scratch_dir("link-attestation-other"));
    server.copy.attested(&vouch_request(&other));
    assert!(server.answer(&Request::Attestation).await.is_err(), "another key's attestation");
    let own = vouch_request(&server.standby);
    server.copy.attested(&own);
    let Reply::Attestation { request } = server.answer(&Request::Attestation).await.unwrap() else {
        panic!("no attestation")
    };
    assert_eq!((request.candidate, request.key), (own.candidate, server.standby.address()));
}

/// A peer's connections count per IPv4 address, and per /64 for IPv6, which one host commonly holds.
#[test]
fn peers_count_per_ipv4_address_and_per_ipv6_64() {
    let v6 = |text: &str| text.parse::<std::net::IpAddr>().unwrap();
    assert_eq!(peer_key(v6("2001:db8:1:2:aaaa::1")), peer_key(v6("2001:db8:1:2:bbbb::2")));
    assert_ne!(peer_key(v6("2001:db8:1:2::1")), peer_key(v6("2001:db8:1:3::1")));
    assert_eq!(peer_key(v6("192.0.2.7")), v6("192.0.2.7"));
}

/// A heavy question while no heavy permit is free is refused at once rather than holding its place.
#[tokio::test]
async fn a_heavy_question_with_no_permit_free_is_refused_at_once() {
    let server = test_server("member-link-busy");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = listener.local_addr().unwrap().to_string();
    let none_free = Semaphore::new(0);
    let serving = async {
        let (stream, _) = listener.accept().await.unwrap();
        server.serve(stream, &none_free, Silent::default().enter(1)).await
    };
    let question = Request::Entries { after: Offset::start_of(1) };
    let asking = Tcp.ask(&endpoint, &question);
    let (served, asked) = tokio::time::timeout(Duration::from_secs(5), async { tokio::join!(serving, asking) })
        .await
        .expect("answered at once");
    served.unwrap();
    assert!(matches!(asked.unwrap(), Reply::Refused { reason } if reason.contains("busy")));
}

/// The member link answers the operator's log check on the endpoint it holds, as a following member
/// answers it where the rule does not run, with no heavy permit free; an endpoint without answers
/// refuses the question.
#[tokio::test]
async fn the_member_link_answers_a_log_check_on_its_endpoint_without_a_permit() {
    use crate::log_check::Answer;
    use crate::log_check::answer::tests::{TURN, answering, ask, head as checked, operator, statement};
    use crate::log_check::answer::{Checked, CheckedHeads};
    let verified = CheckedHeads::default();
    verified.record(Checked { head: checked(40), taken: Some((TURN, 1)), signer: operator() });
    for answers in [true, false] {
        let mut server = test_server(&format!("member-link-log-check-{answers}"));
        server.answering = answers.then(|| Arc::new(answering(verified.clone())));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = listener.local_addr().unwrap();
        let none_free = Semaphore::new(0);
        let serving = async {
            let (stream, _) = listener.accept().await.unwrap();
            server.serve(stream, &none_free, Silent::default().enter(1)).await
        };
        let asking = async {
            let mut stream = TcpStream::connect(endpoint).await.unwrap();
            send(&mut stream, &ask(&[0x7a; 32], statement(checked(40)))).await.unwrap();
            receive::<Answer>(&mut stream).await
        };
        let (served, answer) = tokio::time::timeout(Duration::from_secs(5), async { tokio::join!(serving, asking) })
            .await
            .expect("answered at once");
        match answers {
            true => {
                served.unwrap();
                assert!(matches!(answer.unwrap(), Answer::Verified { .. }), "the log check was not verified");
            }
            false => {
                assert!(served.is_err(), "an endpoint with no answers took the log check");
                assert!(answer.is_err(), "an endpoint with no answers answered the log check");
            }
        }
    }
}

/// Once every place is taken, the oldest connection that has not asked gives its place up at once,
/// well inside its question timeout, while a newer one keeps its place and is answered when it asks.
#[tokio::test]
async fn the_oldest_silent_connection_gives_its_place_up_to_a_newer_one() {
    let server = test_server("member-link-silent");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = listener.local_addr().unwrap();
    let (silent, heavy) = (Silent::default(), Semaphore::new(HEAVY));
    let _oldest_client = TcpStream::connect(endpoint).await.unwrap();
    let (oldest, _) = listener.accept().await.unwrap();
    let mut newer_client = TcpStream::connect(endpoint).await.unwrap();
    let (newer, _) = listener.accept().await.unwrap();
    let (oldest_place, newer_place) = (silent.enter(1), silent.enter(2));
    let evicting = async {
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(silent.evict_oldest(), "a silent connection to evict");
    };
    let (served, ()) = tokio::time::timeout(QUESTION_TIMEOUT / 2, async {
        tokio::join!(server.serve(oldest, &heavy, oldest_place), evicting)
    })
    .await
    .expect("the place given up before the question timeout");
    let refused = served.expect_err("the oldest silent connection gave its place up");
    assert!(format!("{refused:#}").contains("newer connection"), "{refused:#}");
    let asking = async {
        send(&mut newer_client, &Request::Status { epoch: 1, nonce: B256::ZERO }).await.unwrap();
        receive::<Reply>(&mut newer_client).await
    };
    let (served, asked) = tokio::join!(server.serve(newer, &heavy, newer_place), asking);
    served.unwrap();
    assert!(matches!(asked.unwrap(), Reply::Status { .. }), "answered");
    assert!(!silent.evict_oldest(), "a connection that asked is no longer silent");
}

// ── Helpers ──

/// A tick command at `seq`.
fn tick(seq: u64) -> LogEntry {
    let sequenced = Sequenced { seq, ts: 1_800_000_000_000 + i64::try_from(seq).unwrap(), cmd: Command::Tick };
    LogEntry::Command(CommandEntry { sequenced, events: None, seal: None })
}

/// A head at `seq`.
fn head(seq: u64) -> Head {
    Head { seq, link: B256::repeat_byte(u8::try_from(seq).unwrap()) }
}

/// A vouch request for `keys` with no usable quote.
fn vouch_request(keys: &StandbyKeys) -> VouchRequest {
    VouchRequest {
        candidate: MEMBER,
        key: keys.address(),
        tls_public_key_sha256: B256::from(keys.tls_public_key_sha256()),
        launch_digest: alloy::primitives::FixedBytes(keys.release().launch_digest),
        config_sha256: B256::from(keys.release().config_sha256),
        nonce: 1,
        expiry: 1,
        evidence: serde_json::from_value(serde_json::json!({
            "provider": "mock",
            "quote": "00",
        }))
        .unwrap_or_else(|_| panic!("a mock evidence parses")),
    }
}

/// A member link over a fresh test standby key in `name`'s scratch directory, on a fake chain.
fn test_server(name: &str) -> Server<Arc<FakeChain>> {
    let keys = Arc::new(StandbyKeys::for_test(&scratch_dir(name)));
    let clock: Arc<dyn crate::clock::Clock> = Arc::new(crate::clock::SystemClock);
    Server {
        listen: std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap(),
        deployment: (DOMAIN.standby.chain_id, DOMAIN.standby.vault),
        member: MEMBER,
        copy: Copy::default(),
        standby: keys.clone(),
        provider: TeeProvider::Mock,
        platform: keys.platform(),
        trusted: TrustedClock::new(clock, tokio::sync::watch::channel(None).1, Duration::from_secs(10)),
        chain: Arc::new(FakeChain::new(ChainState::default())),
        domain: DOMAIN.standby,
        answering: None,
    }
}
