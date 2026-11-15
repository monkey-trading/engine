//! A member's wait for its turn against a stand-in registry, schedule, posted heads and trusted time,
//! stand-in operators serving the signed stream over TLS, and a stand-in archive: it follows the
//! operator in effect and moves at a turn's boundary, bootstraps an empty copy from the archive, pages
//! a posted head its copy contradicts, and holds once its keys are sealed for its turn.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::bail;
use axum::Router;
use axum::extract::ws::{Message, WebSocketUpgrade};
use axum::http::{StatusCode, Uri};
use axum::response::IntoResponse;
use axum::routing::get;
use engine_core::Fixed;
use engine_core::command::Command;
use engine_core::eip712::B256;
use engine_core::types::{AccountId, Millis};
use engine_wire::{Connection, Deployment, FRAME_VERSION, SignedFrame};
use tokio::net::TcpListener;

use super::*;
use crate::checkpoints::{CurrentRelease, HandoverSource, Observation, VaultState};
use crate::config::NodeConfig;
use crate::limits::ApiLimits;
use crate::log::{Checkpoint, Offset};
use crate::log_heads::Head;
use crate::metrics::Metrics;
use crate::node::member::Handed;
use crate::tee::keys::KeySet;
use crate::tee::sealing::{KeyStore, MockSealer, Release};
use crate::tee::{EnclaveKey, MockAttestor};
use crate::test_support::{TestSequencer, capture_alerts, dev_config, log_entries, log_messages, scratch_dir};
use crate::trusted_time::Sample;

/// This member's stand-in launch measurement, and the generation its turn's keys are sealed in.
const DIGEST: [u8; 48] = [0x5e; 48];
const GENERATION: u64 = 7;
/// The boundaries of the turns the stand-in schedule draws: another member's, a second member's,
/// then this one's.
const FIRST: Millis = 1_000_000;
const SECOND: Millis = 2_000_000;
const OWN: Millis = 3_000_000;
/// The boundary of the turn after this member's, another member's.
const AFTER: Millis = 4_000_000;

/// Under the operator in effect it follows that operator's stream; a hostname the registry moves to
/// before the boundary does not move it; at the next turn's boundary it follows the new one; once its
/// own turn's keys are sealed it holds.
#[tokio::test]
async fn a_member_follows_the_operator_in_effect_and_moves_at_the_boundary() {
    let fixture = Fixture::start("member-moves", true).await;
    let (first, second) = (fixture.operator(true).await, fixture.operator(true).await);
    fixture.registry.name(&first.host);
    fixture.time.set(FIRST + 1);
    let mut member = fixture.member();
    let drive = async {
        first.wait_for_connections(1).await;
        // The registry names another operator before the boundary: the member stays on the first.
        fixture.registry.name(&second.host);
        first.wait_for_connections(first.connections() + 2).await;
        assert_eq!(second.connections(), 0, "the member moved before the boundary");
        fixture.time.set(SECOND + 1);
        second.wait_for_connections(1).await;
        fixture.seal_own_turn();
        fixture.time.set(OWN + 1);
    };
    let (turn, ()) = tokio::join!(member.until_turn(), drive);
    assert!(matches!(turn.unwrap(), Turn::Hold), "the member did not hold its turn once its keys were sealed");
}

/// An empty copy is bootstrapped from the archive before the member follows: the stand-in stream
/// serves no history from the start of the log, so the copy holds it only through the archive.
#[tokio::test]
async fn a_member_bootstraps_an_empty_copy_from_the_archive() {
    let fixture = Fixture::start("member-bootstrap", false).await;
    let operator = fixture.operator(false).await;
    fixture.registry.name(&operator.host);
    fixture.time.set(FIRST + 1);
    let mut member = fixture.member();
    let drive = async {
        operator.wait_for_connections(2).await;
        fixture.seal_own_turn();
        fixture.time.set(OWN + 1);
    };
    let (turn, ()) = tokio::join!(member.until_turn(), drive);
    assert!(matches!(turn.unwrap(), Turn::Hold));
    assert_eq!(log_entries(&fixture.cfg.node.log_dir), log_entries(&fixture.log), "the copy is not the operator's log");
}

/// A posted log head naming another history than the copy holds at its seq pages Critical
/// `operator_log_rollback`; one the copy holds pages nothing.
#[tokio::test]
async fn a_member_pages_a_posted_head_its_copy_contradicts() {
    let (_guard, mut alerts) = capture_alerts();
    let fixture = Fixture::start("member-posted", true).await;
    let operator = fixture.operator(true).await;
    fixture.registry.name(&operator.host);
    fixture.time.set(FIRST + 1);
    let held = crate::sequencer::replay_path(&fixture.log).unwrap().prefix_ring;
    let seq = 3;
    *fixture.heads.0.lock().unwrap() = Some(Head { seq, link: held.prefix_at(seq).unwrap() });
    let mut member = fixture.member();
    let drive = async {
        operator.wait_for_connections(2).await;
        assert!(
            std::iter::from_fn(|| alerts.try_recv().ok()).all(|alert| alert["alert"] != "operator_log_rollback"),
            "a head the copy holds paged"
        );
        *fixture.heads.0.lock().unwrap() = Some(Head { seq, link: B256::repeat_byte(0x99) });
        let paged = crate::test_support::wait_for("the rollback page", async {
            loop {
                match alerts.recv().await {
                    Some(alert) if alert["alert"] == "operator_log_rollback" => break alert,
                    Some(_) => {}
                    None => panic!("the alert channel closed"),
                }
            }
        })
        .await;
        assert_eq!((paged["level"].as_str(), paged["seq"].as_str()), ("ERROR", seq.to_string().as_str()));
        fixture.seal_own_turn();
        fixture.time.set(OWN + 1);
    };
    let (turn, ()) = tokio::join!(member.until_turn(), drive);
    assert!(matches!(turn.unwrap(), Turn::Hold));
}

/// At its own turn's boundary, with no copy to take a tail onto and no keys sealed for it, the member
/// starts as a node with no copy does, without following anyone.
#[tokio::test]
async fn a_member_detects_its_turn_and_with_no_copy_starts_without_following() {
    let fixture = Fixture::start("member-own-turn", false).await;
    let operator = fixture.operator(true).await;
    fixture.registry.name(&operator.host);
    fixture.registry.draw(fixture.release().id());
    fixture.time.set(OWN + 1);
    let mut member = fixture.member();
    let turn = crate::test_support::wait_for("the member to stop for its own turn", member.until_turn()).await;
    assert!(matches!(turn.unwrap(), Turn::Hold));
    assert_eq!(operator.connections(), 0, "the member followed at its own turn");
    assert!(!file_for(&fixture.keys, &fixture.release(), GENERATION).exists(), "keys were sealed");
}

/// A member that handed its turn over never holds it again: a later trusted time sample just below
/// the successor's boundary, in its own turn, whose keys it still has sealed, does not give it that
/// turn back; and the generation it gave away keeps it off those keys even with no floor.
#[tokio::test]
async fn a_member_that_handed_its_turn_over_follows_on_a_sample_below_the_boundary() {
    let fixture = Fixture::start("member-handed", true).await;
    let operator = fixture.operator(true).await;
    fixture.registry.name(&operator.host);
    fixture.seal_own_turn();
    fixture.time.set(AFTER - 5_000);
    let mut member = fixture.member();
    member.handed_over(Handed { generation: Some(GENERATION), reached: Some(AFTER + 1) });
    tokio::select! {
        _ = member.until_turn() => panic!("the member came back to the turn it handed over"),
        () = operator.wait_for_connections(1) => {}
    }

    let mut member = fixture.member();
    member.handed_over(Handed { generation: Some(GENERATION), reached: None });
    let followed = operator.connections() + 1;
    tokio::select! {
        _ = member.until_turn() => panic!("the member held the generation it handed over"),
        () = operator.wait_for_connections(followed) => {}
    }
}

/// While a new turn's member takes the keys and comes up, a member following it expects failed
/// stints and pages nothing; once `COMING_UP` has passed, a failure pages `standby_follow_failed`.
#[tokio::test]
async fn a_member_pages_no_follow_failure_while_the_turns_member_comes_up() {
    let (_guard, mut alerts) = capture_alerts();
    let fixture = Fixture::start("member-coming-up", true).await;
    fixture.registry.name("127.0.0.1:9");
    fixture.time.set(SECOND + 1);
    let mut member = fixture.member();
    let drive = async {
        tokio::time::sleep(STINT * 2).await;
        let paged =
            std::iter::from_fn(|| alerts.try_recv().ok()).any(|alert| alert["alert"] == "standby_follow_failed");
        assert!(!paged, "a failure while the turn's member came up paged");
        fixture.time.set(SECOND + COMING_UP + 1);
        crate::test_support::wait_for("the follow failure's page", async {
            loop {
                match alerts.recv().await {
                    Some(alert) if alert["alert"] == "standby_follow_failed" => break,
                    Some(_) => {}
                    None => panic!("the alert channel closed"),
                }
            }
        })
        .await;
    };
    tokio::select! {
        _ = member.until_turn() => panic!("the member stopped following"),
        () = drive => {}
    }
}

mod episodes;
mod stop;
mod take;

// ── Helpers ──

/// The stand-ins a member reads, and its config.
struct Fixture {
    cfg: NodeConfig,
    /// The operator's log, which its stand-ins serve.
    log: std::path::PathBuf,
    /// This member's sealed keys.
    keys: std::path::PathBuf,
    registry: Registry,
    heads: Heads,
    time: Time,
}

impl Fixture {
    /// A member whose copy holds the operator's log as it stood, when `copied`, or nothing, with the
    /// archive of the operator's log as its mirror.
    async fn start(name: &str, copied: bool) -> Self {
        let log = scratch_dir(&format!("{name}-operator"));
        let node = TestSequencer::start(&log, 1024);
        for _ in 0..12 {
            node.submit(Command::Tick).await;
        }
        node.stop().await;
        let copy = scratch_dir(&format!("{name}-copy"));
        if copied {
            std::fs::create_dir_all(&copy).unwrap();
            for file in std::fs::read_dir(&log).unwrap().map(Result::unwrap) {
                std::fs::copy(file.path(), copy.join(file.file_name())).unwrap();
            }
        }
        let keys = scratch_dir(&format!("{name}-keys"));
        let mirror = archive(&log).await;
        let mut cfg = dev_config();
        cfg.operator.rotation_enabled = true;
        cfg.tee.use_published_mock_key = false;
        cfg.tee.sealing = Some(
            toml::from_str(&format!(
                "dir = {:?}\n[handover]\nlisten = '127.0.0.1:0'\nmirrors = [{mirror:?}]\ndanger_accept_invalid_certs = true",
                keys.display().to_string()
            ))
            .unwrap(),
        );
        cfg.node.log_dir = copy;
        cfg.history.dir = scratch_dir(&format!("{name}-history"));
        let registry = Registry::default();
        Self { cfg, log, keys, registry, heads: Heads::default(), time: Time::default() }
    }

    /// The member, reading the stand-ins.
    fn member(&self) -> Member<'_, Registry, Heads, Time> {
        let sealer = MockSealer { launch_digest: DIGEST };
        Member {
            cfg: &self.cfg,
            metrics: Arc::new(Metrics::new()),
            vault: self.registry.clone(),
            heads: Some(self.heads.clone()),
            time: self.time.clone(),
            sealer: Box::new(sealer),
            attestor: Box::new(MockAttestor),
            release: self.release(),
            sample: None,
            start: crate::node::KeyStart { mock_launch_digest: Some(DIGEST), ..Default::default() },
            handed: None,
            episodes: Default::default(),
            stopping: Default::default(),
            checked: Default::default(),
            answering: None,
        }
    }

    /// This member's release.
    fn release(&self) -> Release {
        Release { launch_digest: DIGEST, config_sha256: self.cfg.file_sha256 }
    }

    /// A stand-in operator serving the operator's log over TLS: from the start of the log too, when
    /// `from_start`.
    async fn operator(&self, from_start: bool) -> Operator {
        Operator::start(self.log.clone(), from_start).await
    }

    /// Seals this member's keys in its turn's generation, as its earlier holding left them.
    fn seal_own_turn(&self) {
        let sealer = MockSealer { launch_digest: DIGEST };
        KeyStore::create(&self.keys, &sealer, self.cfg.file_sha256, GENERATION, KeySet::generate().unwrap()).unwrap();
        self.registry.draw(self.release().id());
    }
}

/// The registry, the schedule and the vault, as the member reads them: two turns drawn for other
/// members, then, once drawn, this member's, taken over from `source` when set, the turn before it
/// from `prior` when set.
#[derive(Clone, Default)]
struct Registry {
    hostname: Arc<Mutex<String>>,
    own: Arc<Mutex<Option<[u8; 32]>>>,
    source: Arc<Mutex<Option<HandoverSource>>>,
    prior: Arc<Mutex<Option<HandoverSource>>>,
}

impl Registry {
    /// Names `host` as the operator in effect's API hostname.
    fn name(&self, host: &str) {
        *self.hostname.lock().unwrap() = host.into();
    }

    /// Names `source` as the member this member's turn takes over from.
    fn source(&self, source: HandoverSource) {
        *self.source.lock().unwrap() = Some(source);
    }

    /// Names `source` as the member the turn before this member's took over from.
    fn prior(&self, source: HandoverSource) {
        *self.prior.lock().unwrap() = Some(source);
    }

    /// Draws this member's turn, for `release`.
    fn draw(&self, release: [u8; 32]) {
        *self.own.lock().unwrap() = Some(release);
    }

    /// The drawn turns: two other members', this one's once drawn, and another member's after it.
    fn turns(&self) -> Vec<ScheduledTurn> {
        let turn = |turn, boundary, release, generation| ScheduledTurn {
            turn,
            boundary,
            candidate: AccountId([turn as u8; 20]),
            admin_key: AccountId([turn as u8; 20]),
            release,
            generation,
            set: 1,
            source: None,
        };
        let own = self.own.lock().unwrap().unwrap_or([0x0b; 32]);
        let own = ScheduledTurn { source: self.source.lock().unwrap().clone(), ..turn(12, OWN, own, GENERATION) };
        let after = turn(13, AFTER, [0x0d; 32], GENERATION + 1);
        let second = ScheduledTurn { source: self.prior.lock().unwrap().clone(), ..turn(11, SECOND, [0x0b; 32], 2) };
        vec![turn(10, FIRST, [0x0a; 32], 1), second, own, after]
    }
}

impl CheckpointChain for Registry {
    async fn state(&self, _view: View) -> anyhow::Result<VaultState> {
        Ok(VaultState {
            height: 1,
            signer: AccountId(EnclaveKey::test_key().address()),
            checkpoint: None,
            balance: Fixed::ZERO,
            deposits: 0,
            deposited: Fixed::ZERO,
            distributions_paid: Fixed::ZERO,
        })
    }

    async fn post(&self, _checkpoint: &Checkpoint) -> anyhow::Result<()> {
        bail!("this vault takes no checkpoints")
    }

    async fn current_release(&self, _at: &Observation) -> anyhow::Result<CurrentRelease> {
        Ok(CurrentRelease { id: [0x0a; 32], generation: 1 })
    }

    async fn operator_hostname(&self) -> anyhow::Result<String> {
        Ok(self.hostname.lock().unwrap().clone())
    }

    async fn scheduled_turn(&self, _at: &Observation, now_ms: Millis) -> anyhow::Result<Option<ScheduledTurn>> {
        Ok(self.turns().into_iter().filter(|turn| turn.boundary <= now_ms).max_by_key(|turn| turn.boundary))
    }
}

/// The latest posted log head.
#[derive(Clone, Default)]
struct Heads(Arc<Mutex<Option<Head>>>);

impl PostedHead for Heads {
    async fn posted_head(&self) -> anyhow::Result<Option<Head>> {
        Ok(*self.0.lock().unwrap())
    }
}

/// Trusted time, as the test sets it.
#[derive(Clone, Default)]
struct Time(Arc<Mutex<Millis>>);

impl Time {
    fn set(&self, ms: Millis) {
        *self.0.lock().unwrap() = ms;
    }
}

impl Sampling for Time {
    async fn sample(&self) -> anyhow::Result<Sample> {
        let ms = *self.0.lock().unwrap();
        Ok(Sample { at: std::time::Instant::now(), earliest_ms: ms, latest_ms: ms + 1_000 })
    }
}

/// A stand-in operator: the operator's log, signed as its stream signs it, over TLS on loopback.
struct Operator {
    host: String,
    connected: Arc<AtomicUsize>,
}

impl Operator {
    /// Serves `/v1/stream` from the log in `log`, closing each connection once it has sent the entries
    /// from the requested seq; a request from seq 0 gets nothing unless `from_start`.
    async fn start(log: std::path::PathBuf, from_start: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let host = listener.local_addr().unwrap().to_string();
        let connected = Arc::new(AtomicUsize::new(0));
        let (counted, from_start) = (connected.clone(), Arc::new(AtomicBool::new(from_start)));
        let app = Router::new().route(
            "/v1/stream",
            get(move |ws: WebSocketUpgrade, uri: Uri| {
                counted.fetch_add(1, Ordering::SeqCst);
                let (log, from_start) = (log.clone(), from_start.load(Ordering::SeqCst));
                async move { ws.on_upgrade(move |socket| stream(socket, uri, log, from_start)) }
            }),
        );
        let api = dev_config().api;
        let (tls, limits) = (crate::tls::TlsIdentity::generate().unwrap(), Arc::new(ApiLimits::new(&api)));
        tokio::spawn(async move { crate::tls::serve(listener, tls.acceptors, app, limits, api).await });
        Self { host, connected }
    }

    fn connections(&self) -> usize {
        self.connected.load(Ordering::SeqCst)
    }

    /// Waits until the stand-in has taken `count` connections.
    async fn wait_for_connections(&self, count: usize) {
        crate::test_support::wait_for("the member's connections", async {
            while self.connections() < count {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await;
    }
}

/// Sends the log's entries from the seq the request names, each frame signed by the test key and
/// bound to the request, then closes.
async fn stream(mut socket: axum::extract::ws::WebSocket, uri: Uri, log: std::path::PathBuf, from_start: bool) {
    let path = uri.path_and_query().map(|query| query.as_str().to_owned()).unwrap_or_default();
    let field = |name: &str| path.split(['?', '&']).find_map(|part| part.strip_prefix(name)).map(str::to_owned);
    let challenge = engine_wire::parse_challenge(&field("challenge=").unwrap()).unwrap();
    let from: u64 = field("from=").unwrap().parse().unwrap();
    let connection = Connection::new(challenge, &path);
    let deployment = Deployment { chain_id: 31_337, vault: [0; 20] };
    let key = EnclaveKey::test_key();
    let messages = if from > 0 || from_start { log_messages(&log, Offset::start_of(from)) } else { Vec::new() };
    let mut previous = [0u8; 32];
    for (n, msg) in messages.into_iter().enumerate() {
        let time = 1_700_000_000_000u64;
        let digest = engine_wire::frame_digest(&deployment, &connection, &previous, n as u64, time, &msg);
        let sig = key.sign_digest(digest);
        previous = digest;
        let frame = serde_json::to_string(&SignedFrame { v: FRAME_VERSION, n: n as u64, time, msg, sig }).unwrap();
        if socket.send(Message::Text(frame.into())).await.is_err() {
            return;
        }
    }
    let _ = socket.send(Message::Close(None)).await;
}

/// Serves the completed segments of the log in `log` and their manifest as an archive mirror, over
/// plain HTTP on loopback: its base URL.
async fn archive(log: &std::path::Path) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let log = log.to_owned();
    let app = Router::new().route(
        "/v1/archive/{file}",
        get(move |axum::extract::Path(file): axum::extract::Path<String>| {
            let path = log.join(&file);
            async move {
                match std::fs::read(path) {
                    Ok(bytes) => bytes.into_response(),
                    Err(_) => StatusCode::NOT_FOUND.into_response(),
                }
            }
        }),
    );
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    url
}
