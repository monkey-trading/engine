//! Parsing and signature verification of an AMD SEV-SNP attestation report, the raw report a
//! configfs-tsm guest reads from `outblob` on SEV-SNP. The layout is the version-2 report the
//! platform firmware issues; offsets are fixed and little-endian where the field is an integer.

use anyhow::{Context, bail, ensure};

/// The report size every supported version occupies.
pub const REPORT_LEN: usize = 0x2A0 + 512;

/// The bytes the report signature covers.
const SIGNED_LEN: usize = 0x2A0;
/// Offset of the ECDSA signature block: `R` (72 bytes), `S` (72 bytes), then reserved.
const SIGNATURE_OFFSET: usize = 0x2A0;
/// The scalar size of the P-384 curve; `R` and `S` hold a value of at most this many bytes.
const SCALAR_LEN: usize = 48;

const VERSION_OFFSET: usize = 0x00;
const POLICY_OFFSET: usize = 0x08;
const SIGNATURE_ALGO_OFFSET: usize = 0x34;
const REPORT_DATA_OFFSET: usize = 0x50;
const MEASUREMENT_OFFSET: usize = 0x90;
const REPORTED_TCB_OFFSET: usize = 0x180;
const CHIP_ID_OFFSET: usize = 0x1A0;

/// The only report version this parser implements.
const SUPPORTED_VERSION: u32 = 2;
/// 1 means ECDSA with the P-384 curve and SHA-384.
const ECDSA_P384_SHA384: u32 = 1;

/// The security-version levels a TCB value carries, high byte to low.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct Tcb {
    pub boot_loader: u8,
    pub tee: u8,
    pub snp: u8,
    pub microcode: u8,
}

impl Tcb {
    fn from_packed(value: u64) -> Self {
        Self {
            boot_loader: (value >> 56) as u8,
            tee: (value >> 48) as u8,
            snp: (value >> 40) as u8,
            microcode: (value >> 32) as u8,
        }
    }

    /// The packed form the report carries.
    #[cfg(test)]
    pub fn to_packed(self) -> u64 {
        ((self.boot_loader as u64) << 56)
            | ((self.tee as u64) << 48)
            | ((self.snp as u64) << 40)
            | ((self.microcode as u64) << 32)
    }
}

/// A parsed attestation report over its original bytes, which the signature covers.
pub struct SnpReport<'a> {
    bytes: &'a [u8],
    /// The guest policy the launch ran under.
    pub policy: u64,
    /// The TCB the report was issued under: what the VCEK certifies.
    pub reported_tcb: Tcb,
    /// The launch measurement the firmware recorded.
    pub measurement: [u8; 48],
    /// The report data the guest asked the quote to bind.
    pub report_data: [u8; 64],
    /// The signing chip's unique identifier.
    pub chip_id: [u8; 64],
}

impl<'a> SnpReport<'a> {
    /// Parses `bytes` as a version-2 SEV-SNP attestation report. Anything else — a truncated
    /// quote, another platform's quote, or a mock quote — is refused here, before any field of
    /// it is trusted.
    /// Error cases:
    /// - the quote is not exactly 1184 bytes, so it cannot be this report format
    /// - the version is not 2, or the signature algorithm is not ECDSA P-384 with SHA-384
    pub fn parse(bytes: &'a [u8]) -> anyhow::Result<Self> {
        ensure!(
            bytes.len() == REPORT_LEN,
            "quote is {} bytes, not the {} of an SEV-SNP attestation report",
            bytes.len(),
            REPORT_LEN
        );
        let version = le_u32(bytes, VERSION_OFFSET);
        ensure!(
            version == SUPPORTED_VERSION,
            "unsupported attestation report version {version}, want {SUPPORTED_VERSION}"
        );
        let algo = le_u32(bytes, SIGNATURE_ALGO_OFFSET);
        ensure!(algo == ECDSA_P384_SHA384, "unsupported report signature algorithm {algo}, want ECDSA P-384/SHA-384");
        Ok(Self {
            bytes,
            policy: le_u64(bytes, POLICY_OFFSET),
            reported_tcb: Tcb::from_packed(le_u64(bytes, REPORTED_TCB_OFFSET)),
            measurement: bytes[MEASUREMENT_OFFSET..MEASUREMENT_OFFSET + 48].try_into().expect("report length checked"),
            report_data: bytes[REPORT_DATA_OFFSET..REPORT_DATA_OFFSET + 64].try_into().expect("report length checked"),
            chip_id: bytes[CHIP_ID_OFFSET..CHIP_ID_OFFSET + 64].try_into().expect("report length checked"),
        })
    }

    /// Verifies the report signature against the VCEK's public key, the SEC 1 uncompressed point
    /// of the VCEK certificate that certified this chip and TCB. The signature is ECDSA P-384 over
    /// SHA-384 of the report's first `0x2A0` bytes.
    /// Error cases:
    /// - `R` or `S` does not fit the curve's scalar size
    /// - the signature does not verify against the VCEK public key
    pub fn verify_signature(&self, vcek_spki_der: &[u8]) -> anyhow::Result<()> {
        let r = scalar(self.bytes, SIGNATURE_OFFSET)?;
        let s = scalar(self.bytes, SIGNATURE_OFFSET + 72)?;
        let mut signature = [0u8; 2 * SCALAR_LEN];
        signature[..SCALAR_LEN].copy_from_slice(&r);
        signature[SCALAR_LEN..].copy_from_slice(&s);
        ring::signature::UnparsedPublicKey::new(&ring::signature::ECDSA_P384_SHA384_FIXED, vcek_spki_der)
            .verify(&self.bytes[..SIGNED_LEN], &signature)
            .context("report signature does not verify against the VCEK")
    }
}

fn le_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("report length checked"))
}

fn le_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().expect("report length checked"))
}

/// The 48-byte big-endian value in a 72-byte signature field, right-justified with zero padding.
/// Error cases:
/// - the value takes more than 48 bytes, so it cannot be a P-384 scalar
fn scalar(bytes: &[u8], offset: usize) -> anyhow::Result<[u8; SCALAR_LEN]> {
    let field = &bytes[offset..offset + 72];
    let first_nonzero = field.iter().position(|b| *b != 0).unwrap_or(72);
    let value = &field[first_nonzero..];
    if value.len() > SCALAR_LEN {
        bail!("report signature scalar takes {} bytes, more than a P-384 scalar", value.len());
    }
    let mut scalar = [0u8; SCALAR_LEN];
    scalar[SCALAR_LEN - value.len()..].copy_from_slice(value);
    Ok(scalar)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_report_of_the_wrong_size_is_refused() {
        assert!(SnpReport::parse(&[0u8; 64]).is_err());
        assert!(SnpReport::parse(&[0u8; REPORT_LEN - 1]).is_err());
    }

    #[test]
    fn a_mock_quote_is_refused_before_any_field_is_read() {
        let mut quote = b"MOCK-ATTESTATION-NOT-A-TEE:".to_vec();
        quote.extend_from_slice(&[0u8; 64]);
        assert!(SnpReport::parse(&quote).is_err());
    }

    #[test]
    fn version_and_signature_algorithm_are_checked() {
        let mut report = vec![0u8; REPORT_LEN];
        report[SIGNATURE_ALGO_OFFSET..SIGNATURE_ALGO_OFFSET + 4].copy_from_slice(&1u32.to_le_bytes());
        assert!(SnpReport::parse(&report).is_err());
        report[VERSION_OFFSET..VERSION_OFFSET + 4].copy_from_slice(&2u32.to_le_bytes());
        SnpReport::parse(&report).expect("version 2 with ECDSA P-384 parses");
    }

    #[test]
    fn tcb_fields_decode_from_the_packed_u64() {
        let tcb = Tcb::from_packed((7 << 56) | (6 << 48) | (5 << 40) | (4 << 32));
        assert_eq!(tcb, Tcb { boot_loader: 7, tee: 6, snp: 5, microcode: 4 });
        assert!(tcb > Tcb { boot_loader: 7, tee: 6, snp: 5, microcode: 3 });
        assert!(tcb < Tcb { boot_loader: 7, tee: 6, snp: 6, microcode: 0 });
    }
}
