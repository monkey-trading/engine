//! The quote's certificate chain: the VCEK that signed the report, the vendor intermediate that
//! signs VCEKs, and the vendor root that signs the intermediate. The roots are pinned in the
//! binary, so nothing fetched to verify a quote enters the trust set. The VCEK's signed extensions
//! must match the quote's chip id and reported TCB, so a served VCEK is bound to the chip that
//! produced the report. Every date is checked against the `now` the caller passes in.

use anyhow::{Context, bail, ensure};
use x509_parser::certificate::X509Certificate;
use x509_parser::prelude::FromDer;
use x509_parser::time::ASN1Time;

use crate::report::{SnpReport, Tcb};
use crate::signature::verify_signed;

/// The vendor roots compiled into the binary. Rotation adds a file here; it never removes one.
const VENDOR_ROOT_PEMS: &[&str] =
    &[include_str!("roots/ark-milan.pem"), include_str!("roots/ark-genoa.pem"), include_str!("roots/ark-turin.pem")];

/// The vendor's extension OIDs signed into every VCEK, binding it to the chip and TCB it serves.
const OID_STRUCT_VERSION: &str = "1.3.6.1.4.1.3704.1.1";
const OID_BL_SPL: &str = "1.3.6.1.4.1.3704.1.3.1";
const OID_TEE_SPL: &str = "1.3.6.1.4.1.3704.1.3.2";
const OID_SNP_SPL: &str = "1.3.6.1.4.1.3704.1.3.3";
const OID_UCODE_SPL: &str = "1.3.6.1.4.1.3704.1.3.8";
const OID_HWID: &str = "1.3.6.1.4.1.3704.1.4";

/// Root certificates the chain is pinned to, by DER bytes. `roots` starts from the vendor roots
/// compiled into the binary; a caller may add one more, never remove one.
#[derive(Default)]
pub(crate) struct Roots {
    der: Vec<Vec<u8>>,
}

impl Roots {
    /// The vendor roots compiled into the binary.
    pub fn vendor() -> Self {
        let mut roots = Self::default();
        for pem in VENDOR_ROOT_PEMS {
            roots.der.push(parse_pem_cert(pem).expect("compiled-in vendor root parses"));
        }
        roots
    }

    /// Adds a root from ASCII PEM, for verifying against a test or private chain.
    /// Error cases:
    /// - the PEM holds no certificate
    pub fn add_pem(&mut self, pem: &str) -> anyhow::Result<()> {
        self.der.push(parse_pem_cert(pem)?);
        Ok(())
    }

    fn contains(&self, der: &[u8]) -> bool {
        self.der.iter().any(|pinned| pinned == der)
    }
}

/// The chain a quote verifies against: VCEK first, then the intermediate, then the root.
pub(crate) struct QuoteChain {
    ders: Vec<Vec<u8>>,
}

/// The span of Unix seconds within which every date check passed, inclusive at both ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Window {
    pub from: i64,
    pub until: i64,
}

impl QuoteChain {
    /// Parses and checks `vcek`, `intermediate`, and `root` DER certificates at `now`: the root
    /// must be a compiled-in pin, each issuer name must match its parent's subject, and each
    /// signature must verify against its parent's key.
    /// Error cases:
    /// - a certificate does not parse, the root is not pinned, an issuer name does not match its
    ///   parent's subject, a signature does not verify, or a certificate below the root is outside
    ///   its validity period at `now`
    pub fn verify(
        vcek: Vec<u8>,
        intermediate: Vec<u8>,
        root: Vec<u8>,
        roots: &Roots,
        now: ASN1Time,
    ) -> anyhow::Result<Self> {
        ensure!(roots.contains(&root), "chain root is not a pinned vendor root");
        Self::link(vcek, intermediate, root, now)
    }

    /// Checks the certificates link and are in date at `now`, without pinning the root: the node
    /// checks what it relays this way, and a verifier pins through `verify`.
    /// Error cases:
    /// - a certificate does not parse, an issuer name does not match its parent's subject, a
    ///   signature does not verify, or a certificate below the root is outside its validity period
    pub fn link(vcek: Vec<u8>, intermediate: Vec<u8>, root: Vec<u8>, now: ASN1Time) -> anyhow::Result<Self> {
        let chain = Self { ders: vec![vcek, intermediate, root] };
        for (child, parent) in [(0, 1), (1, 2)] {
            check_signed_by(&chain.ders[child], &chain.ders[parent])?;
            // The pinned root is trusted by byte equality, not by its clock window; the two
            // certificates it vouches for must be within theirs. The verifier runs outside the
            // enclave, so its clock is the reference for this check.
            let cert = parse_cert(&chain.ders[child])?;
            ensure!(
                cert.validity().is_valid_at(now),
                "certificate {} is outside its validity period",
                subject_name(&cert)
            );
        }
        Ok(chain)
    }

    /// The span within which both certificates below the root are valid.
    /// Error cases:
    /// - a certificate does not parse
    pub fn validity(&self) -> anyhow::Result<Window> {
        let (vcek, intermediate) = (parse_cert(&self.ders[0])?, parse_cert(&self.ders[1])?);
        Ok(Window {
            from: vcek.validity().not_before.timestamp().max(intermediate.validity().not_before.timestamp()),
            until: vcek.validity().not_after.timestamp().min(intermediate.validity().not_after.timestamp()),
        })
    }

    /// The intermediate's serial as big-endian bytes: the only serial the root's revocation list
    /// can name, since certificate serials are unique only within an issuing CA and the root
    /// issues the intermediate, not the VCEK.
    pub fn intermediate_serial(&self) -> anyhow::Result<Vec<u8>> {
        Ok(parse_cert(&self.ders[1])?.serial.to_bytes_be())
    }

    /// The root certificate the chain is pinned to, DER bytes.
    pub fn root_der(&self) -> &[u8] {
        &self.ders[2]
    }

    /// Requires the VCEK's signed extensions to bind it to `report`: the struct version must be
    /// the implemented 0 (Milan, Genoa, Siena), the hardware id to the quote's chip id, and each
    /// security patch level to the quote's reported TCB. A chain whose VCEK certifies another
    /// chip or TCB — one a mirror or a `--kds` override could serve from genuine vendor
    /// certificates — is refused here.
    /// Error cases:
    /// - an extension is missing or malformed, carries an unsupported struct version, or
    ///   disagrees with the report's chip id or reported TCB
    pub fn bind_vcek(&self, report: &SnpReport<'_>) -> anyhow::Result<()> {
        let cert = parse_cert(&self.ders[0])?;
        let extension = |oid: &str| {
            cert.extensions()
                .iter()
                .find(|extension| extension.oid.to_id_string() == oid)
                .with_context(|| format!("VCEK carries no extension {oid}"))
        };
        let integer = |oid: &str, what: &str| -> anyhow::Result<u8> { extension_integer(extension(oid)?, what) };
        let struct_version = integer(OID_STRUCT_VERSION, "struct version")?;
        ensure!(
            struct_version == 0,
            "unsupported VCEK TCB struct version {struct_version}; only version 0 (Milan, Genoa, Siena) is implemented"
        );
        let hwid = extension_hwid(extension(OID_HWID)?)?;
        ensure!(hwid == report.chip_id, "VCEK hardware id does not match the quote's chip id");
        let tcb = Tcb {
            boot_loader: integer(OID_BL_SPL, "boot loader SPL")?,
            tee: integer(OID_TEE_SPL, "TEE SPL")?,
            snp: integer(OID_SNP_SPL, "SNP SPL")?,
            microcode: integer(OID_UCODE_SPL, "microcode SPL")?,
        };
        ensure!(
            tcb == report.reported_tcb,
            "VCEK TCB {tcb:?} does not match the quote's reported TCB {:?}",
            report.reported_tcb
        );
        Ok(())
    }

    /// The VCEK's public key as its SEC 1 uncompressed point, which the verifier's signature
    /// algorithm parses.
    pub fn vcek_public_key(&self) -> anyhow::Result<Vec<u8>> {
        Ok(parse_cert(&self.ders[0])?.public_key().subject_public_key.data.to_vec())
    }
}

/// Parses a DER certificate.
/// Error cases:
/// - the bytes are not a certificate
fn parse_cert(der: &[u8]) -> anyhow::Result<X509Certificate<'_>> {
    let (_, cert) = X509Certificate::from_der(der).map_err(|err| anyhow::anyhow!("parsing a certificate: {err:?}"))?;
    Ok(cert)
}

/// Checks a child certificate's issuer name and signature against its parent.
/// Error cases:
/// - a certificate does not parse, the issuer name does not match the parent's subject, or the
///   signature does not verify
pub(crate) fn check_signed_by(child: &[u8], parent: &[u8]) -> anyhow::Result<()> {
    let child_cert = parse_cert(child)?;
    let parent_cert = parse_cert(parent)?;
    ensure!(
        child_cert.tbs_certificate.issuer() == parent_cert.tbs_certificate.subject(),
        "certificate issuer does not match its parent's subject"
    );
    verify_signed(
        parent_cert.public_key(),
        &child_cert.signature_algorithm,
        &child_cert.signature_value,
        child_cert.tbs_certificate.as_ref(),
    )
    .map_err(|err| anyhow::anyhow!("certificate signature does not verify against its parent: {err:?}"))
}

/// A certificate's subject as a one-line name, for errors.
fn subject_name(cert: &X509Certificate<'_>) -> String {
    cert.subject().to_string()
}

/// The root's list names the chain's intermediate: the chain is revoked. It is raised only for a
/// list issued and signed by the root, so it is proof of revocation, not an input error.
#[derive(Debug)]
pub(crate) struct IntermediateRevoked(pub(crate) String);

impl std::fmt::Display for IntermediateRevoked {
    /// The revoked serial.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "certificate 0x{} is revoked", self.0)
    }
}

impl std::error::Error for IntermediateRevoked {}

/// Checks `crl`, the processor's certificate revocation list, against the pinned root and the
/// chain's intermediate at `now`: the CRL must be the root's, signed by it, current, and not list
/// the intermediate's serial. Certificate serials are unique only within an issuing CA, and the
/// root issues the intermediate alone, so a root-issued entry that collides with the VCEK's serial
/// cannot revoke it. A revocation outlasts its list, so an authentic list naming the intermediate
/// is reported as `IntermediateRevoked` before its dates are read. Returns the span within which
/// the list is current.
/// Error cases:
/// - the CRL does not parse or verify against the root, or is not current at `now`
/// - the CRL revokes the intermediate (`IntermediateRevoked`)
pub(crate) fn check_revocation(crl: &[u8], chain: &QuoteChain, now: ASN1Time) -> anyhow::Result<Window> {
    let (_, crl) = x509_parser::revocation_list::CertificateRevocationList::from_der(crl)
        .map_err(|err| anyhow::anyhow!("parsing the certificate revocation list: {err:?}"))?;
    let root = parse_cert(chain.root_der())?;
    ensure!(crl.issuer() == root.subject(), "revocation list issuer is not the chain's root");
    verify_signed(root.public_key(), &crl.signature_algorithm, &crl.signature_value, crl.tbs_cert_list.as_ref())
        .map_err(|err| anyhow::anyhow!("revocation list signature does not verify against the root: {err:?}"))?;
    let intermediate = chain.intermediate_serial()?;
    let revoked = crl
        .iter_revoked_certificates()
        .find(|entry| trim_leading_zeros(&intermediate) == trim_leading_zeros(entry.raw_serial()));
    if let Some(entry) = revoked {
        return Err(IntermediateRevoked(entry.raw_serial_as_string()).into());
    }
    ensure!(crl.last_update() <= now, "revocation list is not yet valid");
    if let Some(next) = crl.next_update() {
        ensure!(now <= next, "revocation list is expired");
    }
    Ok(Window {
        from: crl.last_update().timestamp(),
        until: crl.next_update().map_or(i64::MAX, |next| next.timestamp()),
    })
}

/// A serial's bytes without leading zeros, so a parsed certificate serial and a CRL entry's raw
/// DER content (which sign-pads positive values with the high bit set) compare as integers.
fn trim_leading_zeros(bytes: &[u8]) -> &[u8] {
    let first_nonzero = bytes.iter().position(|byte| *byte != 0).unwrap_or(bytes.len());
    &bytes[first_nonzero..]
}

/// Parses the first PEM certificate in `pem` to DER.
/// Error cases:
/// - the PEM holds no certificate
fn parse_pem_cert(pem: &str) -> anyhow::Result<Vec<u8>> {
    pem_certificates(pem)?.into_iter().next().context("PEM holds no certificate")
}

/// Every PEM certificate in `pem`, as DER.
/// Error cases:
/// - a PEM block is malformed
pub fn pem_certificates(pem: &str) -> anyhow::Result<Vec<Vec<u8>>> {
    let mut certs = Vec::new();
    for block in x509_parser::pem::Pem::iter_from_buffer(pem.as_bytes()) {
        let block = block.map_err(|err| anyhow::anyhow!("malformed PEM: {err}"))?;
        if block.label == "CERTIFICATE" {
            certs.push(block.contents);
        }
    }
    Ok(certs)
}

/// A DER INTEGER extension value as a `u8`.
/// Error cases:
/// - the value is not a DER INTEGER or does not fit a `u8`
fn extension_integer(extension: &x509_parser::extensions::X509Extension<'_>, what: &str) -> anyhow::Result<u8> {
    let value = extension.value;
    ensure!(value.first() == Some(&0x02), "VCEK {what} extension is not a DER INTEGER");
    let len = *value.get(1).with_context(|| format!("VCEK {what} extension is truncated"))? as usize;
    let content = value.get(2..2 + len).with_context(|| format!("VCEK {what} extension is truncated"))?;
    match content {
        [byte] => Ok(*byte),
        [0, byte] => Ok(*byte),
        _ => bail!("VCEK {what} extension does not fit a u8"),
    }
}

/// The hardware id extension value: the vendor signs the 64 bytes with no ASN.1 tag; an OCTET
/// STRING-wrapped form is accepted in case the vendor adds the tag.
/// Error cases:
/// - the value is neither 64 raw bytes nor a DER OCTET STRING holding 64 bytes
fn extension_hwid(extension: &x509_parser::extensions::X509Extension<'_>) -> anyhow::Result<[u8; 64]> {
    let value = extension.value;
    if value.len() == 64 {
        return value.try_into().context("VCEK hardware id extension is not 64 bytes");
    }
    ensure!(value.first() == Some(&0x04), "VCEK hardware id extension is not an OCTET STRING");
    let len = *value.get(1).context("VCEK hardware id extension is truncated")? as usize;
    value
        .get(2..2 + len)
        .and_then(|bytes| bytes.try_into().ok())
        .context("VCEK hardware id extension does not hold 64 bytes")
}
