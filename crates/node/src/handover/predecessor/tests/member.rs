//! A turn's member held to its own enclave: an enclave on the member's release, binding the member's
//! assignment, gets no keys without the signature of the member's promotable standby key over its
//! request, with another key's, or with the member's over another request; the member's own enclave
//! takes them; a member with no promotable key, as before its registration lands or once a release
//! move or a promotion voids it, takes them only with its turn's admin key's admission of the standby
//! key that signs, while a listed key alone answers whatever admission comes; and with no latest
//! assignment the finalised schedule's turn holds its member the same way.

use super::*;

use engine_core::eip712;
use k256::ecdsa::SigningKey;

use super::assignment::logged;
use crate::handover::Requester;
use crate::handover::assigned::{Assigned, Binding, Reason};
use crate::handover::tail::version_name;
use crate::handover::{bound_report_data, member_digest};
use crate::secrets::tests::operator_key;
use crate::standby_key::StandbyKeys;

/// The skip's generation.
const SKIPPED: u64 = 11;

/// The admin key of the turn `logged` writes, as its member's operator holds it.
fn admin(key: AccountId) -> SigningKey {
    let admin = SigningKey::from_slice(&[0x55; 32]).unwrap();
    assert_eq!(eip712::address(admin.verifying_key()), key, "the logged turn's admin key");
    admin
}

#[tokio::test]
async fn an_enclave_on_the_members_release_without_its_standby_key_gets_no_keys() {
    let (engine, _run, turn, key, boundary) = logged("handover-member-refused-log");
    let mut exchange = Exchange::start_logged("handover-member-refused", Some(engine.clone())).await;
    exchange.draw_other(turn, key, boundary);
    exchange.reach(boundary + 600_000);
    let other = StandbyKeys::for_test(&scratch_dir("handover-member-refused-other"));
    let other = Requester { key: Arc::new(other), admission: None };
    exchange.assignments.set(Some(exchange.member_skip(turn, key, Some(exchange.requesting.keys.address()))));
    for (what, requester, refusal) in [
        ("no standby signature", None, "carries no signature by a standby key"),
        ("another standby key", Some(&other), "promotable standby key is"),
    ] {
        let err = exchange.take_signed(requester).await.err().unwrap_or_else(|| panic!("{what}: the keys moved"));
        assert!(format!("{err:#}").contains(refusal), "{what}: {err:#}");
        exchange.assert_kept(what);
    }

    // The member's own key over another request: one bound to the same assignment, on another
    // connection, as a host that recorded it would replay it.
    let binding = exchange.member_skip(turn, key, None).binding;
    let (domain, member) = (exchange.domain(), exchange.requesting.keys.clone());
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
    assert!(reason.contains("promotable standby key is"), "{reason}");
    exchange.assert_kept("a signature over another request");
}

#[tokio::test]
async fn the_members_own_enclave_takes_the_keys_with_its_standby_signature() {
    let (engine, _run, turn, key, boundary) = logged("handover-member-admitted-log");
    let mut exchange = Exchange::start_logged("handover-member-admitted", Some(engine.clone())).await;
    exchange.draw_other(turn, key, boundary);
    exchange.reach(boundary + 600_000);
    exchange.admit_by(None);
    exchange.assignments.set(Some(exchange.member_skip(turn, key, Some(exchange.requesting.keys.address()))));
    let head = write_log_tail(&exchange.successor_log, 9, &exchange.old.enclave_key().unwrap());
    let answer = exchange.answer_drain(Ok(head));
    let store =
        exchange.take_signed(Some(&exchange.requester())).await.expect("the member's own enclave took the keys");
    assert_eq!((store.release(), store.generation()), (exchange.upgrade(), SKIPPED), "sealed in the skip's generation");
    timeout(Duration::from_secs(5), exchange.run).await.expect("the predecessor ended").unwrap().unwrap();
    assert_eq!(answer.await.unwrap(), 1, "sequencing stopped once");
}

/// A member whose candidate holds no promotable standby key, as before its registration lands or once a
/// promotion voids it, takes its turn only with its turn's admin key's admission of the key that signs.
#[tokio::test]
async fn a_member_with_no_promotable_standby_key_takes_its_turn_only_with_its_admin_keys_admission() {
    let (engine, _run, turn, key, boundary) = logged("handover-member-keyless-log");
    let mut exchange = Exchange::start_logged("handover-member-keyless", Some(engine.clone())).await;
    exchange.draw_other(turn, key, boundary);
    exchange.reach(boundary + 600_000);
    exchange.assignments.set(Some(exchange.member_skip(turn, key, None)));
    exchange.assert_admitted_only_by(admin(key)).await;
    assert_eq!(exchange.taken_generation().await, SKIPPED);
}

/// A member's move to another release in office revises its assignment and voids its standby key, and a
/// promotion consumes the promoted member's: the member's enclave then takes the keys only with the
/// admission of the admin key the finalised turn names, which the latest agreed block's must be.
#[tokio::test]
async fn a_member_whose_upgrade_or_promotion_voided_its_key_takes_the_keys_only_with_its_admin_keys_admission() {
    let cases = [("an upgrade in office", Reason::Draw, true), ("a promotion", Reason::Promotion, false)];
    for (what, reason, revised) in cases {
        let name = format!("handover-member-{}", if revised { "upgrade" } else { "promotion" });
        let (engine, _run, turn, key, boundary) = logged(&format!("{name}-log"));
        let mut exchange = Exchange::start_logged(&name, Some(engine.clone())).await;
        exchange.schedule_successor(turn, key, boundary);
        exchange.reach(boundary + 600_000);
        let generation = exchange.scheduled.lock().unwrap().as_ref().unwrap().generation;
        let named = Assigned { reason, revised, standby: None, ..exchange.skip_to_successor(turn, key, 0, generation) };
        // An admin key the latest agreed block names, but the finalised turn does not, admits nothing.
        let other = SigningKey::from_slice(&[0x77; 32]).unwrap();
        exchange.assignments.set(Some(Assigned { admin_key: eip712::address(other.verifying_key()), ..named }));
        exchange.admit_by(Some(other));
        let err = exchange.take_signed(Some(&exchange.requester())).await.err();
        let err = format!("{:#}", err.unwrap_or_else(|| panic!("{what}: keys by another admin key's admission")));
        assert!(err.contains("the finalised schedule names admin key"), "{what}: {err}");
        exchange.assert_kept(what);
        exchange.assignments.set(Some(named));
        exchange.assert_admitted_only_by(admin(key)).await;
        assert_eq!(exchange.taken_generation().await, generation, "{what}");
    }
}

/// With no assignment at the latest agreed block, the turn the finalised schedule names, as the custody
/// quorum reads it too, holds its member to its promotable standby key, or with none, as with no reader of
/// the latest block, to its admin key's admission, the same way.
#[tokio::test]
async fn the_finalised_turns_member_is_held_to_its_standby_key_with_no_latest_assignment() {
    let (engine, _run, turn, key, boundary) = logged("handover-member-finalised-log");
    let mut exchange = Exchange::start_logged("handover-member-finalised", Some(engine.clone())).await;
    exchange.schedule_successor(turn, key, boundary);
    exchange.reach(boundary);
    // While the schedule lists another promotable key, that key alone answers, admission or not.
    let other = StandbyKeys::for_test(&scratch_dir("handover-member-finalised-other"));
    *exchange.assignments.standby.lock().unwrap() = Some(other.address());
    for (what, signer) in [("no admission", None), ("its admin key's admission", Some(admin(key)))] {
        exchange.admit_by(signer);
        let err =
            exchange.take().await.err().unwrap_or_else(|| panic!("{what}: keys for a key the schedule does not list"));
        assert!(format!("{err:#}").contains("promotable standby key is"), "{what}: {err:#}");
        exchange.assert_kept(what);
    }
    *exchange.assignments.standby.lock().unwrap() = None;
    exchange.assert_admitted_only_by(admin(key)).await;
    let generation = exchange.scheduled.lock().unwrap().as_ref().unwrap().generation;
    assert_eq!(exchange.taken_generation().await, generation);
}

// ── Helpers ──

impl Exchange {
    /// A skip of `turn` to the set's candidate 0xc1 with `key` at position 1, in `SKIPPED`, whose
    /// candidate holds the promotable standby key `standby`.
    fn member_skip(&self, turn: u64, key: AccountId, standby: Option<AccountId>) -> Assigned {
        Assigned { standby, ..self.skip_to_successor(turn, key, 1, SKIPPED) }
    }

    /// The successor gets no keys with no admission or the log operator's, and its admin key's admits it,
    /// for the holder to end.
    async fn assert_admitted_only_by(&mut self, admin: SigningKey) {
        for (what, signer, refusal) in [
            ("no admission", None, "no admission names it"),
            ("the log operator's", Some(operator_key()), "is signed by"),
        ] {
            self.admit_by(signer);
            let err = self.take_signed(Some(&self.requester())).await.err();
            let err = err.unwrap_or_else(|| panic!("{what}: the keys moved"));
            assert!(format!("{err:#}").contains(refusal), "{what}: {err:#}");
            self.assert_kept(what);
        }
        self.admit_by(Some(admin));
    }

    /// Takes the keys the admitted successor is given, once the holder stops, and the generation they are
    /// sealed in, once the holder has ended.
    async fn taken_generation(mut self) -> u64 {
        let head = write_log_tail(&self.successor_log, 9, &self.old.enclave_key().unwrap());
        let answer = self.answer_drain(Ok(head));
        let store = self.take_signed(Some(&self.requester())).await.expect("the admitted successor took the keys");
        assert_eq!(store.release(), self.upgrade(), "sealed for another release");
        timeout(Duration::from_secs(5), self.run).await.expect("the predecessor ended").unwrap().unwrap();
        assert_eq!(answer.await.unwrap(), 1, "sequencing stopped once");
        store.generation()
    }

    /// Nothing moved for `what`: sequencing goes on, the predecessor serves, and nothing is sealed.
    pub(super) fn assert_kept(&mut self, what: &str) {
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
            admission: None,
            evidence,
        };
        (stream, request)
    }
}
