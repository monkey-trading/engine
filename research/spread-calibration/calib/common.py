"""Shared inputs: the launch markets from the genesis, and the sibling studies this one builds on.

The tick-level price paths, the stratified sample of hours and the attack models are
`research/rogue-operator`'s (#549); the hourly bars, the client flow and the house simulation are
`research/cap-calibration`'s (#111, #134). Both name their code `model`, so a process uses one of
them: `use("rogue")` or `use("cap")` puts that study first on the import path, and this study's own
code lives in `calib`.
"""

import sys
import tomllib
from pathlib import Path

HERE = Path(__file__).resolve().parents[1]
RESEARCH = HERE.parent
STUDIES = {"rogue": RESEARCH / "rogue-operator", "cap": RESEARCH / "cap-calibration"}
GENESIS = RESEARCH.parent / "config" / "production-genesis.toml"
CRYPTO = ("BTC-USD", "ETH-USD", "SOL-USD")


def use(study: str) -> Path:
    """Puts `study`'s directory first on the import path, so `model` is its package."""
    path = STUDIES[study]
    other = [p for name, p in STUDIES.items() if name != study]
    if any(str(p) in sys.path for p in other):
        raise RuntimeError(f"a process imports one sibling study's `model`, not both: {study}")
    if str(path) not in sys.path:
        sys.path.insert(0, str(path))
    return path


def genesis_markets() -> list[dict]:
    with GENESIS.open("rb") as fh:
        return tomllib.load(fh)["genesis"]["markets"]
