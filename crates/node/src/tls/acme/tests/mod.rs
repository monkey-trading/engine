//! The certificate task's checks and schedule, without a CA: what it accepts from one, when it
//! renews, and what it alerts. `scripts/dev-acme.sh` runs it against a real ACME test CA.

use super::*;

use rcgen::{CertificateParams, DnType, IsCa, Issuer, KeyPair, date_time_ymd};
use sha2::{Digest, Sha256};
use x509_parser::certification_request::X509CertificationRequest;

use crate::tls::TlsIdentity;
use crate::tls::acme::schedule::EXPIRING_WITHIN;
use crate::trusted_time::tests::logs::Logs;

const DOMAIN: &str = "api.engine.test";
const DAY: Duration = Duration::from_secs(86_400);

#[test]
fn an_issued_certificate_names_the_domain_and_lasts_from_not_before_to_not_after() {
    let tls = TlsIdentity::generate().unwrap();
    let chain = certified(&tls.key, &[DOMAIN], (2026, 10, 1), (2026, 12, 30));
    let (lifetime, not_after_seconds) = check(&chain, DOMAIN).unwrap();
    assert_eq!(lifetime, 90 * DAY);
    assert_eq!(not_after_seconds, 1_798_588_800, "2026-12-30T00:00:00Z");
    // Names compare as DNS does, without case.
    check(&chain, "API.engine.test").unwrap();
    // Any of its names will do.
    check(&certified(&tls.key, &["www.engine.test", DOMAIN], (2026, 10, 1), (2026, 12, 30)), DOMAIN).unwrap();
}

#[test]
fn a_certificate_for_another_name_or_with_no_life_is_refused() {
    let tls = TlsIdentity::generate().unwrap();
    let cases: [(&str, Vec<CertificateDer<'static>>); 4] = [
        ("does not name api.engine.test", certified(&tls.key, &["other.engine.test"], (2026, 10, 1), (2026, 12, 30))),
        ("does not name api.engine.test", certified(&tls.key, &["engine.test"], (2026, 10, 1), (2026, 12, 30))),
        ("notAfter is not after its notBefore", certified(&tls.key, &[DOMAIN], (2026, 10, 1), (2026, 10, 1))),
        ("returned no certificate", Vec::new()),
    ];
    for (expected, chain) in cases {
        let err = format!("{:#}", check(&chain, DOMAIN).unwrap_err());
        assert!(err.contains(expected), "expected {expected:?}, got {err}");
    }
}

#[test]
fn only_a_certificate_for_the_attested_key_is_installed() {
    let tls = TlsIdentity::generate().unwrap();
    let other = TlsIdentity::generate().unwrap();
    let err = tls.key.install(certified(&other.key, &[DOMAIN], (2026, 10, 1), (2026, 12, 30))).unwrap_err();
    assert!(err.to_string().contains("not for the attested key"), "{err}");
    assert!(tls.key.install(Vec::new()).is_err());
    tls.key.install(certified(&tls.key, &[DOMAIN], (2026, 10, 1), (2026, 12, 30))).unwrap();
}

#[test]
fn the_certificate_request_is_for_the_attested_key_and_the_domain_alone() {
    let tls = TlsIdentity::generate().unwrap();
    let der = tls.key.certificate_request(DOMAIN).unwrap();
    let (_, request) = X509CertificationRequest::from_der(&der).unwrap();
    request.verify_signature().expect("signed by the key it carries");
    let key = request.certification_request_info.subject_pki.raw;
    assert_eq!(<[u8; 32]>::from(Sha256::digest(key)), tls.public_key_sha256, "the attested key");
    let names: Vec<String> = request
        .requested_extensions()
        .into_iter()
        .flatten()
        .filter_map(|extension| match extension {
            x509_parser::extensions::ParsedExtension::SubjectAlternativeName(names) => Some(names),
            _ => None,
        })
        .flat_map(|names| names.general_names.iter())
        .map(|name| match name {
            GeneralName::DNSName(name) => name.to_string(),
            other => panic!("unexpected name {other:?}"),
        })
        .collect();
    assert_eq!(names, [DOMAIN]);
}

#[tokio::test(start_paused = true)]
async fn renewal_waits_until_a_third_of_the_life_remains_unless_renew_before_says_otherwise() {
    let (logs, _guard) = Logs::capture();
    let now = Instant::now();
    let mut schedule = Schedule::start(DOMAIN.into(), None);
    assert_eq!(schedule.issued(90 * DAY, now, now), 60 * DAY);

    let mut schedule = Schedule::start(DOMAIN.into(), Some(30 * DAY));
    assert_eq!(schedule.issued(6 * DAY, now, now), 4 * DAY, "a renew_before as long as the life falls back to a third");
    assert_eq!(logs.count("is not shorter than the certificate's life"), 1);
    assert_eq!(schedule.issued(90 * DAY, now, now), 60 * DAY);
    assert_eq!(schedule.issued(30 * DAY + Duration::from_secs(1), now, now), Duration::from_secs(1));
    // Its life counts from its order's finalize request, however late it arrives.
    assert_eq!(schedule.issued(90 * DAY, now, now + 10 * DAY), 50 * DAY);
    assert_eq!(schedule.issued(90 * DAY, now, now + 70 * DAY), Duration::ZERO, "inside the window: renew at once");
}

#[tokio::test(start_paused = true)]
async fn the_api_is_pending_until_the_first_certificate_and_retries_with_backoff() {
    let (logs, _guard) = Logs::capture();
    let now = Instant::now();
    let mut schedule = Schedule::start(DOMAIN.into(), None);
    assert_eq!(logs.warnings("certificate_pending"), 1, "raised before the first attempt");

    let retries: Vec<u64> = (0..8).map(|_| schedule.failed(&anyhow!("refused"), now).as_secs()).collect();
    assert_eq!(retries, [60, 120, 240, 480, 960, 1920, 3600, 3600]);
    assert_eq!(logs.warnings("certificate_pending"), 9);
    assert_eq!(logs.warnings("certificate_expiring"), 0);
    assert!(logs.count("domain=api.engine.test") >= 9, "the alert names the domain");

    schedule.issued(90 * DAY, now, now);
    assert_eq!(logs.count("alert_resolved=\"certificate_pending\""), 1);
    schedule.issued(90 * DAY, now, now);
    assert_eq!(logs.count("alert_resolved=\"certificate_pending\""), 1, "resolved by the first certificate only");
    assert_eq!(schedule.failed(&anyhow!("refused"), now).as_secs(), 60, "a certificate resets the backoff");
}

#[tokio::test(start_paused = true)]
async fn a_failed_renewal_alerts_once_fewer_than_14_days_remain() {
    let (logs, _guard) = Logs::capture();
    let arrived = Instant::now();
    let mut schedule = Schedule::start(DOMAIN.into(), None);
    schedule.issued(90 * DAY, arrived, arrived);

    // 76 days in, exactly 14 days remain; a second later, 14 days less a second.
    let fourteen_left = arrived + 90 * DAY - EXPIRING_WITHIN;
    schedule.failed(&anyhow!("refused"), fourteen_left);
    assert_eq!(logs.warnings("certificate_expiring"), 0, "14 days remain");
    assert_eq!(logs.count("certificate renewal failed"), 1);
    schedule.failed(&anyhow!("refused"), fourteen_left + Duration::from_secs(1));
    assert_eq!(logs.warnings("certificate_expiring"), 1);
    // Past expiry too; the served certificate is still the only one.
    schedule.failed(&anyhow!("refused"), arrived + 100 * DAY);
    assert_eq!(logs.warnings("certificate_expiring"), 2);
    assert_eq!(logs.warnings("certificate_pending"), 1, "only the start's");

    schedule.issued(90 * DAY, arrived + 100 * DAY, arrived + 100 * DAY);
    assert_eq!(logs.count("alert_resolved=\"certificate_expiring\""), 2, "every issuance ends the episode");
}

mod fake_ca;
mod http;
mod orders;

// ── Helpers ──

/// A chain for `key` naming `names`, valid between two dates, issued by a fresh CA.
fn certified(key: &TlsKey, names: &[&str], from: (i32, u8, u8), to: (i32, u8, u8)) -> Vec<CertificateDer<'static>> {
    let ca_key = KeyPair::generate().unwrap();
    let mut ca = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca.distinguished_name.push(DnType::CommonName, "test CA");
    ca.is_ca = IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca_cert = ca.self_signed(&ca_key).unwrap();
    let mut leaf = CertificateParams::new(names.iter().map(|name| name.to_string()).collect::<Vec<_>>()).unwrap();
    leaf.not_before = date_time_ymd(from.0, from.1, from.2);
    leaf.not_after = date_time_ymd(to.0, to.1, to.2);
    let leaf = leaf.signed_by(key.key_pair(), &Issuer::new(ca, &ca_key)).unwrap();
    vec![leaf.der().clone(), ca_cert.der().clone()]
}
