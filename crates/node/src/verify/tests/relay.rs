//! The collateral relay: the node refuses collateral that would not verify for its own quote,
//! serves the last good copy, and a verifier takes the relayed bytes only after its own checks.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use rcgen::SerialNumber;
use tokio::time::timeout;

use super::super::chain::Collateral;
use super::super::refresh::{Relay, is_stale, next_wait};
use super::super::relay::RelayedCollateral;
use super::super::report::{SnpReport, Tcb};
use super::super::{Options, load_relayed_collateral, verify_attestation};
use super::fixtures::*;
use super::stub::{kds_stub, kds_stub_swappable, kds_stub_switched};
use crate::config::TeeSection;
use crate::test_support::capture_alerts;

/// The collateral a node would hold for `chain`, with `crl_pem` as its list.
pub(super) fn collateral_of(chain: &TestChain, crl_pem: &str) -> RelayedCollateral {
    let mut blocks = x509_parser::pem::Pem::iter_from_buffer(crl_pem.as_bytes());
    RelayedCollateral {
        vcek: chain.vcek.der().to_vec(),
        intermediate: chain.ask.der().to_vec(),
        root: chain.ark.der().to_vec(),
        crl: blocks.next().unwrap().unwrap().contents,
        fetched_at: 1,
    }
}

fn quote_of(attestation: &crate::tee::Attestation) -> Vec<u8> {
    hex::decode(attestation.quote.trim_start_matches("0x")).unwrap()
}

pub(super) fn tee(base: &str, refresh: &str) -> TeeSection {
    toml::from_str(&format!(
        "provider = 'mock'\nprocessor = '{PROCESSOR}'\nkds_base = '{base}'\ncollateral_refresh = '{refresh}'"
    ))
    .unwrap()
}

#[tokio::test]
async fn fetched_collateral_checks_and_round_trips_through_the_served_json() {
    let (chain, _enclave, attestation) = fixtures();
    let stub = kds_stub(&chain).await;
    let quote = quote_of(&attestation);
    let report = SnpReport::parse(&quote).unwrap();

    let fetched = RelayedCollateral::fetch(&Collateral::new(&stub.base, PROCESSOR).unwrap(), &report).await.unwrap();
    fetched.check(&report).expect("collateral for the node's own quote checks");
    assert_eq!(RelayedCollateral::from_json(&fetched.to_json()).unwrap(), fetched);
    assert!(fetched.crl_next_update().unwrap().is_some());
}

#[test]
fn the_node_refuses_collateral_that_would_not_verify_for_its_quote() {
    let (chain, enclave, attestation) = fixtures();
    let quote = quote_of(&attestation);
    let report = SnpReport::parse(&quote).unwrap();
    let good = collateral_of(&chain, &crl_revoking(&chain, &[]));
    good.check(&report).unwrap();

    // A VCEK the vendor's intermediate signed for another chip or TCB, and one from another chain.
    let mut other_chip = enclave.chip_id;
    other_chip[0] ^= 1;
    let other_chip_vcek =
        RelayedCollateral { vcek: vcek_for(&chain, other_chip, REPORTED_TCB).der().to_vec(), ..good.clone() };
    assert!(other_chip_vcek.check(&report).is_err(), "served a VCEK for another chip");
    let other_tcb = Tcb { microcode: REPORTED_TCB.microcode + 1, ..REPORTED_TCB };
    let other_tcb_vcek =
        RelayedCollateral { vcek: vcek_for(&chain, enclave.chip_id, other_tcb).der().to_vec(), ..good.clone() };
    assert!(other_tcb_vcek.check(&report).is_err(), "served a VCEK for another TCB");
    let other_chain = test_chain(enclave.chip_id, REPORTED_TCB);
    let other_chain_vcek = RelayedCollateral { vcek: other_chain.vcek.der().to_vec(), ..good.clone() };
    assert!(other_chain_vcek.check(&report).is_err(), "served a VCEK from another chain");

    // A list that revokes the intermediate, an expired list, and a list another key signed.
    let ask_serial = SerialNumber::from_slice(&x509_serial(chain.ask.der()));
    let revoked = collateral_of(&chain, &crl_revoking(&chain, &[ask_serial]));
    let err = revoked.check(&report).expect_err("a list revoking the intermediate does not pass");
    assert!(RelayedCollateral::is_revoked(&err), "{err:#}");
    let expired = collateral_of(&chain, &crl_signed(&chain, &[], (2024, 1, 1), (2025, 1, 1), None));
    assert!(expired.check(&report).is_err(), "served an expired list");
    let other_key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P384_SHA384).unwrap();
    let forged = collateral_of(&chain, &crl_signed(&chain, &[], (2024, 1, 1), (2049, 1, 1), Some(&other_key)));
    assert!(forged.check(&report).is_err(), "served a list the root did not sign");

    // A revocation outlasts its list: an authentic list naming the intermediate is a revocation
    // whatever its dates, so a skewed clock cannot turn it into an ordinary refusal.
    let ask_serial = SerialNumber::from_slice(&x509_serial(chain.ask.der()));
    let old_revoking =
        collateral_of(&chain, &crl_signed(&chain, std::slice::from_ref(&ask_serial), (2024, 1, 1), (2025, 1, 1), None));
    assert!(RelayedCollateral::is_revoked(&old_revoking.check(&report).unwrap_err()), "an expired revoking list");
    // Anything else wrong with the collateral is not an authentic revocation and is still dropped:
    // a list the root did not sign, a VCEK for another chip beside a revoking list.
    let forged_revoking = collateral_of(
        &chain,
        &crl_signed(&chain, std::slice::from_ref(&ask_serial), (2024, 1, 1), (2049, 1, 1), Some(&other_key)),
    );
    assert!(!RelayedCollateral::is_revoked(&forged_revoking.check(&report).unwrap_err()), "a forged revocation");
    let foreign_revoking = RelayedCollateral { vcek: other_chip_vcek.vcek.clone(), ..revoked.clone() };
    assert!(!RelayedCollateral::is_revoked(&foreign_revoking.check(&report).unwrap_err()), "a foreign VCEK");

    // A chain whose intermediate does not link to its root, and garbage in place of a certificate.
    let unlinked = RelayedCollateral { intermediate: chain.vcek.der().to_vec(), ..good.clone() };
    assert!(unlinked.check(&report).is_err(), "served an unlinked chain");
    let garbage = RelayedCollateral { root: vec![1, 2, 3], ..good };
    assert!(garbage.check(&report).is_err(), "served a malformed certificate");
}

#[tokio::test]
async fn a_list_that_revokes_the_intermediate_replaces_the_served_one_and_pages_critical() {
    let (chain, _enclave, attestation) = fixtures();
    // The vendor first publishes a list that revokes nothing, then one that revokes the intermediate.
    let clean = crl_revoking(&chain, &[]);
    let ask_serial = SerialNumber::from_slice(&x509_serial(chain.ask.der()));
    let cell = Arc::new(std::sync::Mutex::new(clean.clone()));
    let stub = kds_stub_swappable(&chain, &cell).await;
    let (_guard, mut alerts) = capture_alerts();
    let relay = Relay::new(&tee(&stub.base, "20ms"), &attestation).unwrap().unwrap();
    let mut served = relay.served();
    let task = tokio::spawn(relay.run());
    let first =
        timeout(Duration::from_secs(20), served.wait_for(Option::is_some)).await.unwrap().unwrap().clone().unwrap();
    assert_eq!(RelayedCollateral::from_json(&first).unwrap().crl, collateral_of(&chain, &clean).crl);

    let revoking = crl_revoking(&chain, &[ask_serial]);
    *cell.lock().unwrap() = revoking.clone();
    let second = timeout(Duration::from_secs(20), served.wait_for(|doc| doc.as_ref() != Some(&first)))
        .await
        .unwrap()
        .unwrap()
        .clone()
        .unwrap();
    let relayed = RelayedCollateral::from_json(&second).unwrap();
    assert_eq!(relayed.crl, collateral_of(&chain, &revoking).crl, "the revoking list is what verifiers get");
    let alert = timeout(Duration::from_secs(20), async {
        loop {
            let alert = alerts.recv().await.unwrap();
            if alert["alert"] == "collateral_revoked" {
                break alert;
            }
        }
    })
    .await
    .expect("no collateral_revoked page");
    assert_eq!(alert["level"], "ERROR", "revocation is a Critical page");

    // A verifier given what the node serves refuses the chain.
    let ark = pem(&chain.ark);
    assert!(verify_relayed(&attestation, &relayed, Some(&ark), "http://127.0.0.1:1").await.is_err());
    task.abort();
}

#[test]
fn the_served_document_bounds_every_item_and_refuses_malformed_json() {
    let (chain, _enclave, _attestation) = fixtures();
    let good = collateral_of(&chain, &crl_revoking(&chain, &[]));
    let json = |edit: &dyn Fn(&mut serde_json::Value)| {
        let mut value: serde_json::Value = serde_json::from_slice(&good.to_json()).unwrap();
        edit(&mut value);
        serde_json::to_vec(&value).unwrap()
    };

    assert_eq!(RelayedCollateral::from_json(&good.to_json()).unwrap(), good);
    let oversized_cert = format!("0x{}", "ab".repeat(16 * 1024 + 1));
    assert!(RelayedCollateral::from_json(&json(&|v| v["vcek"] = oversized_cert.clone().into())).is_err());
    let oversized_crl = format!("0x{}", "ab".repeat(256 * 1024 + 1));
    assert!(RelayedCollateral::from_json(&json(&|v| v["crl"] = oversized_crl.clone().into())).is_err());
    assert!(RelayedCollateral::from_json(&json(&|v| v["root"] = "0x".into())).is_err(), "an empty item");
    assert!(RelayedCollateral::from_json(&json(&|v| v["root"] = "0xzz".into())).is_err(), "non-hex");
    assert!(RelayedCollateral::from_json(&json(&|v| drop(v.as_object_mut().unwrap().remove("crl")))).is_err());
    assert!(RelayedCollateral::from_json(b"not json").is_err());
}

#[tokio::test]
async fn a_verifier_checks_relayed_collateral_against_its_pinned_roots_with_no_vendor_call() {
    let (chain, enclave, attestation) = fixtures();
    // Nothing listens here: any call to the vendor would fail the verification.
    let dead = "http://127.0.0.1:1";
    let ark = pem(&chain.ark);
    let good = collateral_of(&chain, &crl_revoking(&chain, &[]));
    verify_relayed(&attestation, &good, Some(&ark), dead).await.expect("relayed collateral verifies");
    // The node's word is not a trust anchor: the same bytes fail without the root pinned.
    assert!(
        verify_relayed(&attestation, &good, None, dead).await.is_err(),
        "accepted a chain whose root the verifier does not pin"
    );

    let mut other_chip = enclave.chip_id;
    other_chip[0] ^= 1;
    let foreign_vcek =
        RelayedCollateral { vcek: vcek_for(&chain, other_chip, REPORTED_TCB).der().to_vec(), ..good.clone() };
    assert!(
        verify_relayed(&attestation, &foreign_vcek, Some(&ark), dead).await.is_err(),
        "accepted a VCEK for another chip"
    );
    let stale = collateral_of(&chain, &crl_signed(&chain, &[], (2024, 1, 1), (2025, 1, 1), None));
    assert!(verify_relayed(&attestation, &stale, Some(&ark), dead).await.is_err(), "accepted an expired list");
    let ask_serial = SerialNumber::from_slice(&x509_serial(chain.ask.der()));
    let revoked = collateral_of(&chain, &crl_revoking(&chain, &[ask_serial]));
    assert!(
        verify_relayed(&attestation, &revoked, Some(&ark), dead).await.is_err(),
        "accepted a list revoking the intermediate"
    );
}

#[tokio::test]
async fn a_saved_collateral_file_loads_and_verifies_and_an_oversized_or_malformed_one_is_refused() {
    let (chain, _enclave, attestation) = fixtures();
    let dir = crate::test_support::scratch_dir("saved-collateral");
    std::fs::create_dir_all(&dir).unwrap();
    let dead = "http://127.0.0.1:1";
    let options = Options { processor: PROCESSOR, kds_base: dead, root_pem: None, collateral: None };
    let saved = |name: &str, bytes: &[u8]| {
        let path = dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        path.to_str().unwrap().to_owned()
    };

    // What an operator saves with `curl -k` loads and verifies against the pinned root.
    let good = collateral_of(&chain, &crl_revoking(&chain, &[]));
    let loaded = load_relayed_collateral(&saved("good.json", &good.to_json()), &options).await.unwrap();
    assert_eq!(loaded, good);
    verify_relayed(&attestation, &loaded, Some(&pem(&chain.ark)), dead).await.expect("the saved file verifies");

    // A file over the fetch limit is refused whatever it holds, and so are malformed and missing ones.
    let limit = 1024 * 1024;
    let mut oversized = good.to_json();
    oversized.resize(limit + 1, b' ');
    let err = load_relayed_collateral(&saved("big.json", &oversized), &options).await.unwrap_err();
    assert!(format!("{err:#}").contains("byte limit"), "{err:#}");
    let mut at_limit = good.to_json();
    at_limit.resize(limit, b' ');
    assert_eq!(load_relayed_collateral(&saved("limit.json", &at_limit), &options).await.unwrap(), good);
    let err = load_relayed_collateral(&saved("junk.json", b"not json"), &options).await.unwrap_err();
    assert!(format!("{err:#}").contains("parsing the collateral document"), "{err:#}");
    assert!(load_relayed_collateral(dir.join("absent.json").to_str().unwrap(), &options).await.is_err());
    std::fs::remove_dir_all(&dir).ok();
}

/// Verifies `attestation` against `collateral` with the vendor service at `kds_base` unreachable.
async fn verify_relayed(
    attestation: &crate::tee::Attestation,
    collateral: &RelayedCollateral,
    root: Option<&str>,
    kds_base: &str,
) -> anyhow::Result<()> {
    let options = Options { processor: PROCESSOR, kds_base, root_pem: root, collateral: Some(collateral) };
    verify_attestation(attestation, &release(), &options).await.map(drop)
}

#[test]
fn a_non_snp_provider_gets_no_relay_and_an_snp_quote_needs_a_supported_report_and_its_processor() {
    let (_chain, enclave, attestation) = fixtures();
    let mut mock_quote = b"MOCK-ATTESTATION-NOT-A-TEE:".to_vec();
    mock_quote.extend_from_slice(&enclave.bundle.report_data());
    let mut mock = super::fixtures::attestation(&enclave.bundle, &mock_quote);
    mock.provider = "mock".into();
    assert!(Relay::new(&tee("https://kds.example", "12h"), &mock).unwrap().is_none());

    // An SEV-SNP guest whose report the verifier cannot parse is an error, not a silent 404.
    let mut unsupported = attestation.clone();
    unsupported.quote = "0x0102".into();
    let err = Relay::new(&tee("https://kds.example", "12h"), &unsupported).err().expect("an unparseable SNP report");
    assert!(err.to_string().contains("supported SEV-SNP report"), "{err:#}");

    let mut unset = tee("https://kds.example", "12h");
    unset.processor = None;
    let err = Relay::new(&unset, &attestation).err().expect("an SNP quote without a processor is refused");
    assert!(err.to_string().contains("tee.processor"), "{err:#}");
    assert!(Relay::new(&tee("https://kds.example", "12h"), &attestation).unwrap().is_some());
}

#[tokio::test]
async fn the_relay_serves_the_collateral_it_fetched() {
    let (chain, _enclave, attestation) = fixtures();
    let stub = kds_stub(&chain).await;
    let relay = Relay::new(&tee(&stub.base, "12h"), &attestation).unwrap().unwrap();
    let mut served = relay.served();
    assert!(served.borrow().is_none(), "served before the first fetch");

    let task = tokio::spawn(relay.run());
    let json =
        timeout(Duration::from_secs(20), served.wait_for(Option::is_some)).await.unwrap().unwrap().clone().unwrap();
    let relayed = RelayedCollateral::from_json(&json).unwrap();
    assert_eq!(relayed.vcek, chain.vcek.der().to_vec());
    task.abort();
}

#[tokio::test]
async fn a_refresh_that_fails_its_checks_leaves_nothing_served_and_pages_stale() {
    let enclave = enclave();
    // The service answers with a VCEK for another chip, so the checks must refuse it.
    let mut other_chip = enclave.chip_id;
    other_chip[0] ^= 1;
    let foreign = test_chain(other_chip, REPORTED_TCB);
    let stub = kds_stub(&foreign).await;
    let attestation = super::fixtures::attestation(
        &enclave.bundle,
        &forged_report(&foreign, LAUNCH_DIGEST, enclave.bundle.report_data(), enclave.chip_id),
    );
    let (_guard, mut alerts) = capture_alerts();
    let relay = Relay::new(&tee(&stub.base, "20ms"), &attestation).unwrap().unwrap();
    let served = relay.served();

    let quote = quote_of(&attestation);
    let client = Collateral::new(&stub.base, PROCESSOR).unwrap();
    let err = relay.refresh_once(&client).await.expect_err("collateral for another chip must be refused");
    assert!(format!("{err:#}").contains("not valid for this node's quote"), "{err:#}");
    assert!(SnpReport::parse(&quote).is_ok());

    let task = tokio::spawn(relay.run());
    let alert = timeout(Duration::from_secs(20), alerts.recv()).await.unwrap().unwrap();
    assert_eq!(alert["alert"], "collateral_stale");
    assert!(served.borrow().is_none(), "served collateral that failed its checks");
    task.abort();
}

/// Collects `alert` and `alert_resolved` lines as `(kind, name)`.
struct AlertLines(tokio::sync::mpsc::UnboundedSender<(&'static str, String)>);

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for AlertLines {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        struct Names(Option<(&'static str, String)>);
        impl tracing::field::Visit for Names {
            fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                match field.name() {
                    "alert" => self.0 = Some(("alert", value.to_owned())),
                    "alert_resolved" => self.0 = Some(("resolved", value.to_owned())),
                    _ => {}
                }
            }
            fn record_debug(&mut self, _: &tracing::field::Field, _: &dyn std::fmt::Debug) {}
        }
        let mut names = Names(None);
        event.record(&mut names);
        if let Some(line) = names.0 {
            let _ = self.0.send(line);
        }
    }
}

#[tokio::test]
async fn an_outage_keeps_the_last_copy_pages_near_expiry_and_recovery_replaces_it_and_resolves() {
    use tracing_subscriber::layer::SubscriberExt as _;

    let (chain, _enclave, attestation) = fixtures();
    let down = Arc::new(AtomicBool::new(false));
    // A list whose nextUpdate is twelve hours away, so a failing refresh pages and the list
    // stays valid for the whole test.
    let crl = crl_signed(&chain, &[], (2024, 1, 1), CrlEnd::SecondsFromNow(12 * 3600), None);
    let stub = kds_stub_switched(&chain, Some(crl), down.clone()).await;
    crate::test_tracing::keep_callsites_interested();
    let (lines_tx, mut lines) = tokio::sync::mpsc::unbounded_channel();
    let _guard = tracing::subscriber::set_default(tracing_subscriber::Registry::default().with(AlertLines(lines_tx)));

    let relay = Relay::new(&tee(&stub.base, "20ms"), &attestation).unwrap().unwrap();
    let mut served = relay.served();
    let task = tokio::spawn(relay.run());
    timeout(Duration::from_secs(30), served.wait_for(Option::is_some)).await.unwrap().unwrap();

    // The vendor goes away: refreshes fail, the page fires, and the last copy stays served.
    down.store(true, Ordering::SeqCst);
    let attempts = stub.attempts.clone();
    let until_failures = |count: usize| {
        let attempts = attempts.clone();
        async move {
            while attempts.load(Ordering::SeqCst) < count {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    };
    timeout(Duration::from_secs(60), until_failures(3)).await.unwrap();
    let kept = served.borrow().clone().expect("the last copy is still served");
    timeout(Duration::from_secs(60), until_failures(6)).await.unwrap();
    assert_eq!(served.borrow().clone(), Some(kept.clone()), "an outage changed the served copy");
    let mut paged = false;
    while let Ok((kind, name)) = lines.try_recv() {
        paged |= (kind, name.as_str()) == ("alert", "collateral_stale");
    }
    assert!(paged, "no collateral_stale page with the list under a day from its nextUpdate");

    // The vendor returns: a new copy replaces the kept one and the page resolves.
    down.store(false, Ordering::SeqCst);
    timeout(Duration::from_secs(60), served.wait_for(|doc| doc.as_ref().is_some_and(|doc| *doc != kept)))
        .await
        .unwrap()
        .unwrap();
    let resolved = timeout(Duration::from_secs(30), async {
        loop {
            if lines.recv().await.unwrap() == ("resolved", "collateral_stale".to_owned()) {
                break;
            }
        }
    })
    .await;
    assert!(resolved.is_ok(), "recovery did not resolve collateral_stale");
    task.abort();
}

#[test]
fn staleness_pages_within_a_day_of_the_lists_next_update_or_after_a_refresh_with_nothing() {
    let refresh = Duration::from_secs(3600);
    let now = 1_000_000;
    let day = 24 * 60 * 60;
    assert!(!is_stale(Some(Some(now + day + 1)), Duration::ZERO, refresh, now), "more than a day left");
    assert!(is_stale(Some(Some(now + day)), Duration::ZERO, refresh, now), "a day left");
    assert!(is_stale(Some(Some(now - 1)), Duration::ZERO, refresh, now), "past nextUpdate");
    assert!(!is_stale(Some(None), refresh * 10, refresh, now), "a list that names no nextUpdate");
    assert!(!is_stale(None, Duration::from_secs(29 * 60), refresh, now), "just started, nothing yet");
    assert!(is_stale(None, refresh, refresh, now), "a whole interval with nothing fetched");
    let daily = Duration::from_secs(24 * 3600);
    assert!(!is_stale(None, Duration::from_secs(29 * 60), daily, now), "under 30 minutes with nothing");
    assert!(is_stale(None, Duration::from_secs(30 * 60), daily, now), "30 minutes with nothing, whatever the cadence");
}

#[test]
fn the_wait_after_a_good_refresh_ends_before_the_served_list_expires() {
    let (refresh, now, hour) = (Duration::from_secs(12 * 3600), 1_000_000, 3600);
    assert_eq!(next_wait(refresh, Some(now + 7 * 24 * hour), now), refresh, "a week left: the configured cadence");
    assert_eq!(next_wait(refresh, None, now), refresh, "a list naming no nextUpdate");
    assert_eq!(next_wait(refresh, Some(now + 4 * hour), now), Duration::from_secs(2 * 3600), "four hours left");
    let five_minutes = Duration::from_secs(5 * 60);
    assert_eq!(next_wait(refresh, Some(now + 60), now), five_minutes, "a minute left is not retried faster");
    assert_eq!(next_wait(refresh, Some(now - 10), now), five_minutes, "already past");
    assert_eq!(next_wait(Duration::from_secs(20), Some(now + 60), now), Duration::from_secs(20), "a short cadence");
}

/// A certificate's serial content bytes.
fn x509_serial(der: &[u8]) -> Vec<u8> {
    use x509_parser::prelude::FromDer as _;
    let (_, cert) = x509_parser::certificate::X509Certificate::from_der(der).unwrap();
    cert.serial.to_bytes_be()
}
