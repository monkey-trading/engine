//! Reading a log entry's leaf (RFC 6962 section 3.4): the certificate or precertificate it holds,
//! reduced to what the watcher compares: the names, the key's hash, and the issuer.

use anyhow::{Context, bail, ensure};
use sha2::{Digest, Sha256};
use x509_parser::certificate::{TbsCertificate, X509Certificate};
use x509_parser::extensions::GeneralName;
use x509_parser::prelude::FromDer;

/// Bounds the issuer text in an alert (ASCII, so bytes too), which is part of the alert's episode
/// key: with the longest log name, index and key hash, the key stays within the outbox's 256-byte
/// limit.
const MAX_ISSUER_CHARS: usize = 48;

/// How long an unparseable entry that mentions the domain stays a finding: past the longest public
/// certificate lifetime, after which it can no longer be used.
const SUSPECT_LIFETIME_MS: u64 = 398 * 86_400_000;

const ENTRY_X509: u16 = 0;
const ENTRY_PRECERT: u16 = 1;

/// What a leaf's certificate says, as far as the watcher cares.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Certificate {
    /// Lowercase DNS names: the subject alternative names, and the subject common names.
    pub names: Vec<String>,
    /// SHA-256 of the DER SubjectPublicKeyInfo, the hash an attestation carries.
    pub key_sha256: [u8; 32],
    pub issuer: String,
    /// When the certificate expires, in milliseconds; an expired one cannot be used.
    pub not_after_ms: u64,
    /// The log's timestamp for the entry, in milliseconds.
    pub logged_ms: u64,
}

/// A cursor over a leaf's bytes that refuses to read past the end.
struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    /// The next `len` bytes.
    /// Error cases:
    /// - fewer than `len` bytes remain
    fn take(&mut self, len: usize) -> anyhow::Result<&'a [u8]> {
        let (head, rest) = self.0.split_at_checked(len).context("the leaf is truncated")?;
        self.0 = rest;
        Ok(head)
    }

    /// The next byte.
    /// Error cases:
    /// - none remain
    fn u8(&mut self) -> anyhow::Result<u8> {
        Ok(self.take(1)?[0])
    }

    /// The next two bytes, big-endian.
    /// Error cases:
    /// - fewer than two remain
    fn u16(&mut self) -> anyhow::Result<u16> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into().expect("two bytes")))
    }

    /// The next eight bytes, big-endian.
    /// Error cases:
    /// - fewer than eight remain
    fn u64(&mut self) -> anyhow::Result<u64> {
        Ok(u64::from_be_bytes(self.take(8)?.try_into().expect("eight bytes")))
    }

    /// A vector with a 3-byte length prefix.
    /// Error cases:
    /// - the prefix or the vector is truncated
    fn opaque24(&mut self) -> anyhow::Result<&'a [u8]> {
        let [a, b, c] = *self.take(3)? else { unreachable!("three bytes") };
        self.take(usize::from(a) << 16 | usize::from(b) << 8 | usize::from(c))
    }
}

/// Parses a `MerkleTreeLeaf` holding an X.509 certificate or a precertificate's TBSCertificate.
/// Error cases:
/// - the leaf is truncated, has trailing bytes, another version, leaf type or entry type, or its
///   certificate does not parse
pub(super) fn parse_leaf(leaf: &[u8]) -> anyhow::Result<Certificate> {
    let mut reader = Reader(leaf);
    ensure!(reader.u8()? == 0, "unknown leaf version");
    ensure!(reader.u8()? == 0, "unknown leaf type");
    let logged_ms = reader.u64()?;
    let entry_type = reader.u16()?;
    let (names, key_sha256, issuer, not_after_ms) = match entry_type {
        ENTRY_X509 => {
            let der = reader.opaque24()?;
            let (rest, cert) = X509Certificate::from_der(der).map_err(|e| anyhow::anyhow!("certificate: {e:?}"))?;
            ensure!(rest.is_empty(), "trailing bytes after the certificate");
            describe(&cert.tbs_certificate)?
        }
        ENTRY_PRECERT => {
            reader.take(32)?; // the issuer key hash
            let der = reader.opaque24()?;
            let (rest, tbs) = TbsCertificate::from_der(der).map_err(|e| anyhow::anyhow!("precertificate: {e:?}"))?;
            ensure!(rest.is_empty(), "trailing bytes after the precertificate");
            describe(&tbs)?
        }
        other => bail!("unknown entry type {other}"),
    };
    let extensions = reader.u16()?;
    reader.take(usize::from(extensions))?;
    ensure!(reader.0.is_empty(), "trailing bytes after the leaf");
    Ok(Certificate { names, key_sha256, issuer, not_after_ms, logged_ms })
}

/// A stand-in certificate for a leaf that does not parse but contains a name that would cover the
/// domain (the domain itself, or `*.` and its parent): it cannot be shown to be harmless, so it is
/// treated as a certificate for the domain with no known key, whose hash is the leaf's own. `None`
/// when the leaf contains neither.
pub(super) fn suspect_unparsed(leaf: &[u8], domain: &str, now: u64) -> Option<Certificate> {
    let wildcard = domain.split_once('.').map(|(_, parent)| format!("*.{parent}"));
    let lower = leaf.to_ascii_lowercase();
    let contains = |needle: &str| lower.windows(needle.len()).any(|window| window == needle.as_bytes());
    (contains(domain) || wildcard.as_deref().is_some_and(contains)).then(|| Certificate {
        names: vec![domain.to_owned()],
        key_sha256: Sha256::digest(leaf).into(),
        issuer: "unparseable entry".to_owned(),
        not_after_ms: now.saturating_add(SUSPECT_LIFETIME_MS),
        logged_ms: now,
    })
}

/// The names, key hash, issuer text and expiry of a TBSCertificate.
/// Error cases:
/// - the subject alternative name extension is malformed or repeated
fn describe(tbs: &TbsCertificate<'_>) -> anyhow::Result<(Vec<String>, [u8; 32], String, u64)> {
    let mut names: Vec<String> = Vec::new();
    if let Some(san) = tbs.subject_alternative_name().map_err(|e| anyhow::anyhow!("subject alternative name: {e:?}"))? {
        for name in &san.value.general_names {
            if let GeneralName::DNSName(dns) = name {
                names.push(normalise(dns));
            }
        }
    }
    names.extend(tbs.subject.iter_common_name().filter_map(|cn| cn.as_str().ok()).map(normalise));
    let key_sha256 = Sha256::digest(tbs.subject_pki.raw).into();
    // ASCII only, so the character bound is a byte bound.
    let issuer: String = tbs
        .issuer
        .to_string()
        .chars()
        .map(|c| if c.is_ascii_graphic() || c == ' ' { c } else { '?' })
        .take(MAX_ISSUER_CHARS)
        .collect();
    let not_after_ms = u64::try_from(tbs.validity().not_after.timestamp().saturating_mul(1000)).unwrap_or(0);
    Ok((names, key_sha256, issuer, not_after_ms))
}

/// Lowercases a name and drops a trailing dot.
fn normalise(name: &str) -> String {
    name.trim_end_matches('.').to_ascii_lowercase()
}

/// Whether a certificate with `names` is valid for `domain`: a name equal to it, or a wildcard
/// name (`*.parent`) whose parent is the domain's parent. `domain` is lowercase.
pub(super) fn covers(names: &[String], domain: &str) -> bool {
    let parent = domain.split_once('.').map(|(_, parent)| parent);
    names.iter().any(|name| {
        name == domain || name.strip_prefix("*.").is_some_and(|wildcard_parent| Some(wildcard_parent) == parent)
    })
}
