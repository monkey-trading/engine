use std::sync::Arc;
use std::time::Duration;

use engine_verify::Tcb;
use tokio::sync::watch;

use super::*;
use crate::clock::Clock;
use crate::clock::test_clock::TestClock;
use crate::config::TeeProvider;
use crate::handover::evidence::{self, Peer, Quoting, no_weaker};
use crate::standby_key::vouch::{
    MAX_VOUCH_LIFETIME, StandbyRegistration, VouchReply, VouchRequest, Voucher, answer, quoted, report_data, request,
    vouch_digest,
};
use crate::tee::{EnclaveKey, MockAttestor};
use crate::trusted_time::{Sample, TrustedClock};

const QUOTING: Quoting<'static> = Quoting { provider: TeeProvider::Mock, processor: None, kds_base: "" };
const NOW: u64 = (NOW_MS / 1000) as u64;

#[test]
fn the_vouch_digest_matches_the_schedule_byte_for_byte() {
    // The same inputs `cast` hashed as `abi.encode(keccak256("engine.standby-vouch.v1"), 31337, …)`.
    let domain = StandbyDomain {
        chain_id: 31337,
        vault: Address::repeat_byte(0x11),
        registry: Address::repeat_byte(0x22),
        schedule: Address::repeat_byte(0x33),
    };
    let digest = vouch_digest(&domain, AccountId([0x44; 20]), AccountId([0x55; 20]), [0x66; 32], 7, 99);
    assert_eq!(hex::encode(digest), "89aa2987e6d3fa977a1bb5e6ae2e014d6b6fa1a2b2c2e01af54a3320fe486110");
    // Signed raw by key 0x1234, as `cast wallet sign --no-hash` signs it.
    let mut secret = [0u8; 32];
    secret[30..].copy_from_slice(&0x1234u16.to_be_bytes());
    let key = EnclaveKey::from_secret(&secret).unwrap();
    assert_eq!(
        key.sign_digest(digest),
        "0xf50ce82781cd59b0f26c8b85508ed9ced83b344221fc6ae51a48de888f6c930364197b9c71527d3f6313e821c26f777232bb\
         1993ff0c05130ce3f8d40b21bb931c"
    );
}

#[test]
fn the_report_data_binds_every_field() {
    let (candidate, key, tls, nonce, expiry) = (AccountId([0x44; 20]), AccountId([0x55; 20]), [0x66; 32], 7, 99);
    let base = report_data(&DOMAIN, candidate, key, &tls, &release(), nonce, expiry);
    assert_eq!(base[32..], [0u8; 32]);
    let other = |byte: u8| AccountId([byte; 20]);
    let changed = [
        (
            "chain",
            report_data(&StandbyDomain { chain_id: 1, ..DOMAIN }, candidate, key, &tls, &release(), nonce, expiry),
        ),
        ("vault", report_data(&StandbyDomain { vault: SCHEDULE, ..DOMAIN }, candidate, key, &tls, &release(), 7, 99)),
        (
            "registry",
            report_data(&StandbyDomain { registry: VAULT, ..DOMAIN }, candidate, key, &tls, &release(), 7, 99),
        ),
        ("candidate", report_data(&DOMAIN, other(0x45), key, &tls, &release(), nonce, expiry)),
        ("key", report_data(&DOMAIN, candidate, other(0x56), &tls, &release(), nonce, expiry)),
        ("TLS key", report_data(&DOMAIN, candidate, key, &[0x67; 32], &release(), nonce, expiry)),
        (
            "measurement",
            report_data(&DOMAIN, candidate, key, &tls, &Release { launch_digest: [0x12; 48], ..release() }, 7, 99),
        ),
        (
            "config",
            report_data(&DOMAIN, candidate, key, &tls, &Release { config_sha256: [0xc2; 32], ..release() }, 7, 99),
        ),
        ("nonce", report_data(&DOMAIN, candidate, key, &tls, &release(), nonce + 1, expiry)),
        ("expiry", report_data(&DOMAIN, candidate, key, &tls, &release(), nonce, expiry + 1)),
    ];
    for (field, data) in changed {
        assert_ne!(data, base, "a changed {field} left the report data unchanged");
    }
}

#[tokio::test]
async fn a_request_quotes_its_own_report_data_with_a_fresh_nonce_and_a_bounded_expiry() {
    let keys = fresh_in(&scratch_dir("vouch-request")).await;
    let first = request(&QUOTING, &MockAttestor, &keys, &DOMAIN, CANDIDATE, NOW_MS).await.unwrap();
    let second = request(&QUOTING, &MockAttestor, &keys, &DOMAIN, CANDIDATE, NOW_MS).await.unwrap();
    assert_ne!(first.nonce, second.nonce, "two requests share a nonce");
    assert_eq!(first.expiry, NOW + 30 * 60);
    assert!(first.expiry < NOW + MAX_VOUCH_LIFETIME.as_secs());
    assert_eq!((first.candidate, first.key), (CANDIDATE, keys.address()));
    assert_eq!(first.tls_public_key_sha256.0, keys.tls_public_key_sha256());
    let tls = keys.tls_public_key_sha256();
    let bound = report_data(&DOMAIN, CANDIDATE, keys.address(), &tls, &release(), first.nonce, first.expiry);
    let peer = evidence::verify(TeeProvider::Mock, &first.evidence, &bound, None, None).unwrap();
    assert_eq!(peer.launch_digest, LAUNCH);
}

#[tokio::test]
async fn the_operator_vouches_with_the_vaults_signer_key() {
    let chain = registered();
    let signer = EnclaveKey::generate();
    chain.set(|chain| chain.signer = Address::from(signer.address()));
    let (member, request) = requested("vouch-operator", NOW + 1800).await;
    let trusted = trusted();
    let reply = answer(&request, &voucher(&trusted, &chain, Some(&signer), None)).await.unwrap();
    assert_eq!(reply.voucher, AccountId(signer.address()));
    let registration = StandbyRegistration::new(&DOMAIN, &request, &reply).unwrap();
    assert_eq!(
        registration,
        StandbyRegistration {
            key: member.address(),
            nonce: request.nonce,
            expiry: request.expiry,
            vouch: reply.vouch,
            voucher: reply.voucher,
        }
    );
}

#[tokio::test]
async fn a_promotable_standby_key_vouches_and_an_unlisted_one_refuses() {
    let chain = registered();
    let (_, request) = requested("vouch-standby-member", NOW + 1800).await;
    let own = fresh_in(&scratch_dir("vouch-standby-voucher")).await;
    // The enclave also holds a turn key the vault no longer names.
    let stale = EnclaveKey::generate();
    let trusted = trusted();
    let err = answer(&request, &voucher(&trusted, &chain, Some(&stale), Some(&own))).await.unwrap_err();
    assert!(format!("{err:#}").contains("holds neither"), "an unlisted standby key vouched: {err:#}");

    let other = AccountId([0x98; 20]);
    chain.list_promotable(other, own.address());
    let reply = answer(&request, &voucher(&trusted, &chain, Some(&stale), Some(&own))).await.unwrap();
    assert_eq!(reply.voucher, own.address());
    StandbyRegistration::new(&DOMAIN, &request, &reply).unwrap();

    // Voided again, as by its member's release move: it no longer vouches.
    chain.set(|chain| chain.promotable.clear());
    assert!(answer(&request, &voucher(&trusted, &chain, None, Some(&own))).await.is_err(), "a voided key vouched");
}

#[tokio::test]
async fn a_substituted_key_or_a_quote_over_other_report_data_is_refused() {
    let (chain, signer) = operated();
    let (_, request) = requested("vouch-substituted", NOW + 1800).await;
    let trusted = trusted();
    let changes = [
        ("key", (|r: &mut VouchRequest| r.key = AccountId([0x99; 20])) as fn(&mut VouchRequest)),
        ("candidate", |r| r.candidate = AccountId([0x98; 20])),
        ("TLS key", |r| r.tls_public_key_sha256 = B256::repeat_byte(0x67)),
        ("config", |r| r.config_sha256 = B256::repeat_byte(0xc2)),
        ("nonce", |r| r.nonce += 1),
        ("expiry", |r| r.expiry -= 1),
    ];
    for (field, change) in changes {
        let mut changed = request.clone();
        change(&mut changed);
        let err = answer(&changed, &voucher(&trusted, &chain, Some(&signer), None)).await.unwrap_err();
        assert!(format!("{err:#}").contains("binds other data"), "a changed {field}: {err:#}");
    }
}

#[tokio::test]
async fn a_quote_proving_another_measurement_than_the_request_names_is_refused() {
    let (chain, signer) = operated();
    let keys = fresh_in(&scratch_dir("vouch-measurement")).await;
    // The quote binds report data naming `LAUNCH`, from a platform that measures another release.
    let tls = keys.tls_public_key_sha256();
    let bound = report_data(&DOMAIN, CANDIDATE, keys.address(), &tls, &release(), 7, NOW + 1800);
    let other = Platform { launch_digest: [0x12; 48], policy: 0, tcb: 0 };
    let evidence = evidence::produce(&QUOTING, &MockAttestor, &other, bound).await.unwrap();
    let request = VouchRequest {
        evidence,
        ..quoted(&QUOTING, &MockAttestor, &keys, &DOMAIN, CANDIDATE, 7, NOW + 1800).await.unwrap()
    };
    let trusted = trusted();
    let err = answer(&request, &voucher(&trusted, &chain, Some(&signer), None)).await.unwrap_err();
    assert!(format!("{err:#}").contains("proves measurement"), "{err:#}");
}

#[tokio::test]
async fn a_release_other_than_the_registered_and_approved_one_is_refused() {
    let trusted = trusted();
    let (_, request) = requested("vouch-release", NOW + 1800).await;
    for (case, register, expected) in [
        ("another release", Some(([0x0e; 32], true)), "registered release"),
        ("an unapproved release", Some((release().id(), false)), "has not approved"),
        ("an unregistered candidate", None, "is not registered"),
    ] {
        let chain = FakeChain::new();
        let signer = EnclaveKey::generate();
        chain.set(|chain| chain.signer = Address::from(signer.address()));
        if let Some((registered, approved)) = register {
            chain.register(CANDIDATE, registered, approved);
        }
        let err = answer(&request, &voucher(&trusted, &chain, Some(&signer), None)).await.unwrap_err();
        assert!(format!("{err:#}").contains(expected), "{case}: {err:#}");
    }
}

#[tokio::test]
async fn an_expired_or_over_long_vouch_is_refused_and_the_bounds_are_inclusive_within() {
    let (chain, signer) = operated();
    let trusted = trusted();
    let hour = MAX_VOUCH_LIFETIME.as_secs();
    for (expiry, vouches) in
        [(NOW - 1, false), (NOW, false), (NOW + 1, true), (NOW + hour, true), (NOW + hour + 1, false)]
    {
        let (_, request) = requested("vouch-expiry", expiry).await;
        let answered = answer(&request, &voucher(&trusted, &chain, Some(&signer), None)).await;
        assert_eq!(answered.is_ok(), vouches, "expiry {} s past now: {answered:?}", expiry as i64 - NOW as i64);
    }
    // Without a trusted time sample nothing is vouched for.
    let (_, request) = requested("vouch-no-time", NOW + 1800).await;
    let unsampled = TrustedClock::new(Arc::new(TestClock::at(NOW_MS)), watch::channel(None).1, Duration::from_secs(1));
    let err = answer(&request, &voucher(&unsampled, &chain, Some(&signer), None)).await.unwrap_err();
    assert!(format!("{err:#}").contains("no trusted time"), "{err:#}");
}

#[tokio::test]
async fn a_vouch_that_does_not_recover_to_its_voucher_does_not_register() {
    let (chain, signer) = operated();
    let (_, request) = requested("vouch-recover", NOW + 1800).await;
    let trusted = trusted();
    let reply = answer(&request, &voucher(&trusted, &chain, Some(&signer), None)).await.unwrap();
    let claimed = VouchReply { voucher: AccountId([0x51; 20]), ..reply.clone() };
    assert!(StandbyRegistration::new(&DOMAIN, &request, &claimed).is_err(), "a misattributed vouch registered");
    let elsewhere = StandbyDomain { schedule: REGISTRY, ..DOMAIN };
    assert!(StandbyRegistration::new(&elsewhere, &request, &reply).is_err(), "another schedule's vouch registered");
}

#[test]
fn a_peer_runs_under_the_same_policy_and_no_lower_tcb() {
    let packed = |snp: u64| (8u64 << 56) | (snp << 48) | 3;
    let own = Platform { launch_digest: LAUNCH, policy: 0x30000, tcb: packed(8) };
    let peer = |policy, snp| Peer { launch_digest: LAUNCH, policy, tcb: Some(Tcb::from_packed(packed(snp))) };
    no_weaker(&own, &peer(0x30000, 8)).unwrap();
    no_weaker(&own, &peer(0x30000, 9)).unwrap();
    let err = no_weaker(&own, &peer(0x30000 | 1 << 19, 8)).unwrap_err();
    assert!(err.to_string().contains("guest policy"), "a debug policy passed: {err:#}");
    let err = no_weaker(&own, &peer(0x30000, 7)).unwrap_err();
    assert!(err.to_string().contains("below this platform's committed"), "{err:#}");
}

// ── Helpers ──

/// Trusted time at exactly `NOW_MS`.
fn trusted() -> TrustedClock {
    let clock = TestClock::at(NOW_MS);
    let sample = Sample { at: clock.instant(), earliest_ms: NOW_MS, latest_ms: NOW_MS };
    TrustedClock::new(Arc::new(clock), watch::channel(Some(sample)).1, Duration::from_secs(1))
}

/// A mock enclave at `LAUNCH` answering on `chain` with `signer` and `standby`.
fn voucher<'a>(
    trusted: &'a TrustedClock,
    chain: &'a FakeChain,
    signer: Option<&'a EnclaveKey>,
    standby: Option<&'a StandbyKeys>,
) -> Voucher<'a, FakeChain> {
    let platform = Platform { launch_digest: LAUNCH, policy: 0, tcb: 0 };
    Voucher { provider: TeeProvider::Mock, platform, trusted, chain, domain: DOMAIN, signer, standby }
}

/// A chain registering `CANDIDATE` on the tests' release, approved.
fn registered() -> FakeChain {
    let chain = FakeChain::new();
    chain.register(CANDIDATE, release().id(), true);
    chain
}

/// `registered`, with the vault naming the key it returns as its signer.
fn operated() -> (FakeChain, EnclaveKey) {
    let (chain, signer) = (registered(), EnclaveKey::generate());
    chain.set(|chain| chain.signer = Address::from(signer.address()));
    (chain, signer)
}

/// New standby keys for `CANDIDATE` in a scratch directory named `name`, and a request for a vouch
/// for them expiring at `expiry`.
async fn requested(name: &str, expiry: u64) -> (StandbyKeys, VouchRequest) {
    let keys = fresh_in(&scratch_dir(name)).await;
    let request = quoted(&QUOTING, &MockAttestor, &keys, &DOMAIN, CANDIDATE, 7, expiry).await.unwrap();
    (keys, request)
}
