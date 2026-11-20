//! The successor-fallback scenario's world: the chain, the members and their callers and fixers, the
//! synthetic log, and the holder whose poster and rule run.

use std::sync::Arc;
use std::sync::atomic::AtomicI64;
use std::time::{Duration, Instant};

use alloy::primitives::{Address, B256, U256, keccak256};
use alloy::sol_types::{SolCall, SolValue};
use tokio::task::JoinHandle;

use super::super::super::chain::{Chain, Parameters, delegateCall, setParametersCall, setRotationCall};
use super::super::super::{CONFIG_HASH, address, monkey};
use super::super::{
    DAY_SECS, Member, SET_CLOSE_SECS, Shifted, Turn, drawCall, etch_arb_sys, latchesCall, receiveSeedCall,
    relayAliasCall, seedEpochCall, setRegistryCall, setScheduleCall, setVaultCall, signerEpochCall, turnOfCall,
};
use super::{
    DIGESTS, KEYS, MAXIMUMS, MINIMUMS, RULE, STAKES, STEP, SWITCH_BUDGET_SECS, TAKE, TakeWindowArg, assignmentOfCall,
    boundary, memberAtCall, postCheckpointCall, seedReceiverCall, signerSinceCall,
};
use crate::checkpoints::scenario::deploy;
use crate::clock::Clock;
use crate::config::LivenessSection;
use crate::liveness::caller::{Caller, Endpoint};
use crate::liveness::chain::LivenessDomain;
use crate::liveness::link::Copy;
use crate::standby_key::StandbyKeys;
use crate::standby_key::chain::StandbyDomain;
use crate::tee::EnclaveKey;
use crate::tee::keys::KeySet;
use crate::tee::sealing::Release;
use crate::test_support::scratch_dir;
use crate::trusted_time::TrustedClock;

use super::super::rig::{Log, Rig, config, sampling};

/// The scenario's running parts: the chain, the shared clock and samples, the members, the synthetic
/// log, the holder's rig and who holds, and the members' callers.
pub(super) struct World {
    pub(super) chain: Chain,
    pub(super) clock: Arc<Shifted>,
    pub(super) samples: tokio::sync::watch::Sender<Option<crate::trusted_time::Sample>>,
    pub(super) members: Vec<Member>,
    pub(super) signing: Arc<EnclaveKey>,
    pub(super) domain: LivenessDomain,
    pub(super) set: u32,
    pub(super) log: Log,
    pub(super) seq: u64,
    pub(super) rig: Option<(Rig, usize)>,
    pub(super) callers: Vec<JoinHandle<anyhow::Result<()>>>,
}

impl World {
    /// Deploys the contracts, registers A, B, C and D with vouched standby keys, adopts them as a set,
    /// and starts every member's caller.
    pub(super) async fn start() -> Self {
        let keys = KeySet::generate().unwrap();
        let signing = Arc::new(keys.enclave_key().unwrap());
        let deployer = k256::ecdsa::SigningKey::from_slice(&[0x41; 32]).unwrap();
        let mut chain = Chain::start(Address::from(signing.address()), address(&deployer)).await;
        etch_arb_sys(&chain).await;
        let clock = Arc::new(Shifted(AtomicI64::new(0)));
        let receiver = deploy(
            &chain.provider,
            chain.accounts[0],
            "SeedReceiver.sol/SeedReceiver.json",
            &(chain.accounts[9], 1_606_824_023u64, DAY_SECS).abi_encode_params(),
        )
        .await;
        let from = chain.accounts[0];
        let log_heads =
            deploy(&chain.provider, from, "LogHeads.sol/LogHeads.json", &(chain.vault,).abi_encode_params()).await;
        let window = TakeWindowArg { slots: TAKE, minimum: TAKE, maximum: 8 };
        let args = (chain.registry, receiver, log_heads, RULE, MINIMUMS, MAXIMUMS, window, SWITCH_BUDGET_SECS, 250u64);
        let schedule =
            deploy(&chain.provider, from, "OperatorSchedule.sol/OperatorSchedule.json", &args.abi_encode_params())
                .await;
        chain.schedule = Some(schedule);
        let members: Vec<Member> = (0..4)
            .map(|i| {
                let dir = scratch_dir(&format!("fallback-member-{i}"));
                let free = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
                Member {
                    candidate: chain.accounts[2 + i],
                    admin: k256::ecdsa::SigningKey::from_slice(&KEYS[i]).unwrap(),
                    digest: DIGESTS[i],
                    endpoint: free.to_string(),
                    standby: Arc::new(StandbyKeys::for_test(&dir)),
                    copy: Copy::default(),
                }
            })
            .collect();
        for (member, stake) in members.iter().zip(STAKES) {
            chain.stake(member.candidate, monkey(stake)).await;
            chain
                .register_serving_at(
                    member.candidate,
                    &member.admin,
                    monkey(2_000),
                    member.digest,
                    &member.endpoint,
                    "127.0.0.1:1",
                )
                .await;
        }
        let daily = Parameters { epochLength: DAY_SECS, ..super::super::super::dev_parameters().0 };
        let mut calls = vec![
            (chain.registry, setParametersCall { parameters: daily }.abi_encode()),
            (chain.registry, setVaultCall { vault_: chain.vault }.abi_encode()),
            (chain.vault, setRegistryCall { registry_: chain.registry }.abi_encode()),
            (chain.registry, setScheduleCall { schedule_: schedule }.abi_encode()),
        ];
        calls.extend(members.iter().map(|member| (chain.registry, chain.approval(member.candidate, member.digest))));
        calls.push((chain.registry, setRotationCall { enabled: true }.abi_encode()));
        chain.govern_each(calls).await;
        for (member, stake) in members.iter().zip(STAKES) {
            let delegate = delegateCall { candidate: member.candidate, amount: monkey(stake) };
            chain.send(member.candidate, chain.staking, delegate.abi_encode()).await;
            let release = B256::from(Release { launch_digest: member.digest, config_sha256: CONFIG_HASH }.id());
            chain
                .register_standby(member.candidate, release, Address::from(member.standby.address().0), &signing)
                .await;
        }
        chain.close_epoch_at(SET_CLOSE_SECS).await;
        let cfg = config(&chain, &scratch_dir("fallback-callers"), log_heads, chain.accounts[5]);
        let domain = LivenessDomain {
            standby: StandbyDomain {
                chain_id: cfg.custody.chain_id,
                vault: chain.vault,
                registry: chain.registry,
                schedule,
            },
            log_heads,
        };
        let (samples, _) = sampling(clock.clone());
        // Each member's liveness caller and fallback fixer, sending from its account one call at a time.
        let callers = members
            .iter()
            .enumerate()
            .flat_map(|(i, member)| {
                let liveness = LivenessSection {
                    caller: chain.accounts[6 + i],
                    min_balance: U256::from(1),
                    read_interval: crate::config::default_liveness_read_interval(),
                    fallback_interval: crate::liveness::caller::FOLLOW_UP,
                };
                let endpoint = || Endpoint::new(cfg.checkpoints.as_ref().unwrap(), &liveness, cfg.custody.chain_id);
                let reader = |name| crate::custody::StandbyReader::connect_named(&cfg.custody, log_heads, name);
                let sends = Arc::new(crate::liveness::fallback::Sends::default());
                let caller = Caller {
                    chain: reader("caller").unwrap(),
                    sending: endpoint().unwrap(),
                    domain,
                    copy: member.copy.clone(),
                    min_balance: U256::from(1),
                    poll: crate::config::default_liveness_read_interval(),
                    clock: clock.clone(),
                    sends: sends.clone(),
                };
                let fixer = crate::liveness::fallback::Fixer {
                    chain: reader("fixer").unwrap(),
                    sending: endpoint().unwrap(),
                    chain_id: cfg.custody.chain_id,
                    vault: chain.vault,
                    // At the follow-up cadence while idle too, so the scenario runs in minutes.
                    idle: crate::liveness::caller::FOLLOW_UP,
                    poll: crate::liveness::caller::FOLLOW_UP,
                    min_balance: U256::from(1),
                    sends,
                };
                [tokio::spawn(caller.run()), tokio::spawn(fixer.run())]
            })
            .collect();
        Self {
            chain,
            clock,
            samples,
            members,
            signing,
            domain,
            set: 1,
            log: Log::default(),
            seq: 0,
            rig: None,
            callers,
        }
    }

    /// Relays for `turn`, from the receiver's relay alias, a seed whose first positions pick the members
    /// `order` names.
    pub(super) async fn relay(&self, turn: u64, order: &[usize]) {
        let schedule = self.domain.standby.schedule;
        let mut found = None;
        for i in 0u64..10_000 {
            let seed = keccak256(("fallback", turn, i).abi_encode());
            let mut matches = true;
            for (position, member) in order.iter().enumerate() {
                let call = memberAtCall { setId: self.set, seed, turn, position: u16::try_from(position).unwrap() };
                if self.chain.view(schedule, call).await != self.members[*member].candidate {
                    matches = false;
                    break;
                }
            }
            if matches {
                found = Some(seed);
                break;
            }
        }
        let seed = found.expect("a seed picking the order");
        let receiver = self.chain.view(schedule, seedReceiverCall {}).await;
        let alias = self.chain.view(receiver, relayAliasCall {}).await;
        let epoch = self.chain.view(receiver, seedEpochCall { turn }).await;
        self.chain.impersonate(alias).await;
        self.chain.send(alias, receiver, receiveSeedCall { turn, epoch, seed }.abi_encode()).await;
    }

    /// Draws `turn` at its draw deadline.
    pub(super) async fn draw_at_deadline(&self, turn: u64) {
        self.chain.mine_at(boundary(turn) - DAY_SECS + 60).await;
        self.draw(turn).await;
    }

    /// Draws `turn` now.
    pub(super) async fn draw(&self, turn: u64) {
        self.chain.send(self.chain.accounts[1], self.domain.standby.schedule, drawCall { turn }.abi_encode()).await;
    }

    /// The release member `index` runs.
    pub(super) fn release(&self, index: usize) -> B256 {
        B256::from(Release { launch_digest: self.members[index].digest, config_sha256: CONFIG_HASH }.id())
    }

    /// The node config member `index`'s holding runs from, in a directory of its own each time.
    pub(super) fn cfg_of(&self, index: usize) -> crate::config::NodeConfig {
        let dir = scratch_dir(&format!("fallback-holder-{index}-{}", self.seq));
        std::fs::create_dir_all(&dir).unwrap();
        let mut cfg = config(&self.chain, &dir, self.domain.log_heads, self.chain.accounts[5]);
        let point = self.members[index].admin.verifying_key().to_sec1_point(false);
        cfg.operator.public_key = format!("0x{}", hex::encode(point.as_bytes()));
        cfg
    }

    /// Hands the keys to member `index`, sealed in the generation `turn`'s latest assignment holds: the
    /// holder's poster stops and the member's starts, as a confirmed handover leaves them.
    pub(super) async fn hold(&mut self, index: usize, turn: u64) {
        let generation = self.chain.view(self.domain.standby.schedule, turnOfCall { turn }).await.generation;
        self.hold_in(index, generation).await;
    }

    /// `hold`, with the keys sealed in `generation`.
    pub(super) async fn hold_in(&mut self, index: usize, generation: u64) {
        if let Some((rig, _)) = self.rig.take() {
            rig.stop();
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
        let cfg = Arc::new(self.cfg_of(index));
        let trusted = TrustedClock::new(self.clock.clone(), self.samples.subscribe(), Duration::from_secs(10));
        let time = (self.samples.subscribe(), self.clock.clone() as Arc<dyn Clock>);
        let keys = (self.release(index), generation);
        let rig =
            Rig::start(cfg, &self.signing, (self.domain, keys), trusted, time, &self.log, &self.members, index).await;
        self.rig = Some((rig, index));
    }

    /// Extends the holder's log by one command and mines a block at the shared clock's time.
    pub(super) async fn step(&mut self) {
        if let Some((rig, holder)) = &self.rig {
            self.seq += 1;
            self.log.extend(rig, &self.members, *holder, self.seq);
        }
        let now = u64::try_from(self.clock.now_ms() / 1000).unwrap();
        if now > self.chain.latest_timestamp().await {
            self.chain.mine_at(now).await;
        }
        tokio::time::sleep(STEP).await;
    }

    /// Steps `count` times.
    pub(super) async fn beat(&mut self, count: usize) {
        for _ in 0..count {
            self.step().await;
        }
    }

    /// Steps until a head latches `turn`'s current assignment, returning the latch's block time.
    pub(super) async fn until_latched(&mut self, turn: u64) -> u64 {
        let started = Instant::now();
        loop {
            let position = self.chain.view(self.domain.standby.schedule, turnOfCall { turn }).await.position;
            let latch = self.chain.view(self.domain.log_heads, latchesCall { turn, position }).await;
            if latch.at != 0 {
                return latch.at;
            }
            assert!(started.elapsed() < Duration::from_secs(180), "no head latched turn {turn}");
            self.step().await;
        }
    }

    /// Steps until a skip names member `to` in `turn`, asserting while `trades` that the holder's log
    /// stays confirmed, and returns the skip's block time.
    pub(super) async fn until_skipped(&mut self, turn: u64, to: usize, trades: bool) -> u64 {
        let started = Instant::now();
        let schedule = self.domain.standby.schedule;
        loop {
            let named: Turn = self.chain.view(schedule, turnOfCall { turn }).await;
            if named.reason == 1 && named.candidate == self.members[to].candidate {
                let assignment = self.chain.view(schedule, assignmentOfCall { turn, position: named.position }).await;
                return assignment._0.at;
            }
            if trades && started.elapsed() > Duration::from_secs(20) {
                let (rig, _) = self.rig.as_ref().expect("a holder");
                assert!(rig.gate.sequences(), "the holder stopped trading in the take window");
            }
            assert!(started.elapsed() < Duration::from_secs(240), "no skip of turn {turn}");
            self.step().await;
        }
    }

    /// Moves the shared clock to `until` with no block mined, as the members' samples follow it.
    pub(super) async fn pause_until(&mut self, until: u64) {
        let now = u64::try_from(self.clock.now_ms() / 1000).unwrap();
        for at in (now..until).step_by(10) {
            let offset = i64::try_from(at).unwrap() * 1000 - crate::clock::SystemClock.now_ms();
            self.clock.0.store(offset, std::sync::atomic::Ordering::SeqCst);
            if let Some((rig, holder)) = &self.rig {
                self.seq += 1;
                self.log.extend(rig, &self.members, *holder, self.seq);
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        let offset = i64::try_from(until).unwrap() * 1000 - crate::clock::SystemClock.now_ms();
        self.clock.0.store(offset, std::sync::atomic::Ordering::SeqCst);
    }

    /// Steps until the vault's signer epoch moves, returning the promotion's block time and the block
    /// time at which the holder was first seen fenced.
    pub(super) async fn until_promoted(&mut self) -> (u64, u64) {
        let started = Instant::now();
        let mut fenced_at = None;
        loop {
            if self.epoch().await > 1 {
                let at = self.chain.view(self.chain.vault, signerSinceCall {}).await;
                return (at, fenced_at.expect("the holder fenced before the promotion"));
            }
            let (rig, _) = self.rig.as_ref().expect("a holder");
            if fenced_at.is_none() && !rig.gate.signs() {
                fenced_at = Some(self.chain.latest_timestamp().await);
            }
            assert!(started.elapsed() < Duration::from_secs(300), "no promotion");
            self.step().await;
        }
    }

    /// The vault's signer epoch.
    pub(super) async fn epoch(&self) -> u64 {
        self.chain.view(self.chain.vault, signerEpochCall {}).await
    }

    /// The revert data of a first checkpoint signed by the key the promotion replaced.
    pub(super) async fn refused_checkpoint(&self) -> alloy::primitives::Bytes {
        let (seq, root) = (1u64, B256::repeat_byte(0x77));
        let mut packed = b"engine.checkpoint.v2".to_vec();
        packed.extend_from_slice(&self.domain.standby.chain_id.to_be_bytes());
        packed.extend_from_slice(self.chain.vault.as_slice());
        packed.extend_from_slice(&seq.to_be_bytes());
        packed.extend_from_slice(root.as_slice());
        packed.push(0);
        packed.extend_from_slice(&0u64.to_be_bytes());
        packed.extend_from_slice(B256::ZERO.as_slice());
        let signature: alloy::primitives::Bytes = self.signing.sign_digest(keccak256(packed).0).parse().unwrap();
        let call = postCheckpointCall { seq, root, hasParent: false, parentSeq: 0, parentRoot: B256::ZERO, signature };
        self.chain.reverted(self.chain.accounts[1], self.chain.vault, call.abi_encode()).await
    }

    /// Stops the holder and the callers.
    pub(super) fn stop(self) {
        if let Some((rig, _)) = self.rig {
            rig.stop();
        }
        self.callers.iter().for_each(JoinHandle::abort);
    }
}
