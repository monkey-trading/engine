//! A daily turn's handover: from the turn's boundary, by both sides' trusted time, the finalised
//! schedule's member takes the keys, sealed under the generation the schedule fixed, while the
//! finalised vault still names the incumbent's release, from its turn's source even with that
//! source's archive unreachable; and only for a turn the incumbent's log accepted, or, once it has
//! stopped, would accept.

use super::*;

use engine_core::auth::Operator;
use engine_core::eip712::B256;

use crate::checkpoints::{HandoverSource, ScheduledTurn};
use crate::sequencer::Request;
use crate::sequencer::tests::operator::{recover_turns, turn_log};
use crate::tee::keys::AcmeAccount;
use crate::trusted_time::Sample;

/// The generation the schedule fixed for the turn.
const TURN_GENERATION: u64 = 9;

/// A logged turn: the sequencer whose log accepted it, answering queries, the turn, its member's
/// key, and its boundary.
struct Logged {
    engine: mpsc::Sender<Request>,
    _run: JoinHandle<anyhow::Result<()>>,
    turn: u64,
    key: AccountId,
    boundary: i64,
}

impl Logged {
    /// A sequencer over a log that accepted a turn for its member's key, which it has yet to apply.
    fn start(name: &str) -> Self {
        let (dir, clock, turn, key) = turn_log(name);
        let operator = Operator { address: key, ..dev_config().operator().unwrap() };
        let sequencer = recover_turns(&dir, clock, operator, B256::repeat_byte(1));
        let (engine, requests) = mpsc::channel(16);
        let boundary = engine_core::turns::boundary(turn).unwrap();
        Self { engine, _run: tokio::spawn(sequencer.run(requests)), turn, key, boundary }
    }
}

impl Exchange {
    /// The finalised schedule names the successor's release for `turn` under `key` from `boundary`,
    /// while the finalised vault still names the predecessor's release.
    fn schedule_successor(&self, turn: u64, key: AccountId, boundary: i64) {
        let running = Release { launch_digest: RELEASE, config_sha256: PREDECESSOR_CONFIG };
        *self.current.lock().unwrap() = CurrentRelease { id: running.id(), generation: GENERATION };
        *self.scheduled.lock().unwrap() = Some(ScheduledTurn {
            turn,
            boundary,
            candidate: AccountId([0xb0; 20]),
            admin_key: key,
            release: self.upgrade().id(),
            generation: TURN_GENERATION,
            set: 1,
            source: None,
        });
    }

    /// The finalised schedule names the successor's release for `turn` under the candidate the log's
    /// set names, with `key`, drawn from `set`.
    fn schedule_set_member(&self, turn: u64, key: AccountId, set: u32) {
        self.schedule_successor(turn, key, engine_core::turns::boundary(turn).unwrap());
        let mut scheduled = self.scheduled.lock().unwrap();
        let scheduled = scheduled.as_mut().expect("the turn is drawn");
        (scheduled.candidate, scheduled.set) = (AccountId([0xc1; 20]), set);
    }

    /// Trusted time on both sides: the predecessor's sample and the successor's have reached `ms`.
    fn reach(&mut self, ms: i64) {
        let at = std::time::Instant::now();
        self.samples.send_replace(Some(Sample { at, earliest_ms: ms, latest_ms: ms + 1_000 }));
        self.reached = Some(ms);
    }
}

#[tokio::test]
async fn from_its_boundary_the_turns_member_takes_the_keys_under_the_schedules_generation() {
    let logged = Logged::start("handover-turn-log");
    let mut exchange = Exchange::start_logged("handover-turn", Some(logged.engine.clone())).await;
    exchange.schedule_successor(logged.turn, logged.key, logged.boundary);
    exchange.reach(logged.boundary);
    let head = write_log_tail(&exchange.successor_log, 9, &exchange.old.enclave_key().unwrap());
    let answer = exchange.answer_drain(Ok(head.clone()));
    let store = exchange.take().await.unwrap();
    assert_eq!(answer.await.unwrap(), 1, "sequencing stopped once");
    assert_eq!((store.release(), store.generation()), (exchange.upgrade(), TURN_GENERATION));
    assert!(file_for(&exchange.successor_dir, &exchange.upgrade(), TURN_GENERATION).exists());
    timeout(Duration::from_secs(5), exchange.run).await.expect("the predecessor ended").unwrap().unwrap();
}

#[tokio::test]
async fn a_turns_member_takes_the_keys_and_the_account_but_no_certificate_of_the_incumbents() {
    let logged = Logged::start("handover-turn-certificate-log");
    let mut exchange = Exchange::start_logged("handover-turn-certificate", Some(logged.engine.clone())).await;
    // The incumbent's keys carry its ACME account, and a certificate for its hostname that an earlier
    // release sealed with them.
    let account = AcmeAccount { directory_url: "https://ca.test/dir".into(), credentials: serde_json::json!({}) };
    exchange.store.update(|keys| keys.acme_account = Some(account.clone())).unwrap();
    exchange.store.update(|keys| keys.set_certificate("https://ca.test/dir", &[vec![0x5a; 48].into()])).unwrap();
    exchange.schedule_successor(logged.turn, logged.key, logged.boundary);
    exchange.reach(logged.boundary);
    let head = write_log_tail(&exchange.successor_log, 9, &exchange.old.enclave_key().unwrap());
    let _answer = exchange.answer_drain(Ok(head));
    let taken = exchange.take().await.unwrap().keys();
    assert!(taken.acme_account == Some(account), "the account did not move with the keys");
    assert_eq!(taken.certificate, None, "the incumbent's certificate moved with the keys");
}

#[tokio::test]
async fn a_turn_the_incumbents_log_has_not_accepted_gets_no_keys_and_sequencing_goes_on() {
    let logged = Logged::start("handover-turn-unlogged-log");
    let mut exchange = Exchange::start_logged("handover-turn-unlogged", Some(logged.engine.clone())).await;
    // The schedule names a key the log's assignment does not, as for a turn the engine refused.
    exchange.schedule_successor(logged.turn, AccountId([0xee; 20]), logged.boundary);
    exchange.reach(logged.boundary);
    let err = exchange.take().await.err().expect("keys left for a turn the log refused");
    assert!(format!("{err:#}").contains("has not accepted turn"), "{err:#}");
    assert!(exchange.drains.try_recv().is_err(), "sequencing stopped for a refused turn");
    assert!(!exchange.run.is_finished(), "the predecessor stopped serving");
}

#[tokio::test]
async fn before_its_boundary_the_turns_member_gets_no_keys_and_sequencing_goes_on() {
    let logged = Logged::start("handover-turn-early-log");
    let mut exchange = Exchange::start_logged("handover-turn-early", Some(logged.engine.clone())).await;
    exchange.schedule_successor(logged.turn, logged.key, logged.boundary);
    // Trusted time a second short of the boundary, on both sides.
    exchange.reach(logged.boundary - 1_000);
    let err = exchange.take().await.err().expect("keys left before the boundary");
    assert!(format!("{err:#}").contains("the vault's current release is"), "{err:#}");
    assert!(exchange.drains.try_recv().is_err(), "sequencing stopped before the boundary");
    assert!(!exchange.run.is_finished(), "the predecessor stopped serving");
    assert!(std::fs::read_dir(&exchange.successor_dir).is_err(), "the successor sealed keys");
}

#[tokio::test]
async fn once_a_turn_names_another_release_the_vaults_stale_release_gets_no_keys() {
    let logged = Logged::start("handover-turn-stale-log");
    let mut exchange = Exchange::start_logged("handover-turn-stale", Some(logged.engine.clone())).await;
    exchange.schedule_successor(logged.turn, logged.key, logged.boundary);
    // The finalised vault still names the successor's release from an earlier holding, but the turn
    // trusted time has reached names another one.
    *exchange.current.lock().unwrap() = CurrentRelease { id: exchange.upgrade().id(), generation: GENERATION };
    exchange.scheduled.lock().unwrap().as_mut().unwrap().release = [0x77; 32];
    exchange.reach(logged.boundary);
    let err = exchange.take().await.err().expect("keys left for a release the turn does not name");
    assert!(format!("{err:#}").contains("the finalised schedule names release"), "{err:#}");
    assert!(exchange.drains.try_recv().is_err(), "sequencing stopped");
}

#[tokio::test]
async fn a_successor_whose_own_time_has_not_reached_the_boundary_seals_nothing() {
    let logged = Logged::start("handover-turn-successor-early-log");
    let mut exchange = Exchange::start_logged("handover-turn-successor-early", Some(logged.engine.clone())).await;
    exchange.schedule_successor(logged.turn, logged.key, logged.boundary);
    exchange.reach(logged.boundary);
    let head = write_log_tail(&exchange.successor_log, 9, &exchange.old.enclave_key().unwrap());
    let _answer = exchange.answer_drain(Ok(head));
    // The predecessor's time has reached the boundary, the successor's has not.
    exchange.reached = Some(logged.boundary - 1);
    let err = exchange.take().await.err().expect("keys sealed before the successor's time reached the boundary");
    assert!(format!("{err:#}").contains("the vault's current release is"), "{err:#}");
    assert!(std::fs::read_dir(&exchange.successor_dir).is_err(), "the successor sealed keys");
    assert!(!exchange.run.is_finished(), "the predecessor ended without a confirmation");
}

#[tokio::test]
async fn a_turns_member_retrying_after_the_stop_is_admitted_once_the_sequencer_is_gone() {
    let Logged { engine, _run: run, turn, key, boundary } = Logged::start("handover-turn-retry-log");
    let mut exchange = Exchange::start_logged("handover-turn-retry", Some(engine.clone())).await;
    exchange.schedule_successor(turn, key, boundary);
    exchange.reach(boundary);
    let head = write_log_tail(&exchange.successor_log, 9, &exchange.old.enclave_key().unwrap());
    let answer = exchange.answer_drain(Ok(head));
    // The first attempt stops sequencing, then fails on the successor's side.
    exchange.reached = Some(boundary - 1);
    assert!(exchange.take().await.is_err(), "keys sealed before the successor's time reached the boundary");
    // The stopped node's sequencer ends; the listener holds no sender of its own to keep it.
    drop(engine);
    timeout(Duration::from_secs(5), run).await.expect("the sequencer ended").unwrap().unwrap();
    exchange.reached = Some(boundary);
    let store = exchange.take().await.expect("the retry is admitted under the turn the log accepted");
    assert_eq!((store.release(), store.generation()), (exchange.upgrade(), TURN_GENERATION));
    timeout(Duration::from_secs(5), exchange.run).await.expect("the predecessor ended").unwrap().unwrap();
    assert_eq!(answer.await.unwrap(), 1, "sequencing stopped once");
}

#[tokio::test]
async fn a_turns_member_takes_the_keys_from_its_source_while_the_sources_archive_is_unreachable() {
    let logged = Logged::start("handover-turn-source-log");
    let mut exchange = Exchange::start_logged("handover-turn-source", Some(logged.engine.clone())).await;
    exchange.schedule_successor(logged.turn, logged.key, logged.boundary);
    // The turn's source answers on its handover listener, while nothing answers on its API hostname.
    let unreachable = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().to_string();
    let source = HandoverSource {
        candidate: AccountId([0xa0; 20]),
        release: [0xa1; 32],
        api_hostname: unreachable,
        handover_endpoint: exchange.address.clone(),
    };
    let turn = {
        let mut scheduled = exchange.scheduled.lock().unwrap();
        scheduled.as_mut().expect("the turn is drawn").source = Some(source);
        scheduled.clone().unwrap()
    };
    exchange.reach(logged.boundary);
    // The member's copy from an earlier start, which the archive's refresh cannot extend.
    let head = write_log_tail(&exchange.successor_log, 9, &exchange.old.enclave_key().unwrap());
    let answer = exchange.answer_drain(Ok(head));
    let vault = Vault {
        signer: AccountId(exchange.old.enclave_key().unwrap().address()),
        current: exchange.current.clone(),
        synced: exchange.synced.clone(),
        rotation: exchange.rotation.clone(),
        scheduled: exchange.scheduled.clone(),
    };
    let at = vault.observe(View::Finalized, &[]).await.unwrap();
    let sealer = MockSealer { launch_digest: UPGRADE };
    let store = crate::handover::turns::take_keys(
        &exchange.successor,
        &vault,
        &at,
        &turn,
        &sealer,
        &MockAttestor,
        &crate::metrics::Metrics::new(),
    )
    .await
    .expect("the keys came over the source's listener");
    assert_eq!((store.release(), store.generation()), (exchange.upgrade(), TURN_GENERATION));
    assert!(file_for(&exchange.successor_dir, &exchange.upgrade(), TURN_GENERATION).exists());
    timeout(Duration::from_secs(5), exchange.run).await.expect("the predecessor ended").unwrap().unwrap();
    assert_eq!(answer.await.unwrap(), 1, "sequencing stopped once");
}

#[tokio::test]
async fn after_an_unconfirmed_handover_the_next_turns_member_is_admitted_under_the_turns_the_stop_retained() {
    let Logged { engine, _run: run, turn, key, boundary } = Logged::start("handover-turn-next-log");
    let mut exchange = Exchange::start_logged("handover-turn-next", Some(engine.clone())).await;
    exchange.schedule_successor(turn, key, boundary);
    exchange.reach(boundary);
    let head = write_log_tail(&exchange.successor_log, 9, &exchange.old.enclave_key().unwrap());
    let answer = exchange.answer_drain(Ok(head));
    // The turn's member is admitted and sequencing stops, but the transfer ends unconfirmed, and the
    // stopped node's sequencer ends.
    exchange.reached = Some(boundary - 1);
    assert!(exchange.take().await.is_err(), "keys sealed before the successor's time reached the boundary");
    drop(engine);
    timeout(Duration::from_secs(5), run).await.expect("the sequencer ended").unwrap().unwrap();

    // At a later boundary a turn the log never mirrored names the set's member: a key the set does
    // not name, a set the log never mirrored, or a turn that has ended gets nothing.
    let (ended, next) = (turn + 1, turn + 2);
    let next_boundary = engine_core::turns::boundary(next).unwrap();
    exchange.reach(next_boundary);
    exchange.schedule_set_member(ended, key, 1);
    let err = exchange.take().await.err().expect("keys left for a turn that has ended");
    assert!(format!("{err:#}").contains(&format!("has not accepted turn {ended}")), "{err:#}");
    for (what, admin_key, set) in [("a key the set does not name", AccountId([0xee; 20]), 1), ("another set", key, 2)] {
        exchange.schedule_set_member(next, admin_key, set);
        let err = exchange.take().await.err().unwrap_or_else(|| panic!("{what}: keys left for a turn the log refuses"));
        assert!(format!("{err:#}").contains(&format!("has not accepted turn {next}")), "{what}: {err:#}");
    }
    exchange.schedule_set_member(next, key, 1);
    let store = exchange.take().await.expect("the next turn's member is admitted under the retained turns");
    assert_eq!((store.release(), store.generation()), (exchange.upgrade(), TURN_GENERATION));
    timeout(Duration::from_secs(5), exchange.run).await.expect("the predecessor ended").unwrap().unwrap();
    assert_eq!(answer.await.unwrap(), 1, "sequencing stopped once");
}
