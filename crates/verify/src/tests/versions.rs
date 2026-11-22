//! Report versions 2 to 6 on the stand-in chain: each from 3 verifies against the release with the
//! fields its version adds read as the firmware wrote them, and a report of version 2, which names no
//! CPU model, of an unknown version, from another CPU family, or signed with a key other than the
//! VCEK is refused with its own message even when the VCEK signed it.

use super::{now, verify};
use crate::fixtures::*;
use crate::{MitigationVectors, SnpReport, verify_quote};

#[test]
fn reports_of_versions_5_and_6_verify_and_read_the_cpuid_and_mitigation_vectors() {
    let enclave = enclave();
    let chain = test_chain(enclave.chip_id, REPORTED_TCB);
    let collateral = collateral_of(&chain, &crl_revoking(&chain, &[]));
    for version in [5, 6] {
        let quote = forged_report_at(&chain, version, LAUNCH_DIGEST, enclave.bundle.report_data(), enclave.chip_id);
        let verified = verify(&attestation(&enclave.bundle, &quote), &[release()], &collateral, now())
            .unwrap_or_else(|err| panic!("version {version}: {err:#}"));
        assert_eq!((verified.release_name.as_str(), verified.config_name.as_str()), ("test-release-v1", "node.toml"));

        let parsed = SnpReport::parse(&quote).unwrap();
        assert_eq!(parsed.version, version);
        assert_eq!(parsed.cpuid, Some(CPUID));
        assert_eq!(parsed.mitigation_vectors, Some(MitigationVectors { launch: 0x1, current: 0x3 }));
        let root = pem_wrap(&collateral.root, "CERTIFICATE");
        let quoted = verify_quote(&quote, &collateral, Some(&root), now(), &[cpu_model()]).unwrap();
        assert_eq!(
            (quoted.measurement, quoted.policy, quoted.reported_tcb, quoted.report_data),
            (LAUNCH_DIGEST, POLICY, REPORTED_TCB, enclave.bundle.report_data()),
            "version {version}"
        );
        assert_eq!(parsed.committed_tcb, COMMITTED_TCB.to_packed());
    }
}

#[test]
fn every_version_from_3_to_6_verifies_and_version_2_which_names_no_cpu_is_refused() {
    let enclave = enclave();
    let chain = test_chain(enclave.chip_id, REPORTED_TCB);
    let collateral = collateral_of(&chain, &crl_revoking(&chain, &[]));
    let root = pem_wrap(&collateral.root, "CERTIFICATE");
    let refusal = "the quote is a version 2 report, which names no CPU model: a release accepts reports of version 3 \
                   or later";
    for version in 2..=6 {
        let quote = forged_report_at(&chain, version, LAUNCH_DIGEST, enclave.bundle.report_data(), enclave.chip_id);
        let verified = verify(&attestation(&enclave.bundle, &quote), &[release()], &collateral, now());
        let quoted = verify_quote(&quote, &collateral, Some(&root), now(), &[cpu_model()]);
        if version == 2 {
            assert_eq!(format!("{:#}", verified.unwrap_err()), refusal);
            assert_eq!(format!("{:#}", quoted.unwrap_err()), refusal);
        } else {
            verified.unwrap_or_else(|err| panic!("version {version}: {err:#}"));
            quoted.unwrap_or_else(|err| panic!("version {version}: {err:#}"));
        }
    }
}

#[test]
fn a_vcek_signed_report_of_another_version_family_or_signing_key_is_refused() {
    let enclave = enclave();
    let chain = test_chain(enclave.chip_id, REPORTED_TCB);
    let collateral = collateral_of(&chain, &crl_revoking(&chain, &[]));
    let not_a_report = "the quote is not an SEV-SNP attestation report; a mock quote never verifies";
    for (what, edit, message) in [
        (
            "version 7",
            (|report: &mut Vec<u8>| report[0x00..0x04].copy_from_slice(&7u32.to_le_bytes())) as fn(&mut Vec<u8>),
            format!("{not_a_report}: unsupported attestation report version 7, want 2 to 6"),
        ),
        (
            "version 1",
            |report| report[0x00..0x04].copy_from_slice(&1u32.to_le_bytes()),
            format!("{not_a_report}: unsupported attestation report version 1, want 2 to 6"),
        ),
        (
            "a Turin part, family 1Ah",
            |report| report[0x188] = 0x1A,
            format!(
                "{not_a_report}: the report is from a CPU of family 0x1a; only family 0x19's TCB layout \
                 (Milan, Genoa, Bergamo, Siena) is implemented"
            ),
        ),
        (
            "a VLEK",
            |report| report[0x48] = 0b0_0100,
            "the report is signed with a VLEK, not the VCEK the key-distribution service issues".to_owned(),
        ),
        (
            "the chip-secret VCEK",
            |report| report[0x48] = 0b0_1000,
            "the report is signed with the chip-secret VCEK, not the VCEK the key-distribution service issues"
                .to_owned(),
        ),
    ] {
        let mut quote = forged_report_at(&chain, 6, LAUNCH_DIGEST, enclave.bundle.report_data(), enclave.chip_id);
        edit(&mut quote);
        sign_report(&chain, &mut quote);
        let err = verify(&attestation(&enclave.bundle, &quote), &[release()], &collateral, now()).unwrap_err();
        assert_eq!(format!("{err:#}"), message, "{what}");
    }
}
