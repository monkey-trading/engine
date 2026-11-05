use super::*;
use crate::node::handover::{Episode, episode};
use std::path::PathBuf;
use std::sync::Mutex;

use engine_core::Fixed;
use engine_core::types::AccountId;

use crate::checkpoints::{CurrentRelease, Observation, VaultState, View};
use crate::log::Checkpoint;
use crate::tee::keys::Head;
use crate::test_support::{capture_alerts, dev_config, scratch_dir};

const DIGEST: [u8; 48] = [0x11; 48];

/// A vault whose current release is the one `current` holds; with `synced` set it reads the fold
/// too, and that is the release its generation has folded.
struct Vault {
    current: Arc<Mutex<CurrentRelease>>,
    synced: Arc<Mutex<Option<[u8; 32]>>>,
    /// The turn the finalised schedule names, at any time at or after its boundary.
    scheduled: Arc<Mutex<Option<crate::checkpoints::ScheduledTurn>>>,
}

impl Vault {
    fn naming(current: CurrentRelease) -> Self {
        Self { current: Arc::new(Mutex::new(current)), synced: Arc::new(Mutex::new(None)), scheduled: Arc::default() }
    }
}

impl CheckpointChain for Vault {
    async fn state(&self, _view: View) -> anyhow::Result<VaultState> {
        let zero = Fixed::ZERO;
        Ok(VaultState {
            height: 1,
            signer: AccountId([0; 20]),
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

    async fn scheduled_turn(
        &self,
        _at: &Observation,
        now_ms: engine_core::types::Millis,
    ) -> anyhow::Result<Option<crate::checkpoints::ScheduledTurn>> {
        Ok(self.scheduled.lock().unwrap().clone().filter(|turn| turn.boundary <= now_ms))
    }

    fn reads_release_fold(&self) -> bool {
        self.synced.lock().unwrap().is_some()
    }

    async fn synced_release(&self, at: &Observation) -> anyhow::Result<[u8; 32]> {
        ensure!(at.view == View::Finalized, "the folded release is read at finality");
        let synced = *self.synced.lock().unwrap();
        synced.context("this vault reads no fold")
    }
}

#[tokio::test]
async fn a_release_without_sealed_keys_starts_only_when_the_operator_asks_for_new_ones() {
    let (cfg, dir) = sealed_config("keys-refuse");
    let err = obtain(&cfg, &start(false), &MockAttestor).await.err().expect("started with no keys");
    assert!(format!("{err:#}").contains("no keys are sealed for this release"), "{err:#}");
    assert!(!file_for(&dir, &release(&cfg), 0).exists(), "a refused start sealed keys");

    let (key, tls, store) = obtain(&cfg, &start(true), &MockAttestor).await.unwrap();
    assert!(store.is_some() && file_for(&dir, &release(&cfg), 0).exists(), "the new keys are not sealed");

    // A restart, without the flag, opens the same keys.
    let (again, tls_again, _) = obtain(&cfg, &start(false), &MockAttestor).await.unwrap();
    assert_eq!(again.address(), key.address());
    assert_eq!(tls_again.public_key_sha256, tls.public_key_sha256);
}

#[tokio::test]
async fn keys_are_opened_or_sealed_only_for_the_vaults_current_release() {
    let (cfg, dir) = sealed_config("keys-current-release");
    let this = release(&cfg);
    let named = |release: Release, generation| Vault::naming(CurrentRelease { id: release.id(), generation });
    let superseded = [
        ("no release", Vault::naming(CurrentRelease::default())),
        ("an upgrade", named(Release { launch_digest: [0x22; 48], ..this }, 2)),
        ("a config change", named(Release { config_sha256: [0xc2; 32], ..this }, 2)),
    ];
    for (what, vault) in &superseded {
        let err = obtain_with(&cfg, &start(true), &MockAttestor, Some(vault)).await.err();
        let err = err.unwrap_or_else(|| panic!("new keys were sealed while the vault names {what}"));
        assert!(format!("{err:#}").contains("the vault's current release is"), "{what}: {err:#}");
        assert!(std::fs::read_dir(&dir).is_err(), "{what}: a refused start sealed keys");
    }
    let (key, _, store) = obtain_with(&cfg, &start(true), &MockAttestor, Some(&named(this, 1))).await.unwrap();
    assert!(file_for(&dir, &this, 1).exists() && store.unwrap().generation() == 1, "sealed outside generation 1");
    let (again, _, _) = obtain_with(&cfg, &start(false), &MockAttestor, Some(&named(this, 1))).await.unwrap();
    assert_eq!(again.address(), key.address());

    // Once governance names another release, this one's sealed keys stay sealed.
    for (what, vault) in &superseded {
        let err = obtain_with(&cfg, &start(false), &MockAttestor, Some(vault)).await.err();
        let err = err.unwrap_or_else(|| panic!("the keys opened while the vault names {what}"));
        assert!(format!("{err:#}").contains("refusing this release's keys"), "{what}: {err:#}");
    }

    // Named again, in a later generation, it finds no keys there: its copy from generation 1 stays
    // sealed, and it takes the keys afresh from the release running then.
    let err = obtain_with(&cfg, &start(false), &MockAttestor, Some(&named(this, 3))).await.err();
    let err = format!("{:#}", err.expect("a release named again reopened its older copy"));
    assert!(err.contains("no keys are sealed for this release") && err.contains("generation 3"), "{err}");
    assert!(file_for(&dir, &this, 1).exists() && !file_for(&dir, &this, 3).exists());
}

#[tokio::test(start_paused = true)]
async fn a_running_node_alerts_once_the_vault_names_another_release_or_generation() {
    let (_guard, mut alerts) = capture_alerts();
    let sealed = Release { launch_digest: DIGEST, config_sha256: [0xc1; 32] };
    let vault = Vault::naming(CurrentRelease { id: sealed.id(), generation: 2 });
    let current = vault.current.clone();
    let every = Duration::from_secs(60);
    let watch = tokio::spawn(watch_release(vault, sealed, 2, every, untrusted()));
    tokio::time::sleep(every * 3).await;
    assert!(alerts.try_recv().is_err(), "the release the vault names raised an alert");

    // Governance names the same release again, then another: one Critical episode.
    *current.lock().unwrap() = CurrentRelease { id: sealed.id(), generation: 3 };
    tokio::time::sleep(every).await;
    let alert = alerts.try_recv().expect("a new generation raises Critical");
    assert_eq!((alert["alert"].as_str(), alert["level"].as_str()), ("release_superseded", "ERROR"));
    *current.lock().unwrap() = CurrentRelease { id: [0x99; 32], generation: 4 };
    tokio::time::sleep(every * 3).await;
    assert!(alerts.try_recv().is_err(), "the same episode alerted again");
    watch.abort();
}

#[tokio::test(start_paused = true)]
async fn from_a_turns_boundary_the_turn_alone_names_a_release() {
    let (_guard, mut alerts) = capture_alerts();
    let (sealed, incumbent) = (
        Release { launch_digest: DIGEST, config_sha256: [0xc1; 32] },
        Release { launch_digest: DIGEST, config_sha256: [0xc2; 32] },
    );
    // The finalised vault still names the incumbent; the turn trusted time has reached names this release.
    let vault = Vault::naming(CurrentRelease { id: incumbent.id(), generation: 3 });
    let boundary = 1_793_226_600_000;
    *vault.scheduled.lock().unwrap() = Some(crate::checkpoints::ScheduledTurn {
        turn: 20_754,
        boundary,
        candidate: AccountId([0xb0; 20]),
        admin_key: AccountId([0xb1; 20]),
        release: sealed.id(),
        generation: 4,
        source: None,
    });
    let scheduled = vault.scheduled.clone();
    let every = Duration::from_secs(60);
    let clock = Arc::new(crate::clock::SystemClock);
    let sample =
        crate::trusted_time::Sample { at: std::time::Instant::now(), earliest_ms: boundary, latest_ms: boundary + 1 };
    let (_samples, samples_rx) = tokio::sync::watch::channel(Some(sample));
    let trusted = crate::trusted_time::TrustedClock::new(clock, samples_rx, Duration::from_secs(1));
    let watch = tokio::spawn(watch_release(vault, sealed, 4, every, trusted));
    tokio::time::sleep(every * 3).await;
    assert!(alerts.try_recv().is_err(), "the turn's member was paged before finality named it");
    // The turn names another release: the vault's finalised one no longer counts.
    scheduled.lock().unwrap().as_mut().unwrap().release = incumbent.id();
    tokio::time::sleep(every).await;
    let alert = alerts.try_recv().expect("a release the turn does not name raises Critical");
    assert_eq!(alert["alert"].as_str(), "release_superseded");
    watch.abort();
}

#[tokio::test(start_paused = true)]
async fn a_superseded_release_alerts_before_the_vault_folds_the_change() {
    let (_guard, mut alerts) = capture_alerts();
    let sealed = Release { launch_digest: DIGEST, config_sha256: [0xc1; 32] };
    let vault = Vault::naming(CurrentRelease { id: sealed.id(), generation: 2 });
    *vault.synced.lock().unwrap() = Some(sealed.id());
    let current = vault.current.clone();
    let every = Duration::from_secs(60);
    let watch = tokio::spawn(watch_release(vault, sealed, 2, every, untrusted()));
    tokio::time::sleep(every * 3).await;
    assert!(alerts.try_recv().is_err(), "the release the vault names raised an alert");

    // The operator moves to another release and the finalized block shows it, but nobody folded
    // the change: the vault reads the new release id at the old generation, and `synced` still names
    // the old release. The running release is already superseded and a restart finds no keys, so the
    // watch must alert now rather than wait for anyone's syncRelease, and not only on the generation.
    *current.lock().unwrap() = CurrentRelease { id: [0x99; 32], generation: 2 };
    tokio::time::sleep(every).await;
    let alert = alerts.try_recv().expect("a superseded release raises Critical before the fold");
    assert_eq!((alert["alert"].as_str(), alert["level"].as_str()), ("release_superseded", "ERROR"));
    watch.abort();
}

#[tokio::test]
async fn a_flag_beside_sealed_keys_refuses_rather_than_replacing_them() {
    let (cfg, _dir) = sealed_config("keys-flag-beside-sealed");
    let (key, _, _) = obtain(&cfg, &start(true), &MockAttestor).await.unwrap();
    let predecessor = KeyStart { predecessor: Some("127.0.0.1:9".into()), ..start(false) };
    for (what, flags) in [("--new-key", start(true)), ("--predecessor", predecessor)] {
        let err = obtain(&cfg, &flags, &MockAttestor).await.err().unwrap_or_else(|| panic!("{what} replaced keys"));
        assert!(format!("{err:#}").contains("already sealed"), "{what}: {err:#}");
    }
    assert_eq!(obtain(&cfg, &start(false), &MockAttestor).await.unwrap().0.address(), key.address());
}

#[tokio::test]
async fn the_stand_in_measurement_comes_only_with_the_mock_tees_sealing() {
    let (mut cfg, _dir) = sealed_config("keys-stand-in");
    let err = obtain(&cfg, &KeyStart { new_key: true, ..Default::default() }, &MockAttestor).await.err();
    assert!(format!("{:#}", err.expect("mock sealing without a stand-in")).contains("--mock-launch-digest"));

    cfg.tee.provider = TeeProvider::ConfigfsTsm;
    let err = obtain(&cfg, &start(true), &MockAttestor).await.err().expect("a stand-in under real attestation");
    assert!(format!("{err:#}").contains("is for the mock TEE"), "{err:#}");

    let unsealed = dev_config();
    let err = obtain(&unsealed, &start(false), &MockAttestor).await.err().expect("a stand-in without sealing");
    assert!(format!("{err:#}").contains("need [tee.sealing]"), "{err:#}");

    assert_eq!(parse_launch_digest(&hex::encode(DIGEST)), Ok(DIGEST));
    assert_eq!(parse_launch_digest(&format!("0x{}", hex::encode(DIGEST))), Ok(DIGEST));
    assert!(parse_launch_digest(&"11".repeat(32)).is_err(), "a 32-byte digest parsed");
    assert!(parse_launch_digest("zz").is_err());
}

#[tokio::test]
async fn without_sealing_only_the_mock_tee_starts_and_takes_no_flag() {
    let mut cfg = dev_config();
    let (key, _, store) = obtain(&cfg, &KeyStart::default(), &MockAttestor).await.unwrap();
    assert!(store.is_none());
    assert_eq!(key.address(), EnclaveKey::test_key().address(), "the configured published key");
    let new_key = KeyStart { new_key: true, ..Default::default() };
    let err = obtain(&cfg, &new_key, &MockAttestor).await.err().expect("--new-key without sealing");
    assert!(format!("{err:#}").contains("need [tee.sealing]"), "{err:#}");

    (cfg.tee.provider, cfg.tee.use_published_mock_key) = (TeeProvider::ConfigfsTsm, false);
    let err = obtain(&cfg, &KeyStart::default(), &MockAttestor).await.err().expect("real attestation without sealing");
    assert!(format!("{err:#}").contains("real attestation needs [tee.sealing]"), "{err:#}");
}

#[tokio::test]
async fn a_restart_attests_the_same_signing_key_and_tls_key() {
    let (cfg, _dir) = sealed_config("keys-attested");
    let first = attested(&cfg, &start(true)).await.unwrap();
    assert_eq!(first.attestation.enclave_address, first.enclave.address_hex());
    assert_eq!(first.attestation.tls_public_key_sha256, format!("0x{}", hex::encode(first.tls.public_key_sha256)));
    let restarted = attested(&cfg, &start(false)).await.unwrap();
    assert_eq!(restarted.attestation.bundle, first.attestation.bundle, "a restart attests other keys");
}

#[tokio::test]
async fn real_attestation_refuses_an_unprotected_clock_before_any_key_is_obtained() {
    let (_guard, mut alerts) = capture_alerts();
    // The mock TEE reads neither the clocksource nor the MSR (the existing mock start above passes
    // without both); real attestation checks them before opening keys.
    let mut cfg = dev_config();
    cfg.tee.provider = TeeProvider::ConfigfsTsm;
    cfg.tee.use_published_mock_key = false;
    // The host running these tests has no Secure TSC guest, so the check refuses before `obtain`
    // touches a key; the message names what the clock failed on.
    let err = attested(&cfg, &start(true)).await.err().expect("started on a clock Secure TSC does not protect");
    let msg = format!("{err:#}");
    assert!(msg.contains("clocksource") || msg.contains("msr"), "{msg}");
    let alert = alerts.try_recv().expect("the refusal raised an alert");
    assert_eq!((alert["alert"].as_str(), alert["level"].as_str()), ("clocksource_unprotected", "ERROR"));
}

#[test]
fn the_first_start_after_a_handover_needs_the_log_to_end_at_the_predecessors_checkpoint() {
    let root = format!("0x{}", "ab".repeat(32));
    let head = Head { seq: 5, root: root.clone() };
    let store = |name: &str| {
        let mut keys = KeySet::generate().unwrap();
        keys.predecessor_head = Some(head.clone());
        KeyStore::create(&scratch_dir(name), &MockSealer { launch_digest: DIGEST }, [0xc1; 32], 1, keys).unwrap()
    };
    let other_root = format!("0x{}", "cd".repeat(32));
    for (what, last_seq, last) in [
        ("a log that ends before the checkpoint", Some(4), Some(checkpoint(4, &root))),
        ("a log that goes on past it", Some(6), Some(checkpoint(5, &root))),
        ("another root at its seq", Some(5), Some(checkpoint(5, &other_root))),
        ("a log with no checkpoint", Some(5), None),
    ] {
        let refused = store(&format!("head-refused-{last_seq:?}-{}", last.is_some()));
        let err = check_predecessor_head(Some(&refused), last_seq, last.as_ref()).err();
        assert!(err.is_some(), "{what} passed");
        assert_eq!(refused.keys().predecessor_head, Some(head.clone()), "{what}: the head was forgotten");
    }

    // A log that ends there passes once, and the head is forgotten: later starts extend the log.
    let passed = store("head-passed");
    check_predecessor_head(Some(&passed), Some(5), Some(&checkpoint(5, &root))).unwrap();
    assert_eq!(passed.keys().predecessor_head, None);
    check_predecessor_head(Some(&passed), Some(9), Some(&checkpoint(9, &other_root))).unwrap();
    // Keys nobody handed over, or no sealing, carry no head.
    check_predecessor_head(None, Some(1), None).unwrap();
}

// ── Helpers ──

/// A start with the stand-in measurement, asking for new keys when `new_key`.
fn start(new_key: bool) -> KeyStart {
    KeyStart { new_key, predecessor: None, mock_launch_digest: Some(DIGEST) }
}

/// The release `cfg` runs under the stand-in measurement.
fn release(cfg: &NodeConfig) -> Release {
    Release { launch_digest: DIGEST, config_sha256: cfg.file_sha256 }
}

/// The dev config with the mock stand-in for sealing into a fresh directory.
fn sealed_config(name: &str) -> (NodeConfig, PathBuf) {
    let dir = scratch_dir(name);
    let mut cfg = dev_config();
    cfg.tee.use_published_mock_key = false;
    cfg.tee.sealing = Some(toml::from_str(&format!("dir = {:?}", dir.display().to_string())).unwrap());
    (cfg, dir)
}

fn checkpoint(seq: u64, root: &str) -> Checkpoint {
    serde_json::from_value(serde_json::json!({ "seq": seq, "root": root, "signature": "0x", "signer": "0x" })).unwrap()
}

#[test]
fn the_handover_episode_escalates_past_the_registry_effective_time_and_resolves() {
    let operator = AccountId([0x42; 20]);
    let other = AccountId([0x63; 20]);
    let election = |registry_effective_at, cancelled| engine_core::election::Election {
        epoch: 7,
        candidate: other,
        admin_key: other,
        effective_at: registry_effective_at + 86_400_000,
        registry_effective_at,
        cancelled,
    };
    let past = 1_700_000_000_000;
    let future = past + 7 * 86_400_000;

    // No election: no episode. Another operator named past the registry's effective time is
    // already overdue, even as the first episode observed; the engine's later deferral does not
    // delay the page.
    assert_eq!(episode(None, operator, Some(past)), Episode::None);
    assert_eq!(episode(Some(election(past, false)), operator, Some(past + 1)), Episode::Overdue);
    let mut ours = election(past, false);
    ours.admin_key = operator;
    assert_eq!(episode(Some(ours), operator, Some(past + 1)), Episode::None);
    assert_eq!(episode(Some(election(past, true)), operator, Some(past + 1)), Episode::None);

    // Another operator named: elected before the registry's effective time, overdue from it, and
    // waiting when trusted time is absent. A cancelled election resolves.
    assert_eq!(episode(Some(election(future, false)), operator, Some(future - 1)), Episode::Elected);
    assert_eq!(episode(Some(election(future, false)), operator, Some(future)), Episode::Overdue);
    assert_eq!(episode(Some(election(future, false)), operator, None), Episode::Elected);
    assert_eq!(episode(Some(election(future, true)), operator, Some(future + 1)), Episode::None);
}

/// A trusted clock with no sample: no turn's boundary has been reached.
fn untrusted() -> crate::trusted_time::TrustedClock {
    let clock = std::sync::Arc::new(crate::clock::SystemClock);
    crate::trusted_time::TrustedClock::new(clock, tokio::sync::watch::channel(None).1, Duration::from_secs(1))
}
