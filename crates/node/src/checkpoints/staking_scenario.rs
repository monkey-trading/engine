//! MONKEY staking and USDC distributions dev scenario on a local Anvil: staking through the real
//! Staking contract, mirrored into the real sequencer, distributions at epoch ends above and below
//! the liquidity floor, USDC claims against posted checkpoint roots, unstaking through the
//! cooldown, and a restart across an epoch boundary.

mod claims;
mod scenario;

use alloy::primitives::{Address, U256};
use alloy::sol;
use alloy::sol_types::{SolCall, SolError};
use axum::body::{Body, to_bytes};
use axum::http::{Request as HttpRequest, StatusCode};
use engine_core::Engine;
use engine_core::Fixed;
use engine_core::command::{AdminCommand, Command, OrderKind, OrderRequest};
use engine_core::emissions::{EmissionSchedule, TokenUnits};
use engine_core::event::Event;
use engine_core::price::SourceQuote;
use engine_core::staking::UsdcUnits;
use engine_core::types::{AccountId, Millis, Side};
use serde_json::{Value, json};
use tokio::sync::oneshot;
use tokio::time::{sleep, timeout};
use tower::ServiceExt;

use crate::clock::Clock;
use crate::sequencer::{Request, replay_path};
use crate::test_support::sample_around;

use scenario::Scenario;

/// Tue 20 Oct 2026 14:00 UTC: where the scenario's clock starts, with emission period 0 and
/// staking epoch 0.
const NOW: Millis = 1_792_504_800_000;
/// The period budget, 1,000 MONKEY: a development value, not an approved production parameter.
const DEV_PERIOD_BUDGET: u128 = 1_000_000_000_000_000_000_000;
/// One staking epoch, half an hour; an emission period is ten minutes.
const EPOCH_MS: Millis = 30 * 60_000;
/// The unstake cooldown, a day.
const COOLDOWN_MS: Millis = 86_400_000;
const COOLDOWN_SECS: u64 = (COOLDOWN_MS / 1000) as u64;
/// The house liquidity floor: the genesis house capital, 10m USDC.
const FLOOR: Fixed = Fixed::from_int(10_000_000);
/// Commands between checkpoints.
const CHECKPOINT_EVERY: u64 = 10;

sol! {
    function claimEmissions(address account, uint256 entitled, bytes32[] siblings) external returns (uint256);
    function claimDistribution(address account, uint256 entitled, bytes32[] siblings) external returns (uint256);
    function approve(address spender, uint256 amount) external returns (bool);
    function mint(address to, uint256 amount) external;
    function stake(uint256 amount) external;
    function startUnstake(uint256 amount) external;
    function completeUnstake() external;
    function monkey() external view returns (address);
    function balanceOf(address account) external view returns (uint256);
    function latestSeq() external view returns (uint64);
    function latestRoot() external view returns (bytes32);
    function distributionsPaid() external view returns (uint256);
    error NothingToClaim(uint256 entitled, uint256 claimed);
}

#[tokio::test]
#[ignore = "requires Anvil and compiled contracts; scripts/dev-monkey-staking.sh runs it"]
async fn dev_scenario_staking_pays_stakers_above_the_floor_and_nothing_below_it() {
    let _tracing = tracing::subscriber::set_default(
        tracing_subscriber::fmt().with_ansi(false).with_writer(std::io::stderr).finish(),
    );
    let mut s = Scenario::start().await;
    let schedule = EmissionSchedule { start_ms: NOW, period_budget: TokenUnits(DEV_PERIOD_BUDGET) };
    let (status, events) = s.post("/v1/admin", s.signed(AdminCommand::InitEmissions(schedule))).await;
    assert_eq!((status, events.as_array().unwrap().last()), (StatusCode::OK, Some(&json!({"type": "admin_applied"}))));
    for staker in s.stakers {
        let (status, body) = s.post("/v1/dev/deposit", json!({ "account": staker, "amount": "1000000" })).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        s.deposits += Fixed::from_int(1_000_000);
        // The vault backs each dev deposit too, so its balance plus paid distributions always
        // equals the ledger.
        s.send_tx(s.funder, s.usdc, mintCall { to: s.vault, amount: U256::from(1_000_000_000_000u64) }.abi_encode())
            .await;
    }

    // 1. Losses of $100 and $300 share the emission period's budget 25% and 75%, and put house NAV
    //    400 above the 10m floor for the staking epoch that ends half an hour in.
    s.losses(&[(s.stakers[0], "1"), (s.stakers[1], "3")], "100000", "99900").await;
    s.advance_to(NOW + 600_000).await;
    s.submit(Command::Tick).await;
    let quarter = DEV_PERIOD_BUDGET / 4;
    for (staker, entitled) in [(s.stakers[0], quarter), (s.stakers[1], 3 * quarter)] {
        assert_eq!(s.claim_emissions(staker).await, U256::from(entitled));
    }

    // 2. Both stakers stake everything through the Staking contract; each Staked event mirrors
    //    into the engine under its own chain reference.
    for staker in s.stakers {
        let address = Address::from(staker.0);
        let amount = s.monkey_balance(staker).await;
        s.send_tx(address, s.monkey, approveCall { spender: s.staking, amount: U256::MAX }.abi_encode()).await;
        let receipt = s.transact(address, s.staking, stakeCall { amount }.abi_encode()).await;
        s.mirror_stake(receipt, staker, TokenUnits(amount.to())).await;
    }
    assert_eq!(s.staked(s.stakers[0]).await, TokenUnits(quarter));
    assert_eq!(s.staked(s.stakers[1]).await, TokenUnits(3 * quarter));

    // 3. A restart across the epoch boundary, and the first tick closes epoch 0: the 400 USDC
    //    above the floor pays the stakers 25/75, and house cash falls by the allocated total.
    s.stop().await;
    s.advance_to(NOW + EPOCH_MS).await;
    s.start_node().await;
    let events = s.submit(Command::Tick).await;
    assert_eq!(s.distribution(s.stakers[0]).await, UsdcUnits(100_000_000), "{events:?}");
    assert_eq!(s.distribution(s.stakers[1]).await, UsdcUnits(300_000_000), "{events:?}");
    assert!(events.iter().any(|e| matches!(e, Event::DistributionEpochClosed { epoch: 0, allocated, excess, .. }
        if *allocated == UsdcUnits(400_000_000) && *excess == UsdcUnits(400_000_000))));
    let replayed = replay_path(&s.dir).expect("the log replays with every checkpoint signature");
    assert_eq!(replayed.engine.staking(), s.staking_state().await.as_ref());
    assert_eq!(replayed.engine.staked_balances(), &s.staked_balances().await);

    // 4. Each staker claims its USDC against the posted root, relayed by another account; a second
    //    claim pays nothing, and the vault's balance plus what it paid still equals the ledger.
    for (staker, usdc) in [(s.stakers[0], 100u64), (s.stakers[1], 300)] {
        assert_eq!(s.claim_distribution(staker).await, U256::from(usdc) * U256::from(1_000_000));
        assert_eq!(s.refused_distribution_claim(staker).await, NothingToClaim::SELECTOR);
    }
    assert_eq!(s.usdc_balance(s.vault).await + s.view(s.vault, distributionsPaidCall {}).await, s.liabilities());

    // 5. Unstaking waits out the cooldown: the stake only leaves the engine's snapshot once the
    //    UnstakeCompleted event mirrors.
    let unstaking = quarter / 2;
    s.send_tx(
        Address::from(s.stakers[0].0),
        s.staking,
        startUnstakeCall { amount: U256::from(unstaking) }.abi_encode(),
    )
    .await;
    s.local_rpc("evm_increaseTime", serde_json::json!([COOLDOWN_SECS])).await;
    s.local_rpc("anvil_mine", serde_json::json!(["0x1"])).await;
    let receipt = s.transact(Address::from(s.stakers[0].0), s.staking, completeUnstakeCall {}.abi_encode()).await;
    assert_eq!(s.monkey_balance(s.stakers[0]).await, U256::from(quarter / 2));
    assert_eq!(s.staked(s.stakers[0]).await, TokenUnits(quarter), "the stake persists through the cooldown");
    s.mirror_unstake(receipt, s.stakers[0], TokenUnits(unstaking)).await;
    assert_eq!(s.staked(s.stakers[0]).await, TokenUnits(quarter / 2));

    // 6. Epoch 1: the house loses more than it earns, so NAV ends below the floor and nothing is
    //    distributed.
    s.losses(&[(s.stakers[0], "1")], "100000", "99600").await;
    // A winning short sells high and buys back low; the house pays the 900 win.
    s.wins(&[(s.stakers[1], "1")], "100900", "100000").await;
    assert!(s.house_nav().await < FLOOR);
    let events = s.next_epoch().await;
    assert!(events.iter().any(|e| matches!(e, Event::DistributionEpochClosed { epoch: 1, allocated, .. }
        if *allocated == UsdcUnits(0))));
    assert_eq!(s.distribution(s.stakers[0]).await, UsdcUnits(100_000_000));
    assert_eq!(s.distribution(s.stakers[1]).await, UsdcUnits(300_000_000));
    s.stop().await;
}

impl Scenario {
    /// Moves the clock to `ts`, and trusted time with it.
    async fn advance_to(&self, ts: Millis) {
        let by = u64::try_from(ts - self.clock.now_ms()).expect("time only moves forward");
        self.clock.advance(std::time::Duration::from_millis(by));
        if let Some(node) = &self.node {
            node.state.tx.send(Request::TimeSample(sample_around(self.clock.as_ref()))).await.unwrap();
        }
    }

    /// Ticks at the end of the open epoch, closing it, and returns the tick's events.
    async fn next_epoch(&self) -> Vec<Event> {
        let open = self.staking_state().await.expect("initialised").epoch;
        self.advance_to(self.genesis.staking.expect("the genesis stakes").epoch_end(open)).await;
        self.submit(Command::Tick).await
    }

    /// Each account buys its quantity of BTC at `from` and sells it at `to` through the API.
    async fn losses(&self, trades: &[(AccountId, &str)], from: &str, to: &str) {
        self.round_trips(trades, from, to, Side::Buy, Side::Sell).await;
    }

    /// Each account sells its quantity of BTC at `from` and buys it back at `to`, winning the
    /// difference.
    async fn wins(&self, trades: &[(AccountId, &str)], from: &str, to: &str) {
        self.round_trips(trades, from, to, Side::Sell, Side::Buy).await;
    }

    async fn round_trips(&self, trades: &[(AccountId, &str)], from: &str, to: &str, open: Side, close: Side) {
        for (mid, side) in [(from, open), (to, close)] {
            let ts = self.clock.now_ms();
            let quotes = ["a", "b"].map(|source| SourceQuote {
                source: source.into(),
                mid: mid.parse().unwrap(),
                ts,
                contract: None,
            });
            self.submit(Command::Prices { market: 1, quotes: quotes.to_vec() }).await;
            for (account, qty) in trades {
                let order = OrderRequest {
                    account: *account,
                    client_id: 1,
                    market: 1,
                    side,
                    qty: qty.parse().unwrap(),
                    kind: OrderKind::Market { quoted: mid.parse().unwrap(), max_slippage_bps: None },
                    reduce_only: false,
                    auth: None,
                };
                let (status, events) = self.post("/v1/orders", serde_json::to_value(order).unwrap()).await;
                assert!(status == StatusCode::OK && events.to_string().contains("\"fill\""), "{events}");
            }
        }
    }

    /// Ticks until a checkpoint follows the last command, then waits for the vault to post it.
    async fn checkpoint_posted(&self) {
        while (self.query(|e| e.last_seq().unwrap()).await + 1) % CHECKPOINT_EVERY != 0 {
            self.submit(Command::Tick).await;
        }
        let last = self.query(|e| e.last_seq().unwrap()).await;
        timeout(std::time::Duration::from_secs(20), async {
            while self.view(self.vault, latestSeqCall {}).await != last {
                sleep(std::time::Duration::from_millis(40)).await;
            }
        })
        .await
        .expect("the checkpoint worker posts the checkpoint");
    }

    /// `command` signed by the dev operator key with the operator's next nonce, as `engine admin`
    /// posts it.
    fn signed(&self, command: AdminCommand) -> Value {
        let key = k256::ecdsa::SigningKey::from_slice(&[0x42; 32]).unwrap();
        serde_json::to_value(crate::admin::sign(&key, &self.operator, command, 0, NOW + 60_000)).unwrap()
    }

    /// Sequences `cmd` and returns its events.
    async fn submit(&self, cmd: Command) -> Vec<Event> {
        let (reply, rx) = oneshot::channel();
        self.node().state.tx.send(Request::Submit { cmd, reply: Some(reply) }).await.unwrap();
        timeout(std::time::Duration::from_secs(10), rx).await.unwrap().expect("trusted time admits the command")
    }

    /// Reads the engine between batches.
    async fn query<T: Send + 'static>(&self, read: impl FnOnce(&Engine) -> T + Send + 'static) -> T {
        let (reply, rx) = oneshot::channel();
        let query = Box::new(move |e: &Engine| {
            let _ = reply.send(read(e));
        });
        self.node().state.tx.send(Request::Query(query)).await.unwrap();
        rx.await.unwrap()
    }

    /// The status and JSON body of a POST of `body` to `path`.
    async fn post(&self, path: &str, body: Value) -> (StatusCode, Value) {
        let request =
            HttpRequest::post(path).header("content-type", "application/json").body(Body::from(body.to_string()));
        self.send(request.unwrap()).await
    }

    /// The status and JSON body of a GET of `path`.
    async fn get(&self, path: &str) -> (StatusCode, Value) {
        self.send(HttpRequest::get(path).body(Body::empty()).unwrap()).await
    }

    /// Serves one request and reads its JSON response.
    async fn send(&self, request: HttpRequest<Body>) -> (StatusCode, Value) {
        let response = self.node().app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body = serde_json::from_slice(&bytes)
            .unwrap_or_else(|error| panic!("{status}: body {:?}: {error}", String::from_utf8_lossy(&bytes)));
        (status, body)
    }
}
