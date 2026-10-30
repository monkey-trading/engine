//! The whole handover over loopback between two mock enclaves, against a vault whose current release
//! the test sets and a node that answers the request to stop sequencing.

mod stop;

use super::*;

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::bail;
use engine_core::Fixed;
use engine_core::command::{Command, Sequenced};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::checkpoints::{CurrentRelease, Observation, VaultState, View};
use crate::clock::SystemClock;
use crate::handover::{Challenge, Kx, take};
use crate::log::{CommandEntry, LogEntry, Offset};
use crate::tee::MockAttestor;
use crate::tee::keys::KeySet;
use crate::tee::sealing::{MockSealer, file_for};
use crate::test_support::{capture_alerts, dev_config, scratch_dir};

const RELEASE: [u8; 48] = [0x11; 48];
const UPGRADE: [u8; 48] = [0x22; 48];

/// The predecessor's config hash, and one no test node runs.
const PREDECESSOR_CONFIG: [u8; 32] = [0xc0; 32];
const OTHER_CONFIG: [u8; 32] = [0xc2; 32];
/// The vault's generation of the successor's release once it names it.
const GENERATION: u64 = 4;

/// A vault that names `signer`, and the release `current` holds as its current one.
struct Vault {
    signer: AccountId,
    current: Arc<Mutex<CurrentRelease>>,
}

/// A running predecessor, the node behind it, and the successor's config.
struct Exchange {
    address: String,
    run: JoinHandle<anyhow::Result<()>>,
    drains: mpsc::Receiver<oneshot::Sender<anyhow::Result<Head>>>,
    old: KeySet,
    successor: NodeConfig,
    successor_dir: PathBuf,
    /// The vault's current release, as both sides read it.
    current: Arc<Mutex<CurrentRelease>>,
    /// Whether the node behind the predecessor signs its checkpoints.
    signing: watch::Sender<bool>,
}

impl CheckpointChain for Vault {
    async fn state(&self, _view: View) -> anyhow::Result<VaultState> {
        let zero = Fixed::ZERO;
        Ok(VaultState {
            height: 1,
            signer: self.signer,
            checkpoint: None,
            balance: zero,
            deposits: 0,
            deposited: zero,
            distributions_paid: zero,
        })
    }

    async fn post(&self, _checkpoint: &Checkpoint) -> anyhow::Result<()> {
        bail!("this vault takes no checkpoints")
    }

    async fn current_release(&self, at: &Observation) -> anyhow::Result<CurrentRelease> {
        ensure!(at.view == View::Finalized, "the current release is read at finality");
        Ok(*self.current.lock().unwrap())
    }
}

#[tokio::test]
async fn a_successor_that_is_not_the_vaults_current_release_gets_no_keys_and_sequencing_goes_on() {
    let mut exchange = Exchange::start("handover-not-current").await;
    let upgrade = exchange.upgrade();
    for (what, current) in [
        ("no release", None),
        ("the running release", Some(Release { launch_digest: RELEASE, config_sha256: PREDECESSOR_CONFIG })),
        ("the running code under the successor's config", Some(Release { launch_digest: RELEASE, ..upgrade })),
        ("the successor's code under another config", Some(Release { config_sha256: OTHER_CONFIG, ..upgrade })),
        ("another code under the successor's config", Some(Release { launch_digest: [0x33; 48], ..upgrade })),
    ] {
        *exchange.current.lock().unwrap() = current.map_or_else(CurrentRelease::default, |release: Release| {
            CurrentRelease { id: release.id(), generation: GENERATION }
        });
        let err = exchange.take().await.err().unwrap_or_else(|| panic!("{what}: the successor took the keys"));
        assert!(format!("{err:#}").contains("the vault's current release is"), "{what}: {err:#}");
        assert!(exchange.drains.try_recv().is_err(), "{what}: sequencing stopped");
        assert!(!exchange.run.is_finished(), "{what}: the predecessor stopped serving");
        assert!(std::fs::read_dir(&exchange.successor_dir).is_err(), "{what}: the successor sealed keys");
    }
}

#[tokio::test]
async fn a_successor_whose_quote_binds_another_config_than_it_names_gets_no_keys() {
    let mut exchange = Exchange::start("handover-config-lie").await;
    exchange.make_upgrade_current();
    // The request names the current release's config, but the enclave quoted another one.
    let (mut stream, _, request) = exchange.connect(OTHER_CONFIG).await;
    send(&mut stream, &request).await.unwrap();
    let reply = timeout(Duration::from_secs(5), receive(&mut stream)).await.expect("no answer in time").unwrap();
    assert!(matches!(reply, Reply::Refused { .. }), "keys for a misnamed config");
    assert!(exchange.drains.try_recv().is_err(), "sequencing stopped for a misnamed config");
    assert!(!exchange.run.is_finished(), "the predecessor stopped serving");
}

#[tokio::test]
async fn the_current_release_takes_the_keys_seals_them_and_the_predecessor_ends() {
    let mut exchange = Exchange::start("handover-current").await;
    exchange.make_upgrade_current();
    // A copy the successor's release sealed when governance named it before stays as it was.
    let sealer = MockSealer { launch_digest: UPGRADE };
    let older = KeySet::generate().unwrap();
    KeyStore::create(&exchange.successor_dir, &sealer, exchange.successor.file_sha256, 1, older.clone()).unwrap();
    let head = Head { seq: 9, root: format!("0x{}", "ab".repeat(32)) };
    let answer = exchange.answer_drain(Ok(head.clone()));
    let store = exchange.take().await.unwrap();
    assert_eq!(answer.await.unwrap(), 1, "sequencing stopped once");

    let taken = store.keys();
    assert_eq!(taken.enclave_key().unwrap().address(), exchange.old.enclave_key().unwrap().address());
    assert_eq!(taken.tls_key().unwrap().serialize_der(), exchange.old.tls_key().unwrap().serialize_der());
    assert_eq!(taken.predecessor_head, Some(head), "the final checkpoint did not come with the keys");
    assert_eq!((store.release(), store.generation()), (exchange.upgrade(), GENERATION), "sealed elsewhere");
    assert!(file_for(&exchange.successor_dir, &exchange.upgrade(), GENERATION).exists());
    let kept = KeyStore::open(&exchange.successor_dir, &sealer, exchange.successor.file_sha256, 1).unwrap();
    assert_eq!(kept.unwrap().keys(), older, "the older copy changed");
    timeout(Duration::from_secs(5), exchange.run).await.expect("the predecessor ended").unwrap().unwrap();
}

#[tokio::test]
async fn a_successor_takes_only_the_key_the_vault_names() {
    let mut exchange = Exchange::start("handover-unnamed-key").await;
    exchange.make_upgrade_current();
    let _answer = exchange.answer_drain(Ok(Head { seq: 9, root: format!("0x{}", "ab".repeat(32)) }));
    // A host-run predecessor hands over a key the vault does not name: nothing is sealed or confirmed.
    let err = exchange.take_naming(AccountId([0xee; 20])).await.err().expect("an unnamed key was taken");
    assert!(format!("{err:#}").contains("is not the vault's signer"), "{err:#}");
    assert!(!file_for(&exchange.successor_dir, &exchange.upgrade(), GENERATION).exists(), "the unnamed key was sealed");
    assert!(!exchange.run.is_finished(), "the predecessor ended without a confirmation");

    // The key the vault names is taken.
    exchange.take().await.unwrap();
    timeout(Duration::from_secs(5), exchange.run).await.expect("the predecessor ended").unwrap().unwrap();
}

#[tokio::test]
async fn a_successor_the_vault_no_longer_names_once_the_keys_arrive_seals_nothing() {
    let mut exchange = Exchange::start("handover-named-another").await;
    exchange.make_upgrade_current();
    let _answer = exchange.answer_drain(Ok(Head { seq: 9, root: format!("0x{}", "ab".repeat(32)) }));
    // Governance names another release after the predecessor's check and before the successor's.
    let signer = AccountId(exchange.old.enclave_key().unwrap().address());
    let named_another = CurrentRelease { id: [0x99; 32], generation: GENERATION + 1 };
    let vault = Vault { signer, current: Arc::new(Mutex::new(named_another)) };
    let err = exchange.take_reading(vault).await.err().expect("keys sealed for a release the vault no longer names");
    assert!(format!("{err:#}").contains("the vault's current release is"), "{err:#}");
    assert!(std::fs::read_dir(&exchange.successor_dir).is_err(), "the successor sealed keys");
    assert!(!exchange.run.is_finished(), "the predecessor ended without a confirmation");
}

#[tokio::test]
async fn a_confirmation_by_another_key_ends_nothing_and_a_later_successor_reuses_the_stop() {
    let mut exchange = Exchange::start("handover-forged-confirm").await;
    exchange.make_upgrade_current();
    let answer = exchange.answer_drain(Ok(Head { seq: 3, root: format!("0x{}", "cd".repeat(32)) }));

    // A successor that cannot sign with the handed-over key never ends the predecessor.
    let (mut stream, report_data, request) = exchange.connect(exchange.successor.file_sha256).await;
    send(&mut stream, &request).await.unwrap();
    assert!(matches!(receive(&mut stream).await.unwrap(), Reply::Keys { .. }));
    let forged = EnclaveKey::generate().sign_digest(confirm_digest(exchange.domain(), &report_data));
    send(&mut stream, &Confirm { signature: forged }).await.unwrap();
    assert!(receive::<Reply>(&mut stream).await.is_err(), "a forged confirmation was acknowledged");
    assert!(!exchange.run.is_finished(), "a forged confirmation ended the predecessor");

    exchange.take().await.unwrap();
    timeout(Duration::from_secs(5), exchange.run).await.expect("the predecessor ended").unwrap().unwrap();
    assert_eq!(answer.await.unwrap(), 1, "the second successor stopped sequencing again");
}

#[tokio::test]
async fn a_stopped_predecessor_keeps_serving_until_a_successor_confirms() {
    let (_guard, mut alerts) = capture_alerts();
    let mut exchange = Exchange::start("handover-keeps-serving").await;
    exchange.make_upgrade_current();
    let answer = exchange.answer_drain(Ok(Head { seq: 3, root: format!("0x{}", "cd".repeat(32)) }));
    let (mut stream, _, request) = exchange.connect(exchange.successor.file_sha256).await;
    send(&mut stream, &request).await.unwrap();
    assert!(matches!(receive(&mut stream).await.unwrap(), Reply::Keys { .. }));
    drop(stream);
    let alert = timeout(Duration::from_secs(5), alerts.recv()).await.expect("no alert").unwrap();
    assert_eq!((alert["alert"].as_str(), alert["level"].as_str()), ("handover_unconfirmed", "ERROR"));

    // A day on, sequencing stays stopped and it still serves: its own copy of the keys no longer
    // opens, so leaving would lose them.
    tokio::time::pause();
    tokio::time::sleep(Duration::from_secs(24 * 3600)).await;
    assert!(!exchange.run.is_finished(), "the stopped predecessor left without a confirmation");
    tokio::time::resume();
    exchange.take().await.unwrap();
    timeout(Duration::from_secs(5), exchange.run).await.expect("the predecessor ended").unwrap().unwrap();
    assert_eq!(answer.await.unwrap(), 1, "sequencing stopped once");
}

#[tokio::test]
async fn a_recorded_request_gets_nothing_on_another_connection() {
    let mut exchange = Exchange::start("handover-replay").await;
    exchange.make_upgrade_current();
    let _answer = exchange.answer_drain(Ok(Head { seq: 3, root: format!("0x{}", "cd".repeat(32)) }));
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

#[tokio::test]
async fn a_successor_without_the_signed_acknowledgement_keeps_the_keys_sealed_but_does_not_start() {
    let (successor, successor_dir) = successor_config("handover-forged-ack");
    let old = KeySet::generate().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let (domain, config) = (domain_of(&successor), successor.file_sha256);
    let mut handed = old.clone();
    handed.predecessor_head = Some(Head { seq: 3, root: format!("0x{}", "cd".repeat(32)) });
    // A predecessor whose acknowledgement the host forges with a key of its own.
    let predecessor = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let challenge = [0x5c; 32];
        send(&mut stream, &Challenge { nonce: hex::encode(challenge) }).await.unwrap();
        let request: Request = receive(&mut stream).await.unwrap();
        let public_key = hex::decode(&request.public_key).unwrap();
        let report_data = request_report_data(domain, &challenge, &public_key, &config);
        let (encapped_key, ciphertext) = seal_keys(domain, &public_key, &report_data, &handed).unwrap();
        let bound = keys_report_data(domain, &report_data, &encapped_key, &ciphertext);
        let platform = Platform { launch_digest: RELEASE, policy: 0, tcb: 0 };
        let quoting = Quoting { provider: TeeProvider::Mock, processor: None, kds_base: "https://kds.test" };
        let evidence = evidence::produce(&quoting, &MockAttestor, &platform, bound).await.unwrap();
        let keys =
            Reply::Keys { encapped_key: hex::encode(encapped_key), ciphertext: hex::encode(ciphertext), evidence };
        send(&mut stream, &keys).await.unwrap();
        let _: Confirm = receive(&mut stream).await.unwrap();
        let forged = EnclaveKey::generate().sign_digest(confirmed_digest(domain, &report_data));
        send(&mut stream, &Reply::Confirmed { signature: forged }).await.unwrap();
    });
    let named = CurrentRelease { id: Release { launch_digest: UPGRADE, config_sha256: config }.id(), generation: 2 };
    let vault = Vault { signer: AccountId(old.enclave_key().unwrap().address()), current: Arc::new(Mutex::new(named)) };
    let sealer = MockSealer { launch_digest: UPGRADE };
    let err = take(&successor, &address, &sealer, &MockAttestor, &vault).await.err().expect("started unacknowledged");
    assert!(format!("{err:#}").contains("did not acknowledge"), "{err:#}");
    predecessor.await.unwrap();

    // The keys stay sealed here, so a later start opens them without a flag.
    let reopened = KeyStore::open(&successor_dir, &sealer, config, 2).unwrap().expect("the keys were not sealed");
    assert_eq!(reopened.keys().enclave_key().unwrap().address(), old.enclave_key().unwrap().address());
}

#[test]
fn a_successor_runs_under_the_same_policy_and_no_lower_tcb() {
    let packed = |snp: u64| (8u64 << 56) | (snp << 48) | 3;
    let own = Platform { launch_digest: RELEASE, policy: 0x30000, tcb: packed(8) };
    let peer = |policy, snp| Peer { launch_digest: UPGRADE, policy, tcb: Some(Tcb::from_packed(packed(snp))) };
    no_weaker(&own, &peer(0x30000, 8)).unwrap();
    no_weaker(&own, &peer(0x30000, 9)).unwrap();
    let err = no_weaker(&own, &peer(0x30000 | 1 << 19, 8)).unwrap_err();
    assert!(err.to_string().contains("guest policy"), "a debug policy passed: {err:#}");
    let err = no_weaker(&own, &peer(0x30000, 7)).unwrap_err();
    assert!(err.to_string().contains("below this platform's committed"), "{err:#}");
    // Mock peers carry no TCB.
    no_weaker(
        &Platform { launch_digest: RELEASE, policy: 0, tcb: 0 },
        &Peer { launch_digest: UPGRADE, policy: 0, tcb: None },
    )
    .unwrap();
}

// ── Helpers ──

impl Exchange {
    /// A predecessor sealed for `RELEASE`, behind a vault that names no current release yet, serving
    /// on loopback.
    async fn start(name: &str) -> Self {
        let (successor, successor_dir) = successor_config(name);

        let old = KeySet::generate().unwrap();
        let (dir, sealer) = (scratch_dir(&format!("{name}-old")), MockSealer { launch_digest: RELEASE });
        // The predecessor runs another config than the successor's, as a config change does.
        let store = KeyStore::create(&dir, &sealer, PREDECESSOR_CONFIG, 1, old.clone()).unwrap();
        let current = Arc::new(Mutex::new(CurrentRelease::default()));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let (drain, drains) = mpsc::channel(1);
        let (signing, signing_rx) = watch::channel(true);
        let predecessor = Predecessor {
            listener,
            domain: CheckpointDomain {
                chain_id: successor.custody.chain_id,
                vault: successor.custody.vault.parse().unwrap(),
            },
            provider: TeeProvider::Mock,
            processor: None,
            kds_base: "https://kds.test".into(),
            signer: AccountId(old.enclave_key().unwrap().address()),
            store: Arc::new(store),
            attestor: Box::new(MockAttestor),
            vault: Vault { signer: AccountId([0; 20]), current: current.clone() },
            trusted: TrustedClock::new(Arc::new(SystemClock), watch::channel(None).1, Duration::from_secs(1)),
            drain,
            signing: signing_rx,
        };
        let run = tokio::spawn(predecessor.run());
        Self { address, run, drains, old, successor, successor_dir, current, signing }
    }

    /// Connects as a successor running `UPGRADE`, takes the connection's challenge, and builds a
    /// request that names the successor's config but whose quote binds `quoted`: the stream, the
    /// report data, and the request.
    async fn connect(&self, quoted: [u8; 32]) -> (TcpStream, [u8; 64], Request) {
        let mut stream = TcpStream::connect(&self.address).await.unwrap();
        let challenge: Challenge = receive(&mut stream).await.unwrap();
        let challenge: [u8; 32] = hex::decode(challenge.nonce).unwrap().try_into().unwrap();
        let (_, public) = <Kx as hpke::Kem>::gen_keypair();
        let public = hpke::Serializable::to_bytes(&public).to_vec();
        let report_data = request_report_data(self.domain(), &challenge, &public, &quoted);
        let platform = Platform { launch_digest: UPGRADE, policy: 0, tcb: 0 };
        let quoting = Quoting { provider: TeeProvider::Mock, processor: None, kds_base: "https://kds.test" };
        let evidence = evidence::produce(&quoting, &MockAttestor, &platform, report_data).await.unwrap();
        let config_sha256 = hex::encode(self.successor.file_sha256);
        (stream, report_data, Request { public_key: hex::encode(&public), config_sha256, evidence })
    }

    /// The successor's release: `UPGRADE` running its config.
    fn upgrade(&self) -> Release {
        Release { launch_digest: UPGRADE, config_sha256: self.successor.file_sha256 }
    }

    /// Has the vault name the successor's release as its current one.
    fn make_upgrade_current(&self) {
        *self.current.lock().unwrap() = CurrentRelease { id: self.upgrade().id(), generation: GENERATION };
    }

    /// The deployment both sides serve.
    fn domain(&self) -> CheckpointDomain {
        domain_of(&self.successor)
    }

    /// Answers every request to stop sequencing with `head`, counting them until the predecessor
    /// ends.
    fn answer_drain(&mut self, head: anyhow::Result<Head>) -> JoinHandle<usize> {
        let mut drains = std::mem::replace(&mut self.drains, mpsc::channel(1).1);
        let head = head.map_err(|e| e.to_string());
        tokio::spawn(async move {
            let mut answered = 0;
            while let Some(reply) = drains.recv().await {
                answered += 1;
                let _ = reply.send(head.clone().map_err(|e| anyhow!(e)));
            }
            answered
        })
    }

    /// Answers the first stop request with the checkpoint gate's refusal of the final checkpoint —
    /// as the node's sequencer gives it — and every later one with `head`, counting them until the
    /// predecessor ends.
    fn refuse_once_then_answer(&mut self, reason: &str, head: Head) -> JoinHandle<usize> {
        let mut drains = std::mem::replace(&mut self.drains, mpsc::channel(1).1);
        let reason = reason.to_owned();
        tokio::spawn(async move {
            let mut answered = 0;
            while let Some(reply) = drains.recv().await {
                answered += 1;
                let result = if answered == 1 {
                    Err(anyhow!(crate::sequencer::HandoverRefused::new(reason.clone())))
                } else {
                    Ok(head.clone())
                };
                let _ = reply.send(result);
            }
            answered
        })
    }

    /// The successor's side, with the stand-in for sealing, against a vault naming the old key.
    async fn take(&self) -> anyhow::Result<KeyStore> {
        let signer = AccountId(self.old.enclave_key().unwrap().address());
        self.take_naming(signer).await
    }

    /// The successor's side, against a vault naming `signer`.
    async fn take_naming(&self, signer: AccountId) -> anyhow::Result<KeyStore> {
        self.take_reading(Vault { signer, current: self.current.clone() }).await
    }

    /// The successor's side, reading `vault`.
    async fn take_reading(&self, vault: Vault) -> anyhow::Result<KeyStore> {
        take(&self.successor, &self.address, &MockSealer { launch_digest: UPGRADE }, &MockAttestor, &vault).await
    }
}

/// A successor's config: the dev config, sealing into a fresh directory, with checkpoints.
fn successor_config(name: &str) -> (NodeConfig, PathBuf) {
    let mut successor = dev_config();
    let successor_dir = scratch_dir(&format!("{name}-new"));
    successor.tee.use_published_mock_key = false;
    successor.tee.sealing = Some(toml::from_str(&format!("dir = {:?}", successor_dir.display().to_string())).unwrap());
    successor.checkpoints = Some(
        toml::from_str(
            "rpc_url = 'http://127.0.0.1:9'\nrelayer = '0x0000000000000000000000000000000000000001'\n\
             retry_interval = '1s'\nrpc_timeout = '1s'",
        )
        .unwrap(),
    );
    (successor, successor_dir)
}

/// The deployment `cfg` names.
fn domain_of(cfg: &NodeConfig) -> CheckpointDomain {
    CheckpointDomain { chain_id: cfg.custody.chain_id, vault: cfg.custody.vault.parse().unwrap() }
}

/// Publishes `entry` as the sequencer does.
fn publish(tx: &broadcast::Sender<Arc<StreamMsg>>, entry: LogEntry) {
    let seq = match &entry {
        LogEntry::Command(command) => command.sequenced.seq,
        LogEntry::Checkpoint(cp) => cp.seq,
        LogEntry::Withdrawal(_) => 0,
    };
    let _ = tx.send(Arc::new(StreamMsg::new(Offset::start_of(seq), entry)));
}

fn command(seq: u64) -> CommandEntry {
    CommandEntry {
        sequenced: Sequenced { seq, ts: 1_000 + seq as i64, cmd: Command::Tick },
        events: Some(Vec::new()),
        seal: None,
    }
}

fn checkpoint(seq: u64, root: &str) -> Checkpoint {
    serde_json::from_value(serde_json::json!({ "seq": seq, "root": root, "signature": "0x", "signer": "0x" })).unwrap()
}
