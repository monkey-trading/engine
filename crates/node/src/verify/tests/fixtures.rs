//! Test fixtures: a stand-in vendor chain with the real shapes, a forged report on the
//! firmware's wire layout, the release manifest, and the key-distribution service stand-ins.

use rcgen::{
    BasicConstraints, Certificate, CertificateParams, CustomExtension, DnType, IsCa, Issuer, KeyPair, KeyUsagePurpose,
    SerialNumber,
};

use super::super::ReleaseManifest;
use super::super::report::{REPORT_LEN, Tcb};
use crate::tee::{Attestation, AttestationBundle};

/// A stand-in vendor chain with the real shape: a self-signed root, an intermediate, and the
/// chip key that signs reports, plus the vendor's extension OIDs binding the VCEK to `chip_id`
/// and `tcb`.
pub(super) struct TestChain {
    pub(super) ark: Certificate,
    pub(super) ask: Certificate,
    pub(super) vcek: Certificate,
    pub(super) vcek_key: KeyPair,
    pub(super) ark_params: CertificateParams,
    pub(super) ark_key: KeyPair,
}

/// The VCEK's serial in every fixture chain, so a revocation list can name it.
pub(super) const VCEK_SERIAL: u64 = 0xA11CE;

pub(super) const PROCESSOR: &str = "TestProc";
pub(super) const POLICY: u64 = 0x30000;
pub(super) const REPORTED_TCB: Tcb = Tcb { boot_loader: 4, tee: 0, snp: 8, microcode: 205 };

/// A DER INTEGER extension value, the encoding the vendor signs: values with the high bit set
/// take a leading zero so the INTEGER stays positive.
pub(super) fn der_integer(value: u8) -> Vec<u8> {
    if value < 0x80 { vec![0x02, 0x01, value] } else { vec![0x02, 0x02, 0x00, value] }
}

/// A DER IA5String extension value.
pub(super) fn der_ia5(text: &str) -> Vec<u8> {
    let mut value = vec![0x16, text.len() as u8];
    value.extend_from_slice(text.as_bytes());
    value
}

pub(super) fn test_chain(chip_id: [u8; 64], tcb: Tcb) -> TestChain {
    test_chain_with(chip_id, tcb, Variants::default())
}

/// What differs from the vendor-shaped chain in a negative test: nothing by default.
#[derive(Default)]
pub(super) struct Variants<'a> {
    pub(super) struct_version: u8,
    /// Signs the VCEK with this key while keeping the intermediate's issuer name, so only the
    /// signature differs; likewise for the intermediate and the root's name.
    pub(super) vcek_signer: Option<&'a KeyPair>,
    pub(super) ask_signer: Option<&'a KeyPair>,
    /// Wraps the hardware id extension in an OCTET STRING, the tagged form the vendor may adopt.
    pub(super) tagged_hwid: bool,
}

/// A chain with the given variations from the vendor shape, for the negative tests.
pub(super) fn test_chain_with(chip_id: [u8; 64], tcb: Tcb, variants: Variants<'_>) -> TestChain {
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
    vcek_params.distinguished_name.push(DnType::CommonName, "VCEK-Test");
    vcek_params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    vcek_params.custom_extensions = vcek_extensions(chip_id, tcb, variants.struct_version, variants.tagged_hwid);
    let vcek_signer = variants.vcek_signer.unwrap_or(&ask_key);
    let vcek = vcek_params.signed_by(&vcek_key, &Issuer::from_params(&ask_params, vcek_signer)).unwrap();
    TestChain { ark, ask, vcek, vcek_key, ark_params, ark_key }
}

/// The vendor's VCEK extension set for `chip_id` and `tcb`.
pub(super) fn vcek_extensions(
    chip_id: [u8; 64],
    tcb: Tcb,
    struct_version: u8,
    tagged_hwid: bool,
) -> Vec<CustomExtension> {
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

/// A revocation list issued by the fixture root, revoking `revoked_serials`, in the PEM the
/// vendor serves it as.
pub(super) fn crl(chain: &TestChain, revoked_serials: &[u64]) -> String {
    use rcgen::{CertificateRevocationListParams, RevokedCertParams};
    let revoked = revoked_serials
        .iter()
        .map(|serial| RevokedCertParams {
            serial_number: SerialNumber::from(*serial),
            revocation_time: rcgen::date_time_ymd(2024, 1, 1),
            reason_code: None,
            invalidity_date: None,
        })
        .collect();
    let params = CertificateRevocationListParams {
        this_update: rcgen::date_time_ymd(2024, 1, 1),
        next_update: rcgen::date_time_ymd(2049, 1, 1),
        crl_number: SerialNumber::from(1),
        issuing_distribution_point: None,
        revoked_certs: revoked,
        key_identifier_method: chain.ark_params.key_identifier_method.clone(),
    };
    let crl = params.signed_by(&Issuer::from_params(&chain.ark_params, &chain.ark_key)).unwrap();
    pem_wrap(crl.der().as_ref(), "X509 CRL")
}

pub(super) fn ca_params(common_name: &str) -> CertificateParams {
    let mut params = CertificateParams::new(Vec::new()).unwrap();
    params.distinguished_name.push(DnType::CommonName, common_name);
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign, KeyUsagePurpose::DigitalSignature];
    params
}

/// A certificate as ASCII PEM, which the issuer-chain endpoint serves and `--root` takes; the
/// crate builds without a PEM writer, so the DER wraps here.
pub(super) fn pem(cert: &Certificate) -> String {
    pem_wrap(cert.der().as_ref(), "CERTIFICATE")
}

/// DER as a PEM block with the given label.
pub(super) fn pem_wrap(der: &[u8], label: &str) -> String {
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
pub(super) struct Enclave {
    pub(super) bundle: AttestationBundle,
    pub(super) chip_id: [u8; 64],
}

pub(super) fn enclave() -> Enclave {
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
pub(super) fn forged_report(
    chain: &TestChain,
    measurement: [u8; 48],
    report_data: [u8; 64],
    chip_id: [u8; 64],
) -> Vec<u8> {
    let mut report = vec![0u8; REPORT_LEN];
    report[0x00..0x04].copy_from_slice(&2u32.to_le_bytes());
    report[0x08..0x10].copy_from_slice(&POLICY.to_le_bytes());
    report[0x34..0x38].copy_from_slice(&1u32.to_le_bytes());
    report[0x38..0x40].copy_from_slice(&REPORTED_TCB.to_packed().to_le_bytes());
    report[0x50..0x90].copy_from_slice(&report_data);
    report[0x90..0xC0].copy_from_slice(&measurement);
    report[0x180..0x188].copy_from_slice(&REPORTED_TCB.to_packed().to_le_bytes());
    report[0x1A0..0x1E0].copy_from_slice(&chip_id);
    let rng = ring::rand::SystemRandom::new();
    let key = ring::signature::EcdsaKeyPair::from_pkcs8(
        &ring::signature::ECDSA_P384_SHA384_FIXED_SIGNING,
        &chain.vcek_key.serialize_der(),
        &rng,
    )
    .unwrap();
    let signature = key.sign(&rng, &report[..0x2A0]).unwrap();
    let signature = signature.as_ref();
    for (i, byte) in signature[..48].iter().rev().enumerate() {
        report[0x2A0 + i] = *byte;
    }
    for (i, byte) in signature[48..].iter().rev().enumerate() {
        report[0x2E8 + i] = *byte;
    }
    report
}

/// The attestation JSON a node with this quote would serve for `bundle`.
pub(super) fn attestation(bundle: &AttestationBundle, quote: &[u8]) -> Attestation {
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

pub(super) const LAUNCH_DIGEST: [u8; 48] = [0x1d; 48];

/// The release manifest the fixture verifies against, as published text: the minimum TCB sits
/// below the fixture's reported TCB in every component.
pub(super) fn release_toml() -> String {
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

pub(super) fn release() -> ReleaseManifest {
    toml::from_str(&release_toml()).unwrap()
}

/// The standard fixtures every happy-path test shares.
pub(super) fn fixtures() -> (TestChain, Enclave, Attestation) {
    let enclave = enclave();
    let chain = test_chain(enclave.chip_id, REPORTED_TCB);
    let attestation = attestation(
        &enclave.bundle,
        &forged_report(&chain, LAUNCH_DIGEST, enclave.bundle.report_data(), enclave.chip_id),
    );
    (chain, enclave, attestation)
}
