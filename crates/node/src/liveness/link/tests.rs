//! The member link: what a member's copy answers, and how an asker checks a status.

use std::sync::Arc;

use alloy::primitives::{Address, B256};
use engine_core::command::{Command, Sequenced};

use super::*;
use crate::liveness::fake::{ChainState, DOMAIN, FakeChain, account};
use crate::log::CommandEntry;
use crate::test_support::scratch_dir;

const MEMBER: AccountId = AccountId([0xb2; 20]);

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
    copy.durable(head(5));
    assert_eq!(copy.status(1).map(|status| status.seen), Some(Some(head(9))), "an older head does not replace it");
    assert_eq!(copy.status(2).map(|status| status.seen), Some(None), "nothing seen of epoch 2");
    for epoch in 2..=6 {
        copy.saw(epoch, head(epoch));
    }
    assert_eq!(copy.status(1).map(|status| status.seen), Some(None), "only the latest epochs are kept");
    assert_eq!(copy.status(1).map(|status| status.durable), Some(head(5)));
    assert!(Copy::default().status(1).is_none(), "nothing durable: no status");
}

#[test]
fn a_status_counts_only_from_the_members_standby_key_over_the_askers_nonce() {
    let key = crate::tee::EnclaveKey::generate();
    let status = Status { epoch: 3, durable: head(10), seen: Some(head(11)) };
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
    copy.durable(head(3));
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
    assert_eq!((status.epoch, status.durable), (1, head(3)));
    let Reply::Entries { entries } =
        Tcp.ask(&endpoint, &Request::Entries { after: Offset::start_of(1) }).await.unwrap()
    else {
        panic!("no entries")
    };
    assert_eq!(entries.iter().map(|(offset, _)| offset.seq).collect::<Vec<_>>(), vec![2, 3]);
    let vouch = Tcp.ask(&endpoint, &Request::Vouch { request: Box::new(vouch_request(&keys)) }).await.unwrap();
    assert!(matches!(vouch, Reply::Refused { .. }), "no trusted time sample: the vouch is refused, not answered");
    task.abort();
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
