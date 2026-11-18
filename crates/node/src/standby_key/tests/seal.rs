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

/// A key the schedule took and no longer lists as the candidate's promotable key is replaced.
#[tokio::test]
async fn a_key_the_schedule_voided_or_holds_for_another_is_replaced() {
    let other = AccountId([0x98; 20]);
    for (case, list) in [
        (
            "registered, then replaced by another key",
            Box::new(|chain: &FakeChain, sealed| {
                chain.list_promotable(CANDIDATE, sealed);
                chain.list_promotable(CANDIDATE, other);
            }) as Box<dyn Fn(&FakeChain, AccountId)>,
        ),
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

/// A key the schedule never took is kept across restarts: a registration written or sent for it,
/// perhaps in a block the quorum has not agreed on yet, stays good for a key the enclave holds.
#[tokio::test]
async fn a_key_the_schedule_never_took_is_kept() {
    let dir = scratch_dir("standby-unregistered");
    let chain = FakeChain::new();
    let sealed = fresh_in(&dir).await.address();
    let (kept, generated) = standby(start(&dir, &chain).await.unwrap());
    assert!(!generated && kept.address() == sealed, "an unregistered key was replaced");
    chain.list_promotable(CANDIDATE, sealed);
    let (registered, generated) = standby(start(&dir, &chain).await.unwrap());
    assert!(!generated && registered.address() == sealed, "its registration landed for the key the enclave holds");
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
            assert!(promotion.same(&PROMOTION), "the prefixes are the promotion's: {promotion:?}");
        }
        other => panic!("the promoted key opened as {other:?}"),
    }
}

#[tokio::test]
async fn a_promotion_is_known_by_its_immutable_assignment_and_commitments() {
    let chain = FakeChain::new();
    let key = AccountId([0x5e; 20]);
    chain.promote(CANDIDATE, key);
    let found = |chain: &FakeChain| {
        let block = futures_util::FutureExt::now_or_never(chain.latest()).unwrap().unwrap();
        let read = promotion_of(&block, &DOMAIN, key, PROMOTION.turn, PROMOTION.position);
        futures_util::FutureExt::now_or_never(read).unwrap().unwrap()
    };
    let effect = |chain: &FakeChain| {
        let block = futures_util::FutureExt::now_or_never(chain.latest()).unwrap().unwrap();
        futures_util::FutureExt::now_or_never(in_effect(&block, &DOMAIN, key, &PROMOTION)).unwrap().unwrap()
    };
    let read_at = u64::try_from(NOW_MS / 1000).unwrap();
    assert!(found(&chain).is_some_and(|found| found.same(&PROMOTION) && found.block == read_at), "read at its block");
    assert!(effect(&chain));
    let later = Promotion { at: 99, block: 7, ..PROMOTION };
    assert!(later.same(&PROMOTION), "included again at a later block, the same one");
    chain.set(|chain| (chain.turn, chain.named) = (PROMOTION.turn + 1, (Address::repeat_byte(0x77), 0, 0, 8)));
    let kept = found(&chain).is_some_and(|found| found.same(&PROMOTION));
    assert!(kept, "the turn moving on leaves the assignment as it was");
    chain.set(|chain| chain.epoch += 1);
    assert!(!effect(&chain), "a later signer epoch ends the promotion's term");
    chain.set(|chain| {
        let (assignment, _) = chain.assignments.get_mut(&(PROMOTION.turn, PROMOTION.position)).unwrap();
        assignment.headLink = B256::repeat_byte(0x99);
    });
    let replaced = found(&chain).unwrap();
    assert!(!replaced.same(&PROMOTION), "another promotion at the same position fixes another history");
    chain.set(|chain| _ = chain.assignments.remove(&(PROMOTION.turn, PROMOTION.position)));
    assert_eq!(found(&chain), None, "a reorg that removed it");
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
        ("the guardian's retirement", Box::new(|chain: &mut ChainState| chain.signer = Address::ZERO)),
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

/// The ACME account the standby certificate is ordered under is sealed with the keys: a restart opens
/// it with them, and a promotion's reseal takes it into the turn's keys.
#[tokio::test]
async fn the_standby_account_opens_with_the_keys_and_a_promotion_reseals_it_as_the_turns() {
    let dir = scratch_dir("standby-account");
    let chain = FakeChain::new();
    let first = fresh_in(&dir).await;
    assert!(first.account().is_none(), "new standby keys hold an account");
    let account =
        AcmeAccount { directory_url: "https://ca.test/dir".into(), credentials: serde_json::json!({ "id": "acct" }) };
    first.seal_account(Some(account.clone())).unwrap();

    chain.list_promotable(CANDIDATE, first.address());
    let (reopened, generated) = standby(start(&dir, &chain).await.unwrap());
    assert!(!generated, "a promotable key was replaced");
    assert!(reopened.account() == Some(account.clone()), "the account did not open with the keys");

    chain.promote(CANDIDATE, first.address());
    let store = promoted(start(&dir, &chain).await.unwrap()).reseal(PROMOTION.generation).unwrap();
    assert!(store.account() == Some(account), "the reseal left the account behind");
}

/// The schedule carries a promotion into its member's next drawn turn (`_carryNext`), and each later
/// draw of the member keeps that generation (`_generation`): past one boundary or several, the key is
/// still the vault's signer in the promotion's generation, so a crash before the reseal reopens it as
/// the promotion's rather than seal a new key over the only copy. A drawn turn in another generation, or
/// drawn for another member, at the last boundary or one before it, leaves it no authority.
#[tokio::test]
async fn a_crash_before_the_reseal_reopens_the_key_past_the_boundaries_into_its_members_drawn_turns() {
    for draws in [1, 3] {
        let dir = scratch_dir("standby-carried");
        let chain = FakeChain::new();
        let key = fresh_in(&dir).await.address();
        chain.promote(CANDIDATE, key);
        (0..draws).for_each(|_| chain.draw_next(CANDIDATE, PROMOTION.generation));
        let keys = promoted(start(&dir, &chain).await.unwrap());
        keys.reseal(PROMOTION.generation).unwrap();
        assert_resealed_once(&dir, key);
    }
    let other = AccountId([0x77; 20]);
    let (carried, fresh) = ((CANDIDATE, PROMOTION.generation), (CANDIDATE, PROMOTION.generation + 1));
    for (case, draws) in [
        ("a fresh generation", vec![fresh]),
        ("another member", vec![(other, PROMOTION.generation)]),
        ("a fresh generation before the last draw", vec![carried, fresh, carried]),
        ("another member before the last draw", vec![carried, (other, PROMOTION.generation), carried]),
    ] {
        let dir = scratch_dir("standby-not-carried");
        let chain = FakeChain::new();
        let key = fresh_in(&dir).await.address();
        chain.promote(CANDIDATE, key);
        draws.iter().for_each(|(drawn, generation)| chain.draw_next(*drawn, *generation));
        let (keys, generated) = standby(start(&dir, &chain).await.unwrap());
        assert!(generated && keys.address() != key, "{case}: the key opened past the boundary");
    }
}

/// The promotion a restart finds names the turn in effect it is carried into, which the start publishes
/// the generation of (`Promotion::generations`): the promotion's own turn, or the member's drawn turn.
#[tokio::test]
async fn a_promotion_found_past_the_boundary_names_the_drawn_turn_it_is_carried_into() {
    let key = AccountId([0x5b; 20]);
    let chain = FakeChain::new();
    chain.promote(CANDIDATE, key);
    let chain: &FakeChain = &chain;
    let state = || async { crate::standby_key::chain::standby_state(chain, &DOMAIN, CANDIDATE, key).await.unwrap() };
    let StandbyState::Promoted(own) = state().await else { panic!("the promotion's own turn") };
    assert_eq!((own.turn, own.carried), (PROMOTION.turn, PROMOTION.turn));
    chain.draw_next(CANDIDATE, PROMOTION.generation);
    let StandbyState::Promoted(carried) = state().await else { panic!("the carried turn") };
    assert!(carried.same(&own), "a later read of the same promotion differs");
    assert_eq!((carried.turn, carried.carried), (PROMOTION.turn, PROMOTION.turn + 1));
    assert_eq!(carried.generations(), [(PROMOTION.turn, 7), (PROMOTION.turn + 1, 7)].into());
}

/// A promotion continued through more drawn turns than the walk follows is not proven unusable: the
/// start fails and the seal stays, while one at the limit still opens.
#[tokio::test]
async fn a_promotion_carried_past_the_walks_limit_keeps_the_seal() {
    for (draws, opens) in [(64, true), (65, false)] {
        let dir = scratch_dir("standby-carried-far");
        let chain = FakeChain::new();
        let key = fresh_in(&dir).await.address();
        chain.promote(CANDIDATE, key);
        (0..draws).for_each(|_| chain.draw_next(CANDIDATE, PROMOTION.generation));
        match start(&dir, &chain).await {
            Ok(opened) => assert!(opens && promoted(opened).address() == key, "{draws} draws opened the key"),
            Err(e) => assert!(!opens && format!("{e:#}").contains("drawn turns back"), "{draws} draws: {e:#}"),
        }
        let sealed = ReleaseFile::new(&dir, STANDBY, sealer(), CONFIG).unwrap().read().unwrap().expect("the seal");
        let sealed: KeySet = serde_json::from_slice(&sealed).unwrap();
        assert_eq!(AccountId(sealed.enclave_key().unwrap().address()), key, "{draws} draws replaced the seal");
    }
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
    KeyStore::create(&dir, &mock, CONFIG, PROMOTION.generation, keys.lock().clone()).unwrap();
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
    KeyStore::create(&dir, &mock, CONFIG, PROMOTION.generation, keys.lock().clone()).unwrap();
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
            assert!(promotion.same(&PROMOTION), "{promotion:?}");
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
