"""The committed results against the genesis and D3's rule for a group."""

import json
import tomllib

from pm.common import GENESIS, RESULTS


def groups():
    return json.loads((RESULTS / "groups.json").read_text())


def genesis_markets():
    return {m["symbol"]: m for m in tomllib.loads(GENESIS.read_text())["genesis"]["markets"]}


def test_a_kept_group_shares_a_class_a_schedule_and_a_usd_quote():
    mk = genesis_markets()
    for name, g in groups()["groups"].items():
        if not g["kept"]:
            continue
        assert len({mk[m]["asset_class"] for m in g["members"]}) == 1, name
        assert len({json.dumps(mk[m].get("schedule"), sort_keys=True) for m in g["members"]}) == 1, name
        assert all(mk[m].get("quote_currency", {"currency": "usd"})["currency"] == "usd" for m in g["members"]), name
        assert "USD-JPY" not in g["members"] and "JP225" not in g["members"]


def test_the_proposal_keeps_its_loadings_and_shocks_positive_and_the_share_in_range():
    out = groups()
    for g in out["groups"].values():
        assert 0 <= g["phi"] <= 1
        assert g["lone_unchanged"]["max_abs_leverage_diff"] == 0 and g["lone_unchanged"]["max_abs_rate_diff"] == 0
        if g["kept"]:
            assert all(v > 0 for v in g["rho"].values()) and all(b > 0 for b in g["beta"].values())


def test_the_toml_names_every_kept_group_and_no_dropped_one():
    t = tomllib.loads((RESULTS / "proposal.toml").read_text())["genesis"]["params"]["portfolio_margin"]
    kept = {n for n, g in groups()["groups"].items() if g["kept"]}
    assert {g["name"] for g in t["groups"]} == kept
    assert {g["name"]: g["floor_share_bps"] for g in t["groups"]} == {n: round(g["phi"] * 1e4) for n, g in groups()["groups"].items() if g["kept"]}
