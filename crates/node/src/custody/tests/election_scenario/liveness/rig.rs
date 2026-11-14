//! The liveness scenario's parts: the operator's poster and rule, the members' links, the synthetic
//! log, and the chain's and enclaves' shared clock.

use std::collections::BTreeMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use alloy::primitives::{Address, B256, Bytes, keccak256};
use engine_core::types::AccountId;
use serde_json::json;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use super::super::chain::Chain;
use super::{Member, Shifted};
use crate::checkpoints::scenario::deploy;
use crate::clock::Clock;
use crate::config::NodeConfig;
use crate::liveness::chain::LivenessDomain;
use crate::liveness::link::{Server, Tcp};
use crate::liveness::operator::{Operator, Signing};
use crate::liveness::{Gate, Released};
use crate::log::CheckpointDomain;
use crate::log_heads::{Head, HeadsRpc, Heartbeat, Hold, Poster, SignedHead};
use crate::sequencer::Request;
use crate::tee::EnclaveKey;
use crate::trusted_time::{Sample, TrustedClock};

/// The operator's running parts: its poster, its rule, its gate and hold, and the members' links.
pub(super) struct Rig {
    pub gate: Gate,
    pub hold: watch::Sender<Hold>,
    released: watch::Sender<Released>,
    tip: watch::Sender<Head>,
    signed: watch::Receiver<Option<SignedHead>>,
    relayer: Address,
    pub poster: JoinHandle<anyhow::Result<()>>,
    tasks: Vec<JoinHandle<()>>,
}

/// The synthetic log the operator sequences: each seq's cumulative prefix and stamp.
#[derive(Clone, Default)]
pub(super) struct Log(Arc<Mutex<BTreeMap<u64, Released>>>);

impl Rig {
    /// Starts the operator's poster and rule for `signing`, its keys sealed in `keys`, and the other
    /// members' links.
    pub(super) async fn start(
        cfg: &NodeConfig,
        signing: &Arc<EnclaveKey>,
        (domain, keys): (LivenessDomain, (B256, u64)),
        trusted: TrustedClock,
        log: &Log,
        members: &[Member],
        operator: usize,
    ) -> Self {
        let checkpoints = cfg.checkpoints.as_ref().unwrap();
        let heads_cfg = cfg.log_heads.as_ref().unwrap();
        let vault = AccountId(domain.standby.vault.0.0);
        let checkpoint_domain = CheckpointDomain { chain_id: cfg.custody.chain_id, vault };
        let quorum = Arc::new(crate::custody::heads_reader(&cfg.custody).unwrap());
        let rpc = HeadsRpc::new(checkpoints, heads_cfg, checkpoint_domain, &cfg.node.log_dir, quorum).unwrap();
        let (tip, tip_rx) = watch::channel(Head::default());
        let (released, released_rx) = watch::channel(Released::default());
        let (signing_tx, signing_rx) = watch::channel(Signing::Nothing);
        let (hold, hold_rx) = watch::channel(Hold::Live);
        let (signed_tx, signed) = watch::channel(None);
        let gate = Gate::closed();
        let heartbeat = Heartbeat {
            keys: Some(keys),
            signing: signing_rx,
            released: released_rx.clone(),
            hold: hold_rx,
            gate: gate.clone(),
            signed: signed_tx,
            trusted: trusted.clone(),
        };
        let poster = Poster {
            chain: rpc,
            key: signing.clone(),
            domain: checkpoint_domain,
            interval: Duration::from_secs(3_600),
            retry: Duration::from_secs(1),
            freeze: Default::default(),
            confirmed: None,
            anchored: None,
            trusted: Some(trusted.clone()),
            prefix_ring: Default::default(),
            stall_intervals: 2,
            heartbeat: Some(heartbeat),
        };
        // The sequencer's prefix ring, as a fake answering from the synthetic log.
        let (sequencer, mut requests) = mpsc::channel(64);
        let prefixes = log.clone();
        let mut tasks = vec![tokio::spawn(async move {
            while let Some(request) = requests.recv().await {
                if let Request::PrefixAt { seq, reply } = request {
                    let _ = reply.send(prefixes.0.lock().unwrap().get(&seq).map(|released| released.head.link));
                }
            }
        })];
        let rule = Operator {
            chain: crate::custody::StandbyReader::connect_named(&cfg.custody, domain.log_heads, "liveness").unwrap(),
            asking: Tcp,
            domain,
            key: AccountId(signing.address()),
            gate: gate.clone(),
            trusted: trusted.clone(),
            released: released_rx,
            signing: signing_tx,
            tick: Duration::from_millis(250),
            sequencer: sequencer.downgrade(),
            vault_signer: watch::channel(None).0,
        };
        tasks.push(tokio::spawn(async move {
            let _sequencer = sequencer;
            let _ = rule.run().await;
        }));
        for member in members.iter().enumerate().filter(|(i, _)| *i != operator).map(|(_, member)| member) {
            let server = Server {
                listen: member.endpoint.parse().unwrap(),
                deployment: (cfg.custody.chain_id, domain.standby.vault),
                member: AccountId(member.candidate.0.0),
                copy: member.copy.clone(),
                standby: member.standby.clone(),
                provider: crate::config::TeeProvider::Mock,
                platform: member.standby.platform(),
                trusted: trusted.clone(),
                chain: crate::custody::StandbyReader::connect_named(&cfg.custody, domain.log_heads, "member_link")
                    .unwrap(),
                domain: domain.standby,
            };
            tasks.push(tokio::spawn(async move {
                let _ = server.run().await;
            }));
        }
        let (sequencer_tx, _) = mpsc::channel::<Request>(1);
        let poster = tokio::spawn(poster.run(tip_rx, sequencer_tx));
        Self { gate, hold, released, tip, signed, relayer: heads_cfg.relayer, poster, tasks }
    }

    /// Takes the operator's relayer's funds, so its posts fail.
    pub(super) async fn silence(&self, chain: &Chain) {
        chain.rpc("anvil_setBalance", json!([format!("{:#x}", self.relayer), "0x0"])).await;
    }

    /// Funds the operator's relayer again.
    pub(super) async fn unsilence(&self, chain: &Chain) {
        chain.rpc("anvil_setBalance", json!([format!("{:#x}", self.relayer), "0x56bc75e2d63100000"])).await;
    }

    /// Stops every part.
    pub(super) fn stop(self) {
        self.poster.abort();
        self.tasks.iter().for_each(JoinHandle::abort);
    }
}

impl Log {
    /// Sequences `seq` on the operator; the other members' copies durably take it, with the operator's
    /// newest signed head from its stream.
    pub(super) fn extend(&self, rig: &Rig, members: &[Member], operator: usize, seq: u64) {
        let link = keccak256(seq.to_be_bytes());
        let stamp = u64::try_from(crate::clock::SystemClock.now_ms()).unwrap();
        let released = Released { head: Head { seq, link }, stamp };
        self.0.lock().unwrap().insert(seq, released);
        rig.tip.send_replace(released.head);
        rig.released.send_replace(released);
        for member in members.iter().enumerate().filter(|(i, _)| *i != operator).map(|(_, member)| member) {
            member.copy.durable(released.head);
            if let Some(signed) = rig.signed.borrow().clone() {
                member.copy.signed_head(signed);
            }
        }
    }
}

/// Etches the chain-backed `ArbSys` stand-in at 0x64.
pub(super) async fn etch_arb_sys(chain: &Chain) {
    let stand_in = deploy(&chain.provider, chain.accounts[0], "ChainArbSys.sol/ChainArbSys.json", &[]).await;
    let code: Bytes = chain.provider.request("eth_getCode", (stand_in, "latest")).await.unwrap();
    chain.rpc("anvil_setCode", json!(["0x0000000000000000000000000000000000000064", code])).await;
}

/// Mines a block at `at` and moves the enclaves' clock with the chain.
pub(super) async fn warp(chain: &Chain, clock: &Shifted, at: u64) {
    chain.mine_at(at).await;
    let now = crate::clock::SystemClock.now_ms();
    clock.0.store(i64::try_from(at).unwrap() * 1000 - now, Ordering::SeqCst);
}

/// A trusted time sample of `clock` every second, a second either side.
pub(super) fn sampling(clock: Arc<Shifted>) -> watch::Sender<Option<Sample>> {
    let (samples, _) = watch::channel(None);
    let sender = samples.clone();
    tokio::spawn(async move {
        loop {
            let now = clock.now_ms();
            sender.send_replace(Some(Sample { at: Instant::now(), earliest_ms: now - 1_000, latest_ms: now + 1_000 }));
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    });
    samples
}

/// The node config the scenario's parts build from: the chain's quorum, its RPC as the checkpoint
/// endpoint, and `[log_heads]` relaying from `relayer`.
pub(super) fn config(chain: &Chain, dir: &std::path::Path, log_heads: Address, relayer: Address) -> NodeConfig {
    let anvil = std::fs::read_to_string(crate::test_support::ANVIL_CONFIG).unwrap();
    let mut cfg: NodeConfig = toml::from_str(&format!(
        "{anvil}\n[checkpoints]\nrpc_url = \"{}\"\nrelayer = \"{:#x}\"\nretry_interval = \"1s\"\nrpc_timeout = \"5s\"\n\n\
         [log_heads]\ncontract = \"{log_heads:#x}\"\nrelayer = \"{relayer:#x}\"\ninterval = \"1h\"\n",
        chain.url, chain.accounts[8]
    ))
    .unwrap();
    cfg.custody = super::super::node::custody_section(chain, dir, cfg.custody);
    cfg.custody.vault = format!("{:#x}", chain.vault);
    cfg.node.log_dir = dir.to_path_buf();
    cfg
}
