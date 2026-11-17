import numpy as np

from pm import model


def specs():
    return model.load_specs()


def group():
    return model.Group("fx", ["EUR-USD", "GBP-USD"], {"EUR-USD": 1.0, "GBP-USD": 1.0}, 0.001, {"EUR-USD": 0.0005, "GBP-USD": 0.0005}, 0.5, 0.01, {"EUR-USD": 0.004, "GBP-USD": 0.004})


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


def test_the_requirement_never_exceeds_per_position_margin_and_holds_the_matched_floor():
    s = specs()
    book = {"EUR-USD": np.array([1e6, 1e6, 3e7, 5e5]), "GBP-USD": np.array([-1e6, 0.0, -3e7, -1e6])}
    for phi in (0.0, 0.3, 0.8, 1.0):
        p = model.Params([group()], phi)
        r, pp = model.requirement(s, p, book), model.per_position(s, book)
        assert (r <= pp + 1e-9).all()
        assert (r >= phi * 0).all()
    assert model.requirement(s, model.Params([group()], 1.0), book).tolist() == model.per_position(s, book).tolist()


def test_a_lone_position_and_a_same_side_book_keep_their_per_position_requirement_exactly():
    s = specs()
    p = model.Params([group()], 0.0)
    for book in ({"EUR-USD": np.array([4e6])}, {"EUR-USD": np.array([4e6]), "GBP-USD": np.array([2e6])}):
        assert model.requirement(s, p, book)[0] == model.per_position(s, book)[0]


def test_matched_exposure_is_the_gross_loading_weighted_exposure_less_the_absolute_net():
    g = group()
    g.beta = {"EUR-USD": 1.0, "GBP-USD": 2.0}
    book = {"EUR-USD": np.array([3e6]), "GBP-USD": np.array([-1e6])}  # x = +3m and -2m
    assert model.matched_exposure(g, book)[0] == 4e6
    # the larger side is matched in part: 2/3 of EUR-USD against all of GBP-USD
    f = model._group_arrays(g, book, False)
    assert abs(f["EUR-USD"][0] - 2 / 3) < 1e-12 and f["GBP-USD"][0] == 1.0


def test_a_partly_matched_book_pays_its_unmatched_part_at_bands_and_the_floor_on_the_rest():
    s = specs()
    g = group()
    book = {"EUR-USD": np.array([3e6]), "GBP-USD": np.array([-1e6])}
    p = model.Params([g], 0.5)
    pp_eur, pp_gbp = (model.per_position(s, {m: v}) [0] for m, v in book.items())
    unmatched = pp_eur / 3
    matched_pp = pp_eur * 2 / 3 + pp_gbp
    r = model.requirement(s, p, book)[0]
    assert r >= unmatched + 0.5 * matched_pp - 1e-6 and r <= unmatched + matched_pp + 1e-6


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


def test_over_a_weekend_gap_the_rule_never_exceeds_per_position_margin_and_a_lone_leg_keeps_today_exactly():
    s = specs()
    p = model.Params([group()], 0.0)
    for book in ({"EUR-USD": np.array([4e6]), "GBP-USD": np.array([-4e6])}, {"EUR-USD": np.array([4e6])}):
        # EUR-USD and GBP-USD have no weekend rate, so none of this book has a requirement over the gap
        assert model.requirement(s, p, book, 0.0, weekend=True)[0] == 0.0
    s["GBP-USD"].weekend = 0.02
    book = {"EUR-USD": np.array([4e6]), "GBP-USD": np.array([-4e6])}
    base = model.staircase(s["GBP-USD"].bands, book["GBP-USD"], 0.02)[0]
    assert model.requirement(s, p, book, 0.0, weekend=True)[0] <= base + 1e-9
    lone = {"GBP-USD": np.array([4e6])}
    assert model.requirement(s, p, lone, 0.0, weekend=True)[0] == model.staircase(s["GBP-USD"].bands, lone["GBP-USD"], 0.02)[0]
