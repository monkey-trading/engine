"""Replays each fixture through the cap and writes every move it would have refused to results/."""

import csv
from datetime import date
from decimal import Decimal
from pathlib import Path

HERE = Path(__file__).resolve().parent
FIXTURES = HERE / "fixtures"
RESULTS = HERE / "results"
BENCHMARKS = ["sofr", "estr", "sonia", "aonia", "tona"]
CAP_BPS_PER_DAY = Decimal(300)


def load(name):
    """A benchmark's observations in date order."""
    with open(FIXTURES / f"{name}.csv", newline="") as file:
        return [(date.fromisoformat(day), Decimal(rate)) for day, rate in csv.reader(file)]


def replay(observations):
    """The cap as the engine applies it to a benchmark's rates in date order: each is measured from
    the last applied, and a refused rate changes nothing, so the next is measured from the same one
    over a wider gap."""
    refused = []
    last_day, last_rate = observations[0]
    for day, rate in observations[1:]:
        gap = (day - last_day).days
        move, limit = abs(rate - last_rate) * 100, CAP_BPS_PER_DAY * gap
        if move > limit:
            refused.append((day, rate, last_day, last_rate, gap, move, limit))
        else:
            last_day, last_rate = day, rate
    return refused


def published_moves(observations):
    """Consecutive published moves past the cap, as if the last published rate had always applied."""
    return [
        (b, y, a, x, (b - a).days)
        for (a, x), (b, y) in zip(observations, observations[1:])
        if abs(y - x) * 100 > CAP_BPS_PER_DAY * (b - a).days
    ]


def closest(observations):
    """The published move nearest the cap, as a share of its limit, after 10 June 1988."""
    pairs = [pair for pair in zip(observations, observations[1:]) if pair[0][0] > date(1988, 6, 10)]
    return max(pairs, key=lambda p: abs(p[1][1] - p[0][1]) * 100 / (CAP_BPS_PER_DAY * (p[1][0] - p[0][0]).days))


def main():
    """Writes refused.csv, the rates a replay refuses, and summary.md, the study's figures."""
    RESULTS.mkdir(exist_ok=True)
    with open(RESULTS / "refused.csv", "w", newline="") as file:
        writer = csv.writer(file, lineterminator="\n")
        writer.writerow(["benchmark", "date", "rate_pct", "last_applied_date", "last_applied_pct", "gap_days", "move_bps", "limit_bps"])
        summary = ["| Benchmark | Observations | From | To | Refused in replay | Published moves past the cap | Closest move after 10 June 1988 |", "| --- | ---: | --- | --- | ---: | ---: | --- |"]
        for name in BENCHMARKS:
            observations = load(name)
            refused = replay(observations)
            for day, rate, last_day, last_rate, gap, move, limit in refused:
                writer.writerow([name.upper(), day, rate, last_day, last_rate, gap, format(move.normalize(), "f"), limit])
            (a, x), (b, y) = closest(observations)
            gap = (b - a).days
            move = abs(y - x) * 100
            near = f"{a} {x}% to {b} {y}%: {format(move.normalize(), 'f')} bps of {CAP_BPS_PER_DAY * gap}"
            summary.append(
                f"| {name.upper()} | {len(observations)} | {observations[0][0]} | {observations[-1][0]} | {len(refused)} | {len(published_moves(observations))} | {near} |"
            )
    (RESULTS / "summary.md").write_text("\n".join(summary) + "\n")
    print("\n".join(summary))


if __name__ == "__main__":
    main()
