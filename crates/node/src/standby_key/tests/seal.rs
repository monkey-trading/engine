use rcgen::PublicKeyData as _;
use sha2::Digest as _;

use super::*;
use crate::tee::sealing::file_for as turn_file_for;

#[tokio::test]
async fn a_first_start_seals_new_keys_that_a_restart_opens_while_promotable() {
    let dir = scratch_dir("standby-first-start");
    let chain = FakeChain::new();
    let (first, generated) = standby(start(&dir, &chain).await.unwrap());
    assert!(generated, "an empty directory held standby keys");
    let tls = first.tls_public_key_sha256();
    assert_eq!(Sha256::digest(first.tls_key().unwrap().subject_public_key_info())[..], tls[..]);
    assert_eq!(first.release(), release());

    chain.list_promotable(CANDIDATE, first.address());
    let (reopened, generated) = standby(start(&dir, &chain).await.unwrap());
    assert!(!generated, "a promotable key was replaced");
    assert_eq!((reopened.address(), reopened.tls_public_key_sha256()), (first.address(), tls));
    // Its signatures are raw: they recover over the digest itself.
    let digest = [0x5a; 32];
    let recovered = engine_core::eip712::recover(&B256::from(digest), &reopened.sign(digest));
    assert_eq!(recovered, Some(first.address()));
}

#[tokio::test]
async fn the_standby_seal_never_opens_as_the_turns_keys_or_another_kind() {
    let dir = scratch_dir("standby-own-kind");
    fresh_in(&dir).await;
    let mock = MockSealer { launch_digest: LAUNCH };
    for generation in 0..8 {
        assert!(KeyStore::open(&dir, &mock, CONFIG, generation).unwrap().is_none(), "found turn keys");
    }
    // Given a turn-key file's name, it does not open as the turn's keys.
    let standby = file_for(&dir, &release(), &STANDBY);
    std::fs::copy(&standby, turn_file_for(&dir, &release(), 1)).unwrap();
    assert!(KeyStore::open(&dir, &mock, CONFIG, 1).is_err(), "the standby seal opened as turn keys");
    // Read as another kind under its own name, it fails authenticated decryption.
    let other = Kind { tag: b"engine.sealed-secrets.v1", suffix: STANDBY.suffix, what: "secrets" };
    let err = ReleaseFile::new(&dir, other, sealer(), CONFIG).unwrap().read().unwrap_err();
    assert!(format!("{err:#}").contains("does not open"), "{err:#}");
}

#[tokio::test]
async fn a_member_that_moves_its_release_seals_a_new_key_at_once() {
    let dir = scratch_dir("standby-release-move");
    let chain = FakeChain::new();
    let before = fresh_in(&dir).await;
    chain.list_promotable(CANDIDATE, before.address());
    let moved = [0xc2; 32];
    let start = StandbyKeys::open_or_generate(&dir, sealer(), moved, &chain, &DOMAIN, CANDIDATE).await.unwrap();
    let (after, generated) = standby(start);
    assert!(generated, "the moved release opened the earlier release's seal");
    assert_ne!(after.address(), before.address());
    assert_eq!(after.release(), Release { launch_digest: LAUNCH, config_sha256: moved });
}

#[tokio::test]
async fn a_key_the_schedule_does_not_list_as_promotable_is_replaced() {
    let other = AccountId([0x98; 20]);
    for (case, list) in [
        ("unregistered", Box::new(|_: &FakeChain, _: AccountId| {}) as Box<dyn Fn(&FakeChain, AccountId)>),
        ("replaced by another key", Box::new(|chain: &FakeChain, _| chain.list_promotable(CANDIDATE, other))),
        (
            "promotable for another candidate",
            Box::new(|chain: &FakeChain, sealed| chain.list_promotable(other, sealed)),
        ),
    ] {
        let dir = scratch_dir("standby-not-promotable");
        let chain = FakeChain::new();
        let sealed = fresh_in(&dir).await.address();
        list(&chain, sealed);
        let (keys, generated) = standby(start(&dir, &chain).await.unwrap());
        assert!(generated && keys.address() != sealed, "{case}: the sealed key opened");
        // The new key replaced the old one's seal.
        chain.list_promotable(CANDIDATE, keys.address());
        let (reopened, generated) = standby(start(&dir, &chain).await.unwrap());
        assert!(!generated && reopened.address() == keys.address(), "{case}: the new key was not sealed");
    }
}

#[tokio::test]
async fn a_failed_read_leaves_the_seal_as_it_was() {
    let dir = scratch_dir("standby-read-fails");
    let chain = FakeChain::new();
    let sealed = fresh_in(&dir).await.address();
    chain.list_promotable(CANDIDATE, sealed);
    chain.set(|chain| chain.down = true);
    let err = start(&dir, &chain).await.unwrap_err();
    assert!(format!("{err:#}").contains("quorum lost"), "{err:#}");
    chain.set(|chain| chain.down = false);
    let (keys, generated) = standby(start(&dir, &chain).await.unwrap());
    assert!(!generated && keys.address() == sealed, "a failed read replaced the seal");
}

#[tokio::test]
async fn a_promotion_still_in_effect_opens_the_key_with_its_prefixes() {
    let dir = scratch_dir("standby-promoted");
    let chain = FakeChain::new();
    let key = fresh_in(&dir).await.address();
    chain.promote(CANDIDATE, key);
    match start(&dir, &chain).await.unwrap() {
        StandbyStart::Promoted { keys, promotion } => {
            assert_eq!(keys.address(), key);
            assert_eq!(promotion, PROMOTION, "the prefixes are the promotion's");
        }
        other => panic!("the promoted key opened as {other:?}"),
    }
}

#[tokio::test]
async fn a_restart_after_the_promotion_is_superseded_refuses_the_seal() {
    for (case, supersede) in [
        (
            "a later promotion",
            Box::new(|chain: &mut ChainState| chain.named = (Address::repeat_byte(0x77), 4, 2, 8))
                as Box<dyn Fn(&mut ChainState)>,
        ),
        ("another signer epoch", Box::new(|chain: &mut ChainState| chain.epoch += 1)),
        ("a release move's generation", Box::new(|chain: &mut ChainState| chain.named.3 += 1)),
    ] {
        let dir = scratch_dir("standby-superseded");
        let chain = FakeChain::new();
        let key = fresh_in(&dir).await.address();
        chain.promote(CANDIDATE, key);
        chain.set(|chain| supersede(chain));
        let (keys, generated) = standby(start(&dir, &chain).await.unwrap());
        assert!(generated && keys.address() != key, "{case}: the superseded key opened");
    }
}

#[tokio::test]
async fn a_stale_standby_seal_after_a_later_turn_does_not_open() {
    let dir = scratch_dir("standby-later-turn");
    let chain = FakeChain::new();
    let key = fresh_in(&dir).await.address();
    chain.promote(CANDIDATE, key);
    // The next turn starts, drawn for another member, and the vault has moved on.
    chain.set(|chain| {
        chain.time += 86_400;
        chain.turn += 1;
        chain.named = (Address::repeat_byte(0x77), 0, 0, 8);
    });
    let (keys, generated) = standby(start(&dir, &chain).await.unwrap());
    assert!(generated && keys.address() != key, "the stale seal opened");
}

#[tokio::test]
async fn a_crash_before_the_reseal_reopens_the_key_and_the_reseal_makes_it_the_turns() {
    let dir = scratch_dir("standby-crash-before");
    let chain = FakeChain::new();
    let key = fresh_in(&dir).await.address();
    chain.promote(CANDIDATE, key);
    // The node crashes before it reseals: the restart finds only the standby seal.
    drop(promoted(start(&dir, &chain).await.unwrap()));
    let keys = promoted(start(&dir, &chain).await.unwrap());
    let store = keys.reseal(PROMOTION.generation).unwrap();
    assert_eq!(AccountId(store.keys().enclave_key().unwrap().address()), key);
    assert_resealed_once(&dir, key);
    // A restart opens the turn's keys, and replaces the retired seal rather than open it.
    let (fresh, generated) = standby(start(&dir, &chain).await.unwrap());
    assert!(generated && fresh.address() != key, "the resealed key opened as a standby key");
    assert_resealed_once(&dir, key);
}

#[tokio::test]
async fn a_crash_during_the_reseal_leaves_a_partial_file_the_reseal_replaces() {
    let dir = scratch_dir("standby-crash-during");
    let chain = FakeChain::new();
    let key = fresh_in(&dir).await.address();
    chain.promote(CANDIDATE, key);
    // The write's temporary file, cut short; the turn's file itself was never renamed in.
    let turn = turn_file_for(&dir, &release(), PROMOTION.generation);
    std::fs::write(turn.with_extension("partial"), b"{\"format\":1,\"launch").unwrap();
    let keys = promoted(start(&dir, &chain).await.unwrap());
    assert_eq!(keys.address(), key);
    keys.reseal(PROMOTION.generation).unwrap();
    assert_resealed_once(&dir, key);
}

#[tokio::test]
async fn a_crash_after_the_reseal_retires_the_standby_seal_without_opening_it() {
    let dir = scratch_dir("standby-crash-after");
    let chain = FakeChain::new();
    let key = fresh_in(&dir).await.address();
    chain.promote(CANDIDATE, key);
    // The reseal's file is durable, and the crash came before the standby seal was deleted.
    let keys = promoted(start(&dir, &chain).await.unwrap());
    let mock = MockSealer { launch_digest: LAUNCH };
    KeyStore::create(&dir, &mock, CONFIG, PROMOTION.generation, keys.keys.clone()).unwrap();
    drop(keys);
    let (fresh, generated) = standby(start(&dir, &chain).await.unwrap());
    assert!(generated && fresh.address() != key, "the standby seal opened after its reseal was durable");
    assert_resealed_once(&dir, key);
}

#[tokio::test]
async fn a_reseal_left_after_its_write_finishes_by_deleting_the_standby_seal() {
    let dir = scratch_dir("standby-reseal-retry");
    let chain = FakeChain::new();
    let key = fresh_in(&dir).await.address();
    chain.promote(CANDIDATE, key);
    let keys = promoted(start(&dir, &chain).await.unwrap());
    let mock = MockSealer { launch_digest: LAUNCH };
    KeyStore::create(&dir, &mock, CONFIG, PROMOTION.generation, keys.keys.clone()).unwrap();
    let store = keys.reseal(PROMOTION.generation).unwrap();
    assert_eq!(AccountId(store.keys().enclave_key().unwrap().address()), key);
    assert!(!file_for(&dir, &release(), &STANDBY).exists(), "the standby seal outlived its reseal");
}

#[tokio::test]
async fn a_turn_file_holding_another_key_refuses_the_reseal_and_keeps_the_standby_seal() {
    let dir = scratch_dir("standby-foreign-turn-file");
    let chain = FakeChain::new();
    let key = fresh_in(&dir).await.address();
    chain.promote(CANDIDATE, key);
    let mock = MockSealer { launch_digest: LAUNCH };
    KeyStore::create(&dir, &mock, CONFIG, PROMOTION.generation, KeySet::generate().unwrap()).unwrap();
    let err = start(&dir, &chain).await.unwrap_err();
    assert!(format!("{err:#}").contains("not the standby key"), "{err:#}");

    let keys = StandbyKeys::new(
        ReleaseFile::new(&dir, STANDBY, sealer(), CONFIG).unwrap(),
        dir.clone(),
        Arc::from(sealer()),
        mock.platform().unwrap(),
        KeySet::generate().unwrap(),
    )
    .unwrap();
    assert!(keys.reseal(PROMOTION.generation).is_err(), "resealed over another key's file");
    let standby = ReleaseFile::new(&dir, STANDBY, sealer(), CONFIG).unwrap().read().unwrap();
    assert!(standby.is_some(), "a refused reseal deleted the standby seal");
}

// ── Helpers ──

/// The keys a start opened as a promotion's, failing otherwise.
fn promoted(start: StandbyStart) -> StandbyKeys {
    match start {
        StandbyStart::Promoted { keys, promotion } => {
            assert_eq!(promotion, PROMOTION);
            keys
        }
        other => panic!("the promoted key did not open: {other:?}"),
    }
}

/// Requires the turn's keys for the promotion's generation to hold `key`, and no standby seal to
/// hold it.
fn assert_resealed_once(dir: &Path, key: AccountId) {
    let mock = MockSealer { launch_digest: LAUNCH };
    let store = KeyStore::open(dir, &mock, CONFIG, PROMOTION.generation).unwrap().expect("the turn's keys");
    assert_eq!(AccountId(store.keys().enclave_key().unwrap().address()), key);
    if let Some(plaintext) = ReleaseFile::new(dir, STANDBY, sealer(), CONFIG).unwrap().read().unwrap() {
        let sealed: KeySet = serde_json::from_slice(&plaintext).unwrap();
        assert_ne!(AccountId(sealed.enclave_key().unwrap().address()), key, "the standby seal still holds the key");
    }
}
