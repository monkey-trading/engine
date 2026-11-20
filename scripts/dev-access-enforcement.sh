#!/usr/bin/env bash
# Runs the API and direct-vault access scenario, including the ordinary finality/restart checks, and the
# node's reuse of a screening decision for an account and its address while it is fresh.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
# A six-second freshness bound, so the cases that wait for the node's kept decisions to run out wait
# seconds, not minutes.
ACCESS_MAX_AGE="${ACCESS_MAX_AGE:-6s}" ACCESS_ENFORCEMENT_SCENARIO=1 "$root/scripts/dev-vault-deposits.sh"
