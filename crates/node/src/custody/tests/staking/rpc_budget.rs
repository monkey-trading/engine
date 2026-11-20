//! What each custody provider sees, in JSON-RPC calls, from one operator and two members sharing its
//! account: the deposit watcher's polls, measured over a fake chain at Arbitrum One's block rate, and
//! one read of each fixed-interval reader through the real quorum, each multiplied by its cadence.
//! The RPC budget in `ops/runbooks/feed-and-custody-failures.md` holds the figures it prints:
//! `cargo test -p engine-node --lib rpc_budget -- --ignored --nocapture --test-threads=1`.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use alloy::primitives::{Address, B256, Bytes, U256};

use super::*;
use crate::custody::QuorumBlock;
use crate::custody::evm::{PostedHead, VaultRecord};
use crate::custody::stall::Polled;
use crate::liveness::chain::{LivenessDomain, members, pending_fallback, promotable, view};
use crate::liveness::fake::{ChainState, DOMAIN, LOG_HEADS, VAULT as LIVENESS_VAULT, answer};
use crate::standby_key::chain::{StandbyChain, StandbyDomain};

const DAY: f64 = 86_400.0;

/// One provider's calls by method.
type ByMethod = BTreeMap<&'static str, f64>;

/// Readers, each with its calls a read and its reads a day.
type Readers<'a> = Vec<(&'static str, &'a ByMethod, f64)>;

/// The liveness rule's chain's providers, each named.
type Providers = Vec<(String, Counted<Liveness>)>;

/// JSON-RPC calls by method, counted as one provider's endpoint receives them.
#[derive(Clone, Default)]
struct Calls(Arc<Mutex<BTreeMap<&'static str, u64>>>);

/// A reader that counts the JSON-RPC calls `VaultRpc` would send for each read it passes on.
#[derive(Clone)]
struct Counted<R>(R, Calls);

/// The liveness rule's chain, answered as `liveness::fake` answers it, as a custody provider whose tip is
/// the given number of blocks ahead of the chain's.
#[derive(Clone)]
struct Liveness(Arc<Mutex<ChainState>>, u64);

impl Calls {
    fn add(&self, method: &'static str, calls: u64) {
        *self.0.lock().unwrap().entry(method).or_default() += calls;
    }

    fn take(&self) -> BTreeMap<&'static str, u64> {
        std::mem::take(&mut self.0.lock().unwrap())
    }
}

impl<R: VaultReader + Sync> VaultReader for Counted<R> {
    async fn identity(&self) -> anyhow::Result<VaultIdentity> {
        self.1.add("eth_chainId", 1);
        self.1.add("eth_call", 1);
        self.0.identity().await
    }
    async fn finality(&self) -> anyhow::Result<Finality> {
        self.1.add("eth_getBlockByNumber", 2);
        self.0.finality().await
    }
    async fn withdrawal_finality(&self) -> anyhow::Result<Finality> {
        self.1.add("eth_getBlockByNumber", 2);
        self.0.withdrawal_finality().await
    }
    async fn headers(&self, from: u64, to: u64) -> anyhow::Result<Vec<BlockLink>> {
        self.1.add("eth_getBlockByNumber", to - from + 1);
        self.0.headers(from, to).await
    }
    async fn deposits(&self, from: u64, to: u64) -> anyhow::Result<Vec<VaultDeposit>> {
        self.1.add("eth_getLogs", 1);
        self.0.deposits(from, to).await
    }
    async fn deposit_count(&self, at: BlockRef) -> anyhow::Result<u64> {
        self.1.add("eth_call", 1);
        self.0.deposit_count(at).await
    }
    async fn deposit_record(&self, at: BlockRef, id: u64) -> anyhow::Result<VaultRecord> {
        self.1.add("eth_call", 2);
        self.0.deposit_record(at, id).await
    }
    async fn staking_events(&self, from: u64, to: BlockRef) -> anyhow::Result<Vec<StakingEvent>> {
        self.1.add("eth_getLogs", 1);
        self.0.staking_events(from, to).await
    }
    async fn pinned_staking_events(&self, blocks: &[BlockRef]) -> anyhow::Result<Vec<StakingEvent>> {
        self.1.add("eth_getLogs", blocks.len() as u64);
        self.0.pinned_staking_events(blocks).await
    }
    async fn staking_registry(&self, at: BlockRef) -> anyhow::Result<Option<Address>> {
        self.1.add("eth_call", 1);
        self.0.staking_registry(at).await
    }
    async fn registry_schedule(&self, at: BlockRef) -> anyhow::Result<Address> {
        self.1.add("eth_call", 1);
        self.0.registry_schedule(at).await
    }
    async fn schedule_registry(&self, at: BlockRef) -> anyhow::Result<Option<Address>> {
        self.1.add("eth_call", 1);
        self.0.schedule_registry(at).await
    }
    async fn account_balance(&self, at: BlockRef, account: Address) -> anyhow::Result<U256> {
        self.1.add("eth_getBalance", 1);
        self.0.account_balance(at, account).await
    }
    async fn contract_call(&self, at: BlockRef, to: Address, data: Bytes) -> anyhow::Result<Bytes> {
        self.1.add("eth_call", 1);
        self.0.contract_call(at, to, data).await
    }
    async fn log_head(&self, contract: Address, at: BlockRef) -> anyhow::Result<PostedHead> {
        self.1.add("eth_call", 2);
        self.0.log_head(contract, at).await
    }
    async fn latest_block(&self) -> anyhow::Result<(u64, B256)> {
        self.1.add("eth_getBlockByNumber", 1);
        self.0.latest_block().await
    }
}

impl Liveness {
    fn hash(number: u64) -> B256 {
        B256::from(U256::from(number))
    }

    fn tip(&self) -> (u64, u64) {
        let state = self.0.lock().unwrap();
        (state.number, state.time)
    }
}

impl VaultReader for Liveness {
    async fn identity(&self) -> anyhow::Result<VaultIdentity> {
        Ok(VaultIdentity { chain_id: DOMAIN.standby.chain_id, token: Some(USDC_TOKEN) })
    }
    async fn finality(&self) -> anyhow::Result<Finality> {
        let (number, time) = self.tip();
        let tagged = Some(BlockRef { number, hash: Self::hash(number) });
        Ok(Finality { head: number, head_time: time, tagged, head_support: None })
    }
    async fn headers(&self, from: u64, to: u64) -> anyhow::Result<Vec<BlockLink>> {
        let time = self.tip().1;
        Ok((from..=to)
            .map(|number| BlockLink {
                number,
                hash: Self::hash(number),
                parent_hash: Self::hash(number.saturating_sub(1)),
                timestamp: time,
            })
            .collect())
    }
    async fn deposits(&self, _from: u64, _to: u64) -> anyhow::Result<Vec<VaultDeposit>> {
        Ok(Vec::new())
    }
    async fn deposit_count(&self, _at: BlockRef) -> anyhow::Result<u64> {
        Ok(0)
    }
    async fn staking_events(&self, _from: u64, _to: BlockRef) -> anyhow::Result<Vec<StakingEvent>> {
        Ok(Vec::new())
    }
    async fn account_balance(&self, _at: BlockRef, _account: Address) -> anyhow::Result<U256> {
        Ok(U256::from(10).pow(U256::from(18)))
    }
    async fn contract_call(&self, _at: BlockRef, to: Address, data: Bytes) -> anyhow::Result<Bytes> {
        answer(&self.0.lock().unwrap(), to, &data).map(Bytes::from)
    }
    async fn log_head(&self, _contract: Address, _at: BlockRef) -> anyhow::Result<PostedHead> {
        let state = self.0.lock().unwrap();
        Ok(PostedHead { epoch: state.epoch, head: Some((state.head.0, Self::hash(state.head.0))) })
    }
    async fn latest_block(&self) -> anyhow::Result<(u64, B256)> {
        let number = self.tip().0 + self.1;
        Ok((number, Self::hash(number)))
    }
}

/// Polls `watcher` until it reaches the eligible block, returning the polls it took.
async fn caught_up(watcher: &mut DepositWatcher<Quorum<Counted<FakeChain>>>, node: &Node) -> u64 {
    for polls in 1..10_000 {
        if !matches!(watcher.poll(&node.tx).await.unwrap(), Polled::Read { caught_up: false }) {
            return polls;
        }
    }
    panic!("the watcher did not catch up");
}

/// The deposit watcher's calls per poll, at `per_poll` new blocks a poll and Arbitrum One's lag of
/// about 4,800 blocks from the head to the finalized tag, once it follows the chain.
async fn watcher_per_poll(per_poll: u64) -> ByMethod {
    let dir = scratch(&format!("rpc-budget-{per_poll}"));
    let node = Node::start_confirmed(&dir);
    init_staking(&node).await;
    let (lag, mut head) = (4_800, 6_000u64);
    let chains = poll_chains(head);
    let calls = Calls::default();
    let toml = format!(
        r#"{}
        staking = {{ contract = "0x5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a", cursor_path = "{}", registry = {{ contract = "{REGISTRY}" }} }}
        "#,
        evm_toml(&dir, 1_000).replace("start_block = 0", "start_block = 1"),
        dir.join("staking-cursor.json").display()
    );
    let cfg: EvmCustodySection = toml::from_str(&toml).unwrap();
    // Only the first provider counts: each sees the same reads.
    let providers = chains
        .iter()
        .enumerate()
        .map(|(i, chain)| {
            let counted = if i == 0 { calls.clone() } else { Calls::default() };
            (format!("p{i}"), Counted(chain.clone(), counted))
        })
        .collect();
    let mut watcher = DepositWatcher::start(Quorum::new(providers).unwrap(), &cfg, CHAIN_ID, VAULT, None).unwrap();
    let mut advance = |blocks: u64| {
        head += blocks;
        for chain in &chains {
            let mut state = chain.0.lock().unwrap();
            state.head = head;
            state.final_block = Some(head);
            state.withdrawal_block = Some(head - lag);
        }
    };
    for _ in 0..300 {
        advance(per_poll);
        caught_up(&mut watcher, &node).await;
    }
    calls.take();
    let (ticks, mut polls) = (100u32, 0);
    for _ in 0..ticks {
        advance(per_poll);
        polls += caught_up(&mut watcher, &node).await;
    }
    println!("   {per_poll} new blocks a poll: {polls} polls over {ticks}");
    node.stop().await;
    calls.take().into_iter().map(|(method, calls)| (method, calls as f64 / f64::from(ticks))).collect()
}

/// Three providers answering the liveness rule's chain, and that chain. The first is counted, and its tip
/// runs a block ahead of the others', as one provider's often does: each read at a block the quorum
/// agrees on then reads its header there too.
fn liveness_quorum(calls: &Calls) -> (Providers, Arc<Mutex<ChainState>>) {
    let members = (0xa1..=0xa3)
        .map(|byte| (Address::repeat_byte(byte), format!("member-{byte}:9000"), Address::repeat_byte(byte + 0x10)))
        .collect();
    let chain = Arc::new(Mutex::new(ChainState { members, ..ChainState::default() }));
    let providers = (0..3)
        .map(|i| {
            let (counted, ahead) = if i == 0 { (calls.clone(), 1) } else { (Calls::default(), 0) };
            (format!("p{i}"), Counted(Liveness(chain.clone(), ahead), counted))
        })
        .collect();
    (providers, chain)
}

/// The calls, by method, of one read of each fixed-interval reader, through the quorum, while a turn of a
/// set of three names the operator.
async fn reads() -> BTreeMap<&'static str, ByMethod> {
    let calls = Calls::default();
    let take = || calls.take().into_iter().map(|(method, count)| (method, count as f64)).collect::<ByMethod>();
    let (providers, chain) = liveness_quorum(&calls);
    let reader = crate::custody::StandbyReader::over(providers.clone(), LOG_HEADS, DOMAIN.standby.chain_id);
    let quorum = Quorum::new(providers).unwrap();
    let mut out = BTreeMap::new();
    // The operator's rule: the domain under the agreed block, then the view.
    {
        let block = reader.latest().await.unwrap();
        let standby = StandbyDomain::at_if_scheduled(&block, DOMAIN.standby.chain_id, LIVENESS_VAULT).await;
        let domain = LivenessDomain { standby: standby.unwrap().unwrap(), log_heads: LOG_HEADS };
        view(&block, &domain).await.unwrap();
    }
    out.insert("rule read", take());
    // Its members, read at the same block as a rule read, every minute.
    {
        let block = reader.latest().await.unwrap();
        take();
        members(&block, &DOMAIN.standby, (1, 20_400, 0, engine_core::types::AccountId([0; 20]))).await.unwrap();
    }
    out.insert("rule members read", take());
    // A member's caller: the view under its fixed domain.
    view(&reader.latest().await.unwrap(), &DOMAIN).await.unwrap();
    out.insert("caller read", take());
    {
        let block = reader.latest().await.unwrap();
        let standby = StandbyDomain::at_if_scheduled(&block, DOMAIN.standby.chain_id, LIVENESS_VAULT).await;
        pending_fallback(&block, &standby.unwrap().unwrap()).await.unwrap();
    }
    out.insert("fallback read", take());
    crate::log_heads::posted(&quorum, LOG_HEADS, DOMAIN.standby.chain_id).await.unwrap();
    out.insert("log-head read", take());
    // A following member's stint: whether a promotion names its standby key and whether the schedule still
    // lists the key as promotable, each at the latest agreed block, then the posted head it pins its copy
    // to, as a log-head read.
    let (candidate, key) = (AccountId([0xa1; 20]), AccountId([0xb1; 20]));
    crate::standby_key::chain::standby_state(&reader, &DOMAIN.standby, candidate, key).await.unwrap();
    promotable(&reader.latest().await.unwrap(), &DOMAIN.standby, candidate).await.unwrap();
    crate::log_heads::posted(&quorum, LOG_HEADS, DOMAIN.standby.chain_id).await.unwrap();
    out.insert("member stint", take());
    // The operator's standby renewal: whether the schedule still lists its standby key as promotable.
    promotable(&reader.latest().await.unwrap(), &DOMAIN.standby, candidate).await.unwrap();
    out.insert("standby renewal", take());
    // The poster's turn read follows its posted read, at the block that read agreed.
    let (_, at) = quorum.posted_head(LOG_HEADS).await.unwrap();
    view(&QuorumBlock::at(&quorum, at).await.unwrap(), &DOMAIN).await.unwrap();
    out.insert("log-head turn read", take());
    quorum.finalized_balance(Address::repeat_byte(0x55)).await.unwrap();
    out.insert("relayer balance read", take());
    // Before the registry names a schedule, as at launch, the rule, the fixer and the turn read stop at
    // the domain, and the rule reads the vault's signer.
    chain.lock().unwrap().scheduled = false;
    {
        let block = reader.latest().await.unwrap();
        let standby = StandbyDomain::at_if_scheduled(&block, DOMAIN.standby.chain_id, LIVENESS_VAULT).await;
        assert!(standby.unwrap().is_none());
        crate::liveness::chain::vault_signer(&block, LIVENESS_VAULT).await.unwrap();
    }
    out.insert("rule read, no schedule", take());
    {
        let block = reader.latest().await.unwrap();
        StandbyDomain::at_if_scheduled(&block, DOMAIN.standby.chain_id, LIVENESS_VAULT).await.unwrap();
    }
    out.insert("fallback read, no schedule", take());
    crate::log_heads::posted(&quorum, LOG_HEADS, DOMAIN.standby.chain_id).await.unwrap();
    take();
    let (_, at) = quorum.posted_head(LOG_HEADS).await.unwrap();
    let block = QuorumBlock::at(&quorum, at).await.unwrap();
    StandbyDomain::at_if_scheduled(&block, DOMAIN.standby.chain_id, LIVENESS_VAULT).await.unwrap();
    out.insert("log-head turn read, no schedule", take());
    out
}

/// Adds `calls` to `into`, each `times` over.
fn add(into: &mut ByMethod, calls: &ByMethod, times: f64) {
    for (method, count) in calls {
        *into.entry(method).or_default() += count * times;
    }
}

#[tokio::test]
#[ignore = "prints the RPC budget; run by hand"]
async fn rpc_budget() {
    let mut watcher = ByMethod::new();
    for per_poll in [4, 60] {
        watcher = watcher_per_poll(per_poll).await;
        println!("== deposit watcher at {per_poll} new blocks a poll: per poll, per provider");
        for (method, calls) in &watcher {
            println!("   {method}: {calls:.2}");
        }
        println!("   total: {:.2}", watcher.values().sum::<f64>());
    }
    let reads = reads().await;
    println!("== one read of each fixed-interval reader, per provider");
    for (reader, calls) in &reads {
        println!("   {reader}: {}", calls.values().sum::<f64>());
    }
    // A day of each node's readers while a turn of a set of three names the operator: the operator's
    // watcher at 15 s on Arbitrum One (60 blocks a poll), its rule's reads at 15 s and members at 60 s, its
    // poster's reads every 10 s with a turn read before each post every 20 s, its relayer's balance at 5
    // minutes, and its standby renewal every minute; each member's caller at 15 s and follow stint every
    // 16 s (a 15 s stint and its 1 s pause); and every node's fixer at 30 s.
    let in_turn: Readers = vec![
        ("operator's deposit watcher", &watcher, DAY / 15.0),
        ("operator's liveness rule", &reads["rule read"], DAY / 15.0),
        ("operator's liveness rule's members", &reads["rule members read"], DAY / 60.0),
        ("operator's log-head reads", &reads["log-head read"], DAY / 10.0),
        ("operator's log-head turn reads", &reads["log-head turn read"], DAY / 20.0),
        ("operator's relayer balance", &reads["relayer balance read"], DAY / 300.0),
        ("operator's standby renewal", &reads["standby renewal"], DAY / 60.0),
        ("liveness callers, two members", &reads["caller read"], 2.0 * DAY / 15.0),
        ("follow stints, two members", &reads["member stint"], 2.0 * DAY / 16.0),
        ("fallback fixers, three nodes", &reads["fallback read"], 3.0 * DAY / 30.0),
    ];
    day("one operator and two members while a turn names the operator", &in_turn);
    // Before the registry names a schedule, as at launch: the rule reads at 15 s, and the poster reads the
    // posted head and the turn every 30 s; three nodes' fixers read at 30 s.
    let unscheduled: Readers = vec![
        ("operator's deposit watcher", &watcher, DAY / 15.0),
        ("operator's liveness rule", &reads["rule read, no schedule"], DAY / 15.0),
        ("operator's log-head reads", &reads["log-head read"], DAY / 30.0),
        ("operator's log-head turn reads", &reads["log-head turn read, no schedule"], DAY / 30.0),
        ("operator's relayer balance", &reads["relayer balance read"], DAY / 300.0),
        ("fallback fixers, three nodes", &reads["fallback read, no schedule"], 3.0 * DAY / 30.0),
    ];
    day("three nodes before the registry names a schedule", &unscheduled);
}

/// Prints a day of `readers` per provider, by reader and by method, before the seed relays and the
/// operator list.
fn day(name: &str, readers: &Readers) {
    println!("== {name}, per provider per day, before the seed relays and the operator list");
    let mut account = ByMethod::new();
    for (reader, calls, times) in readers {
        add(&mut account, calls, *times);
        println!("   {reader}: {:.0}", calls.values().sum::<f64>() * times);
    }
    for (method, calls) in &account {
        println!("   {method}: {calls:.0}");
    }
    println!("   total: {:.0}", account.values().sum::<f64>());
}
