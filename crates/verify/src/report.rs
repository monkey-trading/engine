//! Parsing and signature verification of an AMD SEV-SNP attestation report, the raw report a
//! configfs-tsm guest reads from `outblob` on SEV-SNP. The layout is the `ATTESTATION_REPORT` of AMD's
//! SEV Secure Nested Paging Firmware ABI Specification, publication 56860, revision 1.59 (August
//! 2026), Table 27; offsets are fixed and little-endian where the field is an integer. Versions 2 to
//! 6 are read. Each keeps every field of the one before at its offset and adds fields in space that
//! was reserved:
//! - 3 adds the signing chip's CPUID family, model and stepping. The family selects the layout of
//!   every TCB value, and only family 19h's (Milan and Genoa, and the Bergamo and Siena parts the
//!   vendor certifies under Genoa) is implemented, so a report from another family is refused. The
//!   vendor numbers TCB values per model, so a verifier holds the CPUID to a model the release lists
//!   (`cpu`), and refuses a version 2 report, which names none.
//! - 4 is unpublished. It is read as 3, as public parsers read the version-4 reports SEV firmware
//!   1.55.31 issues; every field read here sits at the same offset in 3 and 5.
//! - 5 (revision 1.58, May 2025) adds the launch and current mitigation vectors. They are read but
//!   not required: each bit is a per-vulnerability check the host asks the firmware to verify, which
//!   AMD's security bulletins define, and a release states its firmware floor as its minimum TCB.
//! - 6 (revision 1.59) adds the extended TCBs, which only family 1Ah "Venice" parts report, so none
//!   is read, and the chip-secret VCEK as a signing key, which `verify_signature` refuses.

use std::ops::RangeInclusive;

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
/// KEY_INFO: SIGNING_KEY in bits 4:2.
const KEY_INFO_OFFSET: usize = 0x48;
const REPORT_DATA_OFFSET: usize = 0x50;
const MEASUREMENT_OFFSET: usize = 0x90;
const REPORTED_TCB_OFFSET: usize = 0x180;
/// CPUID_FAM_ID, CPUID_MOD_ID and CPUID_STEP, one byte each, from version 3.
const CPUID_OFFSET: usize = 0x188;
const CHIP_ID_OFFSET: usize = 0x1A0;
const COMMITTED_TCB_OFFSET: usize = 0x1E0;
/// LAUNCH_MIT_VECTOR and CURRENT_MIT_VECTOR, from version 5.
const LAUNCH_MIT_VECTOR_OFFSET: usize = 0x1F8;
const CURRENT_MIT_VECTOR_OFFSET: usize = 0x200;

/// The report versions this parser implements.
const SUPPORTED_VERSIONS: RangeInclusive<u32> = 2..=6;
const CPUID_FROM_VERSION: u32 = 3;
const MITIGATION_VECTORS_FROM_VERSION: u32 = 5;
/// The CPUID family whose TCB layout `Tcb::from_packed` decodes (Table 5 of the specification).
pub(crate) const FAMILY_19H: u8 = 0x19;
/// 1 means ECDSA with the P-384 curve and SHA-384.
const ECDSA_P384_SHA384: u32 = 1;
/// SIGNING_KEY 0: the VCEK the vendor's key-distribution service issues.
const SIGNING_KEY_VCEK: u32 = 0;

/// The security-version levels a TCB value carries, in family 19h's packed form: `boot_loader` in
/// bits 7:0, `tee` in 15:8, `snp` in 55:48, and `microcode` in 63:56, the reserved bytes between
/// `tee` and `snp` ignored.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Tcb {
    pub boot_loader: u8,
    pub tee: u8,
    pub snp: u8,
    pub microcode: u8,
}

impl Tcb {
    /// The levels a packed TCB value carries.
    pub fn from_packed(value: u64) -> Self {
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

/// The signing chip's CPUID identification, as the firmware reports it from version 3.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Cpuid {
    /// The extended family plus the family.
    pub family: u8,
    /// The extended model and the model.
    pub model: u8,
    pub stepping: u8,
}

/// The mitigations the firmware has verified, each bit one vulnerability's, from version 5.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MitigationVectors {
    /// Those verified when the guest launched.
    pub launch: u64,
    /// Those verified when the report was issued.
    pub current: u64,
}

/// A parsed attestation report over its original bytes, which the signature covers.
pub struct SnpReport<'a> {
    bytes: &'a [u8],
    /// The report's version, one of `SUPPORTED_VERSIONS`.
    pub version: u32,
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
    /// The TCB the platform has committed to, below which its firmware cannot be rolled back,
    /// packed as the guest's derived-key request takes it.
    pub committed_tcb: u64,
    /// The signing chip's CPUID identification, from version 3; its family is 19h.
    pub cpuid: Option<Cpuid>,
    /// The verified mitigations, from version 5.
    pub mitigation_vectors: Option<MitigationVectors>,
}

impl<'a> SnpReport<'a> {
    /// Parses `bytes` as an SEV-SNP attestation report of version 2 to 6. Anything else — a
    /// truncated quote, another platform's quote, or a mock quote — is refused here, before any
    /// field of it is trusted.
    /// Error cases:
    /// - the quote is not exactly 1184 bytes, so it cannot be this report format
    /// - the version is not 2 to 6, or the signature algorithm is not ECDSA P-384 with SHA-384
    /// - from version 3, the CPUID family is not 19h, so the report's TCB values have another layout
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
            "unsupported attestation report version {version}, want {} to {}",
            SUPPORTED_VERSIONS.start(),
            SUPPORTED_VERSIONS.end()
        );
        let algo = le_u32(bytes, SIGNATURE_ALGO_OFFSET);
        ensure!(algo == ECDSA_P384_SHA384, "unsupported report signature algorithm {algo}, want ECDSA P-384/SHA-384");
        let cpuid = (version >= CPUID_FROM_VERSION).then(|| Cpuid {
            family: bytes[CPUID_OFFSET],
            model: bytes[CPUID_OFFSET + 1],
            stepping: bytes[CPUID_OFFSET + 2],
        });
        if let Some(cpuid) = cpuid {
            ensure!(
                cpuid.family == FAMILY_19H,
                "the report is from a CPU of family 0x{:02x}; only family 0x19's TCB layout (Milan, Genoa, \
                 Bergamo, Siena) is implemented",
                cpuid.family
            );
        }
        let mitigation_vectors = (version >= MITIGATION_VECTORS_FROM_VERSION).then(|| MitigationVectors {
            launch: le_u64(bytes, LAUNCH_MIT_VECTOR_OFFSET),
            current: le_u64(bytes, CURRENT_MIT_VECTOR_OFFSET),
        });
        Ok(Self {
            bytes,
            version,
            policy: le_u64(bytes, POLICY_OFFSET),
            reported_tcb: Tcb::from_packed(le_u64(bytes, REPORTED_TCB_OFFSET)),
            measurement: bytes[MEASUREMENT_OFFSET..MEASUREMENT_OFFSET + 48].try_into().expect("report length checked"),
            report_data: bytes[REPORT_DATA_OFFSET..REPORT_DATA_OFFSET + 64].try_into().expect("report length checked"),
            chip_id: bytes[CHIP_ID_OFFSET..CHIP_ID_OFFSET + 64].try_into().expect("report length checked"),
            committed_tcb: le_u64(bytes, COMMITTED_TCB_OFFSET),
            cpuid,
            mitigation_vectors,
        })
    }

    /// Verifies the report signature against the VCEK's public key, the SEC 1 uncompressed point
    /// of the VCEK certificate that certified this chip and TCB. The signature is ECDSA P-384 over
    /// SHA-384 of the report's first `0x2A0` bytes.
    /// Error cases:
    /// - the report names another signing key: a VLEK, the chip-secret VCEK, none, or a reserved one
    /// - `R` or `S` does not fit the curve's scalar size
    /// - the signature does not verify against the VCEK public key
    pub fn verify_signature(&self, vcek_spki_der: &[u8]) -> anyhow::Result<()> {
        let signing_key = (le_u32(self.bytes, KEY_INFO_OFFSET) >> 2) & 0b111;
        ensure!(
            signing_key == SIGNING_KEY_VCEK,
            "the report is signed with {}, not the VCEK the key-distribution service issues",
            match signing_key {
                1 => "a VLEK".to_owned(),
                2 => "the chip-secret VCEK".to_owned(),
                7 => "no key".to_owned(),
                reserved => format!("reserved signing key {reserved}"),
            }
        );
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
        let mut report = report_of(2);
        report[SIGNATURE_ALGO_OFFSET..SIGNATURE_ALGO_OFFSET + 4].copy_from_slice(&0u32.to_le_bytes());
        assert!(SnpReport::parse(&report).is_err(), "version 2 with no signature algorithm is refused");
        report[SIGNATURE_ALGO_OFFSET..SIGNATURE_ALGO_OFFSET + 4].copy_from_slice(&2u32.to_le_bytes());
        assert!(SnpReport::parse(&report).is_err(), "version 2 with a wrong algorithm is refused");
        for version in 2..=6 {
            assert_eq!(SnpReport::parse(&report_of(version)).expect("a supported version parses").version, version);
        }
        for version in [0, 1, 7, 8, u32::MAX] {
            let err = SnpReport::parse(&report_of(version)).err().expect("an unknown version is refused");
            assert_eq!(err.to_string(), format!("unsupported attestation report version {version}, want 2 to 6"));
        }
    }

    #[test]
    fn each_added_field_is_read_from_the_version_that_adds_it() {
        for version in 2..=6 {
            let mut report = report_of(version);
            report[CPUID_OFFSET..CPUID_OFFSET + 3].copy_from_slice(&[0x19, 0xA0, 0x02]);
            report[LAUNCH_MIT_VECTOR_OFFSET..LAUNCH_MIT_VECTOR_OFFSET + 8].copy_from_slice(&0x05u64.to_le_bytes());
            report[CURRENT_MIT_VECTOR_OFFSET..CURRENT_MIT_VECTOR_OFFSET + 8]
                .copy_from_slice(&0x8000_0000_0000_0007u64.to_le_bytes());
            let parsed = SnpReport::parse(&report).unwrap();
            // Version 2 holds neither field, and versions 3 and 4 no mitigation vectors: those bytes
            // are reserved there, so nothing is read from them.
            let cpuid = (version >= 3).then_some(Cpuid { family: 0x19, model: 0xA0, stepping: 0x02 });
            let vectors = (version >= 5).then_some(MitigationVectors { launch: 0x05, current: 0x8000_0000_0000_0007 });
            assert_eq!((parsed.cpuid, parsed.mitigation_vectors), (cpuid, vectors), "version {version}");
        }
    }

    #[test]
    fn a_report_from_a_family_other_than_19h_is_refused() {
        for version in 3..=6 {
            let mut report = report_of(version);
            for model in [0x01, 0x11, 0xA0, 0xFF] {
                report[CPUID_OFFSET..CPUID_OFFSET + 2].copy_from_slice(&[0x19, model]);
                SnpReport::parse(&report).unwrap_or_else(|err| panic!("family 19h model {model:#x}: {err:#}"));
            }
            // Turin and Venice are family 1Ah, whose TCB values carry other fields at other bytes.
            for family in [0x00, 0x17, 0x1A] {
                report[CPUID_OFFSET] = family;
                let err = SnpReport::parse(&report).err().expect("another family is refused");
                assert_eq!(
                    err.to_string(),
                    format!(
                        "the report is from a CPU of family 0x{family:02x}; only family 0x19's TCB layout \
                         (Milan, Genoa, Bergamo, Siena) is implemented"
                    ),
                    "version {version}"
                );
            }
        }
        // Version 2 carries no CPUID: those bytes are reserved and not read.
        let mut report = report_of(2);
        report[CPUID_OFFSET] = 0x1A;
        assert_eq!(SnpReport::parse(&report).unwrap().cpuid, None);
    }

    #[test]
    fn a_signing_key_other_than_the_vcek_is_refused_before_the_signature_is_read() {
        let mut report = report_of(6);
        for (key_info, named) in [
            (0b0_0100, "a VLEK"),
            (0b0_1000, "the chip-secret VCEK"),
            (0b1_1100, "no key"),
            (0b0_1100, "reserved signing key 3"),
            // AUTHOR_KEY_EN and MASK_CHIP_KEY, bits 0 and 1, leave the signing key as it is.
            (0b0_0111, "a VLEK"),
        ] {
            report[KEY_INFO_OFFSET..KEY_INFO_OFFSET + 4].copy_from_slice(&(key_info as u32).to_le_bytes());
            let err = SnpReport::parse(&report).unwrap().verify_signature(&[]).unwrap_err();
            assert_eq!(
                err.to_string(),
                format!("the report is signed with {named}, not the VCEK the key-distribution service issues")
            );
        }
        // The VCEK, with or without the author key bit: the signature check runs, and fails here on a
        // key that is no P-384 point.
        for key_info in [0b00u32, 0b01] {
            report[KEY_INFO_OFFSET..KEY_INFO_OFFSET + 4].copy_from_slice(&key_info.to_le_bytes());
            let err = SnpReport::parse(&report).unwrap().verify_signature(&[]).unwrap_err();
            assert_eq!(err.to_string(), "report signature does not verify against the VCEK");
        }
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

    // ── Helpers ──

    /// An unsigned report of `version` with the ECDSA P-384 algorithm and a family 19h CPUID byte,
    /// which version 2 does not read; every other byte zero.
    fn report_of(version: u32) -> Vec<u8> {
        let mut report = vec![0u8; REPORT_LEN];
        report[VERSION_OFFSET..VERSION_OFFSET + 4].copy_from_slice(&version.to_le_bytes());
        report[SIGNATURE_ALGO_OFFSET..SIGNATURE_ALGO_OFFSET + 4].copy_from_slice(&ECDSA_P384_SHA384.to_le_bytes());
        report[CPUID_OFFSET] = FAMILY_19H;
        report
    }
}
