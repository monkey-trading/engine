import pytest

from calib import owner_report


def test_the_trigger_may_let_the_classes_it_sees_take_the_share_less_the_rest():
    classes = {"latency": (500.0, True), "pairs": (51.0, True), "skimming": (47.0, False)}
    b = owner_report.budget(400.0, classes)
    assert not b["holds"]
    assert b["operator_budget_per_day"] == pytest.approx(353.0)
    assert b["house_loss_budget_per_day"] == pytest.approx(353.0 / 0.8)
    assert b["cut_needed_share"] == pytest.approx(1 - 353.0 / 551.0)


def test_classes_within_the_share_need_no_cut():
    b = owner_report.budget(1000.0, {"latency": (500.0, True), "skimming": (100.0, False)})
    assert b["holds"] and b["cut_needed_share"] == 0.0
