"""Shared inputs: the genesis, and the sibling studies this one runs.

The hourly house simulation, its client flow and its price bars are `research/cap-calibration`'s
(#111, #134); the trade grids and the unpriced-spell measure are `research/rogue-operator`'s
(#549); the fee curves and target flow are `research/spread-calibration`'s (#623, #697). The first
two both name their code `model`, so a process uses one of them: `use("cap")` or `use("rogue")`
puts that study first on the import path. `use_spread()` adds the third, whose code is `calib`.
This study's own code is `caps`.
"""

import sys
import tomllib
from pathlib import Path

HERE = Path(__file__).resolve().parents[1]
RESEARCH = HERE.parent
STUDIES = {"rogue": RESEARCH / "rogue-operator", "cap": RESEARCH / "cap-calibration"}
SPREAD = RESEARCH / "spread-calibration"
GENESIS = RESEARCH.parent / "config" / "production-genesis.toml"
RESULTS = HERE / "results"


def use(study: str) -> Path:
    """Puts `study`'s directory first on the import path, so `model` is its package."""
    path = STUDIES[study]
    if any(str(p) in sys.path for name, p in STUDIES.items() if name != study):
        raise RuntimeError(f"a process imports one sibling study's `model`, not both: {study}")
    if str(path) not in sys.path:
        sys.path.insert(0, str(path))
    return path


def use_spread() -> None:
    """Puts `research/spread-calibration` on the import path, for its `calib` package."""
    if str(SPREAD) not in sys.path:
        sys.path.append(str(SPREAD))


def genesis() -> dict:
    with GENESIS.open("rb") as fh:
        return tomllib.load(fh)["genesis"]
