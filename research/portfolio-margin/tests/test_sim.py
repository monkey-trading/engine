import numpy as np

from pm import model, sim
from pm.checks import pair_book


class FakeWindow:
    """Two always-open members that move together, then one member jumps."""

    def __init__(self, hours=400, jump=None):
        self.members = ["EUR-USD", "GBP-USD"]
        ret = np.zeros((hours, 2))
        if jump is not None:
            ret[50] = jump
        self.ret = ret
        self.open = np.ones((hours, 2), bool)
        self.all_open = np.ones(hours, bool)
        self.gap_start = np.zeros(hours, bool)
        self.starts = np.arange(0, hours - 200)


def group():
    return model.Group("fx", ["EUR-USD", "GBP-USD"], {"EUR-USD": 1.0, "GBP-USD": 1.0}, 0.001, {"EUR-USD": 0.0005, "GBP-USD": 0.0005})


def run(w, rule, phi, tier_floor=0.0):
    s = model.load_specs()
    n0 = np.tile(pair_book(w.members, {"EUR-USD": 1.0, "GBP-USD": 1.0}, "EUR-USD", "GBP-USD", 1e6, 1.0), (5, 1))
    return sim.simulate(w, s, model.Params([group()], phi), rule, w.members, n0, np.zeros(5, int), tier_floor)


def test_a_full_floor_share_runs_as_per_position_margin():
    w = FakeWindow(jump=[-0.004, 0.0])
    a, b = run(w, "bands", 1.0), run(w, "portfolio", 1.0)
    for k in ("e0", "closed_out", "past_balance", "house"):
        assert np.allclose(a[k], b[k])


def test_the_rule_opens_with_less_equity_and_a_quiet_week_closes_nobody():
    w = FakeWindow()
    a, b = run(w, "bands", 0.5), run(w, "portfolio", 0.5)
    assert (b["e0"] < a["e0"]).all()
    assert not b["closed_out"].any()


def test_a_jump_past_the_balance_is_the_houses_and_equity_never_goes_below_zero():
    w = FakeWindow(jump=[-0.2, 0.0])
    r = run(w, "portfolio", 0.5)
    assert r["closed_out"].all() and (r["past_balance"] > 0).all()
    assert np.allclose(r["house"], r["e0"])  # the client loses its whole balance, the house the rest
