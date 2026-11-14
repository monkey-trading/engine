//! A rotation-set member and the node it runs while it holds a turn: sealed keys under the stand-in
//! for sealing, the real handover listener (`Predecessor`) bound to the endpoint its candidacy
//! registers, its API over TLS at the hostname its candidacy registers, and the node's answer to the
//! listener's stop request, as a node answers it (`node::shutdown`): a final checkpoint through the
//! sequencer, then the drained head. Between its holdings the member runs its own wait for its turn
//! (`node::member`) on the scenario's clock: it follows the operator in effect into a live replica and
//! takes only the tail past its tip.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use alloy::primitives::Address;
use engine_core::eip712::B256;
use engine_core::types::AccountId;
use k256::ecdsa::SigningKey;
use tokio::sync::{Notify, mpsc, oneshot};
use tokio::task::JoinHandle;

use super::super::CONFIG_HASH;
use super::super::chain::Chain;
use super::super::node::{Node, SEAT_SEGMENT_COMMANDS, Seat, custody_section};
use crate::checkpoints::staking_scenario::access::Source;
use crate::clock::Clock;
use crate::clock::test_clock::TestClock;
use crate::config::{CustodyProvider, NodeConfig};
use crate::handover::{DrainReply, Predecessor};
use crate::metrics::Metrics;
use crate::node::member::{Member as Waiting, Sampling, Turn};
use crate::node::ready::{Moved, Start};
use crate::sequencer::Request;
use crate::tee::MockAttestor;
use crate::tee::sealing::{KeyStore, MockSealer, Release};
use crate::test_support::{dev_config, scratch_dir};
use crate::trusted_time::Sample;

/// A registered candidate: its admin key, which its node's config names as the operator key, the
/// release it runs, its handover listener and API, the account its checkpoint posts are sent from, and
/// the directories its node keeps across holdings.
#[derive(Clone)]
pub(super) struct Member {
    pub(super) name: &'static str,
    pub(super) candidate: Address,
    pub(super) key: SigningKey,
    pub(super) digest: [u8; 48],
    pub(super) endpoint: String,
    pub(super) api: SocketAddr,
    pub(super) dir: PathBuf,
    /// Its own, so no member's pending post holds another's, and the same whether its node holds a
    /// turn or takes one, since a take reads the submission its last holding left.
    pub(super) relayer: Address,
    keys: PathBuf,
}

impl Member {
    /// `name`, as `candidate` with the admin key `key` running `digest`, posting from `relayer`, on free
    /// handover and API ports.
    pub(super) fn new(
        name: &'static str,
        candidate: Address,
        (key, digest): ([u8; 32], [u8; 48]),
        relayer: Address,
    ) -> Self {
        let free = || std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
        Self {
            name,
            candidate,
            key: SigningKey::from_slice(&key).unwrap(),
            digest,
            relayer,
            endpoint: free().to_string(),
            api: free(),
            dir: scratch_dir(&format!("daily-turns-{name}")),
            keys: scratch_dir(&format!("daily-turns-{name}-keys")),
        }
    }

    /// The admin key's address.
    pub(super) fn operator(&self) -> AccountId {
        engine_core::eip712::address(self.key.verifying_key())
    }

    /// The release the registry records for it.
    pub(super) fn release(&self) -> Release {
        Release { launch_digest: self.digest, config_sha256: CONFIG_HASH }
    }

    /// The API hostname its candidacy registers: where its node serves while it holds a turn.
    pub(super) fn hostname(&self) -> String {
        self.api.to_string()
    }

    /// Seals `keys` for its release in `generation`, as a node's first start does.
    pub(super) fn seal(&self, keys: crate::tee::keys::KeySet, generation: u64) -> KeyStore {
        let sealer = MockSealer { launch_digest: self.digest };
        KeyStore::create(&self.keys, &sealer, CONFIG_HASH, generation, keys).unwrap()
    }

    /// The config its node runs: rotation on, sealing with a handover listener at its endpoint that
    /// accepts the operator's self-signed certificate, its log and history, the chain's custody and
    /// vault, and the config hash its release names.
    pub(super) fn config(&self, chain: &Chain) -> NodeConfig {
        let mut cfg = dev_config();
        cfg.operator.rotation_enabled = true;
        cfg.tee.use_published_mock_key = false;
        cfg.tee.sealing = Some(
            toml::from_str(&format!(
                "dir = {:?}\n[handover]\nlisten = '{}'\ndanger_accept_invalid_certs = true",
                self.keys.display().to_string(),
                self.endpoint
            ))
            .unwrap(),
        );
        cfg.node.log_dir = self.dir.clone();
        cfg.node.segment_commands = SEAT_SEGMENT_COMMANDS;
        cfg.history.dir = self.dir.join("history");
        cfg.custody.provider = CustodyProvider::Evm;
        cfg.custody.vault = format!("{:#x}", chain.vault);
        let custody = custody_section(chain, &self.dir, cfg.custody);
        cfg.custody = custody;
        cfg.checkpoints = Some(
            toml::from_str(&format!(
                "rpc_url = {:?}\nrelayer = '{:#x}'\nretry_interval = '40ms'\nrpc_timeout = '2s'",
                chain.url, self.relayer
            ))
            .unwrap(),
        );
        cfg.file_sha256 = CONFIG_HASH;
        cfg
    }

    /// Runs this member's wait for its turns on `clock`'s time under `cfg`, from boot or once `resume`
    /// first says it handed over: each turn it takes with the tail goes out on `taken` as `index`'s,
    /// and once `resume` says it handed that turn over it follows again.
    pub(super) async fn follow(
        &self,
        cfg: &NodeConfig,
        index: usize,
        clock: Arc<TestClock>,
        metrics: Arc<Metrics>,
        (taken, resume): (mpsc::Sender<(usize, Box<Start>)>, Arc<Notify>),
        from_boot: bool,
    ) {
        if !from_boot {
            resume.notified().await;
        }
        let sealer = Box::new(MockSealer { launch_digest: self.digest });
        let mut waiting = Waiting::on_clock(cfg, metrics, sealer, OnClock(clock));
        loop {
            match waiting.until_turn_answering().await.expect("a member's wait for its turn") {
                Turn::Replica(start) => {
                    taken.send((index, start)).await.unwrap();
                    resume.notified().await;
                }
                Turn::Hold => panic!("{} holds a turn it did not take", self.name),
                Turn::Stop => return,
            }
        }
    }
}

/// The scenario's trusted time: its test clock's, within a second either way, taken now on the
/// monotonic clock, as a Roughtime sample is, so the member's reach moves on from it and it resamples.
struct OnClock(Arc<TestClock>);

impl Sampling for OnClock {
    async fn sample(&self) -> anyhow::Result<Sample> {
        let now = self.0.now_ms();
        Ok(Sample { at: Instant::now(), earliest_ms: now - 1_000, latest_ms: now + 1_000 })
    }
}

/// The node of the member holding the turn, and its handover listener.
pub(super) struct Holder {
    /// The member's index in the scenario's list.
    pub(super) member: usize,
    pub(super) node: Node,
    listener: JoinHandle<anyhow::Result<()>>,
    drains: mpsc::Receiver<DrainReply>,
}

/// What an incumbent's release of its turn left.
pub(super) struct Released {
    /// The final checkpoint's seq, the last command the incumbent sequenced.
    pub(super) head: u64,
    /// When the incumbent last published a command, on the wall clock.
    pub(super) last_command: Instant,
    /// The incumbent's log.
    pub(super) dir: PathBuf,
    /// The screening source the incumbent's node ran with, for its successor's.
    pub(super) source: Option<Source>,
}

impl Holder {
    /// Starts `members[member]`'s node over its log on `clock`, with the keys in `store` and the
    /// screening `source`, and binds its handover listener.
    pub(super) async fn start(
        chain: &Chain,
        members: &[Member],
        member: usize,
        (store, source): (KeyStore, Option<Source>),
        clock: Arc<TestClock>,
    ) -> Self {
        let (holding, store) = (&members[member], Arc::new(store));
        let key = Arc::new(store.keys().enclave_key().unwrap());
        let release = B256::from(store.release().id());
        let seat = Seat { operator: holding.operator(), release, api: holding.api, relayer: holding.relayer };
        let seated = (seat, Arc::new(Metrics::new()), store.clone());
        let node = Node::start_seated(chain, key, seated, holding.dir.clone(), clock, source).await;
        Self::listen(chain, holding, member, store, node).await
    }

    /// Starts `members[member]`'s node from the replica its tail handover left in `start`, on `clock`,
    /// and binds its handover listener: what the handover moved.
    pub(super) async fn from_replica(
        chain: &Chain,
        members: &[Member],
        (member, metrics): (usize, Arc<Metrics>),
        (start, source): (Box<Start>, Option<Source>),
        clock: Arc<TestClock>,
    ) -> (Self, Moved) {
        let holding = &members[member];
        let Start { replica, store, moved, .. } = *start;
        let store = Arc::new(store);
        let key = Arc::new(store.keys().enclave_key().unwrap());
        let release = B256::from(store.release().id());
        let seat = Seat { operator: holding.operator(), release, api: holding.api, relayer: holding.relayer };
        let seated = (seat, metrics, store.clone());
        let node = Node::start_from_replica(chain, key, seated, (holding.dir.clone(), clock), replica, source).await;
        (Self::listen(chain, holding, member, store, node).await, moved)
    }

    /// Binds `holding`'s handover listener over `node` with the keys in `store`.
    async fn listen(chain: &Chain, holding: &Member, member: usize, store: Arc<KeyStore>, node: Node) -> Self {
        let key = Arc::new(store.keys().enclave_key().unwrap());
        let signing = node.handover.as_ref().expect("a seated node follows its stream").1.clone();
        let (drain, drains) = mpsc::channel(1);
        let attestor = Box::new(MockAttestor);
        let listener =
            Predecessor::bind(&holding.config(chain), &store, attestor, &key, node.trusted(), drain, signing)
                .await
                .unwrap()
                .expect("[tee.sealing.handover] binds the listener")
                .with_engine(&node.state.tx);
        Self { member, node, listener: tokio::spawn(listener.run()), drains }
    }

    /// Answers the listener's stop request as a node does — a final checkpoint through the sequencer,
    /// then the drained head — and waits for the successor's confirmation, which ends the listener.
    pub(super) async fn release(self) -> Released {
        let Holder { mut node, listener, mut drains, .. } = self;
        let reply = drains.recv().await.expect("the listener asks the node to stop");
        let (stop, stopped) = oneshot::channel();
        node.state.tx.send(Request::FinalCheckpoint(stop)).await.unwrap();
        stopped.await.unwrap().expect("the checkpoint gate admits the final checkpoint");
        let (published, _) = node.handover.take().expect("a seated node follows its stream");
        let (stamps, dir) = (node.stamps.clone(), node.dir.clone());
        let source = node.stop().await;
        let head = published.final_head().expect("the drained log ends at its final checkpoint");
        let seq = head.seq;
        let _ = reply.send(Ok(head));
        listener.await.unwrap().expect("the listener ends once the successor confirms");
        let last_command = stamps.lock().unwrap().last_command.expect("the incumbent published commands");
        Released { head: seq, last_command, dir, source }
    }

    /// Stops the node and its listener, returning the node's log directory.
    pub(super) async fn stop(self) -> PathBuf {
        self.listener.abort();
        let dir = self.node.dir.clone();
        self.node.stop().await;
        dir
    }
}
