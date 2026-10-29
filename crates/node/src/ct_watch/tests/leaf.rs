use super::*;

use crate::ct_watch::leaf::{covers, parse_leaf, suspect_unparsed};

fn names(list: &[&str]) -> Vec<String> {
    list.iter().map(|n| (*n).to_owned()).collect()
}

#[test]
fn an_x509_entry_yields_its_names_key_hash_issuer_and_log_time() {
    let cert = cert(&["API.Example.com.", "www.example.com"], "Test Issuer");
    let parsed = parse_leaf(&x509_leaf(1_700_000_000_123, &cert.der)).unwrap();
    assert_eq!(parsed.names[..2], ["api.example.com", "www.example.com"]);
    assert_eq!(parsed.key_sha256, cert.key_sha256);
    assert!(parsed.issuer.contains("Test Issuer"), "{}", parsed.issuer);
    assert_eq!(parsed.logged_ms, 1_700_000_000_123);
    assert!(parsed.not_after_ms > 1_700_000_000_000, "{}", parsed.not_after_ms);
}

#[test]
fn a_precertificate_entry_yields_the_same_certificate_facts() {
    let cert = cert(&["api.example.com"], "Test Issuer");
    let precert = parse_leaf(&precert_leaf(5, &cert.der)).unwrap();
    let final_cert = parse_leaf(&x509_leaf(5, &cert.der)).unwrap();
    assert_eq!(precert, final_cert);
    assert_eq!(precert.key_sha256, cert.key_sha256);
}

#[test]
fn the_subject_common_name_counts_as_a_name() {
    let cert = cert(&[], "api.example.com");
    let parsed = parse_leaf(&x509_leaf(1, &cert.der)).unwrap();
    assert!(covers(&parsed.names, "api.example.com"), "{:?}", parsed.names);
}

#[test]
fn a_malformed_leaf_is_refused_rather_than_skipped() {
    let cert = cert(&["api.example.com"], "x");
    let good = x509_leaf(1, &cert.der);
    assert!(parse_leaf(&good).is_ok());
    // Every truncation, and a leaf with a byte after its end.
    for cut in [0, 1, 9, 11, 14, good.len() - 3, good.len() - 1] {
        assert!(parse_leaf(&good[..cut]).is_err(), "cut at {cut}");
    }
    assert!(parse_leaf(&[good.clone(), vec![0]].concat()).is_err());
    // Unknown version, leaf type and entry type.
    for (index, value) in [(0, 1), (1, 1), (11, 2)] {
        let mut bad = good.clone();
        bad[index] = value;
        assert!(parse_leaf(&bad).is_err(), "byte {index} = {value}");
    }
    // A certificate that is not DER, in a well-formed leaf.
    assert!(parse_leaf(&x509_leaf(1, b"not a certificate")).is_err());
    // A certificate followed by a byte inside its length.
    assert!(parse_leaf(&x509_leaf(1, &[cert.der.clone(), vec![0]].concat())).is_err());
}

#[test]
fn a_name_covers_the_domain_only_exactly_or_by_a_wildcard_for_its_parent() {
    let domain = "api.example.com";
    assert!(covers(&names(&["api.example.com"]), domain));
    assert!(covers(&names(&["other.example.com", "*.example.com"]), domain));
    for not_covering in [
        "*.api.example.com",
        "example.com",
        "*.com",
        "*.other.com",
        "apiexample.com",
        "x.api.example.com",
        "*example.com",
        "api.example.com.evil.test",
    ] {
        assert!(!covers(&names(&[not_covering]), domain), "{not_covering}");
    }
    assert!(!covers(&names(&["*.example.com"]), "example.com"), "a wildcard does not cover its parent");
    assert!(!covers(&[], domain));
}

#[test]
fn an_entry_that_does_not_parse_is_suspect_only_when_it_mentions_the_domains_parent() {
    let cert = cert(&["api.example.com"], "x");
    let mut broken = x509_leaf(1, &cert.der);
    broken.truncate(broken.len() - 5);
    assert!(parse_leaf(&broken).is_err());
    let suspect = suspect_unparsed(&broken, "api.example.com", 77).unwrap();
    assert!(covers(&suspect.names, "api.example.com"));
    assert_eq!(suspect.logged_ms, 77);
    assert!(suspect.not_after_ms > 77 && suspect.not_after_ms < u64::MAX, "a suspect entry does not page forever");
    assert_ne!(suspect.key_sha256, [0; 32]);
    assert!(suspect_unparsed(b"garbage", "api.example.com", 77).is_none());
    // A wildcard covers the domain from its parent, so the parent alone is suspect.
    assert!(suspect_unparsed(b"\x30\x00*.EXAMPLE.com", "api.example.com", 77).is_some());
    assert!(suspect_unparsed(b"example.org", "api.example.com", 77).is_none());
    // A sibling name, or the parent alone (an OCSP URL, say), does not cover the domain.
    assert!(suspect_unparsed(b"www.example.com http://ocsp.example.com", "api.example.com", 77).is_none());
    // An apex domain has no parent to make a wildcard from: only its own name counts, never the TLD.
    assert!(suspect_unparsed(b"http://ocsp.digicert.com", "example.com", 77).is_none());
    assert!(suspect_unparsed(b"x example.com x", "example.com", 77).is_some());
}

#[test]
fn the_issuer_is_bounded_to_48_ascii_bytes() {
    let long = "A very long issuer organisation name, Inc. ".repeat(10);
    let cert = cert(&["api.example.com"], &long);
    let parsed = parse_leaf(&x509_leaf(1, &cert.der)).unwrap();
    assert!(parsed.issuer.chars().count() <= 48, "{}", parsed.issuer);
}

#[test]
fn a_non_ascii_issuer_is_reduced_to_ascii_within_the_byte_bound() {
    let cert = cert(&["api.example.com"], &"Émetteur 発行者 ".repeat(20));
    let parsed = parse_leaf(&x509_leaf(1, &cert.der)).unwrap();
    assert!(parsed.issuer.is_ascii() && parsed.issuer.len() <= 48, "{} bytes: {}", parsed.issuer.len(), parsed.issuer);
}
