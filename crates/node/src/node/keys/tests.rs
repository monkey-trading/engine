use super::*;
use crate::node::handover::{Episode, episode};
use std::path::PathBuf;

use engine_core::eip712::B256;
use engine_core::types::AccountId;

use crate::checkpoints::CurrentRelease;
use crate::log::Checkpoint;
use crate::tee::MockAttestor;
use crate::tee::keys::Head;
use crate::tee::sealing::MockSealer;
use crate::test_support::{capture_alerts, dev_config, scratch_dir};

const DIGEST: [u8; 48] = [0x11; 48];

#[tokio::test]
async fn a_release_without_sealed_keys_starts_only_when_the_operator_asks_for_new_ones() {
    let (cfg, dir) = sealed_config("keys-refuse");
    let err =
        obtain(&cfg, &start(false), &MockAttestor, &Metrics::new(), None).await.err().expect("started with no keys");
    assert!(format!("{err:#}").contains("no keys are sealed for this release"), "{err:#}");
    assert!(!file_for(&dir, &release(&cfg), 0).exists(), "a refused start sealed keys");

    let (key, tls, store) = obtain(&cfg, &start(true), &MockAttestor, &Metrics::new(), None).await.unwrap();
    assert!(store.is_some() && file_for(&dir, &release(&cfg), 0).exists(), "the new keys are not sealed");

    // A restart, without the flag, opens the same keys.
    let (again, tls_again, _) = obtain(&cfg, &start(false), &MockAttestor, &Metrics::new(), None).await.unwrap();
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
        let err =
            obtain_with(&cfg, &start(true), &MockAttestor, Some(vault), trusted_at(0), &Metrics::new()).await.err();
        let err = err.unwrap_or_else(|| panic!("new keys were sealed while the vault names {what}"));
        assert!(format!("{err:#}").contains("the vault's current release is"), "{what}: {err:#}");
        assert!(std::fs::read_dir(&dir).is_err(), "{what}: a refused start sealed keys");
    }
    let (key, _, store) =
        obtain_with(&cfg, &start(true), &MockAttestor, Some(&named(this, 1)), trusted_at(0), &Metrics::new())
            .await
            .unwrap();
    assert!(file_for(&dir, &this, 1).exists() && store.unwrap().generation() == 1, "sealed outside generation 1");
    let (again, _, _) =
        obtain_with(&cfg, &start(false), &MockAttestor, Some(&named(this, 1)), trusted_at(0), &Metrics::new())
            .await
            .unwrap();
    assert_eq!(again.address(), key.address());

    // Once governance names another release, this one's sealed keys stay sealed.
    for (what, vault) in &superseded {
        let err =
            obtain_with(&cfg, &start(false), &MockAttestor, Some(vault), trusted_at(0), &Metrics::new()).await.err();
        let err = err.unwrap_or_else(|| panic!("the keys opened while the vault names {what}"));
        assert!(format!("{err:#}").contains("refusing this release's keys"), "{what}: {err:#}");
    }

    // Named again, in a later generation, it finds no keys there: its copy from generation 1 stays
    // sealed, and it takes the keys afresh from the release running then.
    let err = obtain_with(&cfg, &start(false), &MockAttestor, Some(&named(this, 3)), trusted_at(0), &Metrics::new())
        .await
        .err();
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
    let watch =
        tokio::spawn(watch_release(vault, sealed_store("keys-watch-generation", sealed, 2), every, untrusted()));
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
        set: 1,
        source: None,
    });
    let scheduled = vault.scheduled.clone();
    let every = Duration::from_secs(60);
    let clock = Arc::new(crate::clock::SystemClock);
    let sample =
        crate::trusted_time::Sample { at: std::time::Instant::now(), earliest_ms: boundary, latest_ms: boundary + 1 };
    let (samples, samples_rx) = tokio::sync::watch::channel(Some(sample));
    let trusted = crate::trusted_time::TrustedClock::new(clock, samples_rx, Duration::from_secs(1));
    let watch = tokio::spawn(watch_release(vault, sealed_store("keys-watch-turn", sealed, 4), every, trusted));
    tokio::time::sleep(every * 3).await;
    assert!(alerts.try_recv().is_err(), "the turn's member was paged before finality named it");
    // The turn names another release: the vault's finalised one no longer counts, though nothing
    // pages while the turn's member takes the keys from this node.
    scheduled.lock().unwrap().as_mut().unwrap().release = incumbent.id();
    tokio::time::sleep(every * 3).await;
    assert!(alerts.try_recv().is_err(), "paged within the turn's handover grace");
    let past = boundary + TURN_HANDOVER_GRACE_MS;
    let late = crate::trusted_time::Sample { at: std::time::Instant::now(), earliest_ms: past, latest_ms: past + 1 };
    samples.send_replace(Some(late));
    tokio::time::sleep(every).await;
    let alert = alerts.try_recv().expect("a release the turn does not name raises Critical past the grace");
    assert_eq!(alert["alert"].as_str(), "release_superseded");
    watch.abort();
}

#[tokio::test(start_paused = true)]
async fn a_turn_naming_this_release_in_another_generation_pages_only_while_the_keys_are_not_sealed_there() {
    let (_guard, mut alerts) = capture_alerts();
    let sealed = Release { launch_digest: DIGEST, config_sha256: [0xc1; 32] };
    let boundary = 1_793_226_600_000;
    // The finalised vault still names the turn's source; the turn names this release in a new generation.
    let vault = || {
        let vault = Vault::naming(CurrentRelease { id: [0x77; 32], generation: 9 });
        *vault.scheduled.lock().unwrap() = Some(crate::checkpoints::ScheduledTurn {
            turn: 20_754,
            boundary,
            candidate: AccountId([0xb0; 20]),
            admin_key: AccountId([0xb1; 20]),
            release: sealed.id(),
            generation: 6,
            set: 1,
            source: None,
        });
        vault
    };
    let (returning, stale) =
        (sealed_store("keys-watch-returning", sealed, 3), sealed_store("keys-watch-stale", sealed, 3));
    let every = Duration::from_secs(60);
    let clock: Arc<dyn crate::clock::Clock> = Arc::new(crate::clock::SystemClock);
    let at = |ms| crate::trusted_time::Sample { at: std::time::Instant::now(), earliest_ms: ms, latest_ms: ms + 1 };
    let (samples, samples_rx) = tokio::sync::watch::channel(Some(at(boundary)));
    let trusted = || crate::trusted_time::TrustedClock::new(clock.clone(), samples_rx.clone(), Duration::from_secs(1));
    let watch = tokio::spawn(watch_release(vault(), returning.clone(), every, trusted()));
    tokio::time::sleep(every * 3).await;
    assert!(alerts.try_recv().is_err(), "paged within the turn's handover grace");
    // The holder seals its keys in the turn's generation as it takes the turn: past the grace, nothing pages.
    returning.reseal(6).unwrap();
    samples.send_replace(Some(at(boundary + TURN_HANDOVER_GRACE_MS)));
    tokio::time::sleep(every * 3).await;
    assert!(alerts.try_recv().is_err(), "paged for keys sealed in the turn's generation");
    watch.abort();

    // Keys left in the earlier generation page past the grace.
    let watch = tokio::spawn(watch_release(vault(), stale, every, trusted()));
    tokio::time::sleep(every).await;
    let alert = alerts.try_recv().expect("keys left in the earlier generation raise Critical past the grace");
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
    let watch = tokio::spawn(watch_release(vault, sealed_store("keys-watch-fold", sealed, 2), every, untrusted()));
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
    let (key, _, _) = obtain(&cfg, &start(true), &MockAttestor, &Metrics::new(), None).await.unwrap();
    let predecessor = KeyStart { predecessor: Some("127.0.0.1:9".into()), ..start(false) };
    for (what, flags) in [("--new-key", start(true)), ("--predecessor", predecessor)] {
        let err = obtain(&cfg, &flags, &MockAttestor, &Metrics::new(), None)
            .await
            .err()
            .unwrap_or_else(|| panic!("{what} replaced keys"));
        assert!(format!("{err:#}").contains("already sealed"), "{what}: {err:#}");
    }
    assert_eq!(
        obtain(&cfg, &start(false), &MockAttestor, &Metrics::new(), None).await.unwrap().0.address(),
        key.address()
    );
}

#[tokio::test]
async fn the_stand_in_measurement_comes_only_with_the_mock_tees_sealing() {
    let (mut cfg, _dir) = sealed_config("keys-stand-in");
    let err = obtain(&cfg, &KeyStart { new_key: true, ..Default::default() }, &MockAttestor, &Metrics::new(), None)
        .await
        .err();
    assert!(format!("{:#}", err.expect("mock sealing without a stand-in")).contains("--mock-launch-digest"));

    cfg.tee.provider = TeeProvider::ConfigfsTsm;
    let err = obtain(&cfg, &start(true), &MockAttestor, &Metrics::new(), None)
        .await
        .err()
        .expect("a stand-in under real attestation");
    assert!(format!("{err:#}").contains("is for the mock TEE"), "{err:#}");

    let unsealed = dev_config();
    let err = obtain(&unsealed, &start(false), &MockAttestor, &Metrics::new(), None)
        .await
        .err()
        .expect("a stand-in without sealing");
    assert!(format!("{err:#}").contains("need [tee.sealing]"), "{err:#}");

    assert_eq!(parse_launch_digest(&hex::encode(DIGEST)), Ok(DIGEST));
    assert_eq!(parse_launch_digest(&format!("0x{}", hex::encode(DIGEST))), Ok(DIGEST));
    assert!(parse_launch_digest(&"11".repeat(32)).is_err(), "a 32-byte digest parsed");
    assert!(parse_launch_digest("zz").is_err());
}

#[tokio::test]
async fn without_sealing_only_the_mock_tee_starts_and_takes_no_flag() {
    let mut cfg = dev_config();
    let (key, _, store) = obtain(&cfg, &KeyStart::default(), &MockAttestor, &Metrics::new(), None).await.unwrap();
    assert!(store.is_none());
    assert_eq!(key.address(), EnclaveKey::test_key().address(), "the configured published key");
    let new_key = KeyStart { new_key: true, ..Default::default() };
    let err =
        obtain(&cfg, &new_key, &MockAttestor, &Metrics::new(), None).await.err().expect("--new-key without sealing");
    assert!(format!("{err:#}").contains("need [tee.sealing]"), "{err:#}");

    (cfg.tee.provider, cfg.tee.use_published_mock_key) = (TeeProvider::ConfigfsTsm, false);
    let err = obtain(&cfg, &KeyStart::default(), &MockAttestor, &Metrics::new(), None)
        .await
        .err()
        .expect("real attestation without sealing");
    assert!(format!("{err:#}").contains("real attestation needs [tee.sealing]"), "{err:#}");
}

#[tokio::test]
async fn a_restart_attests_the_same_signing_key_and_tls_key() {
    let (cfg, _dir) = sealed_config("keys-attested");
    let first = attested(&cfg, &start(true), &Metrics::new(), None).await.unwrap();
    assert_eq!(first.attestation.enclave_address, first.enclave.address_hex());
    assert_eq!(first.attestation.tls_public_key_sha256, format!("0x{}", hex::encode(first.tls.public_key_sha256)));
    let restarted = attested(&cfg, &start(false), &Metrics::new(), None).await.unwrap();
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
    let err = attested(&cfg, &start(true), &Metrics::new(), None)
        .await
        .err()
        .expect("started on a clock Secure TSC does not protect");
    let msg = format!("{err:#}");
    assert!(msg.contains("clocksource") || msg.contains("msr"), "{msg}");
    let alert = alerts.try_recv().expect("the refusal raised an alert");
    assert_eq!((alert["alert"].as_str(), alert["level"].as_str()), ("clocksource_unprotected", "ERROR"));
}

#[test]
fn a_certificate_store_that_does_not_open_refuses_the_start_with_a_critical_alert() {
    let (_guard, mut alerts) = capture_alerts();
    let (mut cfg, dir) = sealed_config("keys-certificates");
    assert!(certificates(&cfg, &start(false)).unwrap().is_none(), "a store opened without [api.tls.acme]");
    let acme =
        "[acme]\ndomain = 'api.engine.test'\ndirectory_url = 'https://ca.test/dir'\ncontact = 'mailto:ops@engine.test'";
    cfg.api.tls = Some(toml::from_str(acme).unwrap());
    let store = certificates(&cfg, &start(false)).unwrap().expect("no store with [api.tls.acme]");
    store.replace("api.engine.test", &[0x7e; 32], "https://ca.test/dir", &[vec![0x5a; 48].into()]).unwrap();
    assert!(alerts.try_recv().is_err(), "a store that opens raised an alert");

    // The host alters the file: the start refuses, with the alert that names the store.
    let path = crate::tee::certificates::file_for(&dir, &store.release());
    let file = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, file.replace("\"format\":1", "\"format\":2")).unwrap();
    let err = certificates(&cfg, &start(false)).err().expect("an altered store opened");
    assert!(format!("{err:#}").contains("has format 2"), "{err:#}");
    let alert = alerts.try_recv().expect("the refusal raised no alert");
    assert_eq!((alert["alert"].as_str(), alert["level"].as_str()), ("sealed_certificates_unreadable", "ERROR"));
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
        let err = check_predecessor_head(Some(&refused), last_seq, last.as_ref(), |_, _| panic!("checked")).err();
        assert!(err.is_some(), "{what} passed");
        assert_eq!(refused.keys().predecessor_head, Some(head.clone()), "{what}: the head was forgotten");
        assert_eq!(refused.keys().log_check, None, "{what}: a refused start recorded a log check");
    }

    // A log that ends there passes once, and the head is forgotten: later starts extend the log. The
    // log check the keys carried becomes what the check makes of it and the head, in the same seal.
    let passed = store("head-passed");
    let verified = VerifiedHead { seq: 5, link: B256::repeat_byte(7), root: root.parse().unwrap() };
    let check = |previous: Option<VerifiedHead>, at: &Head| {
        assert_eq!((previous, at), (None, &head));
        Some(verified)
    };
    check_predecessor_head(Some(&passed), Some(5), Some(&checkpoint(5, &root)), check).unwrap();
    assert_eq!((passed.keys().predecessor_head, passed.keys().log_check), (None, Some(verified)));
    check_predecessor_head(Some(&passed), Some(9), Some(&checkpoint(9, &other_root)), |_, _| panic!("checked"))
        .unwrap();
    assert_eq!(passed.keys().log_check, Some(verified), "a start with no handover changed the log check");
    // Keys nobody handed over, or no sealing, carry no head.
    check_predecessor_head(None, Some(1), None, |_, _| panic!("checked")).unwrap();
}

// ── Helpers ──

/// Keys sealed for `release` in `generation`, in a fresh directory for `name`.
fn sealed_store(name: &str, release: Release, generation: u64) -> Arc<KeyStore> {
    let dir = scratch_dir(name);
    let sealer = MockSealer { launch_digest: release.launch_digest };
    Arc::new(KeyStore::create(&dir, &sealer, release.config_sha256, generation, KeySet::generate().unwrap()).unwrap())
}

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

/// Trusted time that has reached `ms`, as a start's sample reads it once a turn is drawn.
fn trusted_at(ms: Millis) -> std::future::Ready<anyhow::Result<Millis>> {
    std::future::ready(Ok(ms))
}

mod turns;
mod vault;
use vault::Vault;
