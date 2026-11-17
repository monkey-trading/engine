"""The groups' loadings and shocks at a tail quantile over a horizon, and their weekend values.

Per group `g` with members `m` (D3's candidates, `common.CANDIDATES`):

- the factor is the equal-weighted mean of the members' returns, `f`; a member's loading is its
  regression slope on it, `beta_m = cov(r_m, f) / var(f)` (so the loadings average 1), and its
  residual is `e_m = r_m - beta_m f`;
- the hourly quantiles are the `q` quantile of `|f|` and of each `|e_m|` over the hours every member
  was open (the cap study's bars), scaled to the horizon by `horizon.ratio` (the move over the
  horizon against the move over the hour, from the 50 ms grids, at least square-root time) and
  divided by the close-out level, so that the level's share of the requirement covers the move;
  stored rounded up to a tenth of a basis point;
- the weekend values are the `q` quantile of the same measures over the closures of 40 hours or more
  (the gaps, undivided: the weekend close-out reads the requirement in full), at the window's
  102 weekends, raised by the ratio of the long history's weekend-span returns' tail to the window's
  where the history's is wider.
"""

import numpy as np

from pm import daily
from pm.comove import KEY
from pm.stats import quantile

STORE = 1e5  # shocks and loadings are stored to a tenth of a basis point, rounded up


def up(x: float, step: float = 1 / STORE) -> float:
    return float(np.ceil(x / step - 1e-9) * step)


def fit(r: np.ndarray) -> tuple[np.ndarray, np.ndarray, np.ndarray]:
    """(factor, loadings, residuals) of the members' returns (rows: observations)."""
    f = r.mean(axis=1)
    beta = (r - r.mean(axis=0)).T @ (f - f.mean()) / ((f - f.mean()) ** 2).sum()
    return f, beta, r - np.outer(f, beta)


def hourly_fit(hr, members: list[str], q: float, scale: float, level: float) -> dict:
    hours, r = hr.joint_hours(members)
    f, beta, e = fit(r)
    half = len(r) // 2
    _, beta1, _ = fit(r[:half])
    _, beta2, _ = fit(r[half:])
    return {
        "hours": len(r),
        "beta": {m: round(float(b), 2) for m, b in zip(members, beta)},
        "beta_halves": {m: (round(float(a), 2), round(float(b), 2)) for m, a, b in zip(members, beta1, beta2)},
        "factor_hour": quantile(np.abs(f), q),
        "rho_hour": {m: quantile(np.abs(e[:, i]), q) for i, m in enumerate(members)},
        "factor": up(quantile(np.abs(f), q) * scale / level),
        "rho": {m: up(quantile(np.abs(e[:, i]), q) * scale / level) for i, m in enumerate(members)},
        "r2": {m: float(1 - e[:, i].var() / r[:, i].var()) for i, m in enumerate(members)},
    }


def weekend_events(hr, members: list[str]) -> np.ndarray:
    """(weeks, members) of the weekend gaps, over the weeks every member has one."""
    gaps = []
    for m in members:
        d = {}
        for kind, last, reopen, gap in hr.closure_gaps(m):
            if kind == "weekend":
                d.setdefault(KEY[kind](last, reopen), gap)
        gaps.append(d)
    keys = sorted(set.intersection(*[set(d) for d in gaps])) if gaps else []
    return np.array([[d[k] for d in gaps] for k in keys]).reshape(len(keys), len(members))


def weekend_span(members: list[str], start: str, end: str) -> np.ndarray:
    """Friday-close-to-Monday-close returns of the members, over the dates in [start, end)."""
    import datetime as dt

    dates, syms, x = daily.load()
    cols = [syms.index(m) for m in members]
    idx = [i for i, d in enumerate(dates) if start <= d < end and np.isfinite(x[i, cols]).all()]
    rows = []
    for a, b in zip(idx[:-1], idx[1:]):
        da, db = dt.date.fromisoformat(dates[a]), dt.date.fromisoformat(dates[b])
        if da.weekday() == 4 and db.weekday() == 0 and (db - da).days == 3:
            rows.append(x[b, cols] - x[a, cols])
    return np.array(rows).reshape(len(rows), len(members))


def weekend_fit(hr, members, beta: dict, q: float, window=("2024-10-28", "2026-10-13")) -> dict:
    g = weekend_events(hr, members)
    if len(g) < 30:
        return {"weeks": len(g)}
    b = np.array([beta[m] for m in members])
    f = g.mean(axis=1)
    e = g - np.outer(f, b)
    long_ = weekend_span(members, "2016-01-01", "2027-01-01")
    win = weekend_span(members, *window)
    fl, fw = long_.mean(axis=1), win.mean(axis=1)
    el, ew = long_ - np.outer(fl, b), win - np.outer(fw, b)
    ratio_f = max(1.0, quantile(np.abs(fl), q) / quantile(np.abs(fw), q))
    out = {
        "weeks": len(g),
        "long_weeks": len(long_),
        "tail_ratio_factor": ratio_f,
        "factor_gap": quantile(np.abs(f), q),
        "factor": up(quantile(np.abs(f), q) * ratio_f),
        "rho": {}, "rho_gap": {}, "tail_ratio_rho": {},
    }
    for i, m in enumerate(members):
        ratio = max(1.0, quantile(np.abs(el[:, i]), q) / quantile(np.abs(ew[:, i]), q))
        out["rho_gap"][m] = quantile(np.abs(e[:, i]), q)
        out["tail_ratio_rho"][m] = ratio
        out["rho"][m] = up(quantile(np.abs(e[:, i]), q) * ratio)
    return out
