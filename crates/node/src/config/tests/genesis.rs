//! Genesis config tests: entries the engine would refuse, duplicate listings, and the
//! close-spread ceilings each listed market needs.

use super::*;
use crate::test_support::DEV_CONFIG as DEV;

#[test]
fn genesis_values_the_engine_would_refuse_fail_the_boot() {
    let dev = std::fs::read_to_string(DEV).unwrap();
    let capital = |amount: &str| dev.replace("house_capital = \"10000000\"", &format!("house_capital = \"{amount}\""));
    let rate = |bps: i64| {
        dev.replace("{ name = \"SOFR\", annual_bps = 400 }", &format!("{{ name = \"SOFR\", annual_bps = {bps} }}"))
    };
    let cap = |orders: usize| dev.replace("max_resting_orders = 100", &format!("max_resting_orders = {orders}"));
    // A market names "Sofr" while genesis rates "SOFR".
    let misspelt = dev.replacen("benchmark = \"SOFR\"", "benchmark = \"Sofr\"", 1);
    // Genesis rates no benchmark at all, leaving every rate to the rate feed.
    let unrated = {
        let start = dev.find("benchmarks = [").unwrap();
        let end = start + dev[start..].find("]\n").unwrap() + 1;
        format!("{}benchmarks = []{}", &dev[..start], &dev[end..])
    };
    let total_floor = dev.replace("house_total_cap_floor = \"2000000\"", "house_total_cap_floor = \"-1\"");
    let market_floor = dev.replace("house_cap_floor = \"300000\"", "house_cap_floor = \"-1\"");
    // Each genesis command: the parameters, a benchmark rate, a market, and the house capital.
    let refused = [
        (dev.replace("profit_fee_bps = 1000", "profit_fee_bps = 10000"), "genesis.params: the engine rejects it"),
        (total_floor, "genesis.params: the engine rejects it: house_total_cap_floor out of range"),
        (rate(40_000), "genesis benchmark SOFR: the engine rejects it: benchmark rate 40000bps out of range"),
        (rate(i64::MIN), "genesis benchmark SOFR: the engine rejects it"),
        (market_floor, "genesis market BTC-USD: the engine rejects it: house_cap_floor out of range"),
        (dev.replace("house_cap_bps = 1000\n", "house_cap_bps = 0\n"), "genesis market AAPL: the engine rejects it"),
        (capital("-1"), "genesis.house_capital: the engine rejects it"),
        (capital("1000000000.000000001"), "genesis.house_capital: the engine rejects it"),
        // The latest version takes a divergence trigger only as a share of each market's bound, and a
        // share above zero can never fire at a zero bound.
        (
            dev.replace("divergence_share_bps = 6000", "divergence_bps = 15"),
            "genesis.params: the engine rejects it: from protocol V13 a divergence trigger is divergence_share_bps",
        ),
        (
            dev.replacen("max_divergence_bps = 25", "max_divergence_bps = 0", 1),
            "genesis market BTC-USD: the engine rejects it: a divergence share above zero can never fire",
        ),
        (cap(0), "genesis.params: the engine rejects it: max_resting_orders must be in 1..=1000"),
        (cap(1_001), "genesis.params: the engine rejects it: max_resting_orders must be in 1..=1000"),
        (misspelt, "the engine rejects it: benchmark Sofr has no rate"),
    ];
    for (text, expected) in &refused {
        assert_ne!(text, &dev, "the replacement must change the config");
        let err = toml::from_str::<NodeConfig>(text).unwrap().validate().unwrap_err();
        assert!(format!("{err:#}").contains(expected), "expected {expected:?}, got {err:#}");
    }
    // A house that starts with no capital, the rate and resting-order limits themselves, and a
    // genesis that rates no benchmark are accepted.
    for text in [capital("0"), rate(-10_000), cap(1_000), unrated] {
        assert_ne!(text, dev, "the replacement must change the config");
        toml::from_str::<NodeConfig>(&text).unwrap().validate().unwrap();
    }
}

#[test]
fn a_genesis_market_converting_at_anything_but_its_pair_listed_before_it_fails_the_boot() {
    let dev = std::fs::read_to_string(DEV).unwrap();
    let via = "quote_currency = { currency = \"eur\", fx_market = 10 }";
    let at = |fx_market: u32| format!("quote_currency = {{ currency = \"eur\", fx_market = {fx_market} }}");
    // US500 (20) is not EUR-USD, 99 is never listed, and XAU-USD (30), renamed EUR-USD here while
    // the EUR-USD listing takes another symbol, is listed after Germany 40.
    let not_the_pair = dev.replace(via, &at(20));
    let unlisted = dev.replace(via, &at(99));
    let listed_after = dev
        .replace(via, &at(30))
        .replace("symbol = \"EUR-USD\"", "symbol = \"EUR-USD-SPOT\"")
        .replace("symbol = \"XAU-USD\"", "symbol = \"EUR-USD\"");
    for text in [&not_the_pair, &unlisted, &listed_after] {
        assert_ne!(text, &dev, "the replacement must change the config");
        let err = toml::from_str::<NodeConfig>(text).unwrap().validate().unwrap_err();
        assert!(format!("{err:#}").contains("is not the listed EUR-USD market"), "{err:#}");
    }
}

#[test]
fn genesis_entries_listed_twice_fail_the_boot() {
    let dev = std::fs::read_to_string(DEV).unwrap();
    let us500 = &dev[dev.find("[[genesis.markets]]\nid = 20").unwrap()..];
    let us500 = &us500[..us500[1..].find("[[genesis.markets]]").unwrap() + 1];
    let ceiling = &dev[dev.find("[[genesis.close_spread_ceilings]]").unwrap()..];
    let ceiling = &ceiling[..ceiling[1..].find("[[genesis.close_spread_ceilings]]").unwrap() + 1];
    let refused = [
        // A block copied from US500 that keeps its id under a new symbol.
        (format!("{dev}\n{}", us500.replace("\"US500\"", "\"US100\"")), "genesis lists market 20 twice"),
        // The same listing under a new id.
        (format!("{dev}\n{}", us500.replace("id = 20", "id = 21")), "genesis lists market symbol US500 twice"),
        (
            dev.replace("{ name = \"ESTR\", annual_bps = 200 }", "{ name = \"SOFR\", annual_bps = 200 }"),
            "genesis lists benchmark SOFR twice",
        ),
        // A ceiling block copied under a new value.
        (
            format!("{dev}\n{}", ceiling.replace("max_close_spread_bps = 30", "max_close_spread_bps = 25")),
            "genesis lists a close-spread ceiling for market 1 twice",
        ),
    ];
    for (text, expected) in &refused {
        assert_ne!(text, &dev, "the replacement must change the config");
        let err = toml::from_str::<NodeConfig>(text).unwrap().validate().unwrap_err();
        assert!(format!("{err:#}").contains(expected), "expected {expected:?}, got {err:#}");
    }
}

#[test]
fn genesis_markets_need_a_close_spread_ceiling_each_and_one_the_engine_refuses_fails_the_boot() {
    let dev = std::fs::read_to_string(DEV).unwrap();
    let first_ceiling = dev.find("[[genesis.close_spread_ceilings]]").unwrap();
    let without = &dev[..first_ceiling];
    let err = toml::from_str::<NodeConfig>(without).unwrap().validate().unwrap_err();
    assert!(format!("{err:#}").contains("genesis lists markets but no close-spread ceilings"), "{err:#}");
    // The engine names the entry its registry refuses: an unlisted market, a value below the
    // launch close charge, and a listed market with no entry.
    let unlisted = dev.replacen("market = 1\n", "market = 77\n", 1);
    let err = toml::from_str::<NodeConfig>(&unlisted).unwrap().validate().unwrap_err();
    assert!(
        format!("{err:#}").contains("genesis.close_spread_ceilings: the engine rejects it: market 77 is not listed"),
        "{err:#}"
    );
    let below = dev.replacen("max_close_spread_bps = 30", "max_close_spread_bps = 5", 1);
    let err = toml::from_str::<NodeConfig>(&below).unwrap().validate().unwrap_err();
    assert!(
        format!("{err:#}").contains("genesis.close_spread_ceilings: the engine rejects it: a nonzero ceiling"),
        "{err:#}"
    );
    let missing = dev.replacen(
        "[[genesis.close_spread_ceilings]]\nmarket = 1\nmax_close_spread_bps = 30\n",
        "",
        1,
    );
    let err = toml::from_str::<NodeConfig>(&missing).unwrap().validate().unwrap_err();
    assert!(
        format!("{err:#}").contains("genesis.close_spread_ceilings: the engine rejects it: market 1 is listed without"),
        "{err:#}"
    );
}

