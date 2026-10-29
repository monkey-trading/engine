#!/usr/bin/env bash
# Dev scenario for executable quotes, on the vault deposit scenario's Anvil chain and node, with the
# trader funded and eligible:
#   1. a signed stream connection asks for the trader's quotes in market 1 and gets verified frames;
#   2. a market order sent at the streamed ask with a 5 bps bound fills at or below the bound;
#   3. once the mock feed moves the index, a new quote arrives, and a buy at the old ask with no
#      slippage allowed is declined with `slippage`;
#   4. quote messages come no closer than the configured interval, and every frame verifies with
#      `engine verify-frames`.
# Passes with `PASS: streamed quote fills within its bound; moved index declines a stale quote`.
#
# Needs what scripts/dev-vault-deposits.sh needs (anvil, forge, cast, curl, jq, python3, and the
# contract dependencies); its port variables apply.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
QUOTES_SCENARIO=1 "$root/scripts/dev-vault-deposits.sh"
