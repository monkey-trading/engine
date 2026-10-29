//! Parsing and signature verification of an AMD SEV-SNP attestation report, the raw report a
//! configfs-tsm guest reads from `outblob` on SEV-SNP. The layout is the version-2 and
//! version-3 report the platform firmware issues, which share every offset this verifier reads;
//! offsets are fixed and little-endian where the field is an integer.

use anyhow::{Context, ensure};
use p384::ecdsa::signature::Verifier as _;

/// The report size every supported version occupies.
pub(crate) const REPORT_LEN: usize = 0x2A0 + 512;

/// The bytes the report signature covers.
const SIGNED_LEN: usize = 0x2A0;
/// Offset of the ECDSA signature block: `R` (72 bytes), `S` (72 bytes), then reserved. `R` and
/// `S` carry their 48-byte value little-endian in the first 48 bytes of the field, zero-extended.
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

/// The report versions this parser implements. Every field the verifier reads sits at the same
/// offset in each: later versions only append CPUID and firmware-version fields.
const SUPPORTED_VERSIONS: [u32; 2] = [2, 3];
/// 1 means ECDSA with the P-384 curve and SHA-384.
const ECDSA_P384_SHA384: u32 = 1;

/// The security-version levels a TCB value carries, in the packed form's byte order:
/// `boot_loader` in bits 7:0, `tee` in 15:8, `snp` in 55:48, and `microcode` in 63:56, the two
/// reserved bytes between `tee` and `snp` ignored.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Tcb {
    pub boot_loader: u8,
    pub tee: u8,
    pub snp: u8,
    pub microcode: u8,
}

impl Tcb {
    fn from_packed(value: u64) -> Self {
        Self {
            boot_loader: value as u8,
            tee: (value >> 8) as u8,
            snp: (value >> 48) as u8,
            microcode: (value >> 56) as u8,
        }
    }

    /// The packed form the report carries.
    #[cfg(any(test, feature = "test-fixtures"))]
    pub fn to_packed(self) -> u64 {
        (self.boot_loader as u64)
            | ((self.tee as u64) << 8)
            | ((self.snp as u64) << 48)
            | ((self.microcode as u64) << 56)
    }

    /// Whether every component is at or above `minimum`'s; a TCB is a floor per component, not a
    /// version tuple, so no component may fall below its minimum.
    pub fn meets(self, minimum: Self) -> bool {
        self.boot_loader >= minimum.boot_loader
            && self.tee >= minimum.tee
            && self.snp >= minimum.snp
            && self.microcode >= minimum.microcode
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
            SUPPORTED_VERSIONS.contains(&version),
            "unsupported attestation report version {version}, want one of {SUPPORTED_VERSIONS:?}"
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
        let verified = p384::ecdsa::VerifyingKey::from_sec1_bytes(vcek_spki_der).and_then(|key| {
            let signature = p384::ecdsa::Signature::from_slice(&signature)?;
            key.verify(&self.bytes[..SIGNED_LEN], &signature)
        });
        verified.context("report signature does not verify against the VCEK")
    }
}

fn le_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("report length checked"))
}

fn le_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().expect("report length checked"))
}

/// The 48-byte big-endian value the verifier reads, from a 72-byte signature field that carries it
/// little-endian in its first 48 bytes, zero-extended.
/// Error cases:
/// - the field's zero padding is not zero, so the value is not a zero-extended scalar
fn scalar(bytes: &[u8], offset: usize) -> anyhow::Result<[u8; SCALAR_LEN]> {
    let field = &bytes[offset..offset + 72];
    ensure!(field[SCALAR_LEN..].iter().all(|b| *b == 0), "report signature scalar's zero padding is not zero");
    let mut scalar: [u8; SCALAR_LEN] = field[..SCALAR_LEN].try_into().expect("field length fixed");
    scalar.reverse();
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
        report[VERSION_OFFSET..VERSION_OFFSET + 4].copy_from_slice(&2u32.to_le_bytes());
        assert!(SnpReport::parse(&report).is_err(), "version 2 with no signature algorithm is refused");
        report[SIGNATURE_ALGO_OFFSET..SIGNATURE_ALGO_OFFSET + 4].copy_from_slice(&2u32.to_le_bytes());
        assert!(SnpReport::parse(&report).is_err(), "version 2 with a wrong algorithm is refused");
        report[SIGNATURE_ALGO_OFFSET..SIGNATURE_ALGO_OFFSET + 4].copy_from_slice(&1u32.to_le_bytes());
        SnpReport::parse(&report).expect("version 2 with ECDSA P-384 parses");
        report[VERSION_OFFSET..VERSION_OFFSET + 4].copy_from_slice(&3u32.to_le_bytes());
        SnpReport::parse(&report).expect("version 3 keeps every offset this parser reads");
        report[VERSION_OFFSET..VERSION_OFFSET + 4].copy_from_slice(&4u32.to_le_bytes());
        assert!(SnpReport::parse(&report).is_err(), "an unknown version is refused");
    }

    #[test]
    fn tcb_fields_decode_from_the_packed_u64() {
        // The wire layout puts boot_loader in byte 0, tee in byte 1, snp in byte 6, and
        // microcode in byte 7; the bytes between stay reserved.
        let packed = 0xCD08_0000_0000_0403u64;
        let tcb = Tcb::from_packed(packed);
        assert_eq!(tcb, Tcb { boot_loader: 3, tee: 4, snp: 8, microcode: 0xcd });
        assert_eq!(tcb.to_packed(), packed);
    }

    #[test]
    fn a_tcb_meets_a_minimum_only_when_every_component_does() {
        let minimum = Tcb { boot_loader: 3, tee: 0, snp: 8, microcode: 115 };
        assert!(Tcb { boot_loader: 3, tee: 0, snp: 8, microcode: 115 }.meets(minimum));
        assert!(Tcb { boot_loader: 9, tee: 2, snp: 9, microcode: 200 }.meets(minimum));
        for below in [
            Tcb { boot_loader: 2, tee: 0, snp: 8, microcode: 115 },
            Tcb { boot_loader: 3, tee: 0, snp: 7, microcode: 115 },
            Tcb { boot_loader: 3, tee: 0, snp: 8, microcode: 114 },
            Tcb { boot_loader: 4, tee: 0, snp: 8, microcode: 114 },
        ] {
            assert!(!below.meets(minimum), "a component below its minimum met it: {below:?}");
        }
    }
}
