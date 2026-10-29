//! The certificate task's attempts against a scripted CA: an order validated and issued, a
//! validation reused, and an order taken up again after a lost exchange but never after a refusal.

use super::*;

use crate::tls::acme::tests::fake_ca::FakeCa;

#[tokio::test(start_paused = true)]
async fn an_order_is_validated_finalized_and_installed_for_the_attested_key() {
    let (task, ca) = scripted();
    let mut progress = Progress::default();
    let issued = task.issue(&mut progress).await.unwrap();
    assert_eq!(issued.lifetime, 90 * DAY);
    assert_eq!([ca.count("new-order"), ca.count("challenge"), ca.count("finalize")], [1, 1, 1]);
    assert!(progress.order.is_none(), "nothing is left unfinished");
    assert!(progress.account.is_some(), "the account serves the renewals");

    // A renewal while the validation is current neither registers nor validates again.
    task.issue(&mut progress).await.unwrap();
    assert_eq!([ca.count("new-order"), ca.count("challenge"), ca.count("finalize")], [2, 1, 2]);
    assert_eq!(ca.count("new-account"), 1);
}

#[tokio::test(start_paused = true)]
async fn a_lost_exchange_resumes_the_order_instead_of_issuing_again() {
    // Each answer the network loses after the CA acted on it: the finalize, then the download.
    for (lose_finalize, lose_downloads) in [(1, 0), (0, 1), (0, 2)] {
        let (task, ca) = scripted();
        {
            let mut script = ca.state();
            (script.lose_finalize, script.lose_downloads) = (lose_finalize, lose_downloads);
        }
        let mut progress = Progress::default();
        for _ in 0..lose_finalize + lose_downloads {
            let err = task.issue(&mut progress).await.unwrap_err();
            assert!(!refused_by_ca(&err), "{err:#}");
            assert!(progress.order.is_some(), "the order is kept for the next attempt");
        }
        task.issue(&mut progress).await.unwrap();
        assert_eq!(ca.count("new-order"), 1, "one order ({lose_finalize}, {lose_downloads})");
        assert_eq!(ca.count("finalize"), 1, "one issuance ({lose_finalize}, {lose_downloads})");
        assert_eq!(ca.count("certificate"), 1 + lose_downloads);
    }
}

#[tokio::test(start_paused = true)]
async fn an_order_the_ca_refused_or_whose_certificate_is_refused_is_replaced() {
    // The CA refuses the validation, saying why.
    let (task, ca) = scripted();
    ca.state().refuse_validation = true;
    let mut progress = Progress::default();
    let err = format!("{:#}", task.issue(&mut progress).await.unwrap_err());
    assert!(err.contains("the order is Invalid") && err.contains("connection refused"), "{err}");
    ca.state().refuse_validation = false;
    task.issue(&mut progress).await.unwrap();
    assert_eq!(ca.count("new-order"), 2, "the invalid order is replaced");

    // The CA answers the download with a problem document.
    let (task, ca) = scripted();
    ca.state().refuse_download = true;
    let mut progress = Progress::default();
    let err = task.issue(&mut progress).await.unwrap_err();
    assert!(refused_by_ca(&err), "{err:#}");
    assert!(progress.order.is_none());
    task.issue(&mut progress).await.unwrap();
    assert_eq!(ca.count("new-order"), 2);

    // The CA issues a certificate for another name, which the task refuses.
    let (task, ca) = scripted();
    ca.state().names = "other.engine.test".into();
    let mut progress = Progress::default();
    let err = format!("{:#}", task.issue(&mut progress).await.unwrap_err());
    assert!(err.contains("does not name api.engine.test"), "{err}");
    assert!(progress.order.is_none(), "an issued order is not taken up again");
    ca.state().names = DOMAIN.into();
    task.issue(&mut progress).await.unwrap();
    assert_eq!([ca.count("new-order"), ca.count("finalize")], [2, 2]);
}

// ── Helpers ──

/// A certificate task for `DOMAIN` against a fresh scripted CA.
fn scripted() -> (CertificateTask<FakeCa>, FakeCa) {
    let ca = FakeCa::new(DOMAIN);
    let task = CertificateTask {
        domain: DOMAIN.into(),
        directory_url: "https://ca.test/dir".into(),
        contact: "mailto:ops@engine.test".into(),
        renew_before: None,
        http: ca.clone(),
        key: TlsIdentity::generate().unwrap().key,
        not_after: FloatGauge::default(),
    };
    (task, ca)
}
