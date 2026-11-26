//! A turn's member held to its own enclave by its registered standby key: an enclave on the member's
//! release, binding the member's assignment, gets no keys without the signature of the member's
//! promotable standby key over its request, with another key's, or with the member's over another
//! request; the member's own enclave takes them; a member with no promotable key is admitted by its
//! release and assignment alone; and with no latest assignment the finalised schedule's turn holds its
//! member the same way.

use super::*;

use super::assignment::logged;
use crate::handover::assigned::{Assigned, Binding};
use crate::handover::tail::version_name;
use crate::handover::{bound_report_data, member_digest};
use crate::standby_key::StandbyKeys;

/// The skip's generation.
const SKIPPED: u64 = 11;

#[tokio::test]
async fn an_enclave_on_the_members_release_without_its_standby_key_gets_no_keys() {
    let (engine, _run, turn, key, boundary) = logged("handover-member-refused-log");
    let mut exchange = Exchange::start_logged("handover-member-refused", Some(engine.clone())).await;
    exchange.draw_other(turn, key, boundary);
    exchange.reach(boundary + 600_000);
    let member = StandbyKeys::for_test(&scratch_dir("handover-member-refused-standby"));
    let other = StandbyKeys::for_test(&scratch_dir("handover-member-refused-other"));
    exchange.assignments.set(Some(exchange.member_skip(turn, key, Some(member.address()))));
    for (what, signer) in [("no standby signature", None), ("another standby key", Some(&other))] {
        let err = exchange.take_signed(signer).await.err().unwrap_or_else(|| panic!("{what}: the keys moved"));
        assert!(format!("{err:#}").contains("standby key"), "{what}: {err:#}");
        exchange.assert_kept(what);
    }

    // The member's own key over another request: one bound to the same assignment, on another
    // connection, as a host that recorded it would replay it.
    let binding = exchange.member_skip(turn, key, None).binding;
    let domain = exchange.domain();
    let (mut stream, request) = exchange
        .connect_signed(binding, |report_data| {
            let mut recorded = *report_data;
            recorded[0] ^= 1;
            Some(member.sign(member_digest(domain, &recorded)).to_string())
        })
        .await;
    send(&mut stream, &request).await.unwrap();
    let reply = timeout(Duration::from_secs(5), receive(&mut stream)).await.expect("no answer in time").unwrap();
    let Reply::Refused { reason } = reply else { panic!("keys for a signature over another request") };
    assert!(reason.contains("standby key"), "{reason}");
    exchange.assert_kept("a signature over another request");
}

#[tokio::test]
async fn the_members_own_enclave_takes_the_keys_with_its_standby_signature() {
    let (engine, _run, turn, key, boundary) = logged("handover-member-admitted-log");
    let mut exchange = Exchange::start_logged("handover-member-admitted", Some(engine.clone())).await;
    exchange.draw_other(turn, key, boundary);
    exchange.reach(boundary + 600_000);
    let member = StandbyKeys::for_test(&scratch_dir("handover-member-admitted-standby"));
    exchange.assignments.set(Some(exchange.member_skip(turn, key, Some(member.address()))));
    let head = write_log_tail(&exchange.successor_log, 9, &exchange.old.enclave_key().unwrap());
    let answer = exchange.answer_drain(Ok(head));
    let store = exchange.take_signed(Some(&member)).await.expect("the member's own enclave took the keys");
    assert_eq!((store.release(), store.generation()), (exchange.upgrade(), SKIPPED), "sealed in the skip's generation");
    timeout(Duration::from_secs(5), exchange.run).await.expect("the predecessor ended").unwrap().unwrap();
    assert_eq!(answer.await.unwrap(), 1, "sequencing stopped once");
}

/// A member whose candidate holds no promotable standby key, as before its registration lands or after a
/// release move voids it, still takes its turn.
#[tokio::test]
async fn a_member_with_no_promotable_standby_key_is_admitted_by_its_release_and_assignment() {
    let (engine, _run, turn, key, boundary) = logged("handover-member-keyless-log");
    let mut exchange = Exchange::start_logged("handover-member-keyless", Some(engine.clone())).await;
    exchange.draw_other(turn, key, boundary);
    exchange.reach(boundary + 600_000);
    exchange.assignments.set(Some(exchange.member_skip(turn, key, None)));
    let head = write_log_tail(&exchange.successor_log, 9, &exchange.old.enclave_key().unwrap());
    let answer = exchange.answer_drain(Ok(head));
    let store = exchange.take_signed(None).await.expect("a member with no promotable key took its turn");
    assert_eq!(store.generation(), SKIPPED);
    timeout(Duration::from_secs(5), exchange.run).await.expect("the predecessor ended").unwrap().unwrap();
    assert_eq!(answer.await.unwrap(), 1, "sequencing stopped once");
}

/// With no assignment at the latest agreed block, the turn the finalised schedule names, as the custody
/// quorum reads it too, holds its member to its promotable standby key the same way.
#[tokio::test]
async fn the_finalised_turns_member_is_held_to_its_standby_key_with_no_latest_assignment() {
    let (engine, _run, turn, key, boundary) = logged("handover-member-finalised-log");
    let mut exchange = Exchange::start_logged("handover-member-finalised", Some(engine.clone())).await;
    exchange.schedule_successor(turn, key, boundary);
    exchange.reach(boundary);
    let member = StandbyKeys::for_test(&scratch_dir("handover-member-finalised-standby"));
    *exchange.assignments.standby.lock().unwrap() = Some(member.address());
    let err = exchange.take().await.err().expect("keys for a request the member's standby key did not sign");
    assert!(format!("{err:#}").contains("standby key"), "{err:#}");
    exchange.assert_kept("no standby signature");
    let head = write_log_tail(&exchange.successor_log, 9, &exchange.old.enclave_key().unwrap());
    let answer = exchange.answer_drain(Ok(head));
    exchange.take_signed(Some(&member)).await.expect("the member's own enclave took the keys");
    assert_eq!(answer.await.unwrap(), 1, "sequencing stopped once");
}

// ── Helpers ──

impl Exchange {
    /// A skip of `turn` to the set's candidate 0xc1 with `key` at position 1, in `SKIPPED`, whose
    /// candidate holds the promotable standby key `standby`.
    fn member_skip(&self, turn: u64, key: AccountId, standby: Option<AccountId>) -> Assigned {
        Assigned { standby, ..self.skip_to_successor(turn, key, 1, SKIPPED) }
    }

    /// Nothing moved for `what`: sequencing goes on, the predecessor serves, and nothing is sealed.
    fn assert_kept(&mut self, what: &str) {
        assert!(self.drains.try_recv().is_err(), "{what}: sequencing stopped");
        assert!(!self.run.is_finished(), "{what}: the predecessor stopped serving");
        assert!(std::fs::read_dir(&self.successor_dir).is_err(), "{what}: the successor sealed keys");
    }

    /// Connects as a successor running `UPGRADE` bound to `binding`, and builds its request for the keys
    /// with the standby signature `sign` makes of the request's report data: the stream and the request.
    async fn connect_signed(
        &self,
        binding: Binding,
        sign: impl FnOnce(&[u8; 64]) -> Option<String>,
    ) -> (TcpStream, Request) {
        let mut stream = TcpStream::connect(&self.address).await.unwrap();
        let challenge: Challenge = receive(&mut stream).await.unwrap();
        let challenge: [u8; 32] = hex::decode(challenge.nonce).unwrap().try_into().unwrap();
        let (_, public) = <Kx as hpke::Kem>::gen_keypair();
        let public = hpke::Serializable::to_bytes(&public).to_vec();
        let reads = version_name(engine_core::protocol::ProtocolVersion::LATEST);
        let config = self.successor.file_sha256;
        let report_data = request_report_data(self.domain(), &challenge, &public, &config, &reads);
        let report_data = bound_report_data(self.domain(), &report_data, &binding);
        let platform = Platform { launch_digest: UPGRADE, policy: 0, tcb: 0 };
        let quoting = Quoting { provider: TeeProvider::Mock, processor: None, kds_base: "https://kds.test" };
        let evidence = evidence::produce(&quoting, &MockAttestor, &platform, report_data).await.unwrap();
        let request = Request {
            public_key: hex::encode(&public),
            config_sha256: hex::encode(config),
            log_tip: None,
            tail: None,
            reads,
            secrets_only: false,
            binding: Some(binding),
            standby_signature: sign(&report_data),
            evidence,
        };
        (stream, request)
    }
}
