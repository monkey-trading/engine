import numpy as np
import pytest

import run
from trigger.paths import BINS_PER_HOUR

DAY = 24 * BINS_PER_HOUR


def test_each_threshold_fires_in_its_share_of_hours_and_reports_its_room():
    hours = 2000
    series = np.full(hours * BINS_PER_HOUR, -1.0)  # the house earns $1 a bin
    loss_hours = np.arange(0, hours, 100)  # one hour in a hundred loses $5,000 in one bin
    series[loss_hours * BINS_PER_HOUR + 180] = 5000.0
    out = run.honest_summary(series, 6, np.ones(hours, dtype=bool), {}, DAY, True)
    half = next(t for t in out["thresholds"] if t["rate"] == 0.005)
    # The hours that lose are 1% of them, so a threshold for 0.5% sits at their loss over the minute.
    assert half["threshold"] == pytest.approx(5000.0 - 5.0)
    assert half["fire_share"] == pytest.approx(0.01)
    # Raised from the loss for the five-minute cooldown, then down once the minute's reading falls.
    assert half["raised_share"] == pytest.approx(30 / BINS_PER_HOUR * 0.01)
    assert out["mean"] == pytest.approx(6 * series.mean(), rel=1e-4)  # the first bins read partial windows
    # A day's room: each minute's threshold less the minute's honest loss, over 1,440 minutes.
    per_minute = series[: 5 * DAY].reshape(5, -1, 6).sum(axis=2)
    assert half["daily_room_mean"] == pytest.approx(np.maximum(half["threshold"] - per_minute, 0).sum(axis=1).mean(), rel=0.01)


def test_a_reading_that_cannot_fall_below_zero_fires_on_any_loss():
    hours = 1000
    series = np.zeros(hours * BINS_PER_HOUR)
    series[10 * BINS_PER_HOUR] = 50.0
    out = run.honest_summary(series, 360, np.ones(hours, dtype=bool), {}, DAY, False)
    clamped = [t for t in out["thresholds"] if t["rate"] >= 0.005]
    assert clamped and all(t["threshold"] == 1.0 and t["fire_share"] == pytest.approx(1 / hours) for t in clamped)
