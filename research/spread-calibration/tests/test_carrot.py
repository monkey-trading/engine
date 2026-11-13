import pytest

from calib import carrot


MARKET = {"bound": 2e6, "s1_bps": 2.0, "s2_bps": 4.0, "margin": 0.01}


def test_a_tier_needs_its_volume_over_enough_days():
    assert carrot.tier_for(30e6) == 2
    assert carrot.tier_for(30e6, days=1) == 0
    assert carrot.tier_for(30e6, days=3) == 1
    assert carrot.tier_for(300e6, days=10) == 3


def test_a_recycled_wallet_stays_at_tier_0_whatever_its_size():
    life = carrot.toxic_life_volume(10e6, 20)
    assert carrot.tier_for(life, days=1) == 0
    assert carrot.tier_for(1e12, days=1) == 0


def test_farming_a_threshold_costs_more_than_a_month_of_its_best_price():
    farm = carrot.farming_cost_bps(2.0, 0.75, 0.7)
    for i, t in enumerate(carrot.TIERS[1:], 1):
        fill = t.best_price_share * MARKET["bound"]
        trader = carrot.Trader("at threshold", volume=t.volume, fill=fill, hold_s=1800.0)
        value = carrot.benefit_values(trader, t, MARKET, 0.0)["best_price"]
        assert farm / 1e4 * t.volume > value, i


def test_farming_pays_a_spread_a_fill_and_the_fee_on_the_winning_side():
    assert carrot.farming_cost_bps(2.0, 0.0, 0.7) == pytest.approx(2.0)
    assert carrot.farming_cost_bps(2.0, 1.0, 0.7, hold_s=60.0) > 2.0


def test_a_rare_trader_is_never_caught_and_a_frequent_one_pays_the_add_on():
    # One (hold, theta) cell: positive 1 and negative 0 per unit a year, at the stated trades a year.
    sched = {1.0: 0.10}
    rare = carrot.best_responses({(600.0, 5.0): (1.0, 0.0, 10.0)}, sched, MARKET, 2.0, 20, 1e6)
    busy = carrot.best_responses({(600.0, 5.0): (1.0, 0.0, 3650.0)}, sched, MARKET, 2.0, 20, 1e6)
    assert rare["keeper"]["per_day"] >= rare["recycler"]["per_day"]
    assert busy["recycler"]["per_day"] > busy["keeper"]["per_day"]


def test_recycling_forfeits_the_kept_tiers_best_price_on_large_fills():
    t = carrot.Trader("large", volume=100e6, fill=2e6, hold_s=600.0)
    r = carrot.recycling(t, 2.0, MARKET, 20, 0.0)
    assert r["kept_tier"] >= 2 and r["recycled_tier"] < r["kept_tier"]
    assert r["benefits"]["best_price"] == pytest.approx((4.0 - 2.0) / 1e4 * 1.5e6 / 2e6 * 100e6)


def test_a_keeper_pays_its_tiers_fee_and_a_recycler_tier_0s():
    # Long holds past the curve: a keeper whose volume earns tier 3 pays the base less 20%, which a
    # recycler at tier 0 never does.
    from calib import fee

    year = {(1800.0, 5.0): (1.0, 0.0, 1000.0)}
    calls = []

    def tier_fee(hold, tier):
        calls.append(tier)
        return fee.d7(hold, 0.10, fee.LAUNCH_DISCOUNTS[tier])

    out = carrot.best_responses(year, {}, MARKET, 2.0, 20, 50e6, tier_fee)
    assert out["keeper"]["tier"] == 3 and 3 in calls
    flat = carrot.best_responses(year, {1.0: 0.10}, MARKET, 2.0, 20, 50e6)
    assert out["keeper"]["per_day"] > flat["keeper"]["per_day"]
    assert out["recycler"]["per_day"] == pytest.approx(flat["recycler"]["per_day"])
