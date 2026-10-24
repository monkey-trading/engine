//! Enclave identity and remote attestation.
//!
//! At boot a hardware enclave generates a secp256k1 key that never leaves it. Mock configurations
//! can explicitly select a published development key; hardware configurations refuse that option. It puts the key's EVM
//! address in the attestation report data, so a hardware quote proves "this address belongs to an
//! enclave running this measurement". The vault contract and anyone reading the log trust
//! signatures from that address and nothing else. The report data also carries the hash of the
//! API's TLS public key (see `tls`), so the same quote proves which key terminates client
//! connections. Every signed digest carries a domain tag, the chain id and vault it is meant for,
//! and a sequence number or nonce, so no signature can be replayed in another context.

use std::path::PathBuf;

use anyhow::{Context, ensure};
use engine_core::Fixed;
use engine_core::eip712::{self, B256};
use engine_core::types::AccountId;
use k256::ecdsa::SigningKey;
use k256::elliptic_curve::Generate;
use serde::Serialize;
use sha3::{Digest, Keccak256};

/// A source of hardware attestation quotes over caller-chosen report data.
pub trait Attestor: Send + Sync {
    /// A quote binding `report_data`, and the provider that produced it.
    /// Error cases:
    /// - the platform cannot produce a quote
    fn attest(&self, report_data: [u8; 64]) -> anyhow::Result<(String, Vec<u8>)>;
}

/// The enclave's signing key. Generated inside the enclave and never exported.
pub struct EnclaveKey {
    key: SigningKey,
}

#[derive(Clone, Debug, Serialize)]
pub struct Attestation {
    pub provider: String,
    pub enclave_address: String,
    /// SHA-256 of the API certificate's DER SubjectPublicKeyInfo, hex.
    pub tls_public_key_sha256: String,
    pub report_data: String,
    /// Hardware quote (TDX quote or SEV-SNP report), hex.
    pub quote: String,
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
        let mut quote = b"MOCK-ATTESTATION-NOT-A-TEE:".to_vec();
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
        let dir = self.root.join(format!("engine-{}", std::process::id()));
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

impl EnclaveKey {
    /// A fresh key from the enclave's RNG.
    ///
    /// TODO: survive restarts by sealing the key to the measurement (or having a KMS release it on
    /// attestation), and hand it over between operators once rotation is on.
    pub fn generate() -> Self {
        Self { key: SigningKey::generate() }
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
    fn sign_digest(&self, digest: [u8; 32]) -> String {
        format!("0x{}", hex::encode(eip712::sign(&self.key, &B256::from(digest))))
    }
}

/// Attests the enclave key and the API's TLS key. The report data is the enclave address in bytes
/// 0..20, zeros in 20..32, and the TLS public key hash in 32..64.
/// Error cases:
/// - the attestor cannot produce a quote
pub fn attest_key(
    attestor: &dyn Attestor,
    key: &EnclaveKey,
    tls_public_key_sha256: &[u8; 32],
) -> anyhow::Result<Attestation> {
    let mut report_data = [0u8; 64];
    report_data[..20].copy_from_slice(&key.address());
    report_data[32..].copy_from_slice(tls_public_key_sha256);
    let (provider, quote) = attestor.attest(report_data)?;
    Ok(Attestation {
        provider,
        enclave_address: key.address_hex(),
        tls_public_key_sha256: format!("0x{}", hex::encode(tls_public_key_sha256)),
        report_data: format!("0x{}", hex::encode(report_data)),
        quote: format!("0x{}", hex::encode(quote)),
    })
}

/// Historical withdrawal authorization bytes, unchanged across checkpoint versions.
pub(crate) fn withdrawal_digest(
    chain_id: u64,
    vault: &[u8; 20],
    account: AccountId,
    amount: Fixed,
    nonce: u64,
) -> [u8; 32] {
    let mut h = Keccak256::new();
    h.update(b"engine.withdrawal.v1");
    h.update(chain_id.to_be_bytes());
    h.update(vault);
    h.update(account.0);
    h.update(amount.to_be_bytes());
    h.update(nonce.to_be_bytes());
    h.finalize().into()
}

/// V2 binds the exact parent so two authorized publishers cannot extend divergent forks.
pub(crate) fn checkpoint_digest_v2(
    chain_id: u64,
    vault: &[u8; 20],
    seq: u64,
    root: &[u8; 32],
    parent: crate::log::CheckpointParent,
) -> [u8; 32] {
    let mut h = Keccak256::new();
    h.update(b"engine.checkpoint.v2");
    h.update(chain_id.to_be_bytes());
    h.update(vault);
    h.update(seq.to_be_bytes());
    h.update(root);
    h.update([u8::from(parent.has_checkpoint)]);
    h.update(parent.seq.to_be_bytes());
    h.update(parent.root.0);
    h.finalize().into()
}

/// The digest a checkpoint signature covers: domain tag, chain id, vault, seq, and root.
pub(crate) fn checkpoint_digest(chain_id: u64, vault: &[u8; 20], seq: u64, root: &[u8; 32]) -> [u8; 32] {
    let mut h = Keccak256::new();
    h.update(b"engine.checkpoint.v1");
    h.update(chain_id.to_be_bytes());
    h.update(vault);
    h.update(seq.to_be_bytes());
    h.update(root);
    h.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use k256::ecdsa::signature::hazmat::PrehashVerifier;
    use k256::ecdsa::{RecoveryId, Signature, VerifyingKey};

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
    fn mock_attestation_binds_the_enclave_address_and_tls_key() {
        let key = EnclaveKey::generate();
        let tls_key_hash = [0xab; 32];
        let att = attest_key(&MockAttestor, &key, &tls_key_hash).unwrap();
        assert_eq!(att.provider, "mock");
        assert_eq!(att.tls_public_key_sha256, format!("0x{}", hex::encode(tls_key_hash)));

        let report_data = hex::decode(att.report_data.trim_start_matches("0x")).unwrap();
        assert_eq!(report_data[..20], key.address());
        assert_eq!(report_data[20..32], [0u8; 12]);
        assert_eq!(report_data[32..], tls_key_hash);
        // The mock quote echoes the report data it was asked to bind.
        assert!(att.quote.ends_with(&hex::encode(&report_data)));
    }
}
