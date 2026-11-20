//! From `V86` a close-out step scores a group's later members at the NAV `Engine::house_nav` reads once
//! the earlier fills are booked, which values each closed position at its mark, so a member's projected
//! stress level is the one its fill pays even where a mark stands apart from the index the account is
//! valued and filled at (#1084). Before `V86` the step took the NAV from the index.

use super::*;

const V85: ProtocolVersion = ProtocolVersion::V85;
const V86: ProtocolVersion = ProtocolVersion::V86;

#[test]
fn from_v86_a_mark_under_its_index_scores_a_later_member_at_the_level_its_fill_pays() {
    // The house's NAV stands $50 over ETH's notional, its cap, so ETH is at level 0, and BTC's mark $1,000
    // under its index: BTC's fill takes the account's long out of the clients' PnL at the mark and leaves
    // the NAV about $10,000 lower, so ETH's fill pays level 1, where closing the pair does not restore the
    // level. From `V86` the step scores ETH there: neither candidate restores the level, so AAPL, gaining the
    // most per dollar, closes first, then the pair. Before it, the step took the NAV BTC's fill leaves from
    // the index, scored ETH at level 0 and the pair as restoring the level, and closed it first, ETH at
    // level 1, leaving the account below its level for AAPL to close too.
    for (version, closed) in [(V85, vec![BTC, ETH, AAPL]), (V86, vec![AAPL, BTC, ETH])] {
        let (mut h, a) = pair_and_shares_at(version);
        let (equity, _) = h.engine.equity_and_closeout_requirement(&h.engine.accounts()[&a.account]);
        eth_capped_at_the_house_nav(&mut h, &a, f("50"), |state| {
            state["markets"][BTC.to_string()]["mark"] = serde_json::json!("99000");
        });
        let eth = f("4000") + (equity - f("20570")).div_int(250);
        let events = h.price(ETH, &eth.to_string());
        assert_eq!(liquidations(&events), closed, "{version:?}: {events:?}");
        let raised = events.iter().position(|e| matches!(e, Event::StressChanged { market: ETH, level: 1 }));
        let filled = events.iter().position(|e| matches!(e, Event::Fill(fill) if fill.market == ETH));
        assert!(raised.zip(filled).is_some_and(|(raised, filled)| raised < filled), "{version:?}: {events:?}");
        assert!(h.engine.accounts()[&a.account].positions.is_empty(), "{version:?}");
    }
}

#[test]
fn from_v86_a_member_that_would_be_refused_at_the_nav_its_fill_reads_waits_with_its_group() {
    // `a_failed_fill_ends_the_close_out_mid_group`'s book: ETH's level 1 spread reaches the price with no
    // ceiling to clamp it. From `V86` the step scores ETH at level 1, where its close does not price, so
    // the pair is no candidate: AAPL closes alone, nothing else can, and the pair stays whole.
    let (mut h, a) = pair_and_shares_at(V86);
    let (equity, _) = h.engine.equity_and_closeout_requirement(&h.engine.accounts()[&a.account]);
    eth_capped_at_the_house_nav(&mut h, &a, f("50"), |state| unpriced_level_one(state, f("99000")));
    let eth = f("4000") + (equity - f("20570")).div_int(250);
    let events = h.price(ETH, &eth.to_string());
    assert_eq!(liquidations(&events), vec![AAPL], "{events:?}");
    let held = &h.engine.accounts()[&a.account];
    assert_eq!((held.position(BTC).qty, held.position(ETH).qty), (f("10"), f("-250")));
}

#[test]
fn from_v86_a_group_whose_members_can_trade_closes_whole_or_waits_whatever_its_marks() {
    // ETH's level 1 does not price, as above, and BTC's mark stands anywhere within $2,000 of its index with
    // the NAV up to $200 over ETH's notional: whichever level BTC's fill leaves ETH at, the step scores it
    // there, so the pair either closes whole or waits, and no fill of it is refused.
    let config = Config { cases: 16, rng_seed: RngSeed::Fixed(0x1084), failure_persistence: None, ..Config::default() };
    TestRunner::new(config)
        .run(&(-2_000i64..2_000, 1i64..200), |(gap, margin)| {
            let (mut h, a) = pair_and_shares_at(V86);
            let (equity, _) = h.engine.equity_and_closeout_requirement(&h.engine.accounts()[&a.account]);
            let mark = f("100000") + Fixed::from_int(gap);
            eth_capped_at_the_house_nav(&mut h, &a, Fixed::from_int(margin), |state| unpriced_level_one(state, mark));
            let eth = f("4000") + (equity - f("20570")).div_int(250);
            let events = h.price(ETH, &eth.to_string());
            let held = &h.engine.accounts()[&a.account];
            let (btc, eth) = (held.position(BTC).qty, held.position(ETH).qty);
            prop_assert_eq!(btc.is_zero(), eth.is_zero(), "gap {} margin {}: {:?}", gap, margin, events);
            Ok(())
        })
        .unwrap();
}

// ── Helpers ──

/// Takes ETH's close-spread ceiling away and raises stress level 1's spread to the whole price, so ETH's close
/// does not price at level 1, and marks BTC at `btc_mark`.
fn unpriced_level_one(state: &mut serde_json::Value, btc_mark: Fixed) {
    state["close_spread_ceilings"].as_object_mut().expect("installed").remove(&ETH.to_string());
    state["params"]["stress_levels"][1]["min_spread_bps"] = serde_json::json!(10_000);
    state["markets"][BTC.to_string()]["mark"] = serde_json::to_value(btc_mark).expect("a mark");
}
