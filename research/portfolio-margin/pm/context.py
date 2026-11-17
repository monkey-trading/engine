"""What every stage needs, built once: the hourly bars, the genesis specs, the proposed standard and
the groups the proposal keeps (read from `results/groups.json` once the `groups` stage has run)."""

from dataclasses import dataclass

from pm import horizon, model
from pm.common import CANDIDATES
from pm.hourly import Hourly

QUANTILE = 0.995  # two-sided tail of the move
HORIZON_S = 60  # the fast-move trigger's window (`stress_move_window_ms`)
LEVEL = 0.5  # the launch close-out level
TIER_FLOORS = {0: 0.05, 1: 0.02, 2: 0.0}  # `margin_floor_bps` of the launch volume tiers (tier 3 as 2)
SEED = 914


@dataclass
class Ctx:
    hr: Hourly
    specs: dict
    q: float = QUANTILE
    h: float = HORIZON_S
    level: float = LEVEL

    @property
    def scale(self) -> float:
        return horizon.ratio(horizon.table(), self.h, self.q)


def make() -> Ctx:
    return Ctx(Hourly(), model.load_specs())


def params_from(groups_json: dict, phi: float | None = None, kept_only: bool = True) -> model.Params:
    gs = []
    for name, g in groups_json["groups"].items():
        if kept_only and not g["kept"]:
            continue
        w = g.get("weekend", {})
        gs.append(model.Group(name, g["members"], g["beta"], g["factor"], g["rho"], w.get("factor", 0.0), w.get("rho", {})))
    return model.Params(gs, groups_json["phi"] if phi is None else phi, groups_json.get("level", LEVEL))


__all__ = ["CANDIDATES", "Ctx", "make", "params_from"]
