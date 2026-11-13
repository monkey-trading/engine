"""The floor proposal across the two ends of the tier range."""

from caps import house


def row(family, level, p_short, refused, total):
    """A joint row whose worst market refuses `refused` and whose other refuses half that."""
    return {"family": family, "level": level, "p_short": p_short, "total_floor": total, "refused_by_market": {"A": refused, "B": refused / 2}}


def test_without_a_set_that_serves_every_market_a_proposal_refuses_the_least_at_its_worst():
    joints = {
        0: [row("uniform", 1e6, 0.005, 0.20, 34e6), row("uniform", 1e7, 0.009, 0.02, 340e6), row("uniform", 3e7, 0.002, 0.01, 1e9)],
        3: [row("uniform", 1e6, 0.004, 0.30, 34e6), row("uniform", 1e7, 0.020, 0.01, 340e6), row("uniform", 3e7, 0.008, 0.015, 1e9)],
    }
    choice = house.propose(joints, "p_short")
    # $10M fails at tier 3; of the two that pass, neither keeps every market under 1% at both ends,
    # and $30M's worst market (1.5%) beats $1M's (30%).
    assert (choice["family"], choice["level"], choice["serves_flow"]) == ("uniform", 3e7, False)
    assert house.propose(joints, "p_short", target=0.001) is None


def test_a_proposal_takes_the_smallest_set_that_meets_the_target_and_serves_every_market():
    joints = {
        0: [row("a", 1, 0.0, 0.03, 50e6), row("b", 1, 0.0, 0.008, 150e6), row("c", 1, 0.0, 0.001, 300e6), row("d", 1, 0.0, 0.0, 1e9)],
        3: [row("a", 1, 0.0, 0.05, 50e6), row("b", 1, 0.0, 0.009, 150e6), row("c", 1, 0.02, 0.001, 300e6), row("d", 1, 0.0, 0.0, 1e9)],
    }
    # "c" fails the target at tier 3; "b" and "d" meet it and serve the flow, and "b" is smaller.
    choice = house.propose(joints, "p_short")
    assert (choice["family"], choice["serves_flow"]) == ("b", True)


def test_each_floor_set_maps_to_its_distinct_run():
    import numpy as np

    sets = [np.array([0.0, 1e6]), np.array([3e7, 3e7]), np.array([0.0, 1e6]), np.array([1e5, 1e5])]
    unique, index = house.distinct(sets)
    assert len(unique) == 3
    for given, i in zip(sets, index):
        assert (unique[i] == given).all()
