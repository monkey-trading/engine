//! A bounded client for one RFC 6962 log: the signed tree head (`get-sth`) and entries
//! (`get-entries`). The log is untrusted input: every response is size-limited and timed out, the
//! tree head's signature is checked against the log's configured key, and a response that is
//! malformed, too long or inconsistent with the request is refused.
//!
//! Consistency and inclusion proofs are not checked, so a log that shows different views to
//! different clients is out of scope.

use std::time::Duration;

use anyhow::{Context, bail, ensure};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use reqwest::{Client, Url};
use ring::signature::{ECDSA_P256_SHA256_ASN1, RSA_PKCS1_2048_8192_SHA256, UnparsedPublicKey};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use x509_parser::prelude::FromDer;
use x509_parser::x509::SubjectPublicKeyInfo;

/// Entries asked for in one `get-entries` call; a log may answer with fewer.
pub(super) const MAX_BATCH: u64 = 256;
/// A tree head is a few hundred bytes.
const MAX_STH_BYTES: usize = 64 * 1024;
/// Bounds a `get-entries` body, chains in `extra_data` included.
const MAX_ENTRIES_BYTES: usize = 32 * 1024 * 1024;
/// Bounds one decoded leaf; a certificate is far smaller.
const MAX_LEAF_BYTES: usize = 256 * 1024;
/// A whole request, connection and body included.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

const OID_EC_PUBLIC_KEY: &str = "1.2.840.10045.2.1";
const OID_P256: &str = "1.2.840.10045.3.1.7";
const OID_RSA_ENCRYPTION: &str = "1.2.840.113549.1.1.1";
/// RFC 5246 hash and signature algorithm codes in a `DigitallySigned`.
const HASH_SHA256: u8 = 4;
const SIGNATURE_RSA: u8 = 1;
const SIGNATURE_ECDSA: u8 = 3;

/// A log's public key, ready to verify its tree head signatures.
#[derive(Debug, Clone)]
pub(crate) struct LogKey {
    /// Hex SHA-256 of the DER SubjectPublicKeyInfo.
    fingerprint: String,
    algorithm: Algorithm,
    /// ECDSA: the uncompressed P-256 point. RSA: the DER `RSAPublicKey`.
    key: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Algorithm {
    EcdsaP256,
    Rsa,
}

/// A tree head whose signature verified.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct TreeHead {
    pub tree_size: u64,
    pub timestamp_ms: u64,
}

/// One log's endpoints.
pub(super) struct LogClient {
    http: Client,
    base: Url,
    key: LogKey,
}

#[derive(Deserialize)]
struct RawTreeHead {
    tree_size: u64,
    timestamp: u64,
    sha256_root_hash: String,
    tree_head_signature: String,
}

#[derive(Deserialize)]
struct RawEntries {
    entries: Vec<RawEntry>,
}

#[derive(Deserialize)]
struct RawEntry {
    leaf_input: String,
}

impl LogKey {
    /// Parses the base64 DER SubjectPublicKeyInfo a log list publishes.
    /// Error cases:
    /// - not base64 or DER, trailing bytes, or neither an ECDSA P-256 nor an RSA key
    pub(crate) fn parse(base64_spki: &str) -> anyhow::Result<Self> {
        let der = STANDARD.decode(base64_spki.trim()).context("the key is not base64")?;
        let (rest, spki) = SubjectPublicKeyInfo::from_der(&der)
            .map_err(|e| anyhow::anyhow!("the key is not a SubjectPublicKeyInfo: {e:?}"))?;
        ensure!(rest.is_empty(), "trailing bytes after the SubjectPublicKeyInfo");
        let key = spki.subject_public_key.data.to_vec();
        let fingerprint = hex::encode(Sha256::digest(&der));
        match spki.algorithm.algorithm.to_id_string().as_str() {
            OID_EC_PUBLIC_KEY => {
                let curve = spki.algorithm.parameters.as_ref().and_then(|p| p.as_oid().ok());
                ensure!(curve.is_some_and(|c| c.to_id_string() == OID_P256), "only the P-256 curve is supported");
                ensure!(key.len() == 65 && key[0] == 4, "the P-256 point is not an uncompressed one");
                Ok(Self { fingerprint, algorithm: Algorithm::EcdsaP256, key })
            }
            OID_RSA_ENCRYPTION => Ok(Self { fingerprint, algorithm: Algorithm::Rsa, key }),
            other => bail!("unsupported key algorithm {other}: use an ECDSA P-256 or RSA log key"),
        }
    }

    /// Hex SHA-256 of the key's DER SubjectPublicKeyInfo, which names the log in the state file.
    pub(crate) fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    /// Verifies a `DigitallySigned` over `message`.
    /// Error cases:
    /// - the structure is malformed, names another hash or signature algorithm than SHA-256 with
    ///   this key's, or the signature does not verify
    fn verify(&self, message: &[u8], digitally_signed: &[u8]) -> anyhow::Result<()> {
        let [hash, signature_algorithm, len_hi, len_lo, signature @ ..] = digitally_signed else {
            bail!("the signature is truncated");
        };
        ensure!(*hash == HASH_SHA256, "the signature does not use SHA-256");
        let expected = match self.algorithm {
            Algorithm::EcdsaP256 => SIGNATURE_ECDSA,
            Algorithm::Rsa => SIGNATURE_RSA,
        };
        ensure!(*signature_algorithm == expected, "the signature algorithm is not the log key's");
        ensure!(
            signature.len() == usize::from(u16::from_be_bytes([*len_hi, *len_lo])),
            "the signature length is wrong"
        );
        let verified = match self.algorithm {
            Algorithm::EcdsaP256 => {
                UnparsedPublicKey::new(&ECDSA_P256_SHA256_ASN1, &self.key).verify(message, signature)
            }
            Algorithm::Rsa => UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, &self.key).verify(message, signature),
        };
        verified.map_err(|_| anyhow::anyhow!("the tree head signature does not verify under the log's key"))
    }
}

impl LogClient {
    /// A client for the log at `base`, whose tree heads `key` signs.
    pub(super) fn new(http: Client, base: Url, key: LogKey) -> Self {
        Self { http, base, key }
    }

    /// The fingerprint of the log's public key.
    pub(super) fn fingerprint(&self) -> &str {
        self.key.fingerprint()
    }

    /// The log's current signed tree head.
    /// Error cases:
    /// - the request fails, times out or is too long, the body is malformed, or the signature does
    ///   not verify under the log's key
    pub(super) async fn tree_head(&self) -> anyhow::Result<TreeHead> {
        let body = self.get("get-sth", None, MAX_STH_BYTES).await?;
        let raw: RawTreeHead = serde_json::from_slice(&body).context("the tree head is not the expected JSON")?;
        let root = STANDARD.decode(&raw.sha256_root_hash).context("the root hash is not base64")?;
        ensure!(root.len() == 32, "the root hash is not 32 bytes");
        let signature = STANDARD.decode(&raw.tree_head_signature).context("the tree head signature is not base64")?;
        // RFC 6962 section 3.5: version v1, signature type tree_hash, timestamp, size, root.
        let mut message = vec![0u8, 1];
        message.extend_from_slice(&raw.timestamp.to_be_bytes());
        message.extend_from_slice(&raw.tree_size.to_be_bytes());
        message.extend_from_slice(&root);
        self.key.verify(&message, &signature)?;
        Ok(TreeHead { tree_size: raw.tree_size, timestamp_ms: raw.timestamp })
    }

    /// The leaves of entries `start..=end`, in order; the log may answer with a prefix of them.
    /// Error cases:
    /// - the range is empty or longer than `MAX_BATCH`, the request fails, times out or is too
    ///   long, the body is malformed, or the log answers with no entry or more than were asked for
    pub(super) async fn leaves(&self, start: u64, end: u64) -> anyhow::Result<Vec<Vec<u8>>> {
        ensure!(start <= end && end - start < MAX_BATCH, "get-entries range {start}..={end} is empty or too long");
        let query = format!("start={start}&end={end}");
        let body = self.get("get-entries", Some(&query), MAX_ENTRIES_BYTES).await?;
        let raw: RawEntries = serde_json::from_slice(&body).context("the entries are not the expected JSON")?;
        ensure!(!raw.entries.is_empty(), "the log returned no entries for {start}..={end}");
        ensure!(raw.entries.len() as u64 <= end - start + 1, "the log returned more entries than asked for");
        raw.entries
            .into_iter()
            .enumerate()
            .map(|(offset, entry)| {
                ensure!(
                    entry.leaf_input.len() <= MAX_LEAF_BYTES * 4 / 3 + 4,
                    "entry {} is too long",
                    start + offset as u64
                );
                STANDARD
                    .decode(&entry.leaf_input)
                    .with_context(|| format!("entry {} leaf_input is not base64", start + offset as u64))
            })
            .collect()
    }

    /// GETs `/ct/v1/{endpoint}` and reads at most `max_bytes` of the body.
    /// Error cases:
    /// - the request fails or exceeds the deadline, the status is not success, or the body is
    ///   longer than `max_bytes`
    async fn get(&self, endpoint: &str, query: Option<&str>, max_bytes: usize) -> anyhow::Result<Vec<u8>> {
        let mut url = self.base.clone();
        url.path_segments_mut()
            .map_err(|()| anyhow::anyhow!("the log URL cannot have path segments"))?
            .pop_if_empty()
            .extend(["ct", "v1", endpoint]);
        url.set_query(query);
        let exchange = async {
            let mut response = self.http.get(url).send().await?.error_for_status()?;
            let mut body = Vec::new();
            while let Some(chunk) = response.chunk().await? {
                ensure!(body.len().saturating_add(chunk.len()) <= max_bytes, "the response exceeds {max_bytes} bytes");
                body.extend_from_slice(&chunk);
            }
            anyhow::Ok(body)
        };
        tokio::time::timeout(REQUEST_TIMEOUT, exchange).await.with_context(|| format!("{endpoint} timed out"))?
    }
}
