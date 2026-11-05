//! A rotation set's member and the node it runs while it holds a turn: sealed keys under the
//! stand-in for sealing, the real handover listener (`Predecessor`) bound to the endpoint its
//! candidacy registers, and the node's answer to the listener's stop request, as a node answers it
//! (`node::shutdown`): a final checkpoint through the sequencer, then the drained head.

use std::path::PathBuf;
use std::sync::Arc;

use alloy::primitives::Address;
use engine_core::eip712::B256;
use engine_core::types::AccountId;
use k256::ecdsa::SigningKey;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use super::super::CONFIG_HASH;
use super::super::chain::Chain;
use super::super::node::{Node, Seat, custody_section};
use crate::checkpoints::{CheckpointChain, ScheduledTurn, View};
use crate::clock::test_clock::TestClock;
use crate::config::{CustodyProvider, NodeConfig};
use crate::handover::{DrainReply, Predecessor};
use crate::sequencer::Request;
use crate::tee::MockAttestor;
use crate::tee::sealing::{KeyStore, MockSealer, Release};
use crate::test_support::{dev_config, scratch_dir};

/// A registered candidate: its admin key, which its node's config names as the operator key, the
/// release it runs, its handover listener, and the directories its node keeps across holdings.
pub(super) struct Member {
    pub(super) name: &'static str,
    pub(super) candidate: Address,
    pub(super) key: SigningKey,
    pub(super) digest: [u8; 48],
    pub(super) endpoint: String,
    dir: PathBuf,
    keys: PathBuf,
}

impl Member {
    /// `name`, as `candidate` with the admin key `key` running `digest`, on a free handover port.
    pub(super) fn new(name: &'static str, candidate: Address, key: [u8; 32], digest: [u8; 48]) -> Self {
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        Self {
            name,
            candidate,
            key: SigningKey::from_slice(&key).unwrap(),
            digest,
            endpoint: format!("127.0.0.1:{port}"),
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

    /// Seals `keys` for its release in `generation`, as a node's first start does.
    pub(super) fn seal(&self, keys: crate::tee::keys::KeySet, generation: u64) -> KeyStore {
        let sealer = MockSealer { launch_digest: self.digest };
        KeyStore::create(&self.keys, &sealer, CONFIG_HASH, generation, keys).unwrap()
    }

    /// The config its node runs: sealing with a handover listener at its endpoint, its log, the
    /// chain's custody and vault, and the config hash its release names.
    fn config(&self, chain: &Chain) -> NodeConfig {
        let mut cfg = dev_config();
        cfg.tee.use_published_mock_key = false;
        cfg.tee.sealing = Some(
            toml::from_str(&format!(
                "dir = {:?}\n[handover]\nlisten = '{}'",
                self.keys.display().to_string(),
                self.endpoint
            ))
            .unwrap(),
        );
        cfg.node.log_dir = self.dir.clone();
        cfg.custody.provider = CustodyProvider::Evm;
        cfg.custody.vault = format!("{:#x}", chain.vault);
        let custody = custody_section(chain, &self.dir, cfg.custody);
        cfg.custody = custody;
        cfg.checkpoints = Some(
            toml::from_str(&format!(
                "rpc_url = {:?}\nrelayer = '{:#x}'\nretry_interval = '40ms'\nrpc_timeout = '2s'",
                chain.url, chain.accounts[9]
            ))
            .unwrap(),
        );
        cfg.file_sha256 = CONFIG_HASH;
        cfg
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

/// What a handover gave the member taking the turn.
pub(super) struct Taken {
    pub(super) store: KeyStore,
    /// The final checkpoint's seq, the last command the incumbent sequenced.
    pub(super) head: u64,
    /// The turn the finalised schedule named when the member resolved its source.
    pub(super) scheduled: ScheduledTurn,
}

impl Holder {
    /// Starts `members[member]`'s node over its log on `clock`, with the keys in `store`, and binds
    /// its handover listener.
    pub(super) async fn start(
        chain: &Chain,
        members: &[Member],
        member: usize,
        store: KeyStore,
        clock: Arc<TestClock>,
    ) -> Self {
        let holding = &members[member];
        let store = Arc::new(store);
        let key = Arc::new(store.keys().enclave_key().unwrap());
        let seat = Seat { operator: holding.operator(), release: B256::from(store.release().id()) };
        let node = Node::start_seated(chain, key.clone(), seat, holding.dir.clone(), clock).await;
        let signing = node.handover.as_ref().expect("a seated node follows its stream").1.clone();
        let (drain, drains) = mpsc::channel(1);
        let attestor = Box::new(MockAttestor);
        let listener =
            Predecessor::bind(&holding.config(chain), &store, attestor, &key, node.trusted(), drain, signing)
                .await
                .unwrap()
                .expect("[tee.sealing.handover] binds the listener")
                .with_engine(node.state.tx.clone());
        Self { member, node, listener: tokio::spawn(listener.run()), drains }
    }

    /// Hands the keys to `to`, as a member that finds no sealed keys for its turn takes them: it
    /// reads its turn and the source it takes over from in the finalised schedule at its trusted
    /// time, connects to that source's registered handover endpoint, and takes the keys and the log
    /// past its own copy (`handover::take`). Meanwhile this node answers its listener's stop request
    /// with a final checkpoint and stops.
    pub(super) async fn hand_over(self, chain: &Chain, to: &Member) -> Taken {
        let cfg = to.config(chain);
        let vault = crate::handover::vault_reader(&cfg).unwrap().expect("[checkpoints] reads the vault");
        let reached = self.node.trusted().reached_ms().expect("the node holds a trusted time sample");
        let at = vault.observe(View::Finalized, &[]).await.unwrap();
        let scheduled = vault.scheduled_turn(&at, reached).await.unwrap().expect("the finalised schedule names a turn");
        let source = scheduled.source.clone().expect("the turn takes over from another member");
        let sealer = MockSealer { launch_digest: to.digest };
        // A refused handover fails here, rather than leaving the incumbent's side waiting for a stop
        // request that never comes.
        let take = async {
            crate::handover::take(&cfg, &source.handover_endpoint, &sealer, &MockAttestor, &vault, Some(reached))
                .await
                .expect("the member takes the keys")
        };
        let Holder { mut node, listener, mut drains, .. } = self;
        let release = async move {
            let reply = drains.recv().await.expect("the listener asks the node to stop");
            let (stop, stopped) = oneshot::channel();
            node.state.tx.send(Request::FinalCheckpoint(stop)).await.unwrap();
            stopped.await.unwrap().expect("the checkpoint gate admits the final checkpoint");
            let (published, _) = node.handover.take().expect("a seated node follows its stream");
            node.stop().await;
            let head = published.final_head().expect("the drained log ends at its final checkpoint");
            let seq = head.seq;
            let _ = reply.send(Ok(head));
            listener.await.unwrap().expect("the listener ends once the successor confirms");
            seq
        };
        let (store, head) = tokio::join!(take, release);
        Taken { store, head, scheduled }
    }

    /// Stops the node and its listener, returning the node's log directory.
    pub(super) async fn stop(self) -> PathBuf {
        self.listener.abort();
        let dir = self.node.dir.clone();
        self.node.stop().await;
        dir
    }
}
