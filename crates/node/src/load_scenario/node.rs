//! The node the load runs against, in this process, assembled as `node::serve_node` assembles one:
//! a sequencer that takes over the launch book on its own thread, with its log syncs on theirs and
//! the stats worker the API's reads go to; every feed connection the production template opens, to
//! the loopback stand-ins; trusted time, the ticker and the price monitor; and the API over TLS
//! behind its trusted PROXY protocol peer, under the template's `[api]` limits and screening slots.
//! The marks cache, which follows the stream off the sequencer's thread, does not run, and nothing
//! posts checkpoints to a chain: the sequencer writes one every 1,000 commands, as the template sets,
//! its root taken on the root worker. With a `Durable` the node also runs as one with `[snapshots]`
//! does (`durable`): its history owner publishes the paired recovery caches, each checkpoint is
//! reported final at once as a chain that keeps up would, and a stopped node starts again from its
//! newest cache. With a `Durable` that runs the launch configuration (`archived`), it runs as one with
//! `[daily_snapshots]` and `[archive]`: its log's ranges and its snapshots go to a stand-in bucket, its log's
//! local window and its older snapshots leave the host, and a stopped node starts again from its newest daily
//! snapshot and the log after it.

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Context;
use engine_core::Engine;
use engine_core::auth::ClientAuth;
use engine_core::command::Command;
use engine_core::types::{AccountId, MarketId};
use tokio::net::TcpListener;
use tokio::sync::{Semaphore, broadcast, mpsc, oneshot, watch};
use tokio::task::AbortHandle;
use tokio::time::{interval, timeout};

use crate::access::AccessClient;
use crate::api::{ApiState, MAX_BACKFILLS, ServedSnapshots, Signing, router};
use crate::clock::SystemClock;
use crate::common::{CHAIN_ID, VAULT, operator};
use crate::config::{ApiSection, ProxyProtocolSection};
use crate::history::Reader;
use crate::limits::ApiLimits;
use crate::load_scenario::archived::StorageTask;
use crate::load_scenario::durable::{self, Durable, Owner};
use crate::load_scenario::indexed;
use crate::load_scenario::venues::{Venues, template};
use crate::log::Manifest;
use crate::metrics::Metrics;
use crate::sequencer::pipeline::SyncEnd;
use crate::sequencer::{Request, Sequencer, SequencerConfig};
use crate::sequencer_thread::Worker;
use crate::stats::Stats;
use crate::tee::{AttestationBundle, EnclaveKey, MockAttestor, attest_bundle};
use crate::test_support::{sequencer_config, trusted_now};
use crate::tls::TlsIdentity;
use crate::trusted_time::TrustedClock;

/// The template's `[node]` cadences: a checkpoint every 1,000 commands, a segment every 10,000, and a
/// tick every 250 ms.
const CHECKPOINT_EVERY: u64 = 1_000;
const SEGMENT_COMMANDS: u64 = 10_000;
const TICK: Duration = Duration::from_millis(250);
/// How often trusted time is sampled, as the node's Roughtime client does.
const SAMPLE_EVERY: Duration = Duration::from_secs(10);
/// The node's request and stream channels.
const CHANNEL_CAPACITY: usize = 65_536;
/// How long the sequencer has to drain and write its final checkpoint.
const DRAIN: Duration = Duration::from_secs(120);
/// How long the log-sync thread of a stopped node has to finish what it was handed.
const SYNC_END: Duration = Duration::from_secs(60);

/// How a node begins: by taking over the launch book, or by recovering the log a node of the same run
/// wrote.
pub(super) enum Boot {
    Book(Box<Engine>),
    Recover,
}

/// The running node.
pub(super) struct Node {
    pub tx: mpsc::Sender<Request>,
    pub metrics: Arc<Metrics>,
    /// The dashboard record the stats worker keeps.
    pub stats: Stats,
    /// The enclave key's address, which signs every checkpoint.
    pub signer: AccountId,
    pub log_dir: PathBuf,
    pub api: ApiSection,
    /// While set, trusted time takes no sample, as when the node's Roughtime servers are out.
    pub time_out: Arc<AtomicBool>,
    sequencer: Worker,
    /// Whether the log-sync thread has ended, which a stopped node waits for before anything reopens the log.
    sync_end: Option<SyncEnd>,
    history: Option<Owner>,
    tasks: Vec<AbortHandle>,
    /// The storage tasks of the launch configuration: the archive's uploader, the log's local window, the
    /// snapshots' uploader and the disk watcher.
    storage: Vec<StorageTask>,
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
    /// Starts the node as `boot` says, its log in `dir`, its API on `listener`, its feeds reading
    /// `venues`' stand-ins, with the paired caches and checkpoint reports of `durable` when it is given.
    pub(super) async fn start(
        boot: Boot,
        dir: &Path,
        listener: TcpListener,
        venues: &Venues,
        durable: Option<&Durable>,
    ) -> Self {
        std::fs::create_dir_all(dir).expect("the node's directory");
        let metrics = Arc::new(Metrics::new());
        let key = durable.map_or_else(|| Arc::new(EnclaveKey::generate()), |durable| durable.key.clone());
        let auth = ClientAuth::Eip712 { chain_id: CHAIN_ID, vault: VAULT };
        let (stream, _) = broadcast::channel(CHANNEL_CAPACITY);
        let (withdrawals, _signed) = mpsc::unbounded_channel();
        let (manifest_tx, manifest) = watch::channel(Manifest::default());
        let access = Arc::new(AccessClient::new(&venues.access).expect("the screening client"));
        let launch = durable.is_some_and(|durable| durable.archived.is_some());
        let (cfg, reports) = match durable {
            // As `recovery_scale`'s node, the config a recovery checks against the book.
            Some(durable) => {
                let (cfg, reports) = Durable::sequencer(dir, manifest_tx);
                let daily_snapshots = durable.daily_snapshots().map(Path::to_owned);
                (SequencerConfig { metrics: metrics.clone(), daily_snapshots, ..cfg }, Some(reports))
            }
            None => {
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
                (cfg, None)
            }
        };
        let stats = crate::test_support::stats();
        let recovering = matches!(boot, Boot::Recover);
        let (mut seq, history) = match durable {
            None => {
                let seq = Sequencer::recover(cfg, key.clone(), stream.clone(), withdrawals).expect("an empty log");
                (seq, None)
            }
            Some(durable) if launch => {
                let seq = durable::open_launch(durable, cfg, (&stats, recovering), &stream, withdrawals)
                    .await
                    .expect("the log opens from its newest daily snapshot");
                let index =
                    indexed::open(durable, dir, seq.last_seq(), &stream).await.expect("the history index opens");
                (seq, Some(index))
            }
            Some(durable) => {
                let (seq, history) = durable::open(durable, cfg, (&stats, recovering), &stream, withdrawals)
                    .expect("the paired log opens");
                (seq, Some(history))
            }
        };
        seq.trust(trusted_now()).expect("a trusted time sample");
        // A node that publishes daily snapshots tracks the record from before its first command, so the fold holds
        // every entry of the log.
        let tracked = match boot {
            Boot::Book(engine) if launch => {
                seq.take_over_tracked(*engine, stats.clone()).expect("the book's first checkpoint");
                true
            }
            Boot::Book(engine) => {
                seq.take_over_checkpointed(*engine).expect("the book's first checkpoint");
                false
            }
            Boot::Recover => false,
        };
        if !tracked {
            seq.track_stats(stats.clone());
        }
        seq.pipeline_log_syncs().expect("the log sync thread");
        let sync_end = seq.log_sync_end();
        let history = history.map(|history| history.run(&seq, dir, &manifest));
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let sequencer = Worker::start(seq.run(rx), None).expect("the sequencer's thread");
        if let Some(reports) = reports {
            let durable = durable.expect("a durable node");
            durable::report_final(reports, tx.downgrade(), durable.finalized.clone(), durable.archived.clone());
        }

        let time_out = Arc::new(AtomicBool::new(false));
        let (samples_tx, samples) = watch::channel(Some(trusted_now()));
        let mut tasks = vec![keep_time(tx.clone(), samples_tx, time_out.clone()), tick(tx.clone())];
        let mut sources: BTreeMap<MarketId, BTreeSet<String>> = BTreeMap::new();
        for (conn, credentials, tls, timing) in venues.connections.clone() {
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
        // The API's screening decisions, refreshed for active traders as the node refreshes them.
        let domain = crate::access::screening_domain(&auth, &operator());
        tasks.push(tokio::spawn(access.clone().refresh(domain, clock.clone())).abort_handle());
        let trusted = TrustedClock::new(clock.clone(), samples, Duration::from_secs(10));
        let limits = Arc::new(ApiLimits::new(&api).with_timing(&metrics.latency));
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
            daily_snapshots: durable
                .filter(|_| launch)
                .and_then(|durable| ServedSnapshots::for_config(&durable.cfg).expect("the snapshots' bucket")),
            manifest: manifest.clone(),
            backfills: Arc::new(Semaphore::new(MAX_BACKFILLS)),
            limits: limits.clone(),
            history: Reader::new(PathBuf::new(), watch::channel(true).1),
            marks: crate::test_support::marks_reader(),
            stats: stats.clone(),
            emission_budget: None,
            staking_terms: None,
            status: Default::default(),
            collateral: None,
            listing: Default::default(),
            archive_objects: durable
                .filter(|_| launch)
                .and_then(|durable| crate::archive::Bucket::for_config(&durable.cfg).expect("the archive's bucket")),
            operators: None,
            operator_hostname: None,
            signing: Arc::new(Signing::new(key.clone(), CHAIN_ID, VAULT, api.heartbeat_interval, trusted)),
        };
        let serving = crate::tls::serve(listener, tls.acceptors, router(state), limits.clone(), api.clone());
        tasks.push(tokio::spawn(serving).abort_handle());
        tasks.push(tokio::spawn(crate::limits::run_retain(limits)).abort_handle());
        let signer = AccountId(key.address());
        let storage = match durable.and_then(|durable| durable.archived.as_ref().map(|a| (durable, a))) {
            Some((durable, archived)) => archived.run_beside(&durable.cfg, manifest, &metrics),
            None => Vec::new(),
        };
        Node {
            tx,
            metrics,
            stats,
            signer,
            log_dir: dir.to_owned(),
            api,
            time_out,
            sequencer,
            sync_end,
            history,
            tasks,
            storage,
        }
    }

    /// Stops every task, then lets the sequencer drain its queue and write its final checkpoint.
    /// Error cases:
    /// - the sequencer failed, or did not drain within `DRAIN`
    /// - the history owner failed, or with `publishes` did not take the final cache in time: a stop that
    ///   nothing starts after leaves the owner to end its turn
    pub(super) async fn stop(self, publishes: bool) -> anyhow::Result<()> {
        let Node { tx, sequencer, sync_end, tasks, history, storage, .. } = self;
        tasks.iter().for_each(AbortHandle::abort);
        storage.iter().for_each(|(_, task)| task.abort());
        // Every sender but this one went with the tasks; the sequencer drains once it goes.
        drop(tx);
        let mut sequencer = sequencer;
        let outcome = timeout(DRAIN, &mut sequencer.outcome).await.context("the sequencer did not drain in time")?;
        outcome.context("the sequencer's thread ended")??;
        log_sync_ended(sync_end).await?;
        match history {
            Some(history) if publishes => history.stop().await,
            Some(history) => history.abort().await,
            None => Ok(()),
        }
    }

    /// How many commands the history index is behind the log, when the owner answers.
    pub(super) async fn history_behind(&self) -> Option<u64> {
        self.history.as_ref()?.behind().await
    }

    /// The storage tasks of the launch configuration that have ended, by what they are: each runs for the node's life.
    pub(super) fn storage_ended(&self) -> Vec<&'static str> {
        self.storage.iter().filter(|(_, task)| task.is_finished()).map(|(name, _)| *name).collect()
    }

    /// Whether the history owner runs: it publishes the recovery caches, or with the launch configuration takes the log.
    pub(super) fn history_runs(&self) -> bool {
        self.history.as_ref().is_some_and(Owner::takes_captures)
    }

    /// Drops the node as a crash does: the sequencer's thread is cancelled where it stands, with no
    /// drain, no final checkpoint and no shutdown cache, and the history owner stopped as it is, so the
    /// newest cache is the last capture and a start replays the log past it.
    /// Error cases:
    /// - the sequencer's thread did not end within `DRAIN`
    pub(super) async fn kill(self) -> anyhow::Result<()> {
        let Node { tx, mut sequencer, sync_end, tasks, history, storage, .. } = self;
        tasks.iter().for_each(AbortHandle::abort);
        storage.iter().for_each(|(_, task)| task.abort());
        let outcome = std::mem::replace(&mut sequencer.outcome, oneshot::channel().1);
        // Dropping the worker cancels the sequencer at its next async boundary. This node's own sender stays
        // until it has ended, so the queue cannot close, and the sequencer drain, before the cancel lands.
        drop(sequencer);
        let ended = timeout(DRAIN, outcome).await.context("the cancelled sequencer did not end in time")?;
        drop(tx);
        stopped_without_failing(&ended)?;
        log_sync_ended(sync_end).await?;
        match history {
            Some(history) => history.abort().await,
            None => Ok(()),
        }
    }
}

/// Waits for the log-sync thread to end, which the batches it was handed are written before.
/// Error cases:
/// - the thread has not ended within `SYNC_END`
async fn log_sync_ended(end: Option<SyncEnd>) -> anyhow::Result<()> {
    let Some(end) = end else { return Ok(()) };
    let ended = async {
        while !end.ended() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    timeout(SYNC_END, ended).await.context("the log-sync thread did not end in time")
}

/// Whether a sequencer's outcome is a stop and not a failure: the worker's own cancellation, or a clean end,
/// which a sequencer busy in a synchronous step reaches when the cancel and a closing queue land together.
/// Error cases:
/// - the sequencer failed before it was stopped, or its outcome was lost
fn stopped_without_failing(ended: &Result<anyhow::Result<()>, oneshot::error::RecvError>) -> anyhow::Result<()> {
    match ended {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) if format!("{error:#}").contains("cancelled after supervisor") => Ok(()),
        other => anyhow::bail!("the sequencer did not stop cleanly: {other:?}"),
    }
}

/// Gives the sequencer, and the API's signing, a trusted time sample every `SAMPLE_EVERY`, but while
/// `out` is set.
fn keep_time(
    tx: mpsc::Sender<Request>,
    samples: watch::Sender<Option<crate::trusted_time::Sample>>,
    out: Arc<AtomicBool>,
) -> AbortHandle {
    tokio::spawn(async move {
        let mut every = interval(SAMPLE_EVERY);
        loop {
            every.tick().await;
            if out.load(Ordering::Relaxed) {
                continue;
            }
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

#[cfg(test)]
mod tests {
    use anyhow::anyhow;

    use super::*;

    #[test]
    fn a_sequencer_that_was_cancelled_or_ended_cleanly_stopped_and_any_other_end_did_not() {
        let cancelled = Err(anyhow!("sequencer cancelled after supervisor stopped"));
        assert!(stopped_without_failing(&Ok(cancelled)).is_ok(), "the worker's cancellation");
        assert!(stopped_without_failing(&Ok(Ok(()))).is_ok(), "a drain that beat the cancel");
        let failed = Err(anyhow!("syncing the log: the disk failed"));
        assert!(stopped_without_failing(&Ok(failed)).is_err(), "a failure before the kill");
        let (gone, outcome) = oneshot::channel::<anyhow::Result<()>>();
        drop(gone);
        assert!(stopped_without_failing(&outcome.blocking_recv()).is_err(), "an outcome that never came");
    }
}
