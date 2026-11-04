from dataclasses import dataclass

import pytest

from calib import target


ROWS = [{"hold_from_s": "0", "hold_to_s": "900", "notional_share": "0.6"},
        {"hold_from_s": "900", "hold_to_s": "3600", "notional_share": "0.2"},
        {"hold_from_s": "3600", "hold_to_s": "inf", "notional_share": "0.2"}]


def test_within_hour_split_is_the_share_held_fifteen_minutes_or_more():
    assert target.within_hour_split(ROWS) == pytest.approx(0.25)


def test_target_flow_keeps_long_holds_and_a_tenth_of_the_short():
    from calib.common import use

    use("cap")
    from model.flow import Flow, FlowClass

    classes = [FlowClass("directional", 0, 0, 0.8, 0.05, 0.2, 0.5), FlowClass("directional", 4, 0, 0.2, 4.0, 2.0, 0.5)]
    flow = Flow(classes, {}, None, {}, 0.0, 0.0, 0.0)
    out = target.target_flow(flow, ROWS)
    shares = sorted(round(c.margin_share, 6) for c in out.classes)
    assert shares == sorted([round(0.8 * 0.75 * target.KEEP_SHORT, 6), round(0.8 * 0.25, 6), 0.2])
    kept = [c for c in out.classes if c.hold_h == 0 and c.mean_hold_h > 0.25]
    assert len(kept) == 1 and 0.25 < kept[0].mean_hold_h < 1.0
