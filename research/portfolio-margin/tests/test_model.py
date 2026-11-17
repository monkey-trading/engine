import numpy as np

from pm import model


def specs():
    return model.load_specs()


def group():
    return model.Group("fx", ["EUR-USD", "GBP-USD"], {"EUR-USD": 1.0, "GBP-USD": 1.0}, 0.001, {"EUR-USD": 0.0005, "GBP-USD": 0.0005}, 0.01, {"EUR-USD": 0.004, "GBP-USD": 0.004})


def test_staircase_charges_each_slice_at_its_band():
    bands = [(2e6, 0.01), (1e7, 0.02), (np.inf, 0.05)]
    assert model.staircase(bands, np.array([3e6]))[0] == 2e6 * 0.01 + 1e6 * 0.02
    assert model.staircase(bands, np.array([-1e6]))[0] == 1e6 * 0.01


def test_a_floor_raises_each_band_to_it():
    bands = [(2e6, 0.01), (np.inf, 0.05)]
    assert model.staircase(bands, np.array([3e6]), 0.02)[0] == 3e6 * 0.02 * 0 + 2e6 * 0.02 + 1e6 * 0.05


def test_no_group_is_per_position_margin():
    s = specs()
    p = model.Params([], 0.5)
    book = {"EUR-USD": np.array([2e6]), "US500": np.array([-3e6])}
    assert model.requirement(s, p, book)[0] == model.per_position(s, book)[0]


def test_the_requirement_never_exceeds_per_position_margin_and_holds_the_floor():
    s = specs()
    book = {"EUR-USD": np.array([1e6, 1e6, 3e7]), "GBP-USD": np.array([-1e6, 0.0, -3e7])}
    for phi in (0.0, 0.3, 0.8, 1.0):
        p = model.Params([group()], phi)
        r, pp = model.requirement(s, p, book), model.per_position(s, book)
        assert (r <= pp + 1e-9).all()
        assert (r >= phi * pp - 1e-9).all()


def test_a_full_floor_share_is_per_position_margin():
    s = specs()
    book = {"EUR-USD": np.array([4e6]), "GBP-USD": np.array([-4e6])}
    assert model.requirement(s, model.Params([group()], 1.0), book)[0] == model.per_position(s, book)[0]


def test_offsetting_loadings_cancel_the_factor_term_only():
    s = specs()
    p = model.Params([group()], 0.0)
    hedged = {"EUR-USD": np.array([1e5]), "GBP-USD": np.array([-1e5])}
    same = {"EUR-USD": np.array([1e5]), "GBP-USD": np.array([1e5])}
    diff = model.model_term(s, p, same)[0] - model.model_term(s, p, hedged)[0]
    assert abs(diff - 0.001 * 2e5) < 1e-6


def test_closing_cost_is_the_ceiling_over_the_level():
    s = specs()
    book = {"EUR-USD": np.array([1e6])}
    assert abs(model.closing_cost(s, book, 0.5)[0] - 1e6 * s["EUR-USD"].ceiling / 0.5) < 1e-9


def test_weekend_values_apply_only_where_the_group_has_them():
    s = specs()
    g = group()
    book = {"EUR-USD": np.array([1e6]), "GBP-USD": np.array([-1e6])}
    p = model.Params([g], 0.0)
    assert model.model_term(s, p, book, weekend=True)[0] > model.model_term(s, p, book)[0]
    g.rho_weekend = {}
    assert model.model_term(s, model.Params([g], 0.0), book, weekend=True)[0] == model.model_term(s, p, book)[0]
