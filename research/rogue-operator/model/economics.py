"""What the operator risks: its share of distributions, its self-bond, and the time until removal.

The house's honest income at launch flow is #134's (`research/cap-calibration`, `retained` stage):
the mean USDC distributed over 90 days at the recommended $107M liquidity floor and the planned
crypto margin bands, taken as a daily rate and, for ten times the flow, scaled tenfold (that stage's
own flow sensitivities move only the cloned markets, so they cannot say). The operator in effect
is paid `OPERATOR_SHARE` of each distribution (V47, #434).

Who bears an extraction:
- the operator's own trades win from the house (latency arbitrage, the held sync, unpriced pairs,
  altered quotes): every dollar comes out of what stakers and the operator are later distributed,
  so the operator keeps `(1 - share) * (1 - stake)` of it, `stake` being its share of the staked
  MONKEY;
- skimming clients and censoring their closes pay the house: the operator receives
  `share + (1 - share) * stake` of it, and the stakers the rest.

Removal (#359, #360, #551), in the days the operator keeps operating once stakers, or governance,
act on what it is doing:
- current rules: the vote counts at the next election close (14-day epochs, so 7 days on average)
  and takes effect 7 days plus the 1-day finality margin later; an operator inside its 90-day
  minimum term cannot be voted out until the term ends;
- the guardian: it can retire the signer at once, after which the vault takes no checkpoint, and
  once the 3-day liveness timeout passes anyone promotes a standby, which resumes at the last
  checkpoint the vault accepted and drops everything after it; what the operator took before the
  guardian acted stands, and so the operating days are the guardian's reaction time (taken as six
  hours); every client's commands of those three days are dropped too;
- the timelock (3-day delay) can turn rotation off and install a fallback operator;
- daily rotation: the role moves every day at 22:30 UTC by a stake-weighted draw, so a member with
  a `weight` share of the rotation's stake runs that share of days. Its seat is worth that share of
  the operator income, while on the day it runs it can take a whole day of any attack, and the next
  operator ends it.
The self-bond (0.1% of MONKEY's maximum supply, #360) is locked, never slashed (#359 §11): it
deters only through what MONKEY loses in price once the extraction is known.
"""

import csv
from dataclasses import dataclass
from pathlib import Path

CAP_RESULTS = Path(__file__).resolve().parents[2] / "cap-calibration" / "results"
OPERATOR_SHARE = 0.20
EPOCH_DAYS = 14
NOTICE_DAYS = 7 + 1
MIN_TERM_DAYS = 90
TIMELOCK_DAYS = 3
RETAINED_FLOOR = 107_374_182.4


def honest_distributions_per_day(results: Path = CAP_RESULTS) -> float:
    """#134's mean distribution a day at launch flow, at the recommended floor."""
    with (results / "retained_nav.csv").open() as fh:
        for row in csv.DictReader(fh):
            if row["variant"] == "baseline" and row["markets"] == "37" and abs(float(row["floor"]) - RETAINED_FLOOR) < 1:
                return float(row["distributed_mean"]) / 90
    raise ValueError("no baseline row at the retained floor")


GUARDIAN_DAYS = 0.25


@dataclass
class Removal:
    name: str
    operating_days: float  # days the operator keeps operating once it is acted on, on average
    seat_share: float = 1.0  # its share of the operator income the removal forfeits


def removals() -> list[Removal]:
    return [
        Removal("current rules, past the minimum term", EPOCH_DAYS / 2 + NOTICE_DAYS),
        Removal("current rules, just elected", MIN_TERM_DAYS + NOTICE_DAYS),
        Removal("timelock installs a fallback", TIMELOCK_DAYS),
        Removal("guardian retires the signer within 6 h", GUARDIAN_DAYS),
        *(Removal(f"daily rotation, one day, {w:.0%} of the stake", 1.0, w) for w in (0.01, 0.05, 0.25)),
    ]


def own_keep(stake: float, share: float = OPERATOR_SHARE) -> float:
    return (1 - share) * (1 - stake)


def house_keep(stake: float, share: float = OPERATOR_SHARE) -> float:
    return share + (1 - share) * stake


def seat_value(distributions_per_day: float, weight: float, years: float, share: float = OPERATOR_SHARE) -> float:
    """The operator income a removal forfeits: `years` of its share, at its share of the days."""
    return share * distributions_per_day * weight * 365 * years


def breakeven_take_per_day(seat: float, removal: Removal) -> float:
    """The take per operating day above which extraction pays even if it is acted on at once: the
    seat it forfeits over the days it still operates."""
    return seat / removal.operating_days
