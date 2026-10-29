//! Test fixtures, for this crate's tests, the node's, and the dev scenarios: a stand-in vendor
//! chain with the real shapes, a forged report on the firmware's wire layout, the release manifest,
//! and the collateral a node would relay for them.

use p384::ecdsa::signature::Signer as _;
use p384::pkcs8::DecodePrivateKey as _;
use rcgen::{
    BasicConstraints, Certificate, CertificateParams, CustomExtension, DnType, IsCa, Issuer, KeyPair, KeyUsagePurpose,
    SerialNumber,
};

use crate::report::REPORT_LEN;
use crate::{Attestation, AttestationBundle, RelayedCollateral, ReleaseManifest, Tcb};

/// A stand-in vendor chain with the real shape: a self-signed root, an intermediate, and the
/// chip key that signs reports, plus the vendor's extension OIDs binding the VCEK to `chip_id`
/// and `tcb`.
pub struct TestChain {
    pub ark: Certificate,
    pub ask: Certificate,
    pub vcek: Certificate,
    pub vcek_key: KeyPair,
    pub ark_params: CertificateParams,
    pub ark_key: KeyPair,
    pub ask_params: CertificateParams,
    pub ask_key: KeyPair,
}

/// The VCEK's serial in every fixture chain, so a revocation list can name it. The high bit of
/// the first significant byte is set, so DER encodings of the serial carry a sign-padding zero.
pub const VCEK_SERIAL: u64 = 0x8A11CE;

pub const PROCESSOR: &str = "TestProc";
pub const POLICY: u64 = 0x30000;
pub const REPORTED_TCB: Tcb = Tcb { boot_loader: 4, tee: 0, snp: 8, microcode: 205 };

/// A DER INTEGER extension value, the encoding the vendor signs: values with the high bit set
/// take a leading zero so the INTEGER stays positive.
pub fn der_integer(value: u8) -> Vec<u8> {
    if value < 0x80 { vec![0x02, 0x01, value] } else { vec![0x02, 0x02, 0x00, value] }
}

/// A DER IA5String extension value.
pub fn der_ia5(text: &str) -> Vec<u8> {
    let mut value = vec![0x16, text.len() as u8];
    value.extend_from_slice(text.as_bytes());
    value
}

pub fn test_chain(chip_id: [u8; 64], tcb: Tcb) -> TestChain {
    test_chain_with(chip_id, tcb, Variants::default())
}

/// What differs from the vendor-shaped chain in a negative test: nothing by default.
#[derive(Default)]
pub struct Variants<'a> {
    pub struct_version: u8,
    /// Signs the VCEK with this key while keeping the intermediate's issuer name, so only the
    /// signature differs; likewise for the intermediate and the root's name.
    pub vcek_signer: Option<&'a KeyPair>,
    pub ask_signer: Option<&'a KeyPair>,
    /// Wraps the hardware id extension in an OCTET STRING, the tagged form the vendor may adopt.
    pub tagged_hwid: bool,
    /// Issues the VCEK already expired, for the validity-window test.
    pub expired_vcek: bool,
    /// Issues the VCEK with a not-before in the future, for the validity-window test.
    pub not_yet_valid_vcek: bool,
}

/// A chain with the given variations from the vendor shape, for the negative tests.
pub fn test_chain_with(chip_id: [u8; 64], tcb: Tcb, variants: Variants<'_>) -> TestChain {
    let ark_key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P384_SHA384).unwrap();
    let mut ark_params = ca_params("ARK-Test");
    ark_params.serial_number = Some(SerialNumber::from(1));
    let ark = ark_params.self_signed(&ark_key).unwrap();

    let ask_key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P384_SHA384).unwrap();
    let mut ask_params = ca_params("ASK-Test");
    ask_params.serial_number = Some(SerialNumber::from(2));
    let ask_signer = variants.ask_signer.unwrap_or(&ark_key);
    let ask = ask_params.signed_by(&ask_key, &Issuer::from_params(&ark_params, ask_signer)).unwrap();

    let vcek_key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P384_SHA384).unwrap();
    let mut vcek_params = CertificateParams::new(Vec::new()).unwrap();
    vcek_params.serial_number = Some(SerialNumber::from(VCEK_SERIAL));
    if variants.expired_vcek {
        vcek_params.not_after = rcgen::date_time_ymd(2020, 1, 1);
    }
    if variants.not_yet_valid_vcek {
        vcek_params.not_before = rcgen::date_time_ymd(2030, 1, 1);
    }
    vcek_params.distinguished_name.push(DnType::CommonName, "VCEK-Test");
    vcek_params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    vcek_params.custom_extensions = vcek_extensions(chip_id, tcb, variants.struct_version, variants.tagged_hwid);
    let vcek_signer = variants.vcek_signer.unwrap_or(&ask_key);
    let vcek = vcek_params.signed_by(&vcek_key, &Issuer::from_params(&ask_params, vcek_signer)).unwrap();
    TestChain { ark, ask, vcek, vcek_key, ark_params, ark_key, ask_params, ask_key }
}

/// A genuine VCEK of `chain` (signed by its intermediate) for another chip.
pub fn vcek_for(chain: &TestChain, chip_id: [u8; 64], tcb: Tcb) -> Certificate {
    let mut params = CertificateParams::new(Vec::new()).unwrap();
    params.serial_number = Some(SerialNumber::from(VCEK_SERIAL));
    params.distinguished_name.push(DnType::CommonName, "VCEK-Test");
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    params.custom_extensions = vcek_extensions(chip_id, tcb, 0, false);
    let key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P384_SHA384).unwrap();
    params.signed_by(&key, &Issuer::from_params(&chain.ask_params, &chain.ask_key)).unwrap()
}

/// The vendor's VCEK extension set for `chip_id` and `tcb`.
pub fn vcek_extensions(chip_id: [u8; 64], tcb: Tcb, struct_version: u8, tagged_hwid: bool) -> Vec<CustomExtension> {
    let hwid = if tagged_hwid {
        let mut tagged = vec![0x04, 64];
        tagged.extend_from_slice(&chip_id);
        tagged
    } else {
        // The vendor signs the hardware id with no ASN.1 tag.
        chip_id.to_vec()
    };
    vec![
        CustomExtension::from_oid_content(&[1, 3, 6, 1, 4, 1, 3704, 1, 1], der_integer(struct_version)),
        CustomExtension::from_oid_content(&[1, 3, 6, 1, 4, 1, 3704, 1, 2], der_ia5(PROCESSOR)),
        CustomExtension::from_oid_content(&[1, 3, 6, 1, 4, 1, 3704, 1, 3, 1], der_integer(tcb.boot_loader)),
        CustomExtension::from_oid_content(&[1, 3, 6, 1, 4, 1, 3704, 1, 3, 2], der_integer(tcb.tee)),
        CustomExtension::from_oid_content(&[1, 3, 6, 1, 4, 1, 3704, 1, 3, 3], der_integer(tcb.snp)),
        CustomExtension::from_oid_content(&[1, 3, 6, 1, 4, 1, 3704, 1, 3, 8], der_integer(tcb.microcode)),
        CustomExtension::from_oid_content(&[1, 3, 6, 1, 4, 1, 3704, 1, 4], hwid),
    ]
}

/// A CRL revoking `revoked_serials` in the vendor's usual form.
pub fn crl_revoking(chain: &TestChain, revoked_serials: &[SerialNumber]) -> String {
    crl_signed(chain, revoked_serials, (2024, 1, 1), (2049, 1, 1), None)
}

/// When a revocation list's `nextUpdate` falls: on a date, or a number of seconds from now.
pub enum CrlEnd {
    Date((i32, u8, u8)),
    SecondsFromNow(i64),
}

impl From<(i32, u8, u8)> for CrlEnd {
    fn from(date: (i32, u8, u8)) -> Self {
        Self::Date(date)
    }
}

/// A revocation list with the given window and signer, in PEM. `signer` keeps the fixture
/// root's issuer name while signing with another key, so only the signature differs.
pub fn crl_signed(
    chain: &TestChain,
    revoked_serials: &[SerialNumber],
    from: (i32, u8, u8),
    to: impl Into<CrlEnd>,
    signer: Option<&KeyPair>,
) -> String {
    use rcgen::{CertificateRevocationListParams, RevokedCertParams};
    let revoked = revoked_serials
        .iter()
        .map(|serial| RevokedCertParams {
            serial_number: serial.clone(),
            revocation_time: rcgen::date_time_ymd(2024, 1, 1),
            reason_code: None,
            invalidity_date: None,
        })
        .collect();
    let params = CertificateRevocationListParams {
        this_update: rcgen::date_time_ymd(from.0, from.1, from.2),
        next_update: match to.into() {
            CrlEnd::Date((year, month, day)) => rcgen::date_time_ymd(year, month, day),
            CrlEnd::SecondsFromNow(secs) => {
                let now = x509_parser::time::ASN1Time::now().timestamp();
                x509_parser::time::ASN1Time::from_timestamp(now + secs).unwrap().to_datetime()
            }
        },
        crl_number: SerialNumber::from(1),
        issuing_distribution_point: None,
        revoked_certs: revoked,
        key_identifier_method: chain.ark_params.key_identifier_method.clone(),
    };
    let signer = signer.unwrap_or(&chain.ark_key);
    let crl = params.signed_by(&Issuer::from_params(&chain.ark_params, signer)).unwrap();
    pem_wrap(crl.der().as_ref(), "X509 CRL")
}

pub fn ca_params(common_name: &str) -> CertificateParams {
    let mut params = CertificateParams::new(Vec::new()).unwrap();
    params.distinguished_name.push(DnType::CommonName, common_name);
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign, KeyUsagePurpose::DigitalSignature];
    params
}

/// A certificate as ASCII PEM, which the issuer-chain endpoint serves and `--root` takes; the
/// crate builds without a PEM writer, so the DER wraps here.
pub fn pem(cert: &Certificate) -> String {
    pem_wrap(cert.der().as_ref(), "CERTIFICATE")
}

/// DER as a PEM block with the given label.
pub fn pem_wrap(der: &[u8], label: &str) -> String {
    use base64::Engine as _;
    let encoded = base64::engine::general_purpose::STANDARD.encode(der);
    let body = encoded
        .as_bytes()
        .chunks(64)
        .map(|chunk| std::str::from_utf8(chunk).expect("base64 is ASCII"))
        .collect::<Vec<_>>()
        .join("\n");
    format!("-----BEGIN {label}-----\n{body}\n-----END {label}-----\n")
}

/// The fixture's enclave identity and configuration: what a real quote would bind.
pub struct Enclave {
    pub bundle: AttestationBundle,
    pub chip_id: [u8; 64],
}

pub fn enclave() -> Enclave {
    Enclave {
        bundle: AttestationBundle {
            enclave_address: [0x5e; 20],
            config_sha256: [0xc0; 32],
            tls_public_key_sha256: [0x71; 32],
        },
        chip_id: [0x9d; 64],
    }
}

/// A report the test VCEK signs over the fixture's report data and the given release fields,
/// with the firmware's wire layout: signature scalars little-endian in each field's first 48
/// bytes, and the TCB packed with boot_loader in byte 0, tee in byte 1, snp in byte 6, and
/// microcode in byte 7.
pub fn forged_report(chain: &TestChain, measurement: [u8; 48], report_data: [u8; 64], chip_id: [u8; 64]) -> Vec<u8> {
    let mut report = vec![0u8; REPORT_LEN];
    report[0x00..0x04].copy_from_slice(&2u32.to_le_bytes());
    report[0x08..0x10].copy_from_slice(&POLICY.to_le_bytes());
    report[0x34..0x38].copy_from_slice(&1u32.to_le_bytes());
    report[0x38..0x40].copy_from_slice(&REPORTED_TCB.to_packed().to_le_bytes());
    report[0x50..0x90].copy_from_slice(&report_data);
    report[0x90..0xC0].copy_from_slice(&measurement);
    report[0x180..0x188].copy_from_slice(&REPORTED_TCB.to_packed().to_le_bytes());
    report[0x1A0..0x1E0].copy_from_slice(&chip_id);
    let key = p384::ecdsa::SigningKey::from_pkcs8_der(&chain.vcek_key.serialize_der()).unwrap();
    let signature: p384::ecdsa::Signature = key.sign(&report[..0x2A0]);
    let signature = signature.to_bytes();
    for (i, byte) in signature[..48].iter().rev().enumerate() {
        report[0x2A0 + i] = *byte;
    }
    for (i, byte) in signature[48..].iter().rev().enumerate() {
        report[0x2E8 + i] = *byte;
    }
    report
}

/// The attestation JSON a node with this quote would serve for `bundle`.
pub fn attestation(bundle: &AttestationBundle, quote: &[u8]) -> Attestation {
    let report_data = bundle.report_data();
    Attestation {
        provider: "sev_guest".into(),
        enclave_address: format!("0x{}", hex::encode(bundle.enclave_address)),
        config_sha256: format!("0x{}", hex::encode(bundle.config_sha256)),
        tls_public_key_sha256: format!("0x{}", hex::encode(bundle.tls_public_key_sha256)),
        bundle: format!("0x{}", hex::encode(bundle.encode())),
        report_data: format!("0x{}", hex::encode(report_data)),
        quote: format!("0x{}", hex::encode(quote)),
    }
}

pub const LAUNCH_DIGEST: [u8; 48] = [0x1d; 48];

/// The release manifest the fixture verifies against, as published text: the minimum TCB sits
/// below the fixture's reported TCB in every component.
pub fn release_toml() -> String {
    format!(
        r#"
name = "test-release-v1"
launch_digest = "0x{}"
policy = "0x{:x}"

[minimum_tcb]
boot_loader = 3
tee = 0
snp = 8
microcode = 115

[config_hashes]
"node.toml" = "0x{}"
"#,
        hex::encode(LAUNCH_DIGEST),
        POLICY,
        hex::encode([0xc0; 32])
    )
}

pub fn release() -> ReleaseManifest {
    toml::from_str(&release_toml()).unwrap()
}

/// The standard fixtures every happy-path test shares.
pub fn fixtures() -> (TestChain, Enclave, Attestation) {
    let enclave = enclave();
    let chain = test_chain(enclave.chip_id, REPORTED_TCB);
    let attestation = attestation(
        &enclave.bundle,
        &forged_report(&chain, LAUNCH_DIGEST, enclave.bundle.report_data(), enclave.chip_id),
    );
    (chain, enclave, attestation)
}

/// The collateral a node would relay for `chain`, with `crl_pem` as its list.
pub fn collateral_of(chain: &TestChain, crl_pem: &str) -> RelayedCollateral {
    let mut blocks = x509_parser::pem::Pem::iter_from_buffer(crl_pem.as_bytes());
    RelayedCollateral {
        vcek: chain.vcek.der().to_vec(),
        intermediate: chain.ask.der().to_vec(),
        root: chain.ark.der().to_vec(),
        crl: blocks.next().expect("a PEM block").expect("well-formed PEM").contents,
        fetched_at: 1,
    }
}
