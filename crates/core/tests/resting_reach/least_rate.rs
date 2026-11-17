//! What the keys rest on: from `V74` no fill pays less of its index than its market's least fill rate,
//! the lower of its least spread band and its close-spread ceiling. Opens, adds through the bands,
//! partial and pure closes and reversals at stress levels 0 and 1, with and without a toxicity tier, at
//! volume tiers 0 and 3, under bands below the ceiling and under bands a relisting raised above it.

use super::*;
use engine_core::volume_tier::{AccountVolume, DayVolume, day_of};

/// TIGHT's launch bands, in tenths of a bp: 5bp to $1m, 10bp to $5m and 20bp above.
const LAUNCH: [(Option<i64>, i64); 3] = [(Some(1_000_000), 50), (Some(5_000_000), 100), (None, 200)];
/// The trades each case makes at a mid of 100,000: an open, an add through the second band, a partial
/// and a pure close, a short, a reversal and its close.
const TRADES: [(Side, &str); 7] = [
    (Side::Buy, "1"),
    (Side::Buy, "40"),
    (Side::Sell, "5"),
    (Side::Sell, "36"),
    (Side::Sell, "2"),
    (Side::Buy, "4"),
    (Side::Sell, "2"),
];

#[test]
fn no_fill_pays_less_than_the_least_fill_rate_its_limits_are_keyed_at() {
    // Under the bands the least band, 5bp, is the rate, and a first-band fill at level 0 with no
    // toxicity pays exactly it; over them the 8bp ceiling is, and a pure close pays exactly it.
    for (raised, ceiling, rate) in [(false, 25, 50), (true, 8, 80)] {
        let mut exact = 0;
        for level in [0, 1] {
            for toxic in [false, true] {
                for tier in [0, 3] {
                    let (mut h, a) = staged(raised, ceiling, level, toxic, tier);
                    for (side, qty) in TRADES {
                        for fill in fills(&trade(&mut h, &a, TIGHT, side, qty, "100000")) {
                            let paid = (fill.price - fill.index).mul_int(fill.side.sign()).raw();
                            let least = fill.index.raw() * rate / 100_000;
                            let case = format!("raised {raised}, level {level}, toxic {toxic}, tier {tier}: {fill:?}");
                            assert!(paid >= least, "{case} pays {paid} under {least}");
                            exact += usize::from(paid == least);
                        }
                    }
                }
            }
        }
        assert!(exact > 0, "no fill pays exactly the rate with raised bands {raised}");
    }
}

/// TIGHT listed at `LAUNCH` with a `ceiling` in bps, under parameters whose levels charge 1x and 2bp
/// over the bands at level 1, and whose one score tier adds 1bp, so the launch close charge is 8bp;
/// with `raised`, relisted at twice `LAUNCH`. The client holds $10m, at stress level `level`, with a
/// score in the tier when `toxic`, and at volume tier `tier`.
fn staged(raised: bool, ceiling: i64, level: usize, toxic: bool, tier: usize) -> (Harness, Client) {
    let mut params = ProtocolParams { volume_tiers: launch_volume_tiers(), ..v54_params() };
    params.stress_levels = vec![
        StressLevel { size_spread_multiplier_bps: 10_000, min_spread_bps: 0, trigger: None },
        StressLevel { size_spread_multiplier_bps: 10_000, min_spread_bps: 2, trigger: None },
    ];
    params.toxicity.score_tiers = vec![ScoreTier { min_score: f("4"), extra_spread_bps: 1 }];
    let mut h = listed(V74, params, &[(TIGHT, "TIGHT", tenths(&LAUNCH), ceiling)]);
    let a = client(&mut h, 1, "10000000");
    if raised {
        relist(&mut h, tenths(&LAUNCH.map(|(up_to, rate)| (up_to, 2 * rate))));
    }
    h.operator_admin(AdminCommand::SetStressFloor { market: Some(TIGHT), level });
    let mut state = serde_json::to_value(&h.engine).expect("encodes");
    let key = a.account.to_string();
    if toxic {
        // A score of 5.66 standard errors over 25 fills, decayed to this hour.
        let score = serde_json::json!({
            "edge": "40000", "weight_squares": "50000000", "fills": "25", "notional": "25000",
            "step": h.ts / 3_600_000
        });
        state["toxicity_scores"][&key] = score;
    }
    if tier == 3 {
        // $10m counted on each of the window's 30 days, past tier 3's $250m.
        let today = day_of(h.ts);
        let days = (today - 29..=today).map(|day| DayVolume { day, volume: f("10000000") }).collect();
        let book = AccountVolume { days, opening: Default::default(), closing: vec![], early: vec![] };
        state["volumes"][&key] = serde_json::to_value(book).expect("encodes");
    }
    h.engine = serde_json::from_value(state).expect("decodes");
    h.price(TIGHT, "100000");
    assert_eq!(h.engine.markets()[&TIGHT].stress.level, level);
    assert_eq!(h.engine.toxicity_standing(a.account).expect("from V54").add_on_bps, i64::from(toxic));
    assert_eq!(h.engine.volume_tier(a.account), tier);
    (h, a)
}
