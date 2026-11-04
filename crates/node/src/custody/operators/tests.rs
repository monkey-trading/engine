use std::sync::{Arc, Mutex};
use std::time::Duration;

use alloy::primitives::{Address, B256, Bytes, U256};
use alloy::sol_types::{SolCall, SolValue};
use serde_json::{Value, json};

use super::lens::registryCall;
use super::lens::{CANDIDATE_LIMIT, Election, Operator, Overview, Parameters, Row, candidatesCall, overviewCall};
use super::*;
use crate::custody::chain::BlockLink;
use crate::custody::evm::{Finality, StakingEvent, VaultDeposit, VaultIdentity};

const CHAIN_ID: u64 = 42_161;
const USDC: Address = Address::repeat_byte(0x0c);
const REGISTRY: Address = Address::repeat_byte(0x0a);
const LENS: Address = Address::repeat_byte(0x0b);
const FINAL: u64 = 1_000;
const FINAL_TIME: u64 = 1_792_504_800;

/// The block the stand-in chain declares final.
fn final_block() -> BlockRef {
    BlockRef { number: FINAL, hash: B256::repeat_byte(0xf1) }
}

/// What a provider says the lens holds, and what it did when asked.
#[derive(Clone)]
struct Model {
    chain_id: u64,
    token: Option<Address>,
    tagged: Option<BlockRef>,
    lens_registry: Address,
    overview: Overview,
    rows: Vec<Row>,
}

#[derive(Clone)]
struct Provider {
    model: Model,
    /// The selector of each lens call made, in order, shared by every clone.
    calls: Arc<Mutex<Vec<[u8; 4]>>>,
    /// Fails every lens call, as a provider that lost the block's state.
    down: bool,
    /// Fails the lens calls with this selector only, as a provider that caps the gas of one.
    fails: Option<[u8; 4]>,
    /// Fails the `candidates` pages from this start, as a gas cap only the later pages reach.
    fail_from: Option<usize>,
}

/// `index`'s candidate: addresses and registration order run opposite ways, so a list in the lens's
/// order is not in registration order.
fn row(index: usize, total: usize) -> Row {
    let byte = u8::try_from(index + 1).unwrap();
    Row {
        candidate: Address::repeat_byte(byte),
        adminKey: Address::repeat_byte(byte.wrapping_add(100)),
        enclave: Address::repeat_byte(byte.wrapping_add(50)),
        bond: U256::from(2_000u64) * U256::from(10).pow(U256::from(18)),
        release: B256::repeat_byte(byte),
        registration: (total - index) as u64,
        apiHostname: format!("api-{index}.example").into_bytes().into(),
        handoverEndpoint: format!("handover-{index}.example:9443").into_bytes().into(),
        votes: U256::from(index as u64 + 1) * U256::from(10).pow(U256::from(18)),
        eligible: index.is_multiple_of(2),
        role: u8::from(index == 1) * 2,
    }
}

fn overview(count: usize) -> Overview {
    Overview {
        rotationEnabled: true,
        parameters: Parameters {
            epochLength: 86_400,
            minTerm: 604_800,
            finalityMargin: 600,
            marginBps: 1_000,
            eligibilityBps: 500,
            quorumBps: 2_000,
            maxCandidates: 100,
            minSelfBond: U256::from(1_000u64) * U256::from(10).pow(U256::from(18)),
            livenessTimeout: 259_200,
        },
        openEpoch: 7,
        openEpochStart: FINAL_TIME - 3_600,
        totalStaked: U256::from(9_000u64) * U256::from(10).pow(U256::from(18)),
        candidateCount: U256::from(count),
        inEffect: Operator {
            candidate: Address::repeat_byte(2),
            adminKey: Address::repeat_byte(102),
            release: B256::repeat_byte(2),
            since: FINAL_TIME - 86_400,
        },
        named: Operator {
            candidate: Address::repeat_byte(2),
            adminKey: Address::repeat_byte(102),
            release: B256::repeat_byte(2),
            since: FINAL_TIME - 86_400,
        },
        pending: Election {
            epoch: 6,
            candidate: Address::repeat_byte(3),
            adminKey: Address::repeat_byte(103),
            release: B256::repeat_byte(3),
            effectiveAt: FINAL_TIME + 604_800,
            promoted: false,
        },
        pendingPromotion: false,
    }
}

impl Provider {
    /// A provider whose lens lists `count` candidates.
    fn with(count: usize) -> Self {
        let model = Model {
            chain_id: CHAIN_ID,
            token: Some(USDC),
            tagged: Some(final_block()),
            lens_registry: REGISTRY,
            overview: overview(count),
            rows: (0..count).map(|index| row(index, count)).collect(),
        };
        Self { model, calls: Arc::default(), down: false, fails: None, fail_from: None }
    }

    fn calls(&self) -> Vec<[u8; 4]> {
        self.calls.lock().unwrap().clone()
    }
}

impl VaultReader for Provider {
    async fn identity(&self) -> anyhow::Result<VaultIdentity> {
        Ok(VaultIdentity { chain_id: self.model.chain_id, token: self.model.token })
    }

    async fn finality(&self) -> anyhow::Result<Finality> {
        Ok(Finality { head: FINAL + 2, head_time: FINAL_TIME, tagged: self.model.tagged, head_support: None })
    }

    async fn headers(&self, from: u64, to: u64) -> anyhow::Result<Vec<BlockLink>> {
        Ok((from..=to)
            .map(|number| BlockLink {
                number,
                hash: if number == FINAL { final_block().hash } else { B256::repeat_byte(0x11) },
                parent_hash: B256::ZERO,
                timestamp: if number == FINAL { FINAL_TIME } else { 0 },
            })
            .collect())
    }

    async fn deposits(&self, _from: u64, _to: u64) -> anyhow::Result<Vec<VaultDeposit>> {
        Ok(Vec::new())
    }

    async fn deposit_count(&self, _at: BlockRef) -> anyhow::Result<u64> {
        Ok(0)
    }

    async fn staking_events(&self, _blocks: &[BlockRef]) -> anyhow::Result<Vec<StakingEvent>> {
        Ok(Vec::new())
    }

    async fn contract_call(&self, at: BlockRef, to: Address, data: Bytes) -> anyhow::Result<Bytes> {
        anyhow::ensure!(at == final_block() && to == LENS, "the call is not to the lens at the final block");
        anyhow::ensure!(!self.down, "the provider lost the block's state");
        let selector: [u8; 4] = data[..4].try_into().unwrap();
        anyhow::ensure!(self.fails != Some(selector), "the provider caps this call's gas");
        self.calls.lock().unwrap().push(selector);
        let model = &self.model;
        Ok(match selector {
            s if s == registryCall::SELECTOR => registryCall::abi_encode_returns(&model.lens_registry).into(),
            s if s == overviewCall::SELECTOR => overviewCall::abi_encode_returns(&model.overview).into(),
            s if s == candidatesCall::SELECTOR => {
                let call = candidatesCall::abi_decode(&data)?;
                anyhow::ensure!(
                    self.fail_from.is_none_or(|from| call.start < U256::from(from)),
                    "the page is over the gas cap"
                );
                let start = usize::try_from(call.start)?.min(model.rows.len());
                let end = usize::try_from(call.limit)?.saturating_add(start).min(model.rows.len());
                candidatesCall::abi_encode_returns(&model.rows[start..end].to_vec()).into()
            }
            other => anyhow::bail!("unexpected selector {other:?}"),
        })
    }
}

/// A reader over three providers, honest or as `tweak` leaves them, and its feed.
fn reader(providers: [Provider; 3], stale_after: Duration) -> (OperatorsReader<Provider>, OperatorsFeed) {
    let quorum = Quorum::new(providers.into_iter().enumerate().map(|(i, p)| (format!("p{i}"), p)).collect())
        .unwrap()
        .named("operators", false);
    OperatorsReader::new(quorum, (CHAIN_ID, USDC), (REGISTRY, LENS), Duration::from_secs(60), stale_after)
}

fn honest(count: usize) -> [Provider; 3] {
    let provider = Provider::with(count);
    [provider.clone(), provider.clone(), provider]
}

/// A feed already holding the list of `count` candidates, read now, served for `stale_after`.
pub(crate) async fn feed_with_a_list(count: usize, stale_after: Duration) -> OperatorsFeed {
    let (reader, feed) = reader(honest(count), stale_after);
    let list = reader.read().await.unwrap();
    reader.reads.send_replace(Some(Arc::new(Read { list: Arc::new(list), at: Instant::now() })));
    // The feed reads the channel, which lives as long as the sender: keep the sender alive.
    std::mem::forget(reader);
    feed
}

fn json_of(list: &OperatorList) -> Value {
    serde_json::to_value(list).unwrap()
}

#[tokio::test]
async fn the_list_names_its_block_and_holds_every_candidate_in_registration_order() {
    let providers = honest(3);
    let (reader, _) = reader(providers.clone(), Duration::from_secs(60));
    let list = json_of(&reader.read().await.unwrap());
    assert_eq!(
        list["block"],
        json!({ "number": FINAL, "hash": format!("{:#x}", final_block().hash), "time_ms": FINAL_TIME * 1000 })
    );
    assert_eq!(list["registry"], json!(format!("{REGISTRY:#x}")));
    assert_eq!(list["lens"], json!(format!("{LENS:#x}")));
    assert_eq!(list["rotation_enabled"], json!(true));
    assert_eq!(list["parameters"]["max_candidates"], json!(100));
    assert_eq!(list["parameters"]["epoch_length_secs"], json!(86_400));
    assert_eq!(list["parameters"]["min_self_bond"], json!("1000000000000000000000"));
    assert_eq!(
        (list["open_epoch"].clone(), list["open_epoch_start_ms"].clone()),
        (json!(7), json!((FINAL_TIME - 3_600) * 1000))
    );
    assert_eq!(list["total_staked"], json!("9000000000000000000000"));
    assert_eq!(list["operator_in_effect"]["since_ms"], json!((FINAL_TIME - 86_400) * 1000));
    assert_eq!(list["pending_election"]["candidate"], json!(format!("{:#x}", Address::repeat_byte(3))));
    assert_eq!(list["pending_election"]["effective_at_ms"], json!((FINAL_TIME + 604_800) * 1000));
    let candidates = list["candidates"].as_array().unwrap();
    // The lens listed 1, 2, 3 with registrations 3, 2, 1: the answer runs by registration.
    let order: Vec<_> = candidates.iter().map(|c| c["registration"].as_u64().unwrap()).collect();
    assert_eq!(order, [1, 2, 3]);
    let first = &candidates[0];
    assert_eq!(first["address"], json!(format!("{:#x}", Address::repeat_byte(3))));
    assert_eq!(first["self_bond"], json!("2000000000000000000000"));
    assert_eq!(first["votes"], json!("3000000000000000000"));
    assert_eq!(first["api_hostname"], json!("api-2.example"));
    assert_eq!(first["handover_endpoint"], json!("handover-2.example:9443"));
    assert_eq!((first["eligible"].clone(), first["role"].clone()), (json!(true), json!("candidate")));
    assert_eq!(candidates[1]["role"], json!("operator"));
}

#[tokio::test]
async fn a_pending_election_that_is_none_reads_as_null() {
    let mut providers = honest(1);
    for provider in &mut providers {
        provider.model.overview.pending.effectiveAt = 0;
    }
    let (reader, _) = reader(providers, Duration::from_secs(60));
    assert_eq!(json_of(&reader.read().await.unwrap())["pending_election"], Value::Null);
}

#[tokio::test]
async fn a_hundred_candidates_are_four_pages_of_twenty_five_after_the_registry_and_overview() {
    let providers = honest(CANDIDATE_LIMIT);
    let (reader, _) = reader(providers.clone(), Duration::from_secs(60));
    let list = json_of(&reader.read().await.unwrap());
    assert_eq!(list["candidates"].as_array().unwrap().len(), 100);
    let calls = providers[0].calls();
    let pages = calls.iter().filter(|selector| **selector == candidatesCall::SELECTOR).count();
    // The three providers share one log: each call is made to every one of them.
    assert_eq!((calls.len() / 3, pages / 3), (6, 4), "the registry, the overview and four pages");
}

#[tokio::test]
async fn an_empty_registry_makes_an_empty_list_without_paging() {
    let providers = honest(0);
    let (reader, _) = reader(providers.clone(), Duration::from_secs(60));
    assert_eq!(json_of(&reader.read().await.unwrap())["candidates"], json!([]));
    assert!(providers[0].calls().iter().all(|selector| *selector != candidatesCall::SELECTOR));
}

#[tokio::test]
async fn a_provider_that_lies_is_outvoted_and_two_that_lie_differently_leave_no_quorum() {
    let mut providers = honest(3);
    providers[0].model.overview.totalStaked = U256::from(1u64);
    let (liar_outvoted, _) = reader(providers.clone(), Duration::from_secs(60));
    assert_eq!(json_of(&liar_outvoted.read().await.unwrap())["total_staked"], json!("9000000000000000000000"));

    providers[1].model.overview.totalStaked = U256::from(2u64);
    let (no_majority, _) = reader(providers, Duration::from_secs(60));
    let error = no_majority.read().await.unwrap_err();
    assert!(format!("{error:#}").contains("quorum lost for lens_overview"), "{error:#}");
}

#[tokio::test]
async fn one_provider_that_lost_the_blocks_state_does_not_stop_the_read() {
    let mut providers = honest(2);
    providers[2].down = true;
    let (reader, _) = reader(providers, Duration::from_secs(60));
    assert_eq!(json_of(&reader.read().await.unwrap())["candidates"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn a_lens_call_that_fails_keeps_its_own_episode_at_warning_under_the_operators_quorum() {
    let mut providers = honest(CANDIDATE_LIMIT);
    providers.iter_mut().for_each(|provider| provider.fails = Some(candidatesCall::SELECTOR));
    let (logs, _guard) = crate::trusted_time::tests::logs::Logs::capture();
    let (reader, _) = reader(providers, Duration::from_secs(60));
    // The registry and the overview win their votes; the first page never does.
    assert!(format!("{:#}", reader.read().await.unwrap_err()).contains("quorum lost for lens_candidates_0"));
    assert_eq!(reader.quorum.lost_operations(), ["lens_candidates_0"]);
    reader.read().await.unwrap_err();
    assert_eq!(reader.quorum.lost_operations(), ["lens_candidates_0"]);
    assert_eq!((logs.warnings("custody_rpc_quorum_lost"), logs.critical("custody_rpc_quorum_lost")), (1, 0));
    // Every quorum and provider alert the reader's quorum raised names it.
    assert_eq!(logs.count("quorum=\"operators\""), logs.count("alert=\"custody_rpc_"));
    assert!(logs.count("alert=\"custody_rpc_provider_failed\"") > 0);
}

#[tokio::test]
async fn a_page_that_wins_its_vote_ends_no_other_pages_episode() {
    // The first page answers and the second never does, as under a gas cap that the larger one hits.
    let mut providers = honest(CANDIDATE_LIMIT);
    for provider in &mut providers {
        provider.fail_from = Some(25);
    }
    let (reader, _) = reader(providers, Duration::from_secs(60));
    reader.read().await.unwrap_err();
    reader.read().await.unwrap_err();
    assert_eq!(
        reader.quorum.lost_operations(),
        ["lens_candidates_1"],
        "page 0 winning again resolves nothing of page 1"
    );
}

#[tokio::test]
async fn a_failed_read_leaves_the_last_list_up_and_a_later_one_replaces_it() {
    let providers = honest(2);
    let (logs, _guard) = crate::trusted_time::tests::logs::Logs::capture();
    let (mut reader, feed) = reader(providers.clone(), Duration::from_secs(60));
    reader.refresh().await;
    let first = feed.current().expect("the first read serves");
    assert_eq!(first.candidate_count(), 2);
    // Every provider now loses the block's state.
    let down: Vec<_> = providers.iter().cloned().map(|provider| Provider { down: true, ..provider }).collect();
    reader.quorum = Quorum::new(down.into_iter().enumerate().map(|(i, p)| (format!("p{i}"), p)).collect())
        .unwrap()
        .named("operators", false);
    reader.refresh().await;
    assert_eq!(*feed.current().expect("the failed read leaves the list up"), *first);
    assert_eq!((logs.warnings("operators_read_failed"), logs.critical("operators_read_failed")), (1, 0));
}

#[tokio::test]
async fn a_candidate_whose_endpoint_is_not_utf8_is_listed_and_the_others_still_read() {
    let mut providers = honest(3);
    for provider in &mut providers {
        provider.model.rows[1].handoverEndpoint = vec![b'h', 0xff, b':'].into();
        provider.model.rows[1].apiHostname = vec![0xc3].into();
    }
    let (reader, _) = reader(providers, Duration::from_secs(60));
    let list = json_of(&reader.read().await.unwrap());
    let candidates = list["candidates"].as_array().unwrap();
    assert_eq!(candidates.len(), 3);
    // Registration order runs opposite to the lens's, so the second row listed is the second last.
    let bad = candidates.iter().find(|c| c["address"] == format!("{:#x}", Address::repeat_byte(2))).unwrap();
    assert_eq!(bad["handover_endpoint"], "h\u{fffd}:");
    assert_eq!(bad["api_hostname"], "\u{fffd}");
    assert!(candidates.iter().filter(|c| c["api_hostname"].as_str().unwrap().ends_with(".example")).count() == 2);
}

#[tokio::test]
async fn providers_on_another_chain_or_token_or_without_a_final_block_are_refused() {
    let refuse = |tweak: fn(&mut Provider)| async move {
        let mut providers = honest(1);
        providers.iter_mut().for_each(tweak);
        let (reader, _) = reader(providers, Duration::from_secs(60));
        format!("{:#}", reader.read().await.unwrap_err())
    };
    assert!(refuse(|p| p.model.chain_id = 1).await.contains("not the configured chain"));
    assert!(refuse(|p| p.model.token = Some(Address::repeat_byte(9))).await.contains("not the configured chain"));
    assert!(refuse(|p| p.model.token = None).await.contains("not the configured chain"));
    assert!(refuse(|p| p.model.tagged = None).await.contains("no final block"));
}

#[tokio::test]
async fn a_lens_that_reads_another_registry_or_answers_inconsistently_makes_no_list() {
    let refuse = |tweak: fn(&mut Model)| async move {
        let mut providers = honest(3);
        providers.iter_mut().for_each(|p| tweak(&mut p.model));
        let (reader, _) = reader(providers, Duration::from_secs(60));
        format!("{:#}", reader.read().await.unwrap_err())
    };
    assert!(refuse(|m| m.lens_registry = Address::repeat_byte(0x77)).await.contains("not the configured"));
    // More rows promised than the limit allows.
    assert!(refuse(|m| m.overview.candidateCount = U256::from(CANDIDATE_LIMIT + 1)).await.contains("lists 101"));
    // Rows promised and not delivered: the lens stops short.
    assert!(refuse(|m| m.overview.candidateCount = U256::from(5)).await.contains("rows for a page"));
    // A candidate listed twice.
    assert!(refuse(|m| m.rows[1].candidate = m.rows[0].candidate).await.contains("twice"));
    assert!(refuse(|m| m.rows[1].candidate = Address::ZERO).await.contains("zero candidate"));
    assert!(refuse(|m| m.rows[1].role = 3).await.contains("role 3"));
    assert!(refuse(|m| m.rows[1].votes = U256::MAX).await.contains("exceeds u128"));
}

#[tokio::test]
async fn the_feed_serves_nothing_before_a_read_the_list_while_fresh_and_nothing_once_stale() {
    let (reader, feed) = reader(honest(1), Duration::from_millis(200));
    assert_eq!(feed.current().unwrap_err(), Unavailable::NotRead);
    let list = reader.read().await.unwrap();
    reader.reads.send_replace(Some(Arc::new(Read { list: Arc::new(list.clone()), at: Instant::now() })));
    assert_eq!(*feed.current().unwrap(), list);
    reader
        .reads
        .send_replace(Some(Arc::new(Read { list: Arc::new(list), at: Instant::now() - Duration::from_millis(250) })));
    assert_eq!(feed.current().unwrap_err(), Unavailable::Stale);
}

#[test]
fn the_lens_selectors_and_return_data_round_trip() {
    // The contract's overview has no dynamic member, so its return data is the tuple's static
    // encoding: a fixed number of words.
    let encoded = overviewCall::abi_encode_returns(&overview(0));
    assert_eq!(encoded.len() % 32, 0);
    assert_eq!(Overview::abi_decode(&encoded).unwrap().openEpoch, 7);
    assert_eq!(candidatesCall::SELECTOR, alloy::primitives::keccak256("candidates(uint256,uint256)")[..4]);
    assert_eq!(overviewCall::SELECTOR, alloy::primitives::keccak256("overview()")[..4]);
}

/// What building the answer costs at the registry's limit: the quorum's votes over the lens's return
/// data, the ABI decode, the checks and the JSON, with the chain reads themselves free. Run with
/// `cargo test -p engine-node --lib operators::tests::build_cost -- --ignored --nocapture`.
#[tokio::test]
#[ignore = "a measurement, not a check"]
async fn build_cost_at_the_registry_limit() {
    let (reader, _) = reader(honest(CANDIDATE_LIMIT), Duration::from_secs(60));
    let runs = 200;
    let started = Instant::now();
    let mut list = reader.read().await.unwrap();
    for _ in 1..runs {
        list = reader.read().await.unwrap();
    }
    let read = started.elapsed() / runs;
    let started = Instant::now();
    let mut bytes = 0;
    for _ in 0..runs {
        bytes = serde_json::to_vec(&list).unwrap().len();
    }
    println!(
        "operators at {CANDIDATE_LIMIT} candidates: read, vote, decode and check {read:?}; serialize {:?}; {bytes} bytes",
        started.elapsed() / runs
    );
}
