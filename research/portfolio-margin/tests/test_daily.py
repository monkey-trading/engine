import numpy as np

from pm import daily
from pm.common import launch_symbols


def test_the_committed_daily_history_covers_every_launch_market_from_2016():
    dates, symbols, x = daily.load()
    assert symbols == launch_symbols()
    assert dates[0] <= "2016-01-05"
    assert (np.isfinite(x).sum(axis=0) > 1000).all()


def test_pair_returns_use_only_dates_both_have():
    x = np.array([[0.0, 0.0], [1.0, np.nan], [2.0, 3.0]])
    assert daily.returns(x, [0, 1]).tolist() == [[2.0, 3.0]]
