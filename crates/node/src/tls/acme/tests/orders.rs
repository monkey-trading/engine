//! The certificate task's attempts against a scripted CA: an order validated, issued, and served; a
//! validation reused; an order taken up again after a lost exchange, a passing problem, or a
//! deadline, but replaced after a refusal; and a late certificate counted from its finalize.

use super::*;

use crate::tls::acme::tests::fake_ca::{FakeCa, State};

#[tokio::test(start_paused = true)]
async fn an_order_is_validated_finalized_and_its_certificate_served_for_the_attested_key() {
    let (task, ca, attested) = scripted();
    let mut progress = Progress::default();
    let issued = task.issue(&mut progress).await.unwrap();
    assert_eq!(issued.lifetime, 90 * DAY);
    assert_eq!([ca.count("new-order"), ca.count("challenge"), ca.count("finalize")], [1, 1, 1]);
    assert!(progress.order.is_none() && progress.finalized.is_none(), "nothing is left unfinished");
    assert!(progress.account.is_some(), "the account serves the renewals");
    let served = task.key.served();
    assert_eq!(served.len(), 2, "the CA's chain, not the self-signed certificate");
    let (_, leaf) = X509Certificate::from_der(&served[0]).unwrap();
    assert_eq!(<[u8; 32]>::from(Sha256::digest(leaf.public_key().raw)), attested);
    assert!(check(&served, DOMAIN).is_ok(), "the served leaf names the domain");
    assert!(!task.key.answering(), "the challenge is answered no longer");

    // A renewal while the validation is current neither registers nor validates again.
    task.issue(&mut progress).await.unwrap();
    assert_eq!([ca.count("new-order"), ca.count("challenge"), ca.count("finalize")], [2, 1, 2]);
    assert_eq!(ca.count("new-account"), 1);
}

#[tokio::test(start_paused = true)]
async fn a_lost_exchange_or_a_passing_problem_resumes_the_order_instead_of_issuing_again() {
    type Script = fn(&mut State);
    let cases: [(&str, usize, Script); 7] = [
        ("a lost finalize answer", 1, |s| s.lose_finalize = 1),
        ("a lost download", 1, |s| s.lose_downloads = 1),
        ("two lost downloads", 2, |s| s.lose_downloads = 2),
        ("a serverInternal download", 1, |s| s.download_problems = vec![(500, "serverInternal")]),
        ("a rateLimited download", 1, |s| s.download_problems = vec![(429, "rateLimited")]),
        ("a serverInternal order read after the finalize, then a lost download", 2, |s| {
            s.lose_downloads = 1;
            s.order_problems = vec![None, Some((503, "serverInternal"))];
        }),
        // The validation's and the issuance's order reads answer; the next attempt's read of the
        // unfinished order meets the problem.
        ("a serverInternal read of the unfinished order", 2, |s| {
            s.lose_downloads = 1;
            s.order_problems = vec![None, None, Some((503, "serverInternal"))];
        }),
    ];
    for (case, failures, script) in cases {
        let (task, ca, _) = scripted();
        script(&mut ca.state());
        let mut progress = Progress::default();
        for _ in 0..failures {
            let err = task.issue(&mut progress).await.unwrap_err();
            assert!(problem(&err).is_none_or(|problem| !ends_order(problem)), "{case}: {err:#}");
            assert!(progress.order.is_some(), "{case}: the order is kept for the next attempt");
        }
        task.issue(&mut progress).await.unwrap();
        assert_eq!([ca.count("new-order"), ca.count("finalize")], [1, 1], "{case}: one order, one issuance");
    }
}

#[tokio::test(start_paused = true)]
async fn an_order_the_ca_ended_or_whose_certificate_is_refused_is_replaced() {
    type Script = fn(&mut State);
    let cases: [(&str, &str, Script, Script); 4] = [
        ("a refused validation", "connection refused", |s| s.refuse_validation = true, |s| s.refuse_validation = false),
        ("a malformed download", "malformed", |s| s.download_problems = vec![(404, "malformed")], |_| {}),
        (
            "a certificate for another name",
            "does not name api.engine.test",
            |s| s.names = "other.engine.test".into(),
            |s| s.names = DOMAIN.into(),
        ),
        (
            "an unfinished order the CA no longer serves",
            "connection reset",
            |s| s.lose_downloads = 1,
            |s| s.order_problems = vec![Some((404, "malformed"))],
        ),
    ];
    for (case, reason, script, unscript) in cases {
        let (task, ca, _) = scripted();
        script(&mut ca.state());
        let mut progress = Progress::default();
        let err = format!("{:#}", task.issue(&mut progress).await.unwrap_err());
        assert!(err.contains(reason), "{case}: {err}");
        unscript(&mut ca.state());
        task.issue(&mut progress).await.unwrap();
        assert_eq!(ca.count("new-order"), 2, "{case}: the order is replaced");
    }
    // The validation failure names the challenge's own reason, beside the order's status.
    let (task, ca, _) = scripted();
    ca.state().refuse_validation = true;
    let err = format!("{:#}", task.issue(&mut Progress::default()).await.unwrap_err());
    assert!(err.contains("the order is Invalid: API error: connection refused"), "{err}");
}

#[tokio::test(start_paused = true)]
async fn an_account_the_ca_forgot_is_registered_again() {
    let (task, ca, _) = scripted();
    let mut progress = Progress::default();
    task.issue(&mut progress).await.unwrap();
    ca.state().forget_account = true;
    let err = task.issue(&mut progress).await.unwrap_err();
    assert!(format!("{err:#}").contains("accountDoesNotExist"), "{err:#}");
    assert!(progress.account.is_none(), "the next attempt registers again");
    task.issue(&mut progress).await.unwrap();
    assert_eq!(ca.count("new-account"), 2);
}

#[tokio::test(start_paused = true)]
async fn an_attempt_past_its_deadline_drops_the_challenge_and_keeps_the_order() {
    // The CA acts on the challenge, or issues, then never answers; the attempt's deadline ends it.
    for stalled in ["challenge", "finalize"] {
        let (task, ca, _) = scripted();
        ca.state().stall = Some(stalled);
        let mut progress = Progress::default();
        assert!(timeout(ATTEMPT_TIMEOUT, task.issue(&mut progress)).await.is_err(), "{stalled} never answers");
        assert!(!task.key.answering(), "{stalled}: no challenge is left answered");
        assert!(progress.order.is_some(), "{stalled}: the order is kept");
        task.issue(&mut progress).await.unwrap();
        assert_eq!([ca.count("new-order"), ca.count("finalize")], [1, 1], "{stalled}: resumed, issued once");
    }
    // A CA that never validates fails the attempt at the poll deadline, keeping the order.
    let (task, ca, _) = scripted();
    ca.state().never_validate = true;
    let mut progress = Progress::default();
    let err = format!("{:#}", task.issue(&mut progress).await.unwrap_err());
    assert!(err.contains("waiting for the CA to validate the domain: timed out"), "{err}");
    assert!(!task.key.answering() && progress.order.is_some());
}

#[tokio::test(start_paused = true)]
async fn a_task_whose_attempt_the_ca_never_answers_fails_it_at_the_deadline_and_retries_after_the_backoff() {
    let (logs, _guard) = Logs::capture();
    let (task, ca, _) = scripted();
    ca.state().stall = Some("new-account");
    let not_after = task.not_after.clone();
    let run = tokio::spawn(task.run());
    tokio::time::sleep(ATTEMPT_TIMEOUT - Duration::from_secs(1)).await;
    assert_eq!(ca.count("new-account"), 1, "the attempt is waiting on the CA");
    assert_eq!(logs.warnings("certificate_pending"), 1, "only the start's: the attempt has not failed");
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(logs.warnings("certificate_pending"), 2, "the attempt failed at its deadline");
    assert_eq!(logs.count("the attempt took over"), 1);
    assert_eq!(ca.count("new-account"), 1, "the retry waits out the backoff");
    assert_eq!(not_after.get(), 0.0);
    tokio::time::sleep(Duration::from_secs(60)).await;
    assert_eq!(ca.count("new-account"), 2, "retried after a minute");
    assert!(not_after.get() > 0.0, "and the retry's certificate serves");
    run.abort();
}

#[tokio::test(start_paused = true)]
async fn a_late_certificate_is_counted_from_its_finalize_and_refused_once_its_life_ran_out() {
    // Downloaded 89 days after its finalize: one day of life left, inside the renewal window.
    let (task, ca, _) = scripted();
    ca.state().lose_downloads = 1;
    let mut progress = Progress::default();
    task.issue(&mut progress).await.unwrap_err();
    tokio::time::advance(89 * DAY).await;
    let issued = task.issue(&mut progress).await.unwrap();
    assert!(issued.finalized.elapsed() >= 89 * DAY, "counted from the first attempt's finalize");
    let mut schedule = Schedule::start(DOMAIN.into(), None, Gauge::default());
    assert_eq!(schedule.issued(issued.lifetime, issued.finalized, Instant::now()), Duration::ZERO);

    // Downloaded after its 90 days: refused, and ordered anew.
    let (task, ca, _) = scripted();
    ca.state().lose_downloads = 1;
    let mut progress = Progress::default();
    task.issue(&mut progress).await.unwrap_err();
    tokio::time::advance(90 * DAY).await;
    let err = format!("{:#}", task.issue(&mut progress).await.unwrap_err());
    assert!(err.contains("life ran out before it arrived"), "{err}");
    assert_eq!(task.key.served().len(), 1, "the self-signed certificate still serves");
    task.issue(&mut progress).await.unwrap();
    assert_eq!(ca.count("new-order"), 2);
}

// ── Helpers ──

/// A certificate task for `DOMAIN` against a fresh scripted CA, and the attested key's hash.
fn scripted() -> (CertificateTask<FakeCa>, FakeCa, [u8; 32]) {
    let ca = FakeCa::new(DOMAIN);
    let tls = TlsIdentity::generate().unwrap();
    let task = CertificateTask {
        domain: DOMAIN.into(),
        directory_url: "https://ca.test/dir".into(),
        contact: "mailto:ops@engine.test".into(),
        renew_before: None,
        http: ca.clone(),
        key: tls.key,
        not_after: FloatGauge::default(),
        pending: Gauge::default(),
        sealed: None,
        restored: None,
    };
    (task, ca, tls.public_key_sha256)
}
