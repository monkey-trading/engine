//! Enclave identity and remote attestation.
//!
//! A hardware enclave's secp256k1 key never leaves it in plaintext: it is generated inside, sealed
//! to the release on the host's disk (`sealing`) so a restart keeps it, and handed to the next
//! release only encrypted to that release's attested key (`handover`). Mock configurations can
//! explicitly select a published development key; hardware configurations refuse that option.
//! The attestation report data carries the SHA-256 of an attestation bundle: the key's EVM address,
//! the SHA-256 of the config file, and the hash of the API's TLS public key (see `tls`). So a
//! hardware quote proves "this address belongs to an enclave running this measurement with this
//! config, and this key terminates its client connections". The vault contract and anyone reading
//! the log trust signatures from that address and nothing else. Every signed digest carries a
//! domain tag, the chain id and vault it is meant for, and a sequence number or nonce, so no
//! signature can be replayed in another context.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, ensure};
use engine_core::Fixed;
use engine_core::eip712::{self, B256};
use engine_core::types::AccountId;
pub(crate) use engine_verify::MOCK_QUOTE_MARKER;
pub use engine_verify::{Attestation, AttestationBundle};
use k256::ecdsa::SigningKey;
use k256::elliptic_curve::Generate;
use sha2::Sha256;

pub(crate) mod clock;
pub(crate) mod keys;
pub(crate) mod sealing;
pub(crate) mod secrets;
#[cfg(target_os = "linux")]
pub(crate) mod snp;

mod digests;

pub(crate) use digests::*;

/// Mints each quote's report entry name.
static NEXT_REPORT: AtomicU64 = AtomicU64::new(0);

/// A source of hardware attestation quotes over caller-chosen report data.
pub trait Attestor: Send + Sync {
    /// A quote binding `report_data`, and the provider that produced it.
    /// Error cases:
    /// - the platform cannot produce a quote
    fn attest(&self, report_data: [u8; 64]) -> anyhow::Result<(String, Vec<u8>)>;
}

/// The enclave's signing key. Generated inside the enclave and leaves it only sealed or encrypted
/// to an attested successor.
pub struct EnclaveKey {
    key: SigningKey,
}

/// No hardware. The "quote" says so, so nobody can mistake it for a real one.
pub struct MockAttestor;

/// Linux configfs-tsm, the kernel's common interface for TDX and SEV-SNP guests (Linux 6.7+).
/// Write the report data to `inblob`, read the quote from `outblob`.
pub struct ConfigfsTsm {
    root: PathBuf,
}

impl Attestor for MockAttestor {
    /// A fake quote: a fixed marker followed by the report data.
    /// Error cases:
    /// - none; the mock always produces a quote
    fn attest(&self, report_data: [u8; 64]) -> anyhow::Result<(String, Vec<u8>)> {
        let mut quote = MOCK_QUOTE_MARKER.to_vec();
        quote.extend_from_slice(&report_data);
        Ok(("mock".into(), quote))
    }
}

impl Default for ConfigfsTsm {
    /// The standard configfs mount point.
    fn default() -> Self {
        Self { root: PathBuf::from("/sys/kernel/config/tsm/report") }
    }
}

impl Attestor for ConfigfsTsm {
    /// A quote from a fresh configfs-tsm report entry, removed afterwards.
    /// Error cases:
    /// - the report directory cannot be created (not a TDX / SEV-SNP guest, or no permission)
    /// - writing `inblob`, or reading `outblob` or `provider`, fails
    fn attest(&self, report_data: [u8; 64]) -> anyhow::Result<(String, Vec<u8>)> {
        let dir = self.report_entry();
        std::fs::create_dir(&dir)
            .with_context(|| format!("creating {} (is this a TDX/SEV-SNP guest?)", dir.display()))?;
        let result = (|| {
            std::fs::write(dir.join("inblob"), report_data).context("writing inblob")?;
            let quote = std::fs::read(dir.join("outblob")).context("reading outblob")?;
            let provider = std::fs::read_to_string(dir.join("provider")).context("reading provider")?;
            Ok((provider.trim().to_owned(), quote))
        })();
        let _ = std::fs::remove_dir(&dir);
        result
    }
}

impl ConfigfsTsm {
    /// A report entry no other quote uses: startup, a handover, and each delivery connection all
    /// quote, and an entry that exists refuses the second.
    fn report_entry(&self) -> PathBuf {
        let entry = NEXT_REPORT.fetch_add(1, Ordering::Relaxed);
        self.root.join(format!("engine-{}-{entry}", std::process::id()))
    }
}

impl EnclaveKey {
    /// A fresh key from the enclave's RNG, which lasts this process only; `[tee.sealing]` keeps one
    /// across restarts.
    pub fn generate() -> Self {
        Self { key: SigningKey::generate() }
    }
    /// The secret authenticating the derived marks cache's rows: SHA-256 of a domain tag and the
    /// signing secret, stable across restarts and every release the sealed key survives, so the
    /// cache can tell a row it wrote from one the host wrote while the node was stopped or running.
    pub(crate) fn marks_key(&self) -> [u8; 32] {
        // In a nested scope: the file's `Digest` import is sha3's older generation, while sha2
        // 0.11 carries its own; sha2's re-export applies here alone.
        fn derive(secret: &[u8]) -> [u8; 32] {
            use sha2::Digest as _;
            let mut hash = Sha256::new();
            hash.update(b"engine.marks.cache.row.v1");
            hash.update(secret);
            hash.finalize().into()
        }
        derive(&self.key.to_bytes())
    }

    /// The key `secret` holds, from sealed or handed-over keys.
    /// Error cases:
    /// - `secret` is not a valid secp256k1 scalar
    pub(crate) fn from_secret(secret: &[u8; 32]) -> anyhow::Result<Self> {
        Ok(Self { key: SigningKey::from_bytes(secret.into()).context("not a secp256k1 signing key")? })
    }

    /// Selects an explicitly configured public mock authority or generates a private fresh key.
    /// Error cases:
    /// - a published development key is requested beside real attestation
    pub(crate) fn for_config(cfg: &crate::config::TeeSection) -> anyhow::Result<Self> {
        ensure!(
            !cfg.use_published_mock_key || cfg.provider == crate::config::TeeProvider::Mock,
            "tee.use_published_mock_key requires tee.provider = \"mock\""
        );
        Ok(if cfg.use_published_mock_key { Self::published_mock_key() } else { Self::generate() })
    }

    /// Public 0x4343…43 development key, listed in config/test-keys.toml. Never a hardware identity.
    fn published_mock_key() -> Self {
        Self { key: SigningKey::from_bytes((&[0x43; 32]).into()).expect("valid public development key") }
    }

    /// The same explicit public authority used by the committed mock configurations.
    #[cfg(test)]
    pub(crate) fn test_key() -> Self {
        Self::published_mock_key()
    }

    /// The key's EVM address: the last 20 bytes of keccak-256 over the uncompressed public key.
    pub fn address(&self) -> [u8; 20] {
        eip712::address(self.key.verifying_key()).0
    }

    /// The address as `0x` hex.
    pub fn address_hex(&self) -> String {
        format!("0x{}", hex::encode(self.address()))
    }

    /// Signs the state root after `seq`, for the vault on `chain_id` that will store it.
    pub fn sign_checkpoint(&self, chain_id: u64, vault: &[u8; 20], seq: u64, root: &[u8; 32]) -> String {
        self.sign_digest(checkpoint_digest(chain_id, vault, seq, root))
    }

    /// Signs the newest released log head for the LogHeads companion contract (#541), naming the
    /// previously accepted head it extends (`None` for the first post of the signer epoch).
    pub(crate) fn sign_log_head(
        &self,
        chain_id: u64,
        vault: &[u8; 20],
        epoch: u64,
        seq: u64,
        link: &[u8; 32],
        parent: Option<(u64, B256)>,
    ) -> String {
        let (parent_seq, parent_link) = parent.unwrap_or((0, B256::ZERO));
        self.sign_digest(log_head_digest(chain_id, vault, epoch, seq, link, parent_seq, &parent_link.0))
    }

    /// Signs an atomic extension of the exact prior vault commitment.
    pub fn sign_checkpoint_v2(
        &self,
        chain_id: u64,
        vault: &[u8; 20],
        seq: u64,
        root: &[u8; 32],
        parent: crate::log::CheckpointParent,
    ) -> String {
        self.sign_digest(checkpoint_digest_v2(chain_id, vault, seq, root, parent))
    }

    /// Signs journal integrity only; this signature grants no custody authority.
    pub(crate) fn seal_command(&self, domain: crate::log::CheckpointDomain, link: B256) -> crate::log::CommandSeal {
        crate::log::CommandSeal {
            domain,
            signer: AccountId(self.address()),
            signature: self.sign_digest(crate::log::command_seal::seal_digest(domain, link)),
        }
    }

    /// Preserves the historical pre-V22 withdrawal bytes for replay of earlier logs.
    pub fn sign_withdrawal(
        &self,
        chain_id: u64,
        vault: &[u8; 20],
        account: AccountId,
        amount: Fixed,
        nonce: u64,
    ) -> String {
        self.sign_digest(withdrawal_digest(chain_id, vault, account, amount, nonce))
    }

    /// Signs a whole-USDC payout only after the caller has verified its durable approval boundary.
    /// Error cases:
    /// - the authorization fields cannot form a valid vault digest
    pub(crate) fn sign_payout(&self, withdrawal: &crate::sequencer::SignedWithdrawal) -> anyhow::Result<String> {
        Ok(self.sign_digest(crate::withdrawals::digest(withdrawal)?.0))
    }

    /// 65-byte `r || s || v` with `v` in {27, 28}, as `ecrecover` expects.
    pub(crate) fn sign_digest(&self, digest: [u8; 32]) -> String {
        format!("0x{}", hex::encode(eip712::sign(&self.key, &B256::from(digest))))
    }
}

/// Attests `bundle`: the quote binds its report data, and the attestation serves the bundle so a
/// verifier can recompute it.
/// Error cases:
/// - the attestor cannot produce a quote
pub fn attest_bundle(attestor: &dyn Attestor, bundle: &AttestationBundle) -> anyhow::Result<Attestation> {
    let report_data = bundle.report_data();
    let (provider, quote) = attestor.attest(report_data)?;
    Ok(Attestation {
        provider,
        enclave_address: format!("0x{}", hex::encode(bundle.enclave_address)),
        config_sha256: format!("0x{}", hex::encode(bundle.config_sha256)),
        tls_public_key_sha256: format!("0x{}", hex::encode(bundle.tls_public_key_sha256)),
        bundle: format!("0x{}", hex::encode(bundle.encode())),
        report_data: format!("0x{}", hex::encode(report_data)),
        quote: format!("0x{}", hex::encode(quote)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use k256::ecdsa::signature::hazmat::PrehashVerifier;
    use k256::ecdsa::{RecoveryId, Signature, VerifyingKey};
    use sha2::{Digest as _, Sha256};

    fn recover(digest: [u8; 32], sig_hex: &str) -> VerifyingKey {
        let sig = hex::decode(sig_hex.trim_start_matches("0x")).unwrap();
        assert_eq!(sig.len(), 65);
        let signature = Signature::from_slice(&sig[..64]).unwrap();
        let recid = RecoveryId::from_byte(sig[64] - 27).unwrap();
        let key = VerifyingKey::recover_from_prehash(&digest, &signature, recid).unwrap();
        key.verify_prehash(&digest, &signature).unwrap();
        key
    }

    #[test]
    fn log_head_signature_matches_the_contract_vector_byte_for_byte() {
        // The same fixed inputs and expected signature as `LogHeads.t.sol`'s vector test: key
        // 0x1234, chain 31337, vault 0x1111…11, epoch 7, seq 42, link 0x…2a, parent (41, 0x…29).
        let secret = 0x1234u64.to_be_bytes();
        let mut raw = [0u8; 32];
        raw[24..].copy_from_slice(&secret);
        let key = EnclaveKey { key: SigningKey::from_bytes((&raw).into()).unwrap() };
        let vault = [0x11; 20];
        let mut link = [0u8; 32];
        link[31] = 0x2a;
        let mut parent_link = [0u8; 32];
        parent_link[31] = 0x29;
        assert_eq!(key.address_hex(), "0xcf03dd0a894ef79cb5b601a43c4b25e3ae4c67ed");
        let parent = Some((41u64, B256::from(parent_link)));
        assert_eq!(
            key.sign_log_head(31337, &vault, 7, 42, &link, parent),
            "0xe6fb2b7d4ce26a25aa0997e2e550785451f8aa52a66a0b3f5f81092acd9f1162753bcfa3ee2e0626\
             ab9261097d77d213d96d3498c5dc6d99ddfbd4d6d1865dfa1b"
        );
        let digest = log_head_digest(31337, &vault, 7, 42, &link, 41, &parent_link);
        assert_eq!(&recover(digest, &key.sign_log_head(31337, &vault, 7, 42, &link, parent)), key.key.verifying_key());
        // The domain separates heads from checkpoints and withdrawals over the same values.
        assert_ne!(digest, checkpoint_digest(31337, &vault, 42, &link));
        assert_ne!(digest, checkpoint_digest_v2(31337, &vault, 42, &link, Default::default()));
    }

    #[test]
    fn log_head_digests_bind_the_chain_vault_epoch_seq_link_and_parent() {
        let (vault, link) = ([9u8; 20], [7u8; 32]);
        let digest = log_head_digest(1, &vault, 3, 42, &link, 10, &link);
        assert_ne!(digest, log_head_digest(2, &vault, 3, 42, &link, 10, &link));
        assert_ne!(digest, log_head_digest(1, &[8u8; 20], 3, 42, &link, 10, &link));
        assert_ne!(digest, log_head_digest(1, &vault, 4, 42, &link, 10, &link));
        assert_ne!(digest, log_head_digest(1, &vault, 3, 43, &link, 10, &link));
        assert_ne!(digest, log_head_digest(1, &vault, 3, 42, &[8u8; 32], 10, &link));
        assert_ne!(digest, log_head_digest(1, &vault, 3, 42, &link, 11, &link));
        assert_ne!(digest, log_head_digest(1, &vault, 3, 42, &link, 10, &[8u8; 32]));
    }

    #[test]
    fn each_quote_takes_its_own_report_entry() {
        let attestor = ConfigfsTsm::default();
        assert_ne!(attestor.report_entry(), attestor.report_entry());
    }

    #[test]
    fn checkpoint_signatures_recover_to_the_enclave_key() {
        let key = EnclaveKey::generate();
        let (vault, root) = ([9u8; 20], [7u8; 32]);
        let sig = key.sign_checkpoint(1, &vault, 42, &root);
        assert_eq!(&recover(checkpoint_digest(1, &vault, 42, &root), &sig), key.key.verifying_key());
    }

    #[test]
    fn tee_withdrawal_signature_matches_the_vault_vector_byte_for_byte() {
        let (withdrawal, vector) = crate::withdrawals::tests::vector();
        let mut secret = [0u8; 32];
        secret[30..].copy_from_slice(&0x1234u16.to_be_bytes());
        let key = EnclaveKey { key: SigningKey::from_bytes((&secret).into()).unwrap() };
        assert_eq!(key.address_hex().to_lowercase(), vector["signer"].as_str().unwrap().to_lowercase());
        assert_eq!(key.sign_payout(&withdrawal).unwrap(), vector["signature"].as_str().unwrap());
    }

    #[test]
    fn checkpoint_digests_bind_the_chain_and_vault() {
        let (vault, root) = ([9u8; 20], [7u8; 32]);
        let digest = checkpoint_digest(1, &vault, 42, &root);
        assert_ne!(digest, checkpoint_digest(2, &vault, 42, &root));
        assert_ne!(digest, checkpoint_digest(1, &[8u8; 20], 42, &root));
        assert_ne!(digest, checkpoint_digest(1, &vault, 43, &root));
    }

    #[test]
    fn a_verifier_recomputes_the_report_data_from_the_served_bundle() {
        let bundle = bundle(EnclaveKey::generate().address(), [0xc1; 32], [0x7e; 32]);
        let att = attest_bundle(&MockAttestor, &bundle).unwrap();
        assert_eq!(att.provider, "mock");

        // What a verifier does: hash the served bundle, compare it with the report data, then read
        // each field from the bundle at its documented offset.
        let served = unhex(&att.bundle);
        let report_data = unhex(&att.report_data);
        assert_eq!(report_data[..32], Sha256::digest(&served)[..]);
        assert_eq!(report_data[32..], [0u8; 32]);
        assert_eq!(served.len(), 105);
        assert_eq!(&served[..21], b"engine.attestation.v1");
        assert_eq!(served[21..41], bundle.enclave_address);
        assert_eq!(served[41..73], bundle.config_sha256);
        assert_eq!(served[73..105], bundle.tls_public_key_sha256);
        // The served fields are the bundle's, and the mock quote echoes the report data it binds.
        assert_eq!(unhex(&att.enclave_address), bundle.enclave_address);
        assert_eq!(unhex(&att.config_sha256), bundle.config_sha256);
        assert_eq!(unhex(&att.tls_public_key_sha256), bundle.tls_public_key_sha256);
        assert!(att.quote.ends_with(&hex::encode(&report_data)));
    }

    #[test]
    fn changing_any_bundle_field_changes_the_report_data() {
        let (address, config, tls) = ([0x11; 20], [0x22; 32], [0x33; 32]);
        let base = bundle(address, config, tls).report_data();
        let mut flipped_address = address;
        flipped_address[19] ^= 1;
        let mut flipped_config = config;
        flipped_config[0] ^= 1;
        let mut flipped_tls = tls;
        flipped_tls[31] ^= 1;
        for (what, other) in [
            ("address", bundle(flipped_address, config, tls)),
            ("config hash", bundle(address, flipped_config, tls)),
            ("TLS key hash", bundle(address, config, flipped_tls)),
            // The fields keep their places: swapping the two hashes is another bundle.
            ("field order", bundle(address, tls, config)),
        ] {
            assert_ne!(other.report_data(), base, "a changed {what} left the report data unchanged");
            assert_eq!(other.report_data()[32..], [0u8; 32]);
        }
    }

    // ── Helpers ──

    fn bundle(
        enclave_address: [u8; 20],
        config_sha256: [u8; 32],
        tls_public_key_sha256: [u8; 32],
    ) -> AttestationBundle {
        AttestationBundle { enclave_address, config_sha256, tls_public_key_sha256 }
    }

    fn unhex(hex: &str) -> Vec<u8> {
        hex::decode(hex.trim_start_matches("0x")).unwrap()
    }
}
