//! The node the load runs against, in this process, assembled as `node::serve_node` assembles one:
//! a sequencer that takes over the launch book on its own thread, with its log syncs on theirs and
//! the stats worker the API's reads go to; every feed connection the production template opens, to
//! the loopback stand-ins; trusted time, the ticker and the price monitor; and the API over TLS
//! behind its trusted PROXY protocol peer, under the template's `[api]` limits and screening slots.
//! The history index and the marks cache, which follow the stream off the sequencer's thread, do not
//! run, and nothing posts checkpoints to a chain: the sequencer writes one every 1,000 commands, as
//! the template sets, its root taken on the root worker.

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use engine_core::Engine;
use engine_core::auth::ClientAuth;
use engine_core::command::Command;
use engine_core::types::{AccountId, MarketId};
use tokio::net::TcpListener;
use tokio::sync::{Semaphore, broadcast, mpsc, watch};
use tokio::task::AbortHandle;
use tokio::time::{interval, timeout};

use crate::access::AccessClient;
use crate::api::{ApiState, MAX_BACKFILLS, Signing, router};
use crate::clock::SystemClock;
use crate::common::{CHAIN_ID, VAULT, operator};
use crate::config::{ApiSection, ProxyProtocolSection};
use crate::history::Reader;
use crate::limits::ApiLimits;
use crate::load_scenario::venues::{Venues, template};
use crate::log::Manifest;
use crate::metrics::Metrics;
use crate::sequencer::{Request, Sequencer, SequencerConfig};
use crate::sequencer_thread::Worker;
use crate::tee::{AttestationBundle, EnclaveKey, MockAttestor, attest_bundle};
use crate::test_support::{sequencer_config, trusted_now};
use crate::tls::TlsIdentity;
use crate::trusted_time::TrustedClock;

/// The template's `[node]` cadences: a checkpoint and a segment every 1,000 commands, and a tick
/// every 250 ms.
const CHECKPOINT_EVERY: u64 = 1_000;
const SEGMENT_COMMANDS: u64 = 1_000;
const TICK: Duration = Duration::from_millis(250);
/// How often trusted time is sampled, as the node's Roughtime client does.
const SAMPLE_EVERY: Duration = Duration::from_secs(10);
/// The node's request and stream channels.
const CHANNEL_CAPACITY: usize = 65_536;
/// How long the sequencer has to drain and write its final checkpoint.
const DRAIN: Duration = Duration::from_secs(120);

/// The running node.
pub(super) struct Node {
    pub tx: mpsc::Sender<Request>,
    pub metrics: Arc<Metrics>,
    /// The enclave key's address, which signs every checkpoint.
    pub signer: AccountId,
    pub log_dir: PathBuf,
    pub api: ApiSection,
    sequencer: Worker,
    tasks: Vec<AbortHandle>,
}

/// The template's `[api]`, with the loopback as the trusted PROXY protocol peer the clients come
/// through and no ACME certificate.
pub(super) fn api_section() -> ApiSection {
    let mut api = template()["api"].clone();
    let table = api.as_table_mut().expect("the template's [api]");
    table.remove("tls");
    let mut api: ApiSection = api.try_into().expect("the template's [api]");
    api.proxy_protocol = Some(ProxyProtocolSection { trusted: vec!["127.0.0.1/32".parse().expect("a range")] });
    api
}

impl Node {
    /// Starts the node on `engine`, its log in `dir`, its API on `listener`, its feeds reading
    /// `venues`' stand-ins.
    pub(super) async fn start(engine: Engine, dir: &Path, listener: TcpListener, venues: &mut Venues) -> Self {
        std::fs::create_dir_all(dir).expect("the node's directory");
        let metrics = Arc::new(Metrics::new());
        let key = Arc::new(EnclaveKey::generate());
        let auth = ClientAuth::Eip712 { chain_id: CHAIN_ID, vault: VAULT };
        let (stream, _) = broadcast::channel(CHANNEL_CAPACITY);
        let (withdrawals, _signed) = mpsc::unbounded_channel();
        let (manifest_tx, manifest) = watch::channel(Manifest::default());
        let access = Arc::new(AccessClient::new(&venues.access).expect("the screening client"));
        let cfg = SequencerConfig {
            access_policy: Some(venues.access.policy()),
            checkpoint_every: CHECKPOINT_EVERY,
            segment_commands: SEGMENT_COMMANDS,
            chain_id: CHAIN_ID,
            vault: VAULT,
            client_auth: auth.clone(),
            operator: operator(),
            metrics: metrics.clone(),
            genesis: Vec::new(),
            ..sequencer_config(dir, manifest_tx)
        };
        let mut seq = Sequencer::recover(cfg, key.clone(), stream.clone(), withdrawals).expect("an empty log");
        let stats = crate::test_support::stats();
        seq.trust(trusted_now()).expect("a trusted time sample");
        seq.take_over_checkpointed(engine).expect("the book's first checkpoint");
        seq.track_stats(stats.clone());
        seq.pipeline_log_syncs().expect("the log sync thread");
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let sequencer = Worker::start(seq.run(rx), None).expect("the sequencer's thread");

        let (samples_tx, samples) = watch::channel(Some(trusted_now()));
        let mut tasks = vec![keep_time(tx.clone(), samples_tx), tick(tx.clone())];
        let mut sources: BTreeMap<MarketId, BTreeSet<String>> = BTreeMap::new();
        for (conn, credentials, tls, timing) in std::mem::take(&mut venues.connections) {
            for symbol in &conn.symbols {
                sources.entry(symbol.market).or_default().insert(conn.venue.name().to_owned());
            }
            let feed = crate::feeds::venue::run(conn, credentials, timing, tls, tx.clone());
            tasks.push(tokio::spawn(feed).abort_handle());
        }
        let no_price_after = Duration::from_secs(5);
        let monitor = crate::monitor::run(tx.clone(), metrics.clone(), no_price_after, sources, samples.clone());
        tasks.push(tokio::spawn(monitor).abort_handle());

        let api = api_section();
        let tls = TlsIdentity::generate().expect("a TLS key");
        let bundle = AttestationBundle {
            enclave_address: key.address(),
            config_sha256: [0; 32],
            tls_public_key_sha256: tls.public_key_sha256,
        };
        let clock = Arc::new(SystemClock);
        let trusted = TrustedClock::new(clock.clone(), samples, Duration::from_secs(10));
        let limits = Arc::new(ApiLimits::new(&api).with_request_timing(metrics.latency.request.clone()));
        let state = ApiState {
            protocol: engine_core::protocol::ProtocolVersion::LATEST,
            tx: tx.clone(),
            stream,
            attestation: Arc::new(attest_bundle(&MockAttestor, &bundle).expect("a mock attestation")),
            auth,
            access: Some(access),
            operator: operator(),
            mock_custody: false,
            payouts_enabled: false,
            clock,
            log_dir: dir.to_owned(),
            manifest,
            backfills: Arc::new(Semaphore::new(MAX_BACKFILLS)),
            limits: limits.clone(),
            history: Reader::new(PathBuf::new(), watch::channel(true).1),
            marks: crate::test_support::marks_reader(),
            stats,
            emission_budget: None,
            staking_terms: None,
            portfolio_margin: None,
            status: Default::default(),
            collateral: None,
            listing: Default::default(),
            operators: None,
            operator_hostname: None,
            signing: Arc::new(Signing::new(key.clone(), CHAIN_ID, VAULT, api.heartbeat_interval, trusted)),
        };
        let serving = crate::tls::serve(listener, tls.acceptors, router(state), limits.clone(), api.clone());
        tasks.push(tokio::spawn(serving).abort_handle());
        tasks.push(tokio::spawn(crate::limits::run_retain(limits)).abort_handle());
        let signer = AccountId(key.address());
        Node { tx, metrics, signer, log_dir: dir.to_owned(), api, sequencer, tasks }
    }

    /// Stops every task, then lets the sequencer drain its queue and write its final checkpoint.
    /// Error cases:
    /// - the sequencer failed, or did not drain within `DRAIN`
    pub(super) async fn stop(self) -> anyhow::Result<()> {
        let Node { tx, sequencer, tasks, .. } = self;
        tasks.iter().for_each(AbortHandle::abort);
        // Every sender but this one went with the tasks; the sequencer drains once it goes.
        drop(tx);
        let mut sequencer = sequencer;
        let outcome = timeout(DRAIN, &mut sequencer.outcome).await.context("the sequencer did not drain in time")?;
        outcome.context("the sequencer's thread ended")?
    }
}

/// Gives the sequencer, and the API's signing, a trusted time sample every `SAMPLE_EVERY`.
fn keep_time(tx: mpsc::Sender<Request>, samples: watch::Sender<Option<crate::trusted_time::Sample>>) -> AbortHandle {
    tokio::spawn(async move {
        let mut every = interval(SAMPLE_EVERY);
        loop {
            every.tick().await;
            let sample = trusted_now();
            samples.send_replace(Some(sample));
            if tx.send(Request::TimeSample(sample)).await.is_err() {
                return;
            }
        }
    })
    .abort_handle()
}

/// Sends a tick every `TICK`, as the node's ticker does.
fn tick(tx: mpsc::Sender<Request>) -> AbortHandle {
    tokio::spawn(async move {
        let mut every = interval(TICK);
        loop {
            every.tick().await;
            if tx.send(Request::Submit { cmd: Command::Tick, reply: None }).await.is_err() {
                return;
            }
        }
    })
    .abort_handle()
}

/// The API's listener, on a free loopback port.
pub(super) async fn listen() -> (TcpListener, SocketAddr) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    (listener, addr)
}
