//! Two mechanisms `research/rogue-operator` rests on, end to end through `Engine::apply` on the
//! `V21` harness, the latest without access screening: a round trip at a zero spread tier costs
//! nothing and pulls a toxicity score back under its first tier, and a pair of opposite positions
//! in two accounts, across a move the market has no price for, pays its owner the move less the
//! profit fee and the losing leg's balance, the house taking the rest as bad debt. From `V54` the
//! same round trips leave the score where they found it.

mod common;

use common::events::{fills, rejection};
use common::toxicity::{FLOOR_BPS, OFF_GRID_MS, another, mark, relisted_v54, standing};
use common::{BTC, Harness, acct, f, market, params};
use engine_core::Fixed;
use engine_core::command::AdminCommand;
use engine_core::event::{Event, Fill, Reject};
use engine_core::market::{MarketSpec, Tier};
use engine_core::params::{NotionalWeighting, ProtocolParams, StressLevel, ToxicityParams, ToxicityTier};
use engine_core::session::Schedule;
use engine_core::spread_tier::SpreadTier;
use engine_core::types::{AccountId, AssetClass, Side};

/// Genesis toxicity: a 2 s horizon, the calibrated prior and memory, and tiers at 1 and 2 bps
/// adding 2 and 3 bps; one stress level with no trigger, so no move widens a spread.
fn genesis_toxicity(h: &mut Harness) {
    let toxicity = ToxicityParams {
        horizon_ms: 2_000,
        notional: Some(NotionalWeighting { prior: f("5629518"), memory: f("11259036") }),
        tiers: vec![
            ToxicityTier { min_score_bps: 1, extra_spread_bps: 2 },
            ToxicityTier { min_score_bps: 2, extra_spread_bps: 3 },
        ],
        ..params().toxicity
    };
    let stress_levels = vec![StressLevel { size_spread_multiplier_bps: 10_000, min_spread_bps: 0, trigger: None }];
    h.operator_admin(AdminCommand::SetParams(ProtocolParams { toxicity, stress_levels, ..params() }));
}

/// Lists BTC-USD as `genesis_btc_spec` does.
fn genesis_btc(h: &mut Harness) {
    h.operator_admin(AdminCommand::UpsertMarket(genesis_btc_spec()));
}

/// BTC-USD as genesis lists it: 1% margin to $2M and no spread to $2M of an account's position.
fn genesis_btc_spec() -> MarketSpec {
    let mut spec = market(BTC, "BTC-USD", AssetClass::Crypto, Schedule::AlwaysOpen, 100);
    spec.margin_tiers = vec![
        Tier { up_to: Some(f("2000000")), bps: 100 },
        Tier { up_to: Some(f("10000000")), bps: 200 },
        Tier { up_to: None, bps: 500 },
    ];
    spec.spread_tiers =
        vec![SpreadTier::bps(Some(f("2000000")), 0), SpreadTier::bps(Some(f("10000000")), 3), SpreadTier::bps(None, 8)];
    spec
}

fn fill(h: &mut Harness, account: AccountId, side: Side, qty: &str, mid: &str) -> Fill {
    match &fills(&h.market_order(account, BTC, side, qty, mid))[..] {
        [fill] => (*fill).clone(),
        other => panic!("expected one fill, got {other:?}"),
    }
}

fn score(h: &Harness, account: AccountId) -> Fixed {
    h.engine.accounts()[&account].toxicity_score()
}

/// One cycle of a trader who knows the next move: buy 5 BTC (about $500k), see the index rise
/// 10 bps a horizon later, sell there, and hold the price a horizon so the sell scores. With
/// `wash`, it then buys and sells 19 BTC (under $2M) three times at a flat price, scored a horizon
/// later. Returns the spread the edge's buy paid.
fn cycle(h: &mut Harness, account: AccountId, mid: &mut Fixed, wash: bool) -> i64 {
    h.price(BTC, &mid.to_string());
    let edge = fill(h, account, Side::Buy, "5", &mid.to_string());
    h.advance(2_000);
    *mid += mid.mul_bps(10);
    h.price(BTC, &mid.to_string());
    fill(h, account, Side::Sell, "5", &mid.to_string());
    h.advance(2_000);
    h.price(BTC, &mid.to_string());
    if wash {
        let before = h.balance(account);
        for _ in 0..3 {
            for side in [Side::Buy, Side::Sell] {
                let w = fill(h, account, side, "19", &mid.to_string());
                assert_eq!((w.spread_bps, w.profit_fee), (0, Fixed::ZERO), "a wash fill paid {w:?}");
            }
        }
        assert_eq!(h.balance(account), before, "a wash round trip cost the account");
        h.advance(2_000);
        h.price(BTC, &mid.to_string());
    }
    edge.spread_bps
}

#[test]
fn free_round_trips_keep_a_toxic_account_under_the_first_tier() {
    let mut paid = Vec::new();
    let mut scores = Vec::new();
    for wash in [false, true] {
        let mut h = Harness::v21();
        genesis_toxicity(&mut h);
        genesis_btc(&mut h);
        let a = acct(1);
        h.deposit(a, "100000");
        let mut mid = f("100000");
        paid.push((0..40).map(|_| cycle(&mut h, a, &mut mid, wash)).collect::<Vec<_>>());
        scores.push(score(&h, a));
    }
    // Without washing, the edge's markouts lift the score past the first tier, and its buys pay
    // the tier's add-on.
    assert!(scores[0] >= f("1"), "unwashed score {}", scores[0]);
    assert!(paid[0].last().is_some_and(|&bps| bps >= 2), "{:?}", paid[0]);
    // Washing about $11M a cycle at no cost keeps the same edge under the first tier: no buy pays a
    // spread.
    assert!(scores[1] < f("1"), "washed score {}", scores[1]);
    assert!(paid[1].iter().all(|&bps| bps == 0), "{:?}", paid[1]);
}

#[test]
fn a_pair_across_an_unpriced_jump_pays_the_move_less_the_fee_and_the_losers_balance() {
    let mut h = Harness::v21();
    genesis_toxicity(&mut h);
    genesis_btc(&mut h);
    let (long, short) = (acct(1), acct(2));
    // Each leg posts its 1% margin on $1M, and a dollar.
    for a in [long, short] {
        h.deposit(a, "10001");
    }
    h.price(BTC, "100000");
    fill(&mut h, long, Side::Buy, "10", "100000");
    fill(&mut h, short, Side::Sell, "10", "100000");
    let house_before = h.engine.house_cash();
    // The host holds a venue back: past the quotes' 1 s age the market has no price, and nothing
    // fills or closes out.
    h.advance(30_000);
    let refused = h.market_order(long, BTC, Side::Sell, "10", "100000");
    assert!(matches!(rejection(&refused), Reject::NoPrice { .. }), "{refused:?}");
    // It lets the venue through once the index has moved 3%: the short closes out past its balance.
    let events = h.price(BTC, "103000");
    let bad_debt: Fixed = events
        .iter()
        .filter_map(|e| {
            if let Event::BadDebt { account, amount } = e { (*account == short).then_some(*amount) } else { None }
        })
        .sum();
    assert_eq!(bad_debt, f("19999"), "{events:?}");
    assert_eq!(h.balance(short), Fixed::ZERO);
    let won = fill(&mut h, long, Side::Sell, "10", "103000");
    assert_eq!((won.realised_pnl, won.profit_fee), (f("30000"), f("3000")));
    // The owner of both accounts is up 0.9 x 3% - 1% of $1M, less the short's spare dollar, and
    // the house is down the same.
    let owner = h.balance(long) + h.balance(short) - f("20002");
    assert_eq!(owner, f("16999"));
    assert_eq!(h.engine.house_cash(), house_before - owner);
    h.assert_conserved(Fixed::ZERO);
}

#[test]
fn from_v54_round_trips_leave_a_toxic_account_at_its_tier() {
    let mut standings = Vec::new();
    for wash in [false, true] {
        // Genesis's BTC-USD, its first band raised to the crypto floor of 1 bp from `V52`.
        let spec = MarketSpec { markout_floor_bps: Some(f(FLOOR_BPS)), ..genesis_btc_spec() };
        let (mut h, _) = relisted_v54([spec]);
        // Funds for the round trips' spreads too.
        let client = another(&mut h, 3, "10000000");
        h.advance(OFF_GRID_MS);
        h.price(BTC, "100000");
        let mut mid = f("100000");
        for _ in 0..25 {
            // The edge: a buy the index rises 10 bps from a horizon later. With `wash`, three round
            // trips of 19 BTC at the buy's index first.
            common::toxicity::fill(&mut h, &client, Side::Buy, "0.5", &mid.to_string());
            if wash {
                for _ in 0..3 {
                    for side in [Side::Buy, Side::Sell] {
                        common::toxicity::fill(&mut h, &client, side, "19", &mid.to_string());
                    }
                }
            }
            mid += mid.mul_bps(10);
            mark(&mut h, &mid.to_string());
        }
        standings.push(standing(&h, client.account));
    }
    // The washing changes nothing: both accounts reach the top tier with the same score.
    assert_eq!(standings[0].tier, 2, "{:?}", standings[0]);
    assert_eq!(standings[0], standings[1]);
}
