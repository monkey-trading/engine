//! The holder's checkpoint endpoint confirmed by the custody quorum: an endpoint that names a release,
//! a generation or a fold the quorum does not read at the same block, or reads the rotation switch off
//! where the quorum reads it on, moves neither the keys nor the delivered secrets, sequencing goes on,
//! and a difference at the same block pages `checkpoint_endpoint_disagrees`; where the two agree, an
//! upgrade takes both as before.

use super::*;

use super::vault::Reads;
use crate::secrets::tests::{full, operator_key};

#[tokio::test]
async fn an_endpoint_naming_a_release_the_custody_quorum_does_not_hands_no_keys_over() {
    let (_guard, mut alerts) = capture_alerts();
    let mut exchange = Exchange::start("quorum-release").await;
    // The endpoint names the successor's release, folded, as the current one.
    exchange.make_upgrade_current();
    *exchange.synced.lock().unwrap() = Some(exchange.upgrade().id());
    // The quorum reads the running release as the current one, the successor's in another generation,
    // or the successor's with the running release folded.
    let running = Release { launch_digest: RELEASE, config_sha256: PREDECESSOR_CONFIG }.id();
    let upgrade = exchange.upgrade().id();
    let current = |id, generation| Reads { current: Some(CurrentRelease { id, generation }), ..Reads::default() };
    for (what, reads) in [
        ("another release", current(running, GENERATION)),
        ("another generation", current(upgrade, GENERATION + 1)),
        ("no fold", Reads { synced: Some(running), ..Reads::default() }),
    ] {
        *exchange.majority.reads.lock().unwrap() = reads;
        let err = exchange.take().await.err().unwrap_or_else(|| panic!("{what}: the successor took the keys"));
        assert!(format!("{err:#}").contains("the custody quorum reads"), "{what}: {err:#}");
        assert!(exchange.drains.try_recv().is_err(), "{what}: sequencing stopped");
        assert!(!exchange.run.is_finished(), "{what}: the predecessor stopped serving");
        assert!(std::fs::read_dir(&exchange.successor_dir).is_err(), "{what}: the successor sealed keys");
        assert_eq!(disagreements(&mut alerts), 1, "{what}: the disagreement paged once");
    }

    // Where the quorum reads what the endpoint does, the upgrade takes the keys.
    *exchange.majority.reads.lock().unwrap() = Reads::default();
    let head = write_log_tail(&exchange.successor_log, 9, &exchange.old.enclave_key().unwrap());
    let answer = exchange.answer_drain(Ok(head));
    let store = exchange.take().await.expect("the keys moved where the quorum agrees");
    assert_eq!((store.release(), store.generation()), (exchange.upgrade(), GENERATION));
    assert_eq!(answer.await.unwrap(), 1, "sequencing stopped once");
    timeout(Duration::from_secs(5), exchange.run).await.expect("the predecessor ended").unwrap().unwrap();
}

#[tokio::test]
async fn an_endpoint_reading_rotation_off_where_the_custody_quorum_reads_it_on_moves_no_secrets() {
    for (what, rotation) in [("at the endpoint's block", (true, false)), ("at the quorum's head", (false, true))] {
        let exchange = Exchange::start(&format!("quorum-secrets-{}", rotation.0)).await;
        exchange.make_upgrade_current();
        exchange.secrets.replace(full(3, "a").sign(exchange.domain(), &operator_key())).unwrap();
        exchange.majority.reads.lock().unwrap().rotation = Some(rotation);
        let (guard, mut alerts) = capture_alerts();
        let err = format!("{:#}", exchange.fetch(exchange.operator()).await.unwrap_err());
        assert!(err.contains("an elected operator brings its own secrets"), "{what}: {err}");
        assert!(!exchange.run.is_finished(), "{what}: the predecessor stopped serving");
        // Only the same block's reads must agree: the endpoint's latest block and the quorum's head may
        // differ.
        let paged = usize::from(rotation.0);
        assert_eq!(disagreements(&mut alerts), paged, "{what}: the disagreement paged where the blocks are one");
        drop(guard);

        // Where the quorum reads it off too, the upgrade takes the secrets.
        exchange.majority.reads.lock().unwrap().rotation = None;
        let signed = exchange.fetch(exchange.operator()).await.expect("the secrets moved where the quorum agrees");
        assert_eq!(signed.bundle, full(3, "a"), "{what}");
    }
}

/// How many `checkpoint_endpoint_disagrees` pages, all at Warning, `alerts` has taken since last asked.
fn disagreements(alerts: &mut tokio::sync::mpsc::UnboundedReceiver<crate::test_support::Alert>) -> usize {
    let paged: Vec<_> = std::iter::from_fn(|| alerts.try_recv().ok())
        .filter(|alert| alert.get("alert").map(String::as_str) == Some("checkpoint_endpoint_disagrees"))
        .collect();
    assert!(paged.iter().all(|alert| alert.get("level").map(String::as_str) == Some("WARN")), "{paged:?}");
    paged.len()
}
