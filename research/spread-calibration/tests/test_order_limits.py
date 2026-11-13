import pytest

from calib import order_limits

TEXT = """WAIT price_stamp_rechecking_0_resting_orders samples=200 p50_us=100 p99_us=284 max_us=288
WAIT price_stamp_rechecking_1000_resting_orders samples=200 p50_us=600 p99_us=785 max_us=871
noise between lines
WAIT price_stamp_rechecking_10000_resting_orders samples=200 p50_us=4100 p99_us=6880 max_us=7282
WAIT price_stamp_with_2500000_resting_orders_away samples=200 p50_us=120 p99_us=300 max_us=310
"""


def test_the_bench_rows_parse_and_each_order_adds_its_mean_cost():
    rows = order_limits.parse_bench(TEXT)
    assert [r["orders"] for r in rows] == [0, 1000, 10000]
    base, slope = order_limits.per_order_us(rows)
    assert base == 100.0
    assert slope == pytest.approx((500 / 1000 + 4000 / 10000) / 2)


def test_the_budget_holds_what_its_room_over_the_base_allows():
    assert order_limits.orders_within(100.0, 0.45) == pytest.approx(2000.0)
    assert order_limits.orders_within(1500.0, 0.45) == 0.0


def test_orders_away_from_the_price_parse_apart():
    rows = order_limits.parse_bench(TEXT, order_limits.AWAY)
    assert rows == [{"orders": 2_500_000, "samples": 200, "p50_us": 120, "p99_us": 300, "max_us": 310}]
    assert 2_500_000 not in [r["orders"] for r in order_limits.parse_bench(TEXT)]
