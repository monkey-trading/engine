//! The CPU models a release accepts. The vendor numbers each TCB component per CPU model, so a quote
//! is held to its own model's minimum only, and a model the release does not list is refused however
//! high its numbers. A quote's report, VCEK and chain must name its model alike.
//!
//! The Siena minimum is the one the launch release sets: TCB[SNP] 0x1D and TCB[MICROCODE] 0x1B, from
//! the vendor's bulletins for EPYC 8004. The Milan and Genoa quotes below carry TCB values that meet
//! those numbers while falling short of their own models' fixes (Milan B1's microcode fix is 0xDE,
//! Genoa B1's 0x56), so a single numeric floor across models admitted them.

use super::{now, verify};
use crate::fixtures::*;
use crate::{
    Attestation, CpuModel, Cpuid, MinimumTcb, RelayedCollateral, ReleaseManifest, SnpReport, Tcb, verify_quote,
};

/// EPYC 7003 B1, CPUID 00A00F11, certified under Milan.
const MILAN: Cpuid = Cpuid { family: 0x19, model: 0x01, stepping: 0x01 };
/// EPYC 9004 B1, CPUID 00A10F11, certified under Genoa.
const GENOA: Cpuid = Cpuid { family: 0x19, model: 0x11, stepping: 0x01 };

const SIENA_MINIMUM: MinimumTcb = MinimumTcb { boot_loader: 0, tee: 0, snp: 0x1D, microcode: 0x1B };
/// Genoa B1's floor for the same fixes.
const GENOA_MINIMUM: MinimumTcb = MinimumTcb { boot_loader: 0, tee: 0, snp: 0x1D, microcode: 0x56 };
/// A Milan chip at microcode 0xD5, below Milan's own fix, with every component at or above Siena's.
const MILAN_UNFIXED: Tcb = Tcb { boot_loader: 4, tee: 0, snp: 0x1D, microcode: 0xD5 };
/// A Genoa chip at microcode 0x55, one below Genoa's own fix, with every component at or above Siena's.
const GENOA_UNFIXED: Tcb = Tcb { boot_loader: 9, tee: 0, snp: 0x1D, microcode: 0x55 };
/// A Siena chip at both fixes.
const SIENA_FIXED: Tcb = Tcb { boot_loader: 9, tee: 0, snp: 0x1D, microcode: 0x1B };

#[test]
fn a_milan_quote_is_refused_by_a_release_set_for_siena() {
    let chain = line_chain("Milan");
    let (attestation, collateral) = quote_from(&chain, 3, MILAN, "Milan-B0", MILAN_UNFIXED);
    let err = verify(&attestation, &[release_for(&[siena()])], &collateral, now()).unwrap_err();
    assert_eq!(
        format!("{err:#}"),
        "the quote's CPU, family 0x19 model 0x01 stepping 0x01, is not a model the release accepts"
    );
}

#[test]
fn a_genoa_quote_is_refused_by_a_release_set_for_siena() {
    // Genoa and Siena share the vendor's chain, so neither the chain nor `--processor` tells them apart.
    let chain = line_chain("Genoa");
    let (attestation, collateral) = quote_from(&chain, 3, GENOA, "Genoa-B1", GENOA_UNFIXED);
    let err = verify(&attestation, &[release_for(&[siena()])], &collateral, now()).unwrap_err();
    assert_eq!(
        format!("{err:#}"),
        "the quote's CPU, family 0x19 model 0x11 stepping 0x01, is not a model the release accepts"
    );
}

#[test]
fn a_peer_quote_is_held_to_its_own_models_floor_never_another_models() {
    // What a holder checks before its keys move: the peer's chip must be a model its table lists, at
    // that model's floor.
    let chain = line_chain("Genoa");
    let (attestation, collateral) = quote_from(&chain, 3, GENOA, "Genoa-B1", GENOA_UNFIXED);
    let quote = hex::decode(attestation.quote.trim_start_matches("0x")).unwrap();
    let root = pem_wrap(&collateral.root, "CERTIFICATE");
    let err = verify_quote(&quote, &collateral, Some(&root), now(), &[siena()]).unwrap_err();
    assert!(err.to_string().ends_with("is not a model the release accepts"), "{err:#}");
    let err = verify_quote(&quote, &collateral, Some(&root), now(), &[siena(), genoa()]).unwrap_err();
    assert!(err.to_string().starts_with("the quote's reported TCB"), "{err:#}");
    let fixed = Tcb { microcode: 0x56, ..GENOA_UNFIXED };
    let (attestation, collateral) = quote_from(&chain, 3, GENOA, "Genoa-B1", fixed);
    let quote = hex::decode(attestation.quote.trim_start_matches("0x")).unwrap();
    let verified = verify_quote(&quote, &collateral, Some(&root), now(), &[siena(), genoa()]).unwrap();
    assert_eq!(verified.reported_tcb, fixed);
}

#[test]
fn a_release_minimum_refuses_its_own_models_unfixed_microcode() {
    let chain = line_chain("Genoa");
    let unfixed = Tcb { microcode: 0x1A, ..SIENA_FIXED };
    let (attestation, collateral) = quote_from(&chain, 3, CPUID, PRODUCT_NAME, unfixed);
    let err = verify(&attestation, &[release_for(&[siena()])], &collateral, now()).unwrap_err();
    assert!(err.to_string().starts_with("the quote's reported TCB"), "{err:#}");
    let (attestation, collateral) = quote_from(&chain, 3, CPUID, PRODUCT_NAME, SIENA_FIXED);
    let verified = verify(&attestation, &[release_for(&[siena()])], &collateral, now()).expect("a fixed Siena quote");
    assert_eq!(verified.product_name, PRODUCT_NAME);
}

#[test]
fn each_quote_meets_only_its_own_models_minimum_in_a_release_of_several() {
    // A Genoa quote between the two floors, and a Siena one at its own, against a release listing both.
    let chain = line_chain("Genoa");
    let release = release_for(&[siena(), genoa()]);
    let (attestation, collateral) = quote_from(&chain, 3, GENOA, "Genoa-B1", GENOA_UNFIXED);
    let err = verify(&attestation, std::slice::from_ref(&release), &collateral, now()).unwrap_err();
    assert_eq!(
        format!("{err:#}"),
        format!(
            "the quote's reported TCB {GENOA_UNFIXED:?} is below the release minimum {:?} for Genoa-B1",
            Tcb::from(GENOA_MINIMUM)
        )
    );
    let (attestation, collateral) = quote_from(&chain, 3, CPUID, PRODUCT_NAME, SIENA_FIXED);
    verify(&attestation, &[release], &collateral, now()).expect("Siena at its own floor");
}

#[test]
fn a_release_manifest_still_refuses_keys_outside_its_format() {
    for key in ["processor = \"Genoa\"", "product = \"Genoa\"", "minimum_tcb = 1"] {
        let text = release_toml().replacen("[[cpu_models]]", &format!("{key}\n\n[[cpu_models]]"), 1);
        let err = ReleaseManifest::from_toml(&text).unwrap_err();
        assert!(err.to_string().contains("unknown field"), "{key}: {err:#}");
    }
}

#[test]
fn a_vcek_naming_another_product_than_the_release_lists_is_refused() {
    // A Siena CPUID whose genuine VCEK, under the same line, names another stepping's product.
    let chain = line_chain("Genoa");
    let (attestation, collateral) = quote_from(&chain, 3, CPUID, "Genoa-B2", SIENA_FIXED);
    let err = verify(&attestation, &[release_for(&[siena()])], &collateral, now()).unwrap_err();
    assert_eq!(
        format!("{err:#}"),
        "the quote's VCEK names product Genoa-B2, not the Genoa-A2 the release lists for family 0x19 model 0xa0 \
         stepping 0x02"
    );
}

#[test]
fn a_vcek_whose_root_or_intermediate_names_another_line_is_refused() {
    // A Genoa product name under a chain named for Milan, then a root and intermediate that disagree.
    let enclave = enclave();
    let milan = test_chain_with(enclave.chip_id, REPORTED_TCB, Variants { line: Some("Milan"), ..Variants::default() });
    let (attestation, collateral) = quote_from(&milan, 3, CPUID, PRODUCT_NAME, SIENA_FIXED);
    let err = verify(&attestation, &[release_for(&[siena()])], &collateral, now()).unwrap_err();
    assert_eq!(format!("{err:#}"), "the VCEK names product Genoa-A2, but its chain's root is ARK-Milan, not ARK-Genoa");

    let mixed = line_chain("Genoa");
    let (ask, ask_params, ask_key) = intermediate_named(&mixed, "SEV-Milan");
    let mixed = TestChain { ask, ask_params, ask_key, ..mixed };
    let (attestation, collateral) = quote_from(&mixed, 3, CPUID, PRODUCT_NAME, SIENA_FIXED);
    let err = verify(&attestation, &[release_for(&[siena()])], &collateral, now()).unwrap_err();
    assert_eq!(
        format!("{err:#}"),
        "the VCEK names product Genoa-A2, but its chain's intermediate is SEV-Milan, not SEV-Genoa"
    );
}

#[test]
fn a_cpuid_outside_the_vcek_product_line_is_refused_before_any_release() {
    // A Milan CPUID in a report whose VCEK and chain are Genoa's: no table can make it a Genoa chip.
    let chain = line_chain("Genoa");
    let (attestation, collateral) = quote_from(&chain, 3, MILAN, PRODUCT_NAME, SIENA_FIXED);
    let message = "the quote's CPU, family 0x19 model 0x01 stepping 0x01, is not certified under Genoa, the product \
                   line of its VCEK";
    let err = verify(&attestation, &[release_for(&[siena()])], &collateral, now()).unwrap_err();
    assert_eq!(format!("{err:#}"), message);
    let quote = hex::decode(attestation.quote.trim_start_matches("0x")).unwrap();
    let root = pem_wrap(&collateral.root, "CERTIFICATE");
    let err = verify_quote(&quote, &collateral, Some(&root), now(), &[siena()]).unwrap_err();
    assert_eq!(format!("{err:#}"), message);
}

#[test]
fn a_version_2_report_is_refused_whatever_the_release_lists() {
    let chain = line_chain("Genoa");
    let (attestation, collateral) = quote_from(&chain, 2, CPUID, PRODUCT_NAME, SIENA_FIXED);
    let quote = hex::decode(attestation.quote.trim_start_matches("0x")).unwrap();
    assert_eq!(SnpReport::parse(&quote).unwrap().cpuid, None);
    let err = verify(&attestation, &[release_for(&[siena(), genoa()])], &collateral, now()).unwrap_err();
    assert!(err.to_string().starts_with("the quote is a version 2 report, which names no CPU model"), "{err:#}");
}

#[test]
fn an_empty_model_list_admits_no_quote() {
    let (chain, _, attestation) = fixtures();
    let collateral = collateral_of(&chain, &crl_revoking(&chain, &[]));
    let quote = hex::decode(attestation.quote.trim_start_matches("0x")).unwrap();
    let root = pem_wrap(&collateral.root, "CERTIFICATE");
    let err = verify_quote(&quote, &collateral, Some(&root), now(), &[]).unwrap_err();
    assert!(err.to_string().ends_with("is not a model the release accepts"), "{err:#}");
}

#[test]
fn the_relayed_collateral_must_be_for_the_named_processor() {
    let (chain, enclave, _) = fixtures();
    let collateral = collateral_of(&chain, &crl_revoking(&chain, &[]));
    let report = forged_report(&chain, LAUNCH_DIGEST, enclave.bundle.report_data(), enclave.chip_id);
    let report = SnpReport::parse(&report).unwrap();
    let at = x509_parser::time::ASN1Time::from_timestamp(now()).unwrap();
    collateral.check(&report, PROCESSOR, at).expect("the processor the VCEK names");
    let err = collateral.check(&report, "Milan", at).unwrap_err();
    assert_eq!(err.to_string(), "the VCEK is for product Genoa-A2, not the processor Milan names");
}

#[test]
fn a_node_checks_that_its_own_list_admits_its_own_chip() {
    let (chain, enclave, _) = fixtures();
    let collateral = collateral_of(&chain, &crl_revoking(&chain, &[]));
    let report = forged_report(&chain, LAUNCH_DIGEST, enclave.bundle.report_data(), enclave.chip_id);
    let report = SnpReport::parse(&report).unwrap();
    let at = x509_parser::time::ASN1Time::from_timestamp(now()).unwrap();
    collateral.check_cpu_model(&report, &[cpu_model()], at).expect("its own model at its floor");
    let err = collateral.check_cpu_model(&report, &[genoa()], at).unwrap_err();
    assert!(err.to_string().ends_with("is not a model the release accepts"), "{err:#}");
    let renamed = CpuModel { product_name: "Genoa-B2".into(), ..cpu_model() };
    let err = collateral.check_cpu_model(&report, &[renamed], at).unwrap_err();
    assert!(err.to_string().starts_with("the quote's VCEK names product Genoa-A2, not the Genoa-B2"), "{err:#}");
    let above =
        CpuModel { minimum_tcb: MinimumTcb { microcode: REPORTED_TCB.microcode + 1, ..SIENA_MINIMUM }, ..cpu_model() };
    let err = collateral.check_cpu_model(&report, &[above], at).unwrap_err();
    assert!(err.to_string().starts_with("the quote's reported TCB"), "{err:#}");
}

// ── Helpers ──

fn siena() -> CpuModel {
    CpuModel { minimum_tcb: SIENA_MINIMUM, ..cpu_model() }
}

fn genoa() -> CpuModel {
    CpuModel {
        product_name: "Genoa-B1".into(),
        family: GENOA.family,
        model: GENOA.model,
        stepping: GENOA.stepping,
        minimum_tcb: GENOA_MINIMUM,
    }
}

fn release_for(models: &[CpuModel]) -> ReleaseManifest {
    let release = ReleaseManifest { name: "release-set-for-siena".into(), cpu_models: models.to_vec(), ..release() };
    release.validate().expect("a valid manifest");
    release
}

/// A stand-in chain named for `line`.
fn line_chain(line: &str) -> TestChain {
    test_chain_with(enclave().chip_id, REPORTED_TCB, Variants { line: Some(line), ..Variants::default() })
}

/// The release's enclave on a chip of `cpuid` at `tcb`, under `chain`: a report of `version` signed by
/// a VCEK the chain issued for that chip and TCB under `product_name`, and the collateral for it.
fn quote_from(
    chain: &TestChain,
    version: u32,
    cpuid: Cpuid,
    product_name: &str,
    tcb: Tcb,
) -> (Attestation, RelayedCollateral) {
    let enclave = enclave();
    let (vcek, key) = vcek_issued(chain, enclave.chip_id, tcb, product_name);
    let mut report = forged_report_at(chain, version, LAUNCH_DIGEST, enclave.bundle.report_data(), enclave.chip_id);
    report[0x38..0x40].copy_from_slice(&tcb.to_packed().to_le_bytes());
    report[0x180..0x188].copy_from_slice(&tcb.to_packed().to_le_bytes());
    if version >= 3 {
        report[0x188..0x18B].copy_from_slice(&[cpuid.family, cpuid.model, cpuid.stepping]);
    }
    sign_report_with(&key, &mut report);
    let collateral = RelayedCollateral { vcek: vcek.der().to_vec(), ..collateral_of(chain, &crl_revoking(chain, &[])) };
    (attestation(&enclave.bundle, &report), collateral)
}
