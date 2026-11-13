#!/usr/bin/env python3
"""Judges the alerts in a dev scenario's node log.

audit LOG [--allow A|B] [--transient C|D] [--window SECONDS] [--now ISO]
  An alert named in --allow is ignored. One named in --transient is a Warning a retry clears,
  such as an RPC provider that has not yet reached the common final block: it is tolerated and
  reported when its `alert_resolved` follows within the window, and a violation when it persists
  past the window or never resolves. `NAME=SECONDS` gives one alert its own window: a missed
  Roughtime round warns `trusted_time_no_sample` until the next round, an interval and a query
  timeout later, and the sequencer halts only past its 60 s grace. Any other alert is a violation. A wrong outcome is caught by
  the scenario's own checks, so a blip that coincides with one still fails the run there.
  Exit 0: fine. 1: violations. 2: a transient alert is open but younger than the window.
selftest
  Checks both cases on synthetic logs: a quick-clearing alert passes and a persisting one fails.
"""
import argparse
import re
import sys
import tempfile
from datetime import datetime, timedelta, timezone

LINE = re.compile(r'^(\d{4}-\d\d-\d\dT[\d:]+)(\.\d+)?Z\s.*?\balert(_resolved)?="([a-z_]+)"')


def parse_time(stamp, fraction):
    when = datetime.strptime(stamp, "%Y-%m-%dT%H:%M:%S").replace(tzinfo=timezone.utc)
    return when + timedelta(seconds=float("0" + (fraction or ".0")))


def field(line, name):
    found = re.search(rf'\b{name}="([^"]*)"', line)
    return found.group(1) if found else None


def audit(lines, allow, transient, window, now):
    """Returns (status, report lines). `transient` maps an alert to its window, or None for the default."""
    report, open_alerts, status = [], {}, 0
    for line in lines:
        found = LINE.match(line)
        if not found:
            if "first trusted time sample: sequencing started" in line:
                # A restarted node samples afresh and never resolves its predecessor's open warning.
                for key in [k for k in open_alerts if k[0] == "trusted_time_no_sample"]:
                    del open_alerts[key]
            continue
        stamp, fraction, resolved, name = found.groups()
        if name in allow:
            continue
        when = parse_time(stamp, fraction)
        key = (name, field(line, "provider"), field(line, "operation"))
        if name not in transient:
            if not resolved:
                report.append(f"violation: unexpected alert: {line.strip()}")
                status = 1
        elif not resolved:
            open_alerts.setdefault(key, when)
        elif key in open_alerts:
            lasted = (when - open_alerts.pop(key)).total_seconds()
            limit = transient[name] or window
            if lasted <= limit:
                report.append(f"tolerated: {key} cleared after {lasted:.2f}s")
            else:
                report.append(f"violation: {key} persisted {lasted:.2f}s, past the {limit}s window")
                status = 1
    for key, since in open_alerts.items():
        age = (now - since).total_seconds()
        limit = transient[key[0]] or window
        if age > limit:
            report.append(f"violation: {key} still open after {age:.2f}s, past the {limit}s window")
            status = 1
        elif status == 0:
            report.append(f"pending: {key} open for {age:.2f}s")
            status = 2
    return status, report


def selftest():
    now = datetime(2026, 1, 1, 0, 1, 0, tzinfo=timezone.utc)
    warn = '2026-01-01T00:00:{s}Z  WARN engine_node::custody::quorum: provider failed alert="custody_rpc_provider_failed" provider="p" operation="finality"'
    clear = '2026-01-01T00:00:{s}Z  INFO engine_node::custody::quorum: provider recovered alert_resolved="custody_rpc_provider_failed" provider="p" operation="finality"'
    other = '2026-01-01T00:00:01.000000Z  WARN x: y alert="api_accept_failed"'
    transient = {"custody_rpc_provider_failed": None}
    cases = [
        ("a warning that clears in 0.2s", [warn.format(s="10.100000"), clear.format(s="10.300000")], set(), 0),
        ("a warning that clears after 30s", [warn.format(s="10.100000"), clear.format(s="40.100000")], set(), 1),
        ("a warning that never clears", [warn.format(s="10.100000")], set(), 1),
        ("a warning still young", [warn.format(s="58.000000")], set(), 2),
        ("an unrelated alert", [other], set(), 1),
        ("an allowed alert that persists", [warn.format(s="10.100000")], set(transient), 0),
    ]
    for name, lines, allow, want in cases:
        got, report = audit(lines, allow, transient, 5, now)
        assert got == want, f"{name}: exit {got}, want {want}: {report}"
    miss = '2026-01-01T00:00:{s}Z  WARN engine_node::trusted_time: no trusted time sample: only 1 replies, 2 needed alert="trusted_time_no_sample"'
    back = '2026-01-01T00:00:{s}Z  INFO engine_node::trusted_time: restored alert_resolved="trusted_time_no_sample"'
    slow = {"trusted_time_no_sample": 30}
    for name, lines, want in [
        ("a missed round that clears in 12s", [miss.format(s="10.000000"), back.format(s="22.000000")], 0),
        ("a missed round that clears after 40s", [miss.format(s="10.000000"), back.format(s="50.000000")], 1),
        ("a missed round never recovering", [miss.format(s="10.000000")], 1),
        ("a missed round still young", [miss.format(s="45.000000")], 2),
    ]:
        got, report = audit(lines, set(), slow, 5, now)
        assert got == want, f"{name}: exit {got}, want {want}: {report}"
    boot = '2026-01-01T00:00:30.000000Z  INFO engine_node::trusted_time::gate: first trusted time sample: sequencing started'
    got, report = audit([miss.format(s="10.000000"), boot], set(), slow, 5, now)
    assert got == 0, f"a restart after a missed round: exit {got}: {report}"
    with tempfile.NamedTemporaryFile("w+") as log:  # the command line, end to end
        log.write(warn.format(s="10.100000") + "\n" + clear.format(s="10.300000") + "\n")
        log.flush()
        assert run(["audit", log.name, "--transient", "custody_rpc_provider_failed"]) == 0
    with tempfile.NamedTemporaryFile("w+") as log:  # a per-alert window on the command line
        log.write(miss.format(s="10.000000") + "\n" + back.format(s="22.000000") + "\n")
        log.flush()
        assert run(["audit", log.name, "--transient", "trusted_time_no_sample=30"]) == 0
        assert run(["audit", log.name, "--transient", "trusted_time_no_sample"]) == 1
    print("PASS: a quick-clearing warning passes; a persisting or unresolved one fails")


def run(argv):
    parser = argparse.ArgumentParser()
    sub = parser.add_subparsers(dest="command", required=True)
    sub.add_parser("selftest")
    check = sub.add_parser("audit")
    check.add_argument("log")
    check.add_argument("--allow", default="")
    check.add_argument("--transient", default="")
    check.add_argument("--window", type=float, default=5)
    args = parser.parse_args(argv)
    if args.command == "selftest":
        selftest()
        return 0
    with open(args.log, errors="replace") as log:
        transient = {}
        for item in filter(None, args.transient.split("|")):
            name, _, seconds = item.partition("=")
            transient[name] = float(seconds) if seconds else None
        status, report = audit(log, set(args.allow.split("|")) - {""}, transient, args.window,
                               datetime.now(timezone.utc))
    print("\n".join(report))
    return status


if __name__ == "__main__":
    sys.exit(run(sys.argv[1:]))
