//! Vouching for a standby key. The member's enclave quotes over report data binding the standby key,
//! its TLS key's hash, the candidate, its release, the deployment and the purpose `engine.standby.v1`,
//! with a nonce and an expiry (`request`). An attested enclave that holds the vault's signer key, or
//! a standby key the schedule lists as promotable, verifies that quote as a handover's predecessor
//! verifies a successor's, checks the candidate's registration through the custody quorum, and signs
//! the schedule's `vouchDigest` (`answer`), which the candidate registers with the key
//! (`StandbyRegistration`). The member-to-member transport carries both messages.

use std::time::Duration;

use alloy::primitives::{Address, B256, FixedBytes, U256, keccak256};
use alloy::sol_types::SolValue;
use anyhow::{Context, bail, ensure};
use engine_core::eip712::{self, Signature};
use engine_core::types::{AccountId, Millis};
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tracing::info;

use crate::config::TeeProvider;
use crate::handover::evidence::{self, Evidence, Quoting, no_weaker};
use crate::handover::padded;
use crate::standby_key::StandbyKeys;
use crate::standby_key::chain::{StandbyChain, StandbyDomain, vouch_view};
use crate::tee::sealing::{Platform, Release};
use crate::tee::{Attestor, EnclaveKey};
use crate::trusted_time::TrustedClock;

/// The purpose the standby key's attestation binds.
const PURPOSE: &[u8] = b"engine.standby.v1";
/// The domain the schedule's `vouchDigest` hashes first, as `OperatorSchedule.VOUCH_DOMAIN` declares
/// it before hashing.
const VOUCH_DOMAIN: &[u8] = b"engine.standby-vouch.v1";
/// The furthest past trusted time a vouch's expiry may lie: one hour, so a vouch a registration has
/// not used soon is dead.
pub(crate) const MAX_VOUCH_LIFETIME: Duration = Duration::from_secs(60 * 60);
/// The expiry a request asks for past the time trusted time has reached: half the bound, so a voucher
/// whose trusted time runs behind the requester's still signs.
const REQUEST_LIFETIME: Duration = Duration::from_secs(30 * 60);

/// A request for a vouch: the standby key, what its attestation binds, and the quote.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub(crate) struct VouchRequest {
    /// The candidate the key is for.
    pub candidate: AccountId,
    /// The standby key's address.
    pub key: AccountId,
    /// The SHA-256 of the TLS key's DER SubjectPublicKeyInfo.
    pub tls_public_key_sha256: B256,
    /// The release's launch measurement, which the quote proves.
    pub launch_digest: FixedBytes<48>,
    /// The SHA-256 of the config the release runs.
    pub config_sha256: B256,
    pub nonce: u64,
    /// The vouch's expiry, in Unix seconds.
    pub expiry: u64,
    /// The member's quote over the request's report data.
    pub evidence: Evidence,
}

/// A vouch: the schedule's `vouchDigest` for the request, signed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub(crate) struct VouchReply {
    /// The key that signed: the vault's signer, or a standby key the schedule lists as promotable.
    pub voucher: AccountId,
    /// 65-byte `r || s || v` over the digest, with no message prefix.
    pub vouch: Signature,
}

/// What the candidate registers with the schedule once a vouch for its standby key arrives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct StandbyRegistration {
    pub key: AccountId,
    pub nonce: u64,
    pub expiry: u64,
    pub vouch: Signature,
    pub voucher: AccountId,
}

/// What a vouching enclave answers with.
pub(crate) struct Voucher<'a, C> {
    /// This enclave's attestation, which takes peers of its own kind only.
    pub provider: TeeProvider,
    /// This release's platform, whose policy and TCB a peer's must match or exceed.
    pub platform: Platform,
    pub trusted: &'a TrustedClock,
    pub chain: &'a C,
    pub domain: StandbyDomain,
    /// The enclave's turn signing key, which vouches while the vault names it.
    pub signer: Option<&'a EnclaveKey>,
    /// The enclave's standby keys, which vouch while the schedule lists them as promotable.
    pub standby: Option<&'a StandbyKeys>,
}

impl StandbyRegistration {
    /// The registration `reply` vouches for, once its vouch recovers to the voucher it names over the
    /// digest of `request` in `domain`. Whether that voucher may vouch is the schedule's to check.
    /// Error cases:
    /// - the vouch does not recover to the voucher
    pub(crate) fn new(domain: &StandbyDomain, request: &VouchRequest, reply: &VouchReply) -> anyhow::Result<Self> {
        let release = Release { launch_digest: request.launch_digest.0, config_sha256: request.config_sha256.0 };
        let digest = vouch_digest(domain, request.candidate, request.key, release.id(), request.nonce, request.expiry);
        ensure!(
            eip712::recover(&B256::from(digest), &reply.vouch) == Some(reply.voucher),
            "the vouch does not recover to its voucher {}",
            reply.voucher
        );
        Ok(Self {
            key: request.key,
            nonce: request.nonce,
            expiry: request.expiry,
            vouch: reply.vouch,
            voucher: reply.voucher,
        })
    }
}

/// A request for a vouch for `keys` as `candidate`'s standby keys in `domain`, quoted by `attestor`,
/// with a fresh nonce and an expiry `REQUEST_LIFETIME` past `now_ms`, the time trusted time has
/// reached.
/// Error cases:
/// - `now_ms` is before the epoch, or the RNG fails
/// - the attestor cannot quote, or a hardware quote's collateral cannot be fetched
pub(crate) async fn request(
    quoting: &Quoting<'_>,
    attestor: &dyn Attestor,
    keys: &StandbyKeys,
    domain: &StandbyDomain,
    candidate: AccountId,
    now_ms: Millis,
) -> anyhow::Result<VouchRequest> {
    let mut nonce = [0u8; 8];
    SystemRandom::new().fill(&mut nonce).map_err(|_| anyhow::anyhow!("the RNG failed"))?;
    let now = u64::try_from(now_ms / 1000).context("a time before the epoch")?;
    quoted(quoting, attestor, keys, domain, candidate, u64::from_be_bytes(nonce), now + REQUEST_LIFETIME.as_secs())
        .await
}

/// A request for a vouch for `keys` with `nonce` and `expiry`, quoted by `attestor`.
/// Error cases:
/// - the attestor cannot quote, or a hardware quote's collateral cannot be fetched
pub(super) async fn quoted(
    quoting: &Quoting<'_>,
    attestor: &dyn Attestor,
    keys: &StandbyKeys,
    domain: &StandbyDomain,
    candidate: AccountId,
    nonce: u64,
    expiry: u64,
) -> anyhow::Result<VouchRequest> {
    let (release, tls) = (keys.release(), keys.tls_public_key_sha256());
    let bound = report_data(domain, candidate, keys.address(), &tls, &release, nonce, expiry);
    let evidence = evidence::produce(quoting, attestor, &keys.platform(), bound).await?;
    Ok(VouchRequest {
        candidate,
        key: keys.address(),
        tls_public_key_sha256: B256::from(tls),
        launch_digest: FixedBytes(release.launch_digest),
        config_sha256: B256::from(release.config_sha256),
        nonce,
        expiry,
        evidence,
    })
}

/// Vouches for `request`'s standby key: its expiry lies after trusted time's upper bound and within
/// `MAX_VOUCH_LIFETIME` of its lower bound; its quote binds the request's report data at trusted
/// time, proves the request's measurement, and runs under a policy and TCB no weaker than this
/// release's; and, at the latest agreed block, the candidate is registered on the release the quote
/// proves, approved for it. Signs with the vault's signer key while this enclave holds it, else with
/// its standby key while the schedule lists it as promotable.
/// Error cases:
/// - no trusted time sample, or any of those checks fails
/// - the chain cannot be read
/// - this enclave holds neither the vault's signer key nor a promotable standby key
pub(crate) async fn answer<C: StandbyChain>(
    request: &VouchRequest,
    voucher: &Voucher<'_, C>,
) -> anyhow::Result<VouchReply> {
    let earliest = voucher.trusted.reached_ms().context("no trusted time sample to check the expiry at")?;
    let latest = voucher.trusted.since_sample_ms().context("no trusted time sample to check the expiry at")?;
    let expiry_ms = Millis::try_from(request.expiry).ok().and_then(|s| s.checked_mul(1000));
    let expiry_ms = expiry_ms.context("the expiry is out of range")?;
    ensure!(expiry_ms > latest, "the vouch would expire at {} s, which trusted time may have reached", request.expiry);
    let lifetime_ms = Millis::try_from(MAX_VOUCH_LIFETIME.as_millis()).expect("an hour fits in milliseconds");
    ensure!(
        expiry_ms <= earliest.saturating_add(lifetime_ms),
        "the vouch would expire at {} s, more than {} s past trusted time",
        request.expiry,
        MAX_VOUCH_LIFETIME.as_secs()
    );
    let domain = &voucher.domain;
    let release = Release { launch_digest: request.launch_digest.0, config_sha256: request.config_sha256.0 };
    let tls = request.tls_public_key_sha256.0;
    let bound = report_data(domain, request.candidate, request.key, &tls, &release, request.nonce, request.expiry);
    let peer = evidence::verify(voucher.provider, &request.evidence, &bound, Some(latest / 1000), None)?;
    ensure!(
        peer.launch_digest == release.launch_digest,
        "the quote proves measurement 0x{}, not the request's",
        hex::encode(peer.launch_digest)
    );
    no_weaker(&voucher.platform, &peer)?;
    let own = voucher.standby.map(StandbyKeys::address);
    let view = vouch_view(voucher.chain, domain, request.candidate, own).await?;
    let registered = view.release.with_context(|| format!("candidate {} is not registered", request.candidate))?;
    ensure!(
        registered.0 == release.id(),
        "the quote proves release 0x{}, not candidate {}'s registered release {registered}",
        hex::encode(release.id()),
        request.candidate
    );
    ensure!(view.approved, "the timelock has not approved release {registered} for candidate {}", request.candidate);
    let digest = vouch_digest(domain, request.candidate, request.key, release.id(), request.nonce, request.expiry);
    let reply = if let Some(signer) = voucher.signer.filter(|signer| AccountId(signer.address()) == view.signer) {
        let vouch = signer.sign_digest(digest).parse().expect("sign_digest writes 65 bytes of 0x hex");
        VouchReply { voucher: view.signer, vouch }
    } else if let Some(standby) = voucher.standby.filter(|_| view.own_promotable) {
        VouchReply { voucher: standby.address(), vouch: standby.sign(digest) }
    } else {
        bail!("this enclave holds neither the vault's signer key nor a standby key the schedule lists as promotable");
    };
    info!(candidate = %request.candidate, key = %request.key, voucher = %reply.voucher, "vouched for a standby key");
    Ok(reply)
}

/// The report data a standby key's quote binds: the purpose, the deployment, the candidate, the key,
/// its TLS key's hash, the release, the nonce and the expiry, every field fixed-length.
pub(super) fn report_data(
    domain: &StandbyDomain,
    candidate: AccountId,
    key: AccountId,
    tls_public_key_sha256: &[u8; 32],
    release: &Release,
    nonce: u64,
    expiry: u64,
) -> [u8; 64] {
    let mut hash = Sha256::new();
    hash.update(PURPOSE);
    hash.update(domain.chain_id.to_be_bytes());
    hash.update(domain.vault);
    hash.update(domain.registry);
    hash.update(candidate.0);
    hash.update(key.0);
    hash.update(tls_public_key_sha256);
    hash.update(release.launch_digest);
    hash.update(release.config_sha256);
    hash.update(nonce.to_be_bytes());
    hash.update(expiry.to_be_bytes());
    padded(hash.finalize().into())
}

/// The schedule's `vouchDigest(candidate, key, release, nonce, expiry)` in `domain`.
pub(super) fn vouch_digest(
    domain: &StandbyDomain,
    candidate: AccountId,
    key: AccountId,
    release: [u8; 32],
    nonce: u64,
    expiry: u64,
) -> [u8; 32] {
    let fields = (
        keccak256(VOUCH_DOMAIN),
        U256::from(domain.chain_id),
        domain.vault,
        domain.registry,
        domain.schedule,
        Address::from(candidate.0),
        Address::from(key.0),
        B256::from(release),
        nonce,
        expiry,
    );
    keccak256(fields.abi_encode()).0
}
