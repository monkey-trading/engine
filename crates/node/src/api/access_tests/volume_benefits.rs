//! The `V68` dev scenario: a wallet at tier 2 pays a lower profit fee, a lower financing markup and
//! earns more emissions weight than a wallet at tier 0, through the signed order and admin routes and
//! the account read on deterministic time. The node's tier table is dev's with the activation's benefits
//! and thresholds a few fills reach: A sells $490k of ETH-USD and holds it 60 s to reach tier 2, and B
//! stays at tier 0. Each holds a winning 0.1 BTC-USD position 45 minutes, so A pays 9% of its gain and B
//! 10%; each holds one over a rollover, so A's markup is a tenth lower; each closes one after two
//! minutes, so both pay the curve's rate; and each loses the same on BTC-USD in one MONKEY period, so
//! A's weight is 1.25 times its loss and B's its loss. The public log replays to the same state, and a
//! restart recovers it.

use engine_core::command::{AdminCommand, Command};
use engine_core::emissions::{EmissionSchedule, TokenUnits};
use engine_core::fixed::Rounding;
use engine_core::holding_fee::{DEFAULT_PROFIT_FEE_CURVE, curve_fee_bps};
use engine_core::types::MarketId;
use engine_core::volume_tier::{
    ACTIVATION_EMISSIONS_BOOSTS_BPS, ACTIVATION_FEE_DISCOUNTS_BPS, ACTIVATION_FINANCING_DISCOUNTS_BPS, MIN_HOLD_MS,
};

use super::earned_leverage::{wait, wait_until};
use super::spreads::{Operator, fill_event, fund, price};
use super::*;
use crate::clock::Clock;

const BTC: MarketId = 1;
const ETH: MarketId = 2;
const MINUTE_MS: u64 = 60_000;

#[tokio::test(flavor = "multi_thread")]
async fn dev_scenario_a_higher_tier_pays_less_fee_and_financing_and_earns_more_emissions() {
    let mut genesis = dev_config().genesis;
    let tiers = [(0, 0), (200_000, 1), (400_000, 2), (10_000_000, 3)];
    for (tier, (threshold, at)) in genesis.params.volume_tiers.iter_mut().zip(tiers) {
        (tier.threshold, tier.day_divisor) = (Fixed::from_int(threshold), 1);
        tier.fee_discount_bps = ACTIVATION_FEE_DISCOUNTS_BPS[at];
        tier.financing_discount_bps = ACTIVATION_FINANCING_DISCOUNTS_BPS[at];
        tier.emissions_boost_bps = ACTIVATION_EMISSIONS_BOOSTS_BPS[at];
    }
    let mut node =
        Node::start_from("volume-benefits-dev-scenario", true, true, false, &genesis, ProtocolVersion::LATEST).await;
    let operator = Operator::serving(&node).await;
    let (a, b) = (key(0x87), key(0x88));
    let (wallet_a, wallet_b) = (eip712::address(a.verifying_key()), eip712::address(b.verifying_key()));
    for client in [&a, &b] {
        node.onboard(client).await;
        fund(&node, client, 1_000_000).await;
    }
    let start_ms = node.clock.now_ms();
    let schedule = EmissionSchedule { start_ms, period_budget: TokenUnits(9_000_000) };
    operator.applies(&node, AdminCommand::InitEmissions(schedule)).await;
    price(&node, BTC, "100000").await;
    price(&node, ETH, "2500").await;

    // 1. A sells $490k of ETH-USD and holds it 60 s, which counts and reaches tier 2; B is at tier 0.
    // The account read gives each wallet's tier and its fee discount, financing discount and boost.
    fill_event(&node, &a, ETH, Side::Sell, "196", "2500").await;
    wait(&node, MIN_HOLD_MS as u64).await;
    price(&node, BTC, "100000").await;
    for (wallet, tier, benefits) in [(wallet_a, 2, (1_000, 1_000, 2_500)), (wallet_b, 0, (0, 0, 0))] {
        let read = node.get(&format!("/v1/accounts/{wallet}")).await["volume"].clone();
        assert_eq!(read["tier"], tier, "{read}");
        let given = ["fee_discount_bps", "financing_discount_bps", "emissions_boost_bps"].map(|key| read[key].as_i64());
        assert_eq!(given, [benefits.0, benefits.1, benefits.2].map(Some), "{read}");
    }

    // 2. Each buys 0.1 BTC and holds it 45 minutes, past the curve's 15, and sells it at a gain: A pays
    // the base less its discount, 9% of its gain, and B the 10% base.
    let mut fees = Vec::new();
    for client in [&a, &b] {
        price(&node, BTC, "100000").await;
        fill_event(&node, client, BTC, Side::Buy, "0.1", "100000").await;
        wait(&node, 45 * MINUTE_MS).await;
        price(&node, BTC, "101000").await;
        fees.push(pays(&fill_event(&node, client, BTC, Side::Sell, "0.1", "101000").await, 45 * MINUTE_MS as i64));
    }
    assert_eq!(fees, [900, 1_000]);

    // 3. Each buys 0.1 BTC and holds it over the rollover: the first command at it charges A the
    // benchmark plus a markup a tenth lower, 4.5% against 5%, so 8.5% a year against 9%.
    price(&node, BTC, "100000").await;
    for client in [&a, &b] {
        fill_event(&node, client, BTC, Side::Buy, "0.1", "100000").await;
    }
    let rollover = node.state.query(|engine| engine.next_rollover().expect("scheduled")).await.unwrap();
    // From `V80` the rollover logs one aggregate, and each position's charge is its derived row.
    let events = tick_at(&node, rollover).await;
    assert!(events.iter().any(|event| matches!(event, Event::FinancingApplied { .. })), "{events:?}");
    let seq = node.state.query(|engine| engine.last_seq().expect("the rollover's command")).await.unwrap();
    let rows = node.state.history.derived().borrow().get(seq).map(|derived| derived.rows.clone()).expect("kept");
    let financing =
        |wallet: AccountId| rows.iter().find(|row| row.account == wallet && row.market == BTC).map(|row| row.amount);
    assert_eq!(financing(wallet_a), Some("2.328767124".parse().unwrap()), "{events:?}");
    assert_eq!(financing(wallet_b), Some("2.465753425".parse().unwrap()), "{events:?}");
    price(&node, BTC, "100000").await;
    for client in [&a, &b] {
        fill_event(&node, client, BTC, Side::Sell, "0.1", "100000").await;
    }

    // 4. Each buys 0.1 BTC and sells it two minutes later at a gain: the curve is above both bases, and
    // both pay its rate.
    let curve = curve_fee_bps(&DEFAULT_PROFIT_FEE_CURVE, 2 * MINUTE_MS as i64);
    for client in [&a, &b] {
        price(&node, BTC, "100000").await;
        fill_event(&node, client, BTC, Side::Buy, "0.1", "100000").await;
        wait(&node, 2 * MINUTE_MS).await;
        price(&node, BTC, "101000").await;
        assert_eq!(
            pays(&fill_event(&node, client, BTC, Side::Sell, "0.1", "101000").await, 2 * MINUTE_MS as i64),
            curve
        );
    }
    assert_eq!(curve, 5_081);

    // 5. In the next MONKEY period each buys 0.1 BTC and sells it $1,000 lower. At the period's close the
    // first command at it weighs A's loss 1.25 times and B's once, and shares the 9,000,000 units 5:4.
    let current = node.state.query(|engine| engine.emissions().expect("initialised").period).await.unwrap();
    let period_end = schedule.period_end(current);
    wait_until(&node, period_end).await;
    price(&node, BTC, "100000").await;
    let mut losses = Vec::new();
    for client in [&a, &b] {
        fill_event(&node, client, BTC, Side::Buy, "0.1", "100000").await;
        price(&node, BTC, "99000").await;
        let sold = fill_event(&node, client, BTC, Side::Sell, "0.1", "99000").await;
        let realised: Fixed = sold["realised_pnl"].as_str().unwrap().parse().unwrap();
        assert!(realised.is_negative(), "{sold}");
        losses.push(-realised);
        price(&node, BTC, "100000").await;
    }
    let events = tick_at(&node, schedule.period_end(current + 1)).await;
    let weight = |wallet: AccountId| {
        events.iter().find_map(|event| match event {
            Event::EmissionAllocated { account, weight, amount, .. } if *account == wallet => Some((*weight, amount.0)),
            _ => None,
        })
    };
    assert_eq!(weight(wallet_a), Some((losses[0].mul_bps(12_500), 5_000_000)), "{events:?}");
    assert_eq!(weight(wallet_b), Some((losses[1], 4_000_000)), "{events:?}");
    assert_eq!(losses[0], losses[1]);

    // 6. The public log replays against the enclave's key to the same state, and a restart recovers the
    // root it replays to.
    let domain = crate::log::CheckpointDomain { chain_id: 31_337, vault: VAULT };
    let signer = AccountId(EnclaveKey::test_key().address());
    let replayed = replay_with_signer(&node.entries(), domain, signer).unwrap().engine;
    let live = node
        .state
        .query(move |engine| {
            (
                [wallet_a, wallet_b].map(|account| engine.accounts()[&account].clone()),
                [wallet_a, wallet_b].map(|account| engine.entitlement(account)),
            )
        })
        .await
        .unwrap();
    let replayed_accounts = [wallet_a, wallet_b].map(|account| replayed.accounts()[&account].clone());
    assert_eq!(live, (replayed_accounts, [wallet_a, wallet_b].map(|account| replayed.entitlement(account))));
    let policy = node.state.query(|engine| engine.access_policy().cloned()).await.unwrap();
    let (operator, clock, dir) = (node.state.operator, node.clock.clone(), node.dir.clone());
    let sequencer = node.tasks.remove(1);
    sequencer.abort();
    let _ = sequencer.await;
    drop(node);
    let cfg = crate::sequencer::SequencerConfig {
        access_policy: policy,
        chain_id: 31_337,
        vault: VAULT,
        operator,
        clock,
        genesis: crate::sequencer::genesis_commands(&genesis),
        ..sequencer_config(&dir, watch::channel(Manifest::default()).0)
    };
    let enclave = Arc::new(EnclaveKey::test_key());
    let mut recovered =
        Sequencer::recover(cfg, enclave, broadcast::channel(16).0, mpsc::unbounded_channel().0).unwrap();
    assert_eq!(recovered.state_root(), { replayed }.state_root());
}

/// Checks a close's fill: the hold it names, and a fee at the rate it names on its realised gain after the
/// spread, rounded up; the rate.
fn pays(fill: &Value, hold_ms: i64) -> i64 {
    let amount = |key: &str| -> Fixed { fill[key].as_str().unwrap().parse().unwrap() };
    let fee_bps = fill["profit_fee_bps"].as_i64().unwrap_or_else(|| panic!("{fill}"));
    assert_eq!(fill["hold_ms"].as_i64(), Some(hold_ms), "{fill}");
    let realised = amount("realised_pnl");
    assert!(realised.is_positive(), "{fill}");
    assert_eq!(amount("profit_fee"), realised.mul_bps_rounded(fee_bps, Rounding::Up), "{fill}");
    fee_bps
}

/// Moves the node's time forward to `ts` and sequences a tick there, as `wait_until` does; its events.
async fn tick_at(node: &Node, ts: i64) -> Vec<Event> {
    let ms = u64::try_from(ts - node.clock.now_ms()).expect("forward");
    node.clock.advance(Duration::from_millis(ms));
    node.retrust().await;
    node.source.lock().unwrap().observed_at = node.clock.now_ms();
    node.state.submit(Command::Tick).await.unwrap()
}
