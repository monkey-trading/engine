//! The seed relay dev scenario, which `scripts/dev-seed-relay.sh` runs: two Anvil chains, one as
//! Ethereum, whose built-in EIP-4788 contract records a stand-in beacon node's block roots as the
//! protocol would, and one as Arbitrum; a stand-in bridge that delivers each ticket the Inbox records
//! from the relay's aliased address; and the node's own relayer task, built from a node config,
//! reading each chain through three RPC proxies.
//! - The relayer proves and relays three daily seeds, and the receiver records each as its epoch's mix.
//! - A relay proving another epoch's mix reverts on Ethereum, and a seed from another sender, or for
//!   another epoch, reverts on Arbitrum.
//! - With the beacon node stopped, relaying fails (`seed_relay_failed`); once the next turn's draw
//!   deadline passes, `seed_missing` fires, and that turn draws from the fallback.

use std::process::{Command as Process, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use alloy::primitives::{Address, B256, Bytes, U64, U256, keccak256};
use alloy::rpc::client::RpcClient;
use alloy::rpc::types::{Filter, Log, TransactionRequest};
use alloy::sol;
use alloy::sol_types::{SolCall, SolError, SolEvent, SolValue};
use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::task::JoinHandle;
use tokio::time::sleep;

use crate::checkpoints::scenario::{Anvil, deploy, transact};
use crate::config::{FinalityTag, NodeConfig, RpcProviderSection};
use crate::metrics::Metrics;
use crate::seed_relay::abi::{ISeedReceiver, ISeedRelay};
use crate::seed_relay::schedule::{draw_deadline, last_final_turn, seed_epoch};
use crate::seed_relay::state::{prove_mix, synthetic_state};
use crate::seed_relay::{BeaconBlockHeader, witness};
use crate::test_support::{ANVIL_CONFIG, Alert, capture_alerts_and_resolutions, wait_for};

sol! {
    event TicketCreated(uint256 indexed ticket, address indexed sender, address indexed to, bytes data);
    function seedOf(uint64 turn) external view returns (bytes32);
    function seedEpoch(uint64 turn) external view returns (uint64);
    function drawDeadline(uint64 turn) external view returns (uint64);
    error NotRelay(address sender);
    error WrongEpoch(uint64 turn, uint64 epoch, uint64 expected);
}

/// The dev beacon chain's genesis time.
const GENESIS: u64 = 1_700_000_000;
/// The address the protocol writes each block's parent beacon root from.
const SYSTEM: Address = alloy::primitives::address!("fffffffffffffffffffffffffffffffffffffffe");
const FULU: [u8; 4] = [6, 0, 0, 0];
/// What Arbitrum adds to an Ethereum contract's address as its messages' sender.
const ALIAS_OFFSET: Address = alloy::primitives::address!("1111000000000000000000000000000000001111");

#[tokio::test]
#[ignore = "dev scenario: needs Anvil, Python 3 and `forge build`; run by scripts/dev-seed-relay.sh"]
async fn dev_scenario_three_daily_seeds_reach_arbitrum_and_a_missing_one_alerts() {
    let (_guard, mut alerts, _) = capture_alerts_and_resolutions();
    // Turn `first`'s deadline is an hour after the chains start.
    let first = last_final_turn(GENESIS) + 10;
    // Local chain ids: Anvil applies a public network's hardfork schedule by its id and the time.
    let ethereum = Chain::start(31337, draw_deadline(first).unwrap() - 3600).await;
    let arbitrum = Chain::start(31338, draw_deadline(first).unwrap() - 3600).await;
    let (relay, receiver, inbox) = deploy_pair(&ethereum, &arbitrum).await;
    let beacon = Beacon::default();
    let server = beacon.serve().await;
    let bridge = tokio::spawn(bridge(ethereum.client.clone(), arbitrum.client.clone(), inbox, receiver));
    let cfg = node_config(&ethereum, &arbitrum, relay, receiver, &server.0);
    let metrics = Metrics::new();
    let relayer = tokio::spawn(crate::seed_relay::task(&cfg, &metrics).unwrap().expect("the relayer is configured"));

    // The schedule the relayer picks turns by is the contracts'.
    for turn in [first + 1, first + 4] {
        assert_eq!(arbitrum.view(receiver, seedEpochCall { turn }).await, seed_epoch(turn, GENESIS).unwrap());
        assert_eq!(arbitrum.view(receiver, drawDeadlineCall { turn }).await, draw_deadline(turn).unwrap());
    }

    // Day by day, the two turns after the final one are relayed once their epochs are finalized.
    for (day, relayed) in [(0, vec![first + 1, first + 2]), (1, vec![first + 3])] {
        advance(&ethereum, &arbitrum, &beacon, draw_deadline(first + day).unwrap() + 3600).await;
        for turn in relayed {
            let seed = arbitrum.wait_for_seed(receiver, turn).await;
            assert_eq!(seed, B256::from(dev_mix(seed_epoch(turn, GENESIS).unwrap())), "turn {turn}");
        }
    }

    // A witness of another epoch's mix is refused on Ethereum.
    let (header, state) = beacon.finalized().unwrap();
    let turn = first + 3;
    let other = prove_mix(&state, seed_epoch(turn, GENESIS).unwrap() - 1).unwrap();
    let timestamp = GENESIS + 12 * (header.slot + 1);
    let fees =
        ISeedRelay::Fees { maxSubmissionCost: U256::from(1) << 64, gasLimit: U256::from(1), maxFeePerGas: U256::ZERO };
    let input = ISeedRelay::relayCall { turn, witness: witness(timestamp, &header, &other), fees }.abi_encode();
    let refusal = ethereum.refusal(ethereum.accounts[2], relay, input, U256::from(1) << 64).await;
    assert_eq!(refusal[..4], ISeedRelay::InvalidSeedProof::SELECTOR);
    // On Arbitrum, a seed from another sender, or for another epoch, is refused.
    let epoch = seed_epoch(first + 5, GENESIS).unwrap();
    let call =
        |epoch| ISeedReceiver::receiveSeedCall { turn: first + 5, epoch, seed: B256::repeat_byte(1) }.abi_encode();
    let refusal = arbitrum.refusal(arbitrum.accounts[0], receiver, call(epoch), U256::ZERO).await;
    assert_eq!(refusal, NotRelay { sender: arbitrum.accounts[0] }.abi_encode());
    let alias = alias(relay);
    arbitrum.impersonate(alias).await;
    let refusal = arbitrum.refusal(alias, receiver, call(epoch + 1), U256::ZERO).await;
    assert_eq!(refusal, WrongEpoch { turn: first + 5, epoch: epoch + 1, expected: epoch }.abi_encode());

    // With the beacon node stopped, the next turn's relay fails, and once its deadline passes the
    // final turn has no seed. Polls before day 0, when the beacon node had no finalized block, failed
    // too: those lines are dropped first.
    while alerts.try_recv().is_ok() {}
    server.1.abort();
    advance(&ethereum, &arbitrum, &beacon, draw_deadline(first + 2).unwrap() + 3600).await;
    let failed = wait_for("seed_relay_failed", next(&mut alerts, "seed_relay_failed")).await;
    assert!(failed["message"].contains(&format!("[{}]", first + 4)), "{failed:?}");
    arbitrum.warp(draw_deadline(first + 4).unwrap() + 60).await;
    let missing = wait_for("seed_missing", next(&mut alerts, "seed_missing")).await;
    assert_eq!(missing["level"], "ERROR");
    assert!(missing["message"].contains(&format!("turn {}'s seed", first + 4)), "{missing:?}");
    let last = B256::from(dev_mix(seed_epoch(first + 3, GENESIS).unwrap()));
    let fallback = keccak256((last, first + 4).abi_encode_params());
    assert_eq!(arbitrum.view(receiver, seedOfCall { turn: first + 4 }).await, fallback);
    relayer.abort();
    bridge.abort();
}

// ── Helpers ──

/// One Anvil chain, its three RPC proxies, and its unlocked accounts.
struct Chain {
    _anvil: Anvil,
    _proxies: Vec<Anvil>,
    client: RpcClient,
    url: String,
    proxies: Vec<String>,
    accounts: Vec<Address>,
}

impl Chain {
    async fn start(chain_id: u64, time: u64) -> Self {
        let port = free_port();
        let anvil = Anvil(
            Process::new("anvil")
                .args(["--port", &port.to_string(), "--silent", "--chain-id", &chain_id.to_string()])
                .args(["--timestamp", &time.to_string()])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("Anvil installed"),
        );
        let url = format!("http://127.0.0.1:{port}");
        let client = RpcClient::new_http(url.parse().unwrap());
        wait_for("Anvil to answer", async {
            while client.request::<_, U64>("eth_chainId", ()).await.is_err() {
                sleep(Duration::from_millis(30)).await;
            }
        })
        .await;
        let (proxies, urls) = proxies(&url).await;
        let accounts = client.request("eth_accounts", ()).await.unwrap();
        Self { _anvil: anvil, _proxies: proxies, client, url, proxies: urls, accounts }
    }

    async fn rpc(&self, method: &'static str, params: Value) -> Value {
        self.client.request(method, params).await.unwrap()
    }

    /// Mines a block at `time`.
    async fn warp(&self, time: u64) {
        self.rpc("evm_setNextBlockTimestamp", json!([time])).await;
        self.rpc("evm_mine", json!([])).await;
    }

    async fn impersonate(&self, account: Address) {
        self.rpc("anvil_setBalance", json!([account, "0xde0b6b3a7640000"])).await;
        self.rpc("anvil_impersonateAccount", json!([account])).await;
    }

    async fn view<C: SolCall>(&self, to: Address, call: C) -> C::Return {
        let request = TransactionRequest::default().to(to).input(call.abi_encode().into());
        let out: Bytes = self.client.request("eth_call", (request, "latest")).await.unwrap();
        C::abi_decode_returns(&out).unwrap()
    }

    /// The revert data of `input` sent from `from`, which must revert.
    async fn refusal(&self, from: Address, to: Address, input: Vec<u8>, value: U256) -> Vec<u8> {
        let request = TransactionRequest::default().from(from).to(to).value(value).input(input.into());
        let error = self.client.request::<_, Bytes>("eth_call", (request, "latest")).await.unwrap_err();
        error.as_error_resp().and_then(|payload| payload.as_revert_data()).expect("a revert").to_vec()
    }

    async fn wait_for_seed(&self, receiver: Address, turn: u64) -> B256 {
        wait_for(&format!("turn {turn}'s seed on Arbitrum"), async {
            loop {
                let seed = self.view(receiver, ISeedReceiver::relayedSeedCall { turn }).await;
                if !seed.is_zero() {
                    return seed;
                }
                sleep(Duration::from_millis(100)).await;
            }
        })
        .await
    }
}

/// A finalized block's header and its serialized state.
type Finalized = (BeaconBlockHeader, Arc<Vec<u8>>);

/// A beacon node that serves one synthetic finalized block and its state.
#[derive(Clone, Default)]
struct Beacon(Arc<Mutex<Option<Finalized>>>);

impl Beacon {
    /// Finalizes a block at `slot` and returns its root.
    fn finalize(&self, slot: u64) -> B256 {
        let state = synthetic_state(slot, FULU, dev_mix);
        let state_root = prove_mix(&state, slot / 32 - 1).unwrap().state_root;
        let header =
            BeaconBlockHeader { slot, proposer_index: 0, parent_root: B256::ZERO, state_root, body_root: B256::ZERO };
        *self.0.lock().unwrap() = Some((header, Arc::new(state)));
        header.root()
    }

    fn finalized(&self) -> Option<Finalized> {
        self.0.lock().unwrap().clone()
    }

    /// Serves the beacon API on a free port: its URL, and the task to abort to stop it.
    async fn serve(&self) -> (String, JoinHandle<()>) {
        let app = Router::new()
            .route("/eth/v1/beacon/headers/finalized", get(header))
            .route("/eth/v2/debug/beacon/states/{slot}", get(state))
            .with_state(self.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        (url, tokio::spawn(async move { axum::serve(listener, app).await.unwrap() }))
    }
}

async fn header(State(beacon): State<Beacon>) -> Response {
    let Some((header, _)) = beacon.finalized() else { return StatusCode::NOT_FOUND.into_response() };
    let message = json!({
        "slot": header.slot.to_string(),
        "proposer_index": header.proposer_index.to_string(),
        "parent_root": header.parent_root,
        "state_root": header.state_root,
        "body_root": header.body_root,
    });
    axum::Json(json!({"data": {"root": header.root(), "header": {"message": message}}})).into_response()
}

async fn state(State(beacon): State<Beacon>, Path(slot): Path<u64>) -> Response {
    match beacon.finalized() {
        Some((header, state)) if header.slot == slot => {
            let mut response = state.as_ref().clone().into_response();
            response.headers_mut().insert("eth-consensus-version", HeaderValue::from_static("fulu"));
            response
        }
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Moves both chains to `time`: the beacon node finalizes a block about 16 minutes earlier, and
/// Ethereum's EIP-4788 contract records its root in the next slot's block, as the protocol's system
/// call does.
async fn advance(ethereum: &Chain, arbitrum: &Chain, beacon: &Beacon, time: u64) {
    let slot = (time - GENESIS) / 12 - 80;
    let root = beacon.finalize(slot);
    ethereum.impersonate(SYSTEM).await;
    ethereum.rpc("evm_setNextBlockTimestamp", json!([GENESIS + 12 * (slot + 1)])).await;
    let record = TransactionRequest::default().from(SYSTEM).to(crate::seed_relay::ethereum::BEACON_ROOTS);
    let receipt = transact(&ethereum.client, record.input(root.to_vec().into())).await;
    assert!(receipt.status(), "the beacon root is recorded");
    arbitrum.warp(time).await;
}

/// Deploys the Inbox stand-in and the relay on Ethereum and the receiver on Arbitrum, the receiver
/// naming the relay's address before the relay exists.
async fn deploy_pair(ethereum: &Chain, arbitrum: &Chain) -> (Address, Address, Address) {
    let deployer = ethereum.accounts[0];
    let inbox = deploy(&ethereum.client, deployer, "MockInbox.sol/MockInbox.json", &[]).await;
    let nonce: U64 = ethereum.client.request("eth_getTransactionCount", (deployer, "latest")).await.unwrap();
    let relay = deployer.create(nonce.to());
    let receiver = deploy(
        &arbitrum.client,
        arbitrum.accounts[0],
        "SeedReceiver.sol/SeedReceiver.json",
        &(relay, GENESIS).abi_encode_params(),
    )
    .await;
    let forks = vec![alloy::primitives::FixedBytes::<4>::from(FULU)];
    let args = (inbox, receiver, GENESIS, forks).abi_encode_params();
    assert_eq!(deploy(&ethereum.client, deployer, "SeedRelay.sol/SeedRelay.json", &args).await, relay);
    (relay, receiver, inbox)
}

/// Delivers every ticket the Inbox records to its target on Arbitrum, from its sender's alias.
async fn bridge(ethereum: RpcClient, arbitrum: RpcClient, inbox: Address, receiver: Address) {
    let mut from = 0u64;
    loop {
        let head: U64 = ethereum.request("eth_blockNumber", ()).await.unwrap();
        if head.to::<u64>() < from {
            sleep(Duration::from_millis(100)).await;
            continue;
        }
        let filter = Filter::new()
            .address(inbox)
            .event_signature(TicketCreated::SIGNATURE_HASH)
            .from_block(from)
            .to_block(head.to::<u64>());
        let logs: Vec<Log> = ethereum.request("eth_getLogs", (filter,)).await.unwrap();
        for log in logs {
            let ticket = TicketCreated::decode_log_data(&log.inner.data).unwrap();
            assert_eq!(ticket.to, receiver);
            let sender = alias(ticket.sender);
            let _: Value = arbitrum.request("anvil_setBalance", json!([sender, "0xde0b6b3a7640000"])).await.unwrap();
            let _: Value = arbitrum.request("anvil_impersonateAccount", json!([sender])).await.unwrap();
            // A ticket the receiver refuses, such as a turn relayed twice, stays undelivered.
            transact(&arbitrum, TransactionRequest::default().from(sender).to(receiver).input(ticket.data.into()))
                .await;
        }
        from = head.to::<u64>() + 1;
        sleep(Duration::from_millis(100)).await;
    }
}

/// The node config the relayer runs from: the Anvil config with its custody providers on Arbitrum's
/// proxies, and a `[seed_relay]` section on Ethereum's, signing as Ethereum's second account.
fn node_config(ethereum: &Chain, arbitrum: &Chain, relay: Address, receiver: Address, beacon: &str) -> NodeConfig {
    let mut cfg = NodeConfig::load(std::path::Path::new(ANVIL_CONFIG)).unwrap();
    // No vault is deployed: the custody quorum's identity read finds no token there, alike on every provider.
    cfg.custody.vault = format!("{}", Address::repeat_byte(0x99));
    let evm = cfg.custody.evm.as_mut().unwrap();
    evm.finality_tag = FinalityTag::Latest;
    evm.providers = providers(&arbitrum.proxies);
    let providers: String = ethereum
        .proxies
        .iter()
        .enumerate()
        .map(|(i, url)| format!("[[providers]]\nname = 'ethereum-{i}'\nrpc_url = '{url}'\n"))
        .collect();
    cfg.seed_relay = Some(
        toml::from_str(&format!(
            "chain_id = 31337\nrpc_url = '{}'\nrelayer = '{}'\nbeacon_api = '{beacon}'\nrelay = '{relay}'\n\
             receiver = '{receiver}'\npoll_interval = '1s'\nrpc_timeout = '5s'\nstate_timeout = '30s'\n\
             resend_after = '1h'\nl2_max_fee_per_gas = '1'\nmin_balance = '0.1'\n{providers}",
            ethereum.url, ethereum.accounts[1],
        ))
        .unwrap(),
    );
    cfg
}

fn providers(urls: &[String]) -> Vec<RpcProviderSection> {
    urls.iter()
        .enumerate()
        .map(|(i, url)| toml::from_str(&format!("name = 'arbitrum-{i}'\nrpc_url = '{url}'")).unwrap())
        .collect()
}

/// Three proxies to `target`, so the quorum reads from three URLs.
async fn proxies(target: &str) -> (Vec<Anvil>, Vec<String>) {
    let mut children = Vec::new();
    let mut urls = Vec::new();
    for _ in 0..3 {
        let port = free_port();
        children.push(Anvil(
            Process::new("python3")
                .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/../../scripts/dev-rpc-proxy.py"))
                .args(["--port", &port.to_string(), "--target", target])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("Python3 installed"),
        ));
        wait_for("the RPC proxy to accept connections", async {
            while tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_err() {
                sleep(Duration::from_millis(30)).await;
            }
        })
        .await;
        urls.push(format!("http://127.0.0.1:{port}"));
    }
    (children, urls)
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// Arbitrum's alias of an Ethereum contract's address.
fn alias(address: Address) -> Address {
    let offset = U256::from_be_slice(ALIAS_OFFSET.as_slice());
    let sum = U256::from_be_slice(address.as_slice()).wrapping_add(offset);
    Address::from_slice(&sum.to_be_bytes::<32>()[12..])
}

/// The stand-in beacon chain's RANDAO mix for `epoch`.
fn dev_mix(epoch: u64) -> [u8; 32] {
    keccak256([b"dev randao".as_slice(), &epoch.to_le_bytes()].concat()).0
}

/// The next line named `name`, skipping others.
async fn next(lines: &mut UnboundedReceiver<Alert>, name: &str) -> Alert {
    loop {
        let line = lines.recv().await.expect("the alert channel stays open");
        if line.get("alert").is_some_and(|value| value == name) {
            return line;
        }
    }
}
