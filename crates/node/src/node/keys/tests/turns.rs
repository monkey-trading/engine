//! A start while rotation is on: from a daily turn's boundary by trusted time, the turn the finalised
//! schedule names decides whether this release opens or takes the keys, and in which generation,
//! whatever release the finalised vault still names; a start with no turn drawn takes no sample.

use super::*;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::checkpoints::{HandoverSource, ScheduledTurn};

/// The turn the schedule draws, and the generation it fixes for its member.
const TURN: u64 = 20_754;
const TURN_GENERATION: u64 = 4;

#[tokio::test]
async fn a_turn_naming_another_release_refuses_the_start_though_the_vault_names_this_one() {
    let (cfg, dir) = rotating_config("keys-turn-other-release");
    let this = release(&cfg);
    let vault = Vault::naming(CurrentRelease { id: this.id(), generation: 1 });
    // With no turn drawn the vault's release applies: this release seals keys in its generation.
    obtain_with(&cfg, &start(true), &MockAttestor, Some(&vault), trusted_at(boundary())).await.unwrap();
    assert!(file_for(&dir, &this, 1).exists());

    let other = Release { config_sha256: [0xc2; 32], ..this };
    *vault.scheduled.lock().unwrap() = Some(turn(&other, TURN_GENERATION, None));
    let err = obtain_with(&cfg, &start(false), &MockAttestor, Some(&vault), trusted_at(boundary())).await.err();
    let err = format!("{:#}", err.expect("the keys opened for a release the turn does not name"));
    assert!(err.contains("refusing this release's keys: a daily turn names another release"), "{err}");
}

#[tokio::test]
async fn keys_sealed_in_the_turns_generation_open_while_the_vault_still_names_the_incumbent() {
    let (cfg, dir) = rotating_config("keys-turn-sealed");
    let this = release(&cfg);
    let incumbent = Release { config_sha256: [0xc2; 32], ..this };
    let vault = Vault::naming(CurrentRelease { id: incumbent.id(), generation: 3 });
    *vault.scheduled.lock().unwrap() = Some(turn(&this, TURN_GENERATION, None));
    let sealer = MockSealer { launch_digest: DIGEST };
    let sealed =
        KeyStore::create(&dir, &sealer, cfg.file_sha256, TURN_GENERATION, KeySet::generate().unwrap()).unwrap();

    // A second short of the boundary the turn names no one yet, and the vault names the incumbent.
    let early = trusted_at(boundary() - 1_000);
    let err = obtain_with(&cfg, &start(false), &MockAttestor, Some(&vault), early).await.err();
    let err = format!("{:#}", err.expect("the keys opened before the turn's boundary"));
    assert!(err.contains("the vault's current release is"), "{err}");

    let (key, _, store) =
        obtain_with(&cfg, &start(false), &MockAttestor, Some(&vault), trusted_at(boundary())).await.unwrap();
    assert_eq!(key.address(), sealed.keys().enclave_key().unwrap().address(), "other keys opened");
    assert_eq!(store.map(|store| store.generation()), Some(TURN_GENERATION));
}

#[tokio::test]
async fn without_sealed_keys_or_a_flag_a_turns_member_takes_the_keys_from_the_turns_source() {
    let (mut cfg, dir) = rotating_config("keys-turn-take");
    cfg.node.log_dir = scratch_dir("keys-turn-take-log");
    let this = release(&cfg);
    let incumbent = Release { config_sha256: [0xc2; 32], ..this };
    let vault = Vault::naming(CurrentRelease { id: incumbent.id(), generation: 3 });
    // Two loopback ports nothing listens on: the source's published archive and its handover listener.
    let listeners = [(); 2].map(|_| std::net::TcpListener::bind("127.0.0.1:0").unwrap());
    let [archive, endpoint] = listeners.map(|listener| listener.local_addr().unwrap().to_string());
    let source = HandoverSource {
        candidate: AccountId([0xa0; 20]),
        release: incumbent.id(),
        api_hostname: archive,
        handover_endpoint: endpoint.clone(),
    };
    *vault.scheduled.lock().unwrap() = Some(turn(&this, TURN_GENERATION, Some(source)));

    let err = obtain_with(&cfg, &start(false), &MockAttestor, Some(&vault), trusted_at(boundary())).await.err();
    let err = format!("{:#}", err.expect("keys taken from a source that serves nothing"));
    // The start asked the turn's source, rather than refusing or resolving an election's incumbent.
    assert!(err.contains(&format!("no operator handed turn {TURN}'s keys over: {endpoint}: ")), "{err}");
    assert!(std::fs::read_dir(&dir).is_err(), "a failed handover sealed keys");
}

#[tokio::test]
async fn a_start_with_no_turn_drawn_takes_no_trusted_time_sample() {
    let (cfg, dir) = rotating_config("keys-turn-none-drawn");
    let this = release(&cfg);
    let vault = Vault::naming(CurrentRelease { id: this.id(), generation: 1 });
    let sampled = AtomicBool::new(false);
    obtain_with(&cfg, &start(true), &MockAttestor, Some(&vault), sampled_at(&sampled, boundary())).await.unwrap();
    assert!(!sampled.load(Ordering::SeqCst), "a start with no turn drawn sampled trusted time");
    assert!(file_for(&dir, &this, 1).exists());

    // A turn drawn is sampled for; before its boundary the vault's release applies, and its keys open.
    let other = Release { config_sha256: [0xc2; 32], ..this };
    *vault.scheduled.lock().unwrap() = Some(turn(&other, TURN_GENERATION, None));
    let early = sampled_at(&sampled, boundary() - 1_000);
    let (_, _, store) = obtain_with(&cfg, &start(false), &MockAttestor, Some(&vault), early).await.unwrap();
    assert!(sampled.load(Ordering::SeqCst), "a start with a turn drawn took no sample");
    assert_eq!(store.map(|store| store.generation()), Some(1));
}

// ── Helpers ──

/// `sealed_config` with the registry's rotation on, so a start reads the schedule.
fn rotating_config(name: &str) -> (NodeConfig, PathBuf) {
    let (mut cfg, dir) = sealed_config(name);
    cfg.operator.rotation_enabled = true;
    (cfg, dir)
}

/// When `TURN` begins.
fn boundary() -> Millis {
    engine_core::turns::boundary(TURN).expect("the turn has a boundary")
}

/// `TURN`, drawn for a member running `release` in `generation`, taking the keys from `source`.
fn turn(release: &Release, generation: u64, source: Option<HandoverSource>) -> ScheduledTurn {
    ScheduledTurn {
        turn: TURN,
        boundary: boundary(),
        candidate: AccountId([0xb0; 20]),
        admin_key: AccountId([0xb1; 20]),
        release: release.id(),
        generation,
        source,
    }
}

/// Trusted time at `ms`, recording in `sampled` that the start took the sample.
async fn sampled_at(sampled: &AtomicBool, ms: Millis) -> anyhow::Result<Millis> {
    sampled.store(true, Ordering::SeqCst);
    Ok(ms)
}
