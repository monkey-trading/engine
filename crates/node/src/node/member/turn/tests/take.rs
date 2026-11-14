//! A member's tries at its turn: a failed take keeps its replica unless the take began changing the
//! copy, a refused tip pages naming the turn, and after `TAIL_ATTEMPTS` refusals the member leaves
//! its turn to a whole-log start, which sets the copy aside.

use super::*;
use tokio::task::JoinHandle;

use crate::handover::stand_in::{Answer, serve_tail};
use crate::handover::tail::{CopyChanged, TailMiss};
use crate::log::{CheckpointDomain, MANIFEST};

/// A turn whose take fails, here at the start's reads, keeps the member's replica: it follows for a
/// stint before each retry and replays nothing again. Its failures are no source's refusals, so it
/// retries past `TAIL_ATTEMPTS` for `RETRY_FOR` before it leaves its turn to the start, which takes
/// the whole log as a node with no copy does.
#[tokio::test]
async fn a_member_whose_turn_cannot_be_taken_keeps_its_replica_and_falls_back() {
    let fixture = Fixture::start("member-fallback", true).await;
    let operator = fixture.operator(true).await;
    fixture.registry.name(&operator.host);
    fixture.registry.draw(fixture.release().id());
    fixture.time.set(FIRST + 1);
    let mut member = fixture.member();
    let drive = async {
        operator.wait_for_connections(1).await;
        fixture.time.set(OWN + 1);
    };
    let started = std::time::Instant::now();
    let (turn, ()) = tokio::join!(member.until_turn(), drive);
    assert!(matches!(turn.unwrap(), Turn::Hold), "the member did not fall back to the cold start");
    assert!(started.elapsed() >= RETRY_FOR, "the member fell back before its failures outlasted RETRY_FOR");
    assert_eq!(member.metrics.full_replays.get(), 1, "a failed take rebuilt the replica");
    assert!(operator.connections() > TAIL_ATTEMPTS, "the member counted failures no source refused");
    assert_left_to_the_start(&fixture.cfg.node.log_dir);
}

/// A take that failed once it began changing the copy, here on a tail the copy cannot take, comes out
/// of the take as `CopyChanged`, so the member rebuilds its replica before the next try; the
/// refusals after it keep the rebuilt one, and each counts toward `TAIL_ATTEMPTS`.
#[tokio::test]
async fn a_take_that_changed_the_copy_rebuilds_the_replica_and_a_refusal_does_not() {
    let refused = Answer::Missed(TailMiss::Lagged);
    let script = vec![Answer::Tail(b"not a log entry\n".to_vec()), refused.clone(), refused];
    let (fixture, operator, served, asked) = in_own_turn("member-copy-changed", script).await;
    let mut member = fixture.member();
    let turn = member.until_turn().await;
    served.abort();
    assert!(matches!(turn.unwrap(), Turn::Hold), "the member did not fall back to the cold start");
    assert_eq!(asked.load(Ordering::SeqCst), TAIL_ATTEMPTS, "the member's tries at the predecessor");
    assert!(operator.connections() >= TAIL_ATTEMPTS, "the member did not follow the incumbent between its tries");
    assert_eq!(member.metrics.full_replays.get(), 2, "only the take that changed the copy rebuilt the replica");
    assert_left_to_the_start(&fixture.cfg.node.log_dir);
}

/// Each refusal of the member's tip pages within the take's span, naming the turn, and keeps the
/// replica: a tip not on the incumbent's log Critical `handover_tip_not_on_log`, one it no longer
/// retains Warning `handover_tail_lagged`.
#[tokio::test]
async fn each_refused_tip_pages_naming_the_turn_and_keeps_the_replica() {
    let (_guard, mut alerts) = capture_alerts();
    let lagged = Answer::Missed(TailMiss::Lagged);
    let script = vec![Answer::Missed(TailMiss::NotOnLog), lagged.clone(), lagged];
    let (fixture, _operator, served, _) = in_own_turn("member-tip-refused", script).await;
    let mut member = fixture.member();
    let turn = member.until_turn().await;
    served.abort();
    assert!(matches!(turn.unwrap(), Turn::Hold), "the member did not fall back to the cold start");
    assert_eq!(member.metrics.full_replays.get(), 1, "a refusal rebuilt the replica");
    let paged: Vec<_> = std::iter::from_fn(|| alerts.try_recv().ok())
        .filter(|alert| alert["alert"].starts_with("handover_"))
        .map(|alert| (alert["alert"].clone(), alert["turn"].clone(), alert["level"].clone()))
        .collect();
    let named = |alert: &str, level: &str| (alert.to_owned(), "12".to_owned(), level.to_owned());
    let lagged = named("handover_tail_lagged", "WARN");
    assert_eq!(paged, [named("handover_tip_not_on_log", "ERROR"), lagged.clone(), lagged]);
}

/// Every source's failure is classified, though a later source, the turn before's, fails another
/// way: the incumbent's tail that changed the copy rebuilds the replica, and its refusals page.
#[tokio::test]
async fn each_sources_failure_is_classified_though_a_later_source_fails() {
    let (_guard, mut alerts) = capture_alerts();
    let refused = Answer::Missed(TailMiss::NotOnLog);
    let script = vec![Answer::Tail(b"not a log entry\n".to_vec()), refused.clone(), refused];
    let (fixture, _operator, served, asked) = in_own_turn("member-sources", script).await;
    let nothing = "127.0.0.1:9".to_owned();
    let (candidate, release) = (AccountId([0x0a; 20]), [0x0a; 32]);
    fixture.registry.prior(HandoverSource {
        candidate,
        release,
        api_hostname: nothing.clone(),
        handover_endpoint: nothing,
    });
    let mut member = fixture.member();
    let turn = member.until_turn().await;
    served.abort();
    assert!(matches!(turn.unwrap(), Turn::Hold), "the member did not fall back to the cold start");
    assert_eq!(asked.load(Ordering::SeqCst), TAIL_ATTEMPTS, "the member's tries at the incumbent");
    assert_eq!(member.metrics.full_replays.get(), 2, "the copy the incumbent's tail changed was not rebuilt");
    let paged =
        std::iter::from_fn(|| alerts.try_recv().ok()).filter(|alert| alert["alert"] == "handover_tip_not_on_log");
    assert_eq!(paged.count(), 2, "the incumbent's refusals were not paged");
}

/// Only a replica that refused an entry, or a copy a handover began changing, is rebuilt.
#[test]
fn only_a_refused_entry_or_a_part_changed_copy_rebuilds_the_replica() {
    assert!(damaged(&anyhow::anyhow!("an entry").context(ReplicaRefused)));
    assert!(damaged(&anyhow::anyhow!("a tail").context(CopyChanged)));
    assert!(!damaged(&anyhow::anyhow!("the predecessor refused the request")));
}

/// A member that comes up in its own turn, its copy holding the operator's log, which `operator`
/// streams as the incumbent: the turn takes over from a stand-in predecessor answering as `script`
/// says, whose task and request count come back too. Rotation is off, so the start's reads need no
/// chain and the take reaches the predecessor.
async fn in_own_turn(name: &str, script: Vec<Answer>) -> (Fixture, Operator, JoinHandle<()>, Arc<AtomicUsize>) {
    let mut fixture = Fixture::start(name, true).await;
    fixture.cfg.operator.rotation_enabled = false;
    let operator = fixture.operator(true).await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    fixture.registry.source(HandoverSource {
        candidate: AccountId([0x0b; 20]),
        release: [0x0b; 32],
        api_hostname: operator.host.clone(),
        handover_endpoint: listener.local_addr().unwrap().to_string(),
    });
    fixture.registry.draw(fixture.release().id());
    let mut keys = KeySet::for_key(&EnclaveKey::test_key()).unwrap();
    keys.predecessor_head = Some(crate::tee::keys::Head { seq: 99, root: format!("0x{}", "ab".repeat(32)) });
    let domain =
        CheckpointDomain { chain_id: fixture.cfg.custody.chain_id, vault: fixture.cfg.custody.vault.parse().unwrap() };
    let (served, asked) = serve_tail(listener, domain, keys, script);
    fixture.time.set(OWN + 1);
    (fixture, operator, served, asked)
}

/// The copy in `dir` is left whole for the start, whose whole-log take sets it aside
/// (`node::keys`'s tests).
fn assert_left_to_the_start(dir: &std::path::Path) {
    assert!(dir.join(MANIFEST).exists(), "the wait moved the copy");
    assert!(!dir.with_extension("aside").exists(), "the wait set the copy aside before the start's take");
}
