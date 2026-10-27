#!/usr/bin/env bash
# Runs the API and direct-vault access scenario, including the ordinary finality/restart checks.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
ACCESS_ENFORCEMENT_SCENARIO=1 "$root/scripts/dev-vault-deposits.sh"
