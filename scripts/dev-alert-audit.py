#!/usr/bin/env python3
"""Judges the alerts in a dev scenario's node log.

audit LOG [--allow A|B] [--transient C|D] [--window SECONDS] [--now ISO]
  An alert named in --allow is ignored. One named in --transient is a Warning a retry clears,
  such as an RPC provider that has not yet reached the common final block: it is tolerated and
  reported when its `alert_resolved` follows within the window, and a violation when it persists
  past the window or never resolves. Any other alert is a violation. A wrong outcome is caught by
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
    """Returns (status, report lines)."""
    report, open_alerts, status = [], {}, 0
    for line in lines:
        found = LINE.match(line)
        if not found:
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
            if lasted <= window:
                report.append(f"tolerated: {key} cleared after {lasted:.2f}s")
            else:
                report.append(f"violation: {key} persisted {lasted:.2f}s, past the {window}s window")
                status = 1
    for key, since in open_alerts.items():
        age = (now - since).total_seconds()
        if age > window:
            report.append(f"violation: {key} still open after {age:.2f}s, past the {window}s window")
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
    transient = {"custody_rpc_provider_failed"}
    cases = [
        ("a warning that clears in 0.2s", [warn.format(s="10.100000"), clear.format(s="10.300000")], set(), 0),
        ("a warning that clears after 30s", [warn.format(s="10.100000"), clear.format(s="40.100000")], set(), 1),
        ("a warning that never clears", [warn.format(s="10.100000")], set(), 1),
        ("a warning still young", [warn.format(s="58.000000")], set(), 2),
        ("an unrelated alert", [other], set(), 1),
        ("an allowed alert that persists", [warn.format(s="10.100000")], transient, 0),
    ]
    for name, lines, allow, want in cases:
        got, report = audit(lines, allow, transient, 5, now)
        assert got == want, f"{name}: exit {got}, want {want}: {report}"
    with tempfile.NamedTemporaryFile("w+") as log:  # the command line, end to end
        log.write(warn.format(s="10.100000") + "\n" + clear.format(s="10.300000") + "\n")
        log.flush()
        assert run(["audit", log.name, "--transient", "custody_rpc_provider_failed"]) == 0
    print("PASS: a quick-clearing provider warning passes; a persisting or unresolved one fails")


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
        status, report = audit(log, set(args.allow.split("|")) - {""}, set(args.transient.split("|")) - {""},
                               args.window, datetime.now(timezone.utc))
    print("\n".join(report))
    return status


if __name__ == "__main__":
    sys.exit(run(sys.argv[1:]))
