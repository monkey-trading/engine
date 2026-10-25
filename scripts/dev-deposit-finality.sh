#!/usr/bin/env bash
# Isolated Anvil fixture and a real RPC-quorum/watcher/sequencer approval-eligibility scenario.
set -euo pipefail
export DEPOSIT_FINALITY_SCENARIO=1
exec "$(dirname "$0")/dev-vault-deposits.sh"
