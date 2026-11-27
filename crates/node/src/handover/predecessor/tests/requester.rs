//! A successor the vault's own release names, with no turn naming the operator, held to its own enclave:
//! an enclave on that release gets neither the keys nor the secrets without the admission of its own
//! standby key, which signs its request, by the operator this log records while rotation is off, or the
//! operator the registry names while it is on; an admission by another key, of another enclave's key or
//! for another release admits nothing; and a stopped holder admits by the key its log held at the stop.

use super::*;

use engine_core::eip712;
use k256::ecdsa::SigningKey;

use super::assignment::logged;
use crate::handover::Requester;
use crate::handover::admission::Admission;
use crate::handover::quorum::Named;
use crate::secrets::tests::operator_key;
use crate::standby_key::StandbyKeys;

/// The successor's standby key, which signs its requests, and the key whose admission of it the
/// successor sends, if any: the dev config's operator's unless a test sets another.
pub(super) struct Requesting {
    pub(super) keys: Arc<StandbyKeys>,
    admission: Mutex<Option<Admission>>,
    admitter: Mutex<Option<SigningKey>>,
}

impl Requesting {
    /// Standby keys sealed for the test `name`, admitted by the dev config's operator.
    pub(super) fn new(name: &str) -> Self {
        let keys = Arc::new(StandbyKeys::for_test(&scratch_dir(&format!("{name}-standby"))));
        Self { keys, admission: Mutex::new(None), admitter: Mutex::new(Some(operator_key())) }
    }
}

impl Exchange {
    /// What the successor signs its requests with: its standby key, and the admission it sends.
    pub(super) fn requester(&self) -> Requester {
        let set = self.requesting.admission.lock().unwrap().clone();
        let admitter = self.requesting.admitter.lock().unwrap().clone();
        let admission = set.or_else(|| admitter.map(|signer| self.admission(&signer)));
        Requester { key: self.requesting.keys.clone(), admission }
    }

    /// `signer`'s admission of the successor's standby key for the successor's release.
    pub(super) fn admission(&self, signer: &SigningKey) -> Admission {
        Admission::sign(signer, self.domain(), self.requesting.keys.address(), self.upgrade().id())
    }

    /// Has the successor send `signer`'s admission of its standby key, or none.
    pub(super) fn admit_by(&self, signer: Option<SigningKey>) {
        *self.requesting.admission.lock().unwrap() = None;
        *self.requesting.admitter.lock().unwrap() = signer;
    }

    /// Has the successor send `admission` as it is, or none.
    fn send_admission(&self, admission: Option<Admission>) {
        *self.requesting.admitter.lock().unwrap() = None;
        *self.requesting.admission.lock().unwrap() = admission;
    }

    /// The successor's standby key's signature over a request's `report_data`.
    pub(super) fn standby_signature(&self, report_data: &[u8; 64]) -> Option<String> {
        Some(self.requesting.keys.sign(crate::handover::member_digest(self.domain(), report_data)).to_string())
    }
}

/// Writes `signer`'s admission of the standby key that a successor running `UPGRADE` under `cfg` seals
/// in its sealing directory, as the operator places it there.
pub(super) fn admit_in(cfg: &NodeConfig, signer: &SigningKey) {
    let dir = &cfg.tee.sealing.as_ref().unwrap().dir;
    let sealer = Box::new(MockSealer { launch_digest: UPGRADE });
    let key = StandbyKeys::open_or_seal(dir, sealer, cfg.file_sha256).unwrap().address();
    let release = Release { launch_digest: UPGRADE, config_sha256: cfg.file_sha256 }.id();
    let admission = Admission::sign(signer, domain_of(cfg), key, release);
    let file = dir.join(crate::handover::admission::ADMISSION_FILE);
    std::fs::write(file, serde_json::to_vec(&admission).unwrap()).unwrap();
}

/// A key of `seed`, and its address.
fn key(seed: u8) -> (SigningKey, AccountId) {
    let key = SigningKey::from_slice(&[seed; 32]).unwrap();
    let address = eip712::address(key.verifying_key());
    (key, address)
}

#[tokio::test]
async fn a_successor_on_the_vaults_release_takes_the_keys_only_with_the_log_operators_admission() {
    let mut exchange = Exchange::start("handover-release-admission").await;
    exchange.make_upgrade_current();
    let (domain, upgrade) = (exchange.domain(), exchange.upgrade().id());
    let own = exchange.requesting.keys.address();
    let another_enclave = StandbyKeys::for_test(&scratch_dir("handover-release-admission-other")).address();
    let cases = [
        ("no admission", None, "no admission names it"),
        ("another key's admission", Some(exchange.admission(&key(0x77).0)), "is signed by"),
        (
            "the operator's admission of another enclave's key",
            Some(Admission::sign(&operator_key(), domain, another_enclave, upgrade)),
            "the admission names standby key",
        ),
        (
            "the operator's admission for another release",
            Some(Admission::sign(&operator_key(), domain, own, [0x99; 32])),
            "the admission names standby key",
        ),
    ];
    for (what, admission, refusal) in cases {
        exchange.send_admission(admission);
        let err = exchange.take().await.err().unwrap_or_else(|| panic!("{what}: the keys moved"));
        assert!(format!("{err:#}").contains(refusal), "{what}: {err:#}");
        exchange.assert_kept(what);
    }

    // The operator's admission sent with a request its standby key did not sign.
    exchange.admit_by(Some(operator_key()));
    let (mut stream, _, request) = exchange.connect(exchange.successor.file_sha256).await;
    send(&mut stream, &Request { standby_signature: None, ..request }).await.unwrap();
    let reply = timeout(Duration::from_secs(5), receive(&mut stream)).await.expect("no answer in time").unwrap();
    let Reply::Refused { reason } = reply else { panic!("keys for a request no standby key signed") };
    assert!(reason.contains("carries no signature by a standby key"), "{reason}");
    exchange.assert_kept("no standby signature");

    let head = write_log_tail(&exchange.successor_log, 9, &exchange.old.enclave_key().unwrap());
    let answer = exchange.answer_drain(Ok(head));
    exchange.take().await.expect("the enclave the operator admitted took the keys");
    timeout(Duration::from_secs(5), exchange.run).await.expect("the predecessor ended").unwrap().unwrap();
    assert_eq!(answer.await.unwrap(), 1, "sequencing stopped once");
}

#[tokio::test]
async fn a_successor_on_the_vaults_release_takes_no_secrets_without_the_log_operators_admission() {
    let mut exchange = Exchange::start("handover-release-admission-secrets").await;
    exchange.make_upgrade_current();
    exchange.secrets.replace(crate::secrets::tests::full(3, "a").sign(exchange.domain(), &operator_key())).unwrap();
    for (what, signer, refusal) in
        [("no admission", None, "no admission names it"), ("another key's", Some(key(0x77).0), "is signed by")]
    {
        exchange.admit_by(signer);
        let err = format!("{:#}", exchange.fetch(exchange.operator()).await.expect_err(what));
        assert!(err.contains(refusal), "{what}: {err}");
    }
    exchange.admit_by(Some(operator_key()));
    let signed = exchange.fetch(exchange.operator()).await.expect("the enclave the operator admitted took the secrets");
    assert_eq!(signed.bundle, crate::secrets::tests::full(3, "a"));
    exchange.assert_kept("moving the secrets");
}

/// While rotation is on and no turn names the operator, as when an election hands over before any
/// schedule exists, the operator the registry names, through the custody quorum, answers for its release.
#[tokio::test]
async fn with_rotation_on_only_the_named_operators_admin_key_admits_a_successor_on_the_vaults_release() {
    let mut exchange = Exchange::start("handover-named-admission").await;
    exchange.make_upgrade_current();
    exchange.rotation.latest.store(true, Ordering::SeqCst);
    exchange.rotation.finalized.store(true, Ordering::SeqCst);
    let (admin, admin_key) = key(0x66);
    let named = Named { admin_key, release: exchange.upgrade().id() };
    exchange.majority.reads.lock().unwrap().named = Some(Some(named));
    let cases = [
        ("the log operator's admission", Some(operator_key()), "is signed by"),
        ("no admission", None, "no admission names it"),
    ];
    for (what, signer, refusal) in cases {
        exchange.admit_by(signer);
        let err = exchange.take().await.err().unwrap_or_else(|| panic!("{what}: the keys moved"));
        assert!(format!("{err:#}").contains(refusal), "{what}: {err:#}");
        exchange.assert_kept(what);
    }
    exchange.admit_by(Some(admin));
    for (what, named, refusal) in [
        (
            "a registry naming another release",
            Some(Named { release: [0x99; 32], ..named }),
            "the registry names release",
        ),
        ("a registry naming no operator", None, "names no registry"),
    ] {
        exchange.majority.reads.lock().unwrap().named = Some(named);
        let err = exchange.take().await.err().unwrap_or_else(|| panic!("{what}: the keys moved"));
        assert!(format!("{err:#}").contains(refusal), "{what}: {err:#}");
        exchange.assert_kept(what);
    }

    exchange.majority.reads.lock().unwrap().named = Some(Some(named));
    let head = write_log_tail(&exchange.successor_log, 9, &exchange.old.enclave_key().unwrap());
    let answer = exchange.answer_drain(Ok(head));
    exchange.take().await.expect("the enclave the named operator admitted took the keys");
    assert_eq!(answer.await.unwrap(), 1, "sequencing stopped once");
}

/// Once its sequencer is gone, a stopped holder still admits by the operator key its log held at the stop.
#[tokio::test]
async fn a_stopped_holder_admits_by_the_operator_key_its_log_held_at_the_stop() {
    let (engine, run, ..) = logged("handover-retained-operator-log");
    let mut exchange = Exchange::start_logged("handover-retained-operator", Some(engine.clone())).await;
    exchange.make_upgrade_current();
    let (reply, answered) = oneshot::channel();
    let read = Box::new(move |engine: &engine_core::Engine| {
        let _ = reply.send(engine.operator().map(|operator| operator.address));
    });
    engine.send(crate::sequencer::Request::Query(read)).await.unwrap();
    let operator = dev_config().operator().unwrap().address;
    assert_eq!(answered.await.unwrap(), Some(operator), "the log's operator key is the dev config's");
    let head = write_log_tail(&exchange.successor_log, 9, &exchange.old.enclave_key().unwrap());
    let answer = exchange.answer_drain(Ok(head));
    // The first successor stops the holder and never confirms; then the sequencer ends.
    let (mut stream, _, request) = exchange.connect(exchange.successor.file_sha256).await;
    send(&mut stream, &request).await.unwrap();
    assert!(matches!(receive(&mut stream).await.unwrap(), Reply::Keys { .. }));
    drop((stream, engine));
    timeout(Duration::from_secs(5), run).await.expect("the sequencer ended").unwrap().unwrap();

    exchange.admit_by(Some(key(0x77).0));
    let err = exchange.take().await.err().expect("keys for another key's admission once the sequencer is gone");
    assert!(format!("{err:#}").contains("is signed by"), "{err:#}");
    exchange.admit_by(Some(operator_key()));
    exchange.take().await.expect("the operator key retained at the stop admitted the successor");
    timeout(Duration::from_secs(5), exchange.run).await.expect("the predecessor ended").unwrap().unwrap();
    assert_eq!(answer.await.unwrap(), 1, "sequencing stopped once");
}

/// A holder from before the standby check names none in its challenge and refuses a request with a
/// field it does not read: the successor sends it neither the standby signature nor the admission, and
/// sends both to a holder whose challenge asks for them.
#[tokio::test]
async fn a_successor_signs_its_request_only_for_a_holder_whose_challenge_asks() {
    let exchange = Exchange::start("handover-older-holder").await;
    let (requester, platform) = (exchange.requester(), Platform { launch_digest: UPGRADE, policy: 0, tcb: 0 });
    let collateral = crate::handover::evidence::CollateralCache::new(Duration::from_secs(60));
    for asks in [false, true] {
        let holder = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = holder.local_addr().unwrap().to_string();
        let asked = tokio::spawn(async move {
            let (mut stream, _) = holder.accept().await.unwrap();
            send(&mut stream, &Challenge { nonce: hex::encode([0x5c; 32]), standby: asks }).await.unwrap();
            receive::<serde_json::Value>(&mut stream).await.unwrap()
        });
        let parts = (None, Some(&requester));
        let asking = crate::handover::successor::ask(
            &exchange.successor,
            &address,
            &MockAttestor,
            &platform,
            (None, None),
            parts,
            &collateral,
        );
        assert!(asking.await.is_err(), "keys from a holder that sent none");
        let request = asked.await.unwrap();
        for field in ["standby_signature", "admission"] {
            assert_eq!(request.get(field).is_some(), asks, "{field}, to a holder whose challenge asks: {asks}");
        }
    }
}
