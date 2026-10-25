#!/usr/bin/env bash
# Dev scenario for vault deposits, on a local Anvil chain with a node on config/anvil.toml:
#   1. start Anvil and deploy a mock USDC and the vault;
#   2. run a node with custody.provider = "evm" against them, after a first boot that never gets
#      trusted time records no custody progress before the log's first command;
#   3. deposit as a trader, for the trader from another account, and as the house, restart the
#      node mid-way (once with a forged forward host cursor), and deposit again;
#   4. reorg a deposit away before it is final, then include the same transaction again.
# Passes when every deposit is credited once, only after the configured confirmations, house NAV
# equals the house deposits, and the node's log replays.
#
# Needs anvil, forge, cast, curl, jq, and python3, and the contract dependencies
# (`forge soldeer install` in contracts/). ANVIL_PORT, API_PORT, and METRICS_PORT override 8545,
# 8080, and 9464. RPC_A_PORT, RPC_B_PORT and RPC_C_PORT override the local proxy ports
# 18545, 18546 and 18547. RPC_QUORUM_SCENARIO=1 also exercises lies, outages and recovery.
set -euo pipefail

# A closed local port stands in for the required Critical route; the quorum scenario checks
# alert emission in the node log. The alert-routing scenario tests delivery separately.
export ENGINE_ALERTS_CRITICAL_URL="http://127.0.0.1:9/alerts"

root="$(cd "$(dirname "$0")/.." && pwd)"
work="$(mktemp -d)"
anvil_port="${ANVIL_PORT:-8545}"
api_port="${API_PORT:-8080}"
metrics_port="${METRICS_PORT:-9464}"
rpc="http://127.0.0.1:$anvil_port"
# The node serves its API over TLS with a key it generates at startup, so curl skips verification.
api="https://127.0.0.1:$api_port"
# Anvil's default test mnemonic: account 0 deploys, 1 trades, 2 funds the house.
mnemonic="test test test test test test test test test test test junk"
key() { cast wallet private-key --mnemonic "$mnemonic" --mnemonic-index "$1"; }
deployer_key="$(key 0)"
trader_key="$(key 1)"
house_key="$(key 2)"
trader="$(cast wallet address --private-key "$trader_key" | tr '[:upper:]' '[:lower:]')"
usdc_units=1000000
anvil_pid=""
node_pid=""
proxy_pids=()
rpc_a_port="${RPC_A_PORT:-18545}"
rpc_b_port="${RPC_B_PORT:-18546}"
rpc_c_port="${RPC_C_PORT:-18547}"

cleanup() {
  [[ -n "$node_pid" ]] && kill "$node_pid" 2>/dev/null || true
  [[ -n "$anvil_pid" ]] && kill "$anvil_pid" 2>/dev/null || true
  for pid in "${proxy_pids[@]}"; do kill "$pid" 2>/dev/null || true; done
  if [[ -n "${SCENARIO_LOG_DIR:-}" ]]; then
    mkdir -p "$SCENARIO_LOG_DIR"
    cp "$work"/*.log "$SCENARIO_LOG_DIR/" 2>/dev/null || true
    if [[ "${DEPOSIT_FINALITY_SCENARIO:-0}" == "1" ]]; then
      cp "$work/anvil.toml" "$SCENARIO_LOG_DIR/finality-config.toml" 2>/dev/null || true
      [[ -d "$work/log" ]] && cp -R "$work/log" "$SCENARIO_LOG_DIR/finality-log" || true
    fi
  fi
  rm -rf "$work"
}
trap cleanup EXIT

step() { echo "==> $*"; }
fail() {
  echo "FAIL: $*" >&2
  [[ -f "$work/node.log" ]] && tail -n 40 "$work/node.log" >&2
  exit 1
}

# Polls `$1` (a shell function name) until it prints `$2`, for up to 20 seconds.
wait_for() {
  local got=""
  for _ in $(seq 1 100); do
    got="$("$1")"
    [[ "$got" == "$2" ]] && return 0
    sleep 0.2
  done
  fail "$1: expected $2, got $got"
}

# Waits long enough for several watcher polls, then checks `$1` still prints `$2`.
stays() {
  sleep 1.5
  local got
  got="$("$1")"
  [[ "$got" == "$2" ]] || fail "$1: expected $2 to hold, got $got"
}

trader_balance() { curl -sk "$api/v1/accounts/$trader" | jq -r '.balance // "none"'; }
fictional_balance() { curl -sk "$api/v1/accounts/0xabababababababababababababababababababab" | jq -r '.balance // "none"'; }
house_nav() { curl -sk "$api/v1/house" | jq -r '.nav'; }
rescanned_deposits() {
  jq -sr '[.[] | select(.entry == "command" and .cmd.type == "custody_finalised") | .cmd.deposit_count] | last // "none"' "$work"/log/[0-9]*.jsonl 2>/dev/null || echo none
}
progress_tip() {
  jq -sr '[.[] | select(.entry == "command" and .cmd.type == "custody_finalised") | .cmd.block] | last // "none"' "$work"/log/[0-9]*.jsonl
}
log_commands() {
  local n=0 f
  for f in "$work"/log/[0-9]*.jsonl; do
    [[ -f "$f" ]] && n=$((n + $(jq -c 'select(.entry == "command")' "$f" | wc -l)))
  done
  echo "$n"
}
mine() { cast rpc anvil_mine "$(printf '0x%x' "$1")" --rpc-url "$rpc" >/dev/null; }
send() { cast send "$@" --rpc-url "$rpc" >/dev/null; }

# Starts the node on config `$1` (default the scenario's), logging to `$2` (default node.log).
start_node() {
  curl -sk "$api/v1/health" >/dev/null 2>&1 && fail "port $api_port already serves an API"
  "$engine" run --config "${1:-$work/anvil.toml}" >>"${2:-$work/node.log}" 2>&1 &
  node_pid=$!
  for _ in $(seq 1 100); do
    kill -0 "$node_pid" 2>/dev/null || fail "node exited at startup"
    curl -skf "$api/v1/health" >/dev/null && return 0
    sleep 0.2
  done
  fail "node did not come up"
}

stop_node() {
  kill -TERM "$node_pid"
  wait "$node_pid" || fail "node did not shut down cleanly"
  node_pid=""
}

# Built first, so a slow build cannot leave the chain's head stale by the time the node starts.
step "Build the node"
cargo build -q --locked -p engine-node --manifest-path "$root/Cargo.toml"
# Wherever Cargo put it: `CARGO_TARGET_DIR` or `build.target-dir` may move the target directory.
engine="$(cargo metadata --format-version 1 --no-deps --manifest-path "$root/Cargo.toml" | jq -r .target_directory)/debug/engine"

step "Start Anvil; with one slot per epoch its safe block trails the head by one"
cast block-number --rpc-url "$rpc" >/dev/null 2>&1 && fail "port $anvil_port already serves an RPC"
anvil --port "$anvil_port" --slots-in-an-epoch 1 --silent &
anvil_pid=$!
anvil_up=""
for _ in $(seq 1 50); do
  kill -0 "$anvil_pid" 2>/dev/null || fail "anvil exited at startup"
  cast block-number --rpc-url "$rpc" >/dev/null 2>&1 && anvil_up=yes && break
  sleep 0.2
done
[[ -n "$anvil_up" ]] || fail "anvil did not come up"

step "Deploy the mock USDC and the vault"
cd "$root/contracts"
# `--constructor-args` takes every argument after it, so the contract and its arguments go last.
create() { forge create --rpc-url "$rpc" --private-key "$deployer_key" --broadcast --json "$@" | jq -r '.deployedTo'; }
usdc="$(create test/mocks/MockUSDC.sol:MockUSDC)"
# The MONKEY period budget, 1 MONKEY, is a development value, not an approved production parameter.
vault="$(create src/Vault.sol:Vault --constructor-args "$usdc" "$trader" 1000000000000000000)"
cd "$root"
grep -qi "vault = \"$vault\"" config/anvil.toml || fail "vault deployed at $vault, not the address config/anvil.toml names"
grep -qi "usdc = \"$usdc\"" config/anvil.toml || fail "USDC deployed at $usdc, not the address config/anvil.toml names"
for funder_key in "$trader_key" "$house_key"; do
  funder="$(cast wallet address --private-key "$funder_key")"
  send "$usdc" "mint(address,uint256)" "$funder" $((10000000 * usdc_units)) --private-key "$deployer_key"
  send "$usdc" "approve(address,uint256)" "$vault" $((10000000 * usdc_units)) --private-key "$funder_key"
done

step "Start three local RPC proxies"
for port in "$rpc_a_port" "$rpc_b_port" "$rpc_c_port"; do
  python3 "$root/scripts/dev-rpc-proxy.py" --port "$port" --target "$rpc" --lie-file "$work/lie-$port" >"$work/proxy-$port.log" 2>&1 &
  proxy_pids+=("$!")
  for _ in $(seq 1 100); do
    kill -0 "${proxy_pids[${#proxy_pids[@]}-1]}" 2>/dev/null || fail "proxy exited"
    cast block-number --rpc-url "http://127.0.0.1:$port" >/dev/null 2>&1 && break
    sleep 0.1
  done
done

step "A first boot without trusted time records no custody progress"
# Rewrite original config tokens once so destination ports cannot cascade into later matches.
python3 - "$root/config/anvil.toml" "$work" "$rpc_a_port" "$rpc_b_port" "$rpc_c_port" "$api_port" "$metrics_port" >"$work/anvil.toml" <<'PY_CONFIG'
import re
import sys
from pathlib import Path

source, work, rpc_a, rpc_b, rpc_c, api, metrics = sys.argv[1:]
replacements = {
    "data/anvil": work,
    "127.0.0.1:8545": "127.0.0.1:" + rpc_a,
    "127.0.0.1:8546": "127.0.0.1:" + rpc_b,
    "127.0.0.1:8547": "127.0.0.1:" + rpc_c,
    "127.0.0.1:8080": "127.0.0.1:" + api,
    "127.0.0.1:9464": "127.0.0.1:" + metrics,
}
pattern = "|".join(map(re.escape, replacements))
print(re.sub(pattern, lambda match: replacements[match[0]], Path(source).read_text()), end="")
PY_CONFIG
if [[ "${DEPOSIT_FINALITY_SCENARIO:-0}" == "1" ]]; then
  step "Prove confirmed collateral, finality-gated approval, and exact-once reversal through the real watcher/quorum/sequencer"
  CARGO_TERM_COLOR=never ENGINE_FINALITY_FIXTURE="$work/anvil.toml" ENGINE_FINALITY_RPC="$rpc" ENGINE_FINALITY_TEST_KEY="$trader_key" \
    cargo test -p engine-node --lib --locked custody::tests::anvil_finality::anvil_confirmation_finality_scenario -- --ignored --exact | tee "$work/finality-test.log"
  grep -q '^test result: ok. 1 passed; 0 failed;' "$work/finality-test.log" || fail "finality scenario did not execute exactly one successful test"
  echo "PASS: immediate trade; finalised-only approval headroom; finality release; canonical debit and Critical"
  echo "Approval eligibility was exercised through the sequencer; no physical payout or HTTP cash-out was attempted."
  exit 0
fi
# Every Roughtime server points at a closed local port, so no sample arrives and nothing is
# sequenced. The watcher may read the chain, but it cannot persist finalised progress or unlock
# principal outside the log. That boot's time alerts are expected and logged apart from node.log.
sed -E 's#^address = ".*"#address = "127.0.0.1:9"#' "$work/anvil.toml" >"$work/anvil-no-time.toml"
start_node "$work/anvil-no-time.toml" "$work/node-no-time.log"
stays log_commands 0
[[ ! -f "$work/custody-cursor.json" ]] || fail "custody persisted a host cursor before trusted sequencing"
stop_node

step "Start the node with EVM custody and replayed progress"
start_node
[[ "$(house_nav)" == "0" ]] || fail "genesis credited house capital under EVM custody"
code="$(curl -sk -o /dev/null -w '%{http_code}' -X POST "$api/v1/dev/deposit" \
  -H 'content-type: application/json' -d "{\"account\":\"$trader\",\"amount\":\"1\"}")"
[[ "$code" == "404" ]] || fail "the mock-only deposit endpoint answered $code under EVM custody"
code="$(curl -sk -o /dev/null -w '%{http_code}' -X POST "$api/v1/withdrawals" \
  -H 'content-type: application/json' -d "{\"account\":\"$trader\",\"amount\":\"1\"}")"
[[ "$code" == "501" ]] || fail "a withdrawal answered $code under EVM custody, which cannot pay it out yet"

step "A trader deposit credits only once three blocks have passed"
send "$vault" "deposit(uint256)" $((1000 * usdc_units)) --private-key "$trader_key"
stays trader_balance none
mine 2
stays trader_balance none
mine 1
wait_for trader_balance 1000

step "A house deposit adds house capital"
send "$vault" "depositToHouse(uint256)" $((5000000 * usdc_units)) --private-key "$house_key"
mine 3
wait_for house_nav 5000000

step "A deposit another account makes for the trader credits the trader"
send "$vault" "depositFor(address,uint256)" "$trader" $((50 * usdc_units)) --private-key "$house_key"
mine 3
wait_for trader_balance 1050
payer="$(cast wallet address --private-key "$house_key" | tr '[:upper:]' '[:lower:]')"
[[ "$(curl -sk "$api/v1/accounts/$payer" | jq -r '.balance // "none"')" == "none" ]] || fail "the paying account was credited"
[[ "$(house_nav)" == "5000000" ]] || fail "a deposit for the trader changed house NAV"

step "A deposit made while the node is down is credited after the restart"
stop_node
send "$vault" "deposit(uint256)" $((250 * usdc_units)) --private-key "$trader_key"
mine 3
start_node
wait_for trader_balance 1300

step "A forged forward host cursor cannot skip replayed custody progress"
stop_node
python3 - "$work/custody-cursor.json" "$vault" <<'PY_CURSOR'
import json
import sys
from pathlib import Path
Path(sys.argv[1]).write_text(json.dumps({"chain_id": 31337, "vault": sys.argv[2], "next_block": 999999,
    "next_deposit_id": 999999, "log_seq": None, "last_block_hash": None, "crediting": None}))
PY_CURSOR
start_node
mine 1
expected_tip="$(cast rpc eth_getBlockByNumber safe false --rpc-url "$rpc" | python3 -c 'import json,sys; print(int(json.load(sys.stdin)["number"], 16) - 2)')"
wait_for progress_tip "$expected_tip"
wait_for rescanned_deposits 4
stays trader_balance 1300
[[ "$(house_nav)" == "5000000" ]] || fail "house NAV changed on the rescan"

step "A deposit reorged away before it is final is never credited"
snapshot="$(cast rpc evm_snapshot --rpc-url "$rpc" | tr -d '"')"
nonce="$(cast nonce "$trader" --rpc-url "$rpc")"
raw="$(cast mktx "$vault" "deposit(uint256)" $((77 * usdc_units)) --private-key "$trader_key" --nonce "$nonce" \
  --gas-limit 200000 --rpc-url "$rpc")"
cast publish "$raw" --rpc-url "$rpc" >/dev/null
mine 1
stays trader_balance 1300
cast rpc evm_revert "$snapshot" --rpc-url "$rpc" >/dev/null
mine 5
stays trader_balance 1300

step "The same transaction included again is credited once"
cast publish "$raw" --rpc-url "$rpc" >/dev/null
mine 3
wait_for trader_balance 1377
mine 3
stays trader_balance 1377

# Ordinary deposits, restarts, rescans and reorgs must remain free of alerts in both modes.
if grep -q 'alert=' "$work/node.log"; then fail "unexpected alert before fault injection: $(grep -m 1 'alert=' "$work/node.log")"; fi
expected_deposits=5
if [[ "${RPC_QUORUM_SCENARIO:-0}" == "1" ]]; then
  step "A fictional deposit from one provider is ignored and alerted"
  touch "$work/lie-$rpc_c_port"
  mine 5
  stays trader_balance 1377
  [[ "$(fictional_balance)" == "none" ]] || fail "fictional account was credited"
  grep -q 'alert="custody_rpc_provider_disagreed".*provider="local-c".*operation="deposits"' "$work/node.log" || fail "no provider disagreement alert"
  rm "$work/lie-$rpc_c_port"
  mine 4
  sleep 1
  grep -q 'alert_resolved="custody_rpc_provider_disagreed".*provider="local-c"' "$work/node.log" || fail "disagreement did not resolve"

  step "One stopped provider leaves a quorum that credits a real deposit"
  kill "${proxy_pids[2]}"
  wait "${proxy_pids[2]}" 2>/dev/null || true
  send "$vault" "deposit(uint256)" $((10 * usdc_units)) --private-key "$trader_key"
  mine 3
  wait_for trader_balance 1387
  grep -q 'alert="custody_rpc_provider_failed".*provider="local-c"' "$work/node.log" || fail "no provider outage alert"

  step "Two stopped providers halt crediting with a Critical alert while the API keeps serving"
  kill "${proxy_pids[1]}"
  wait "${proxy_pids[1]}" 2>/dev/null || true
  send "$vault" "deposit(uint256)" $((20 * usdc_units)) --private-key "$trader_key"
  mine 3
  stays trader_balance 1387
  grep -q 'ERROR.*alert="custody_rpc_quorum_lost"' "$work/node.log" || fail "no Critical lost-quorum alert"
  curl -skf "$api/v1/markets" >/dev/null || fail "node stopped serving markets"

  step "Restoring one provider resumes crediting exactly once"
  python3 "$root/scripts/dev-rpc-proxy.py" --port "$rpc_b_port" --target "$rpc" >"$work/proxy-recovered.log" 2>&1 &
  proxy_pids+=("$!")
  wait_for trader_balance 1407
  grep -q 'alert_resolved="custody_rpc_quorum_lost"' "$work/node.log" || fail "lost quorum did not resolve"
  expected_deposits=7
fi

step "The log holds one command per deposit and replays"
stop_node
# Genesis's zero house capital credit marks genesis done; every other credit is a vault deposit.
deposits="$(jq -c 'select(.entry == "command" and .cmd.type == "custody_deposit" and .cmd.action == "credit")' "$work"/log/[0-9]*.jsonl | wc -l | tr -d ' ')"
[[ "$deposits" == "$expected_deposits" ]] || fail "expected $expected_deposits deposit commands in the log, found $deposits"
"$engine" replay --log "$work/log" | grep -q '^verified' || fail "the log does not replay"
# Restarts, the rescan, and the reorg are all expected: none of them may raise an alert.
if [[ "${RPC_QUORUM_SCENARIO:-0}" == "1" ]]; then
  unexpected="$(grep 'alert=' "$work/node.log" | grep -Ev 'alert="(custody_rpc_provider_failed|custody_rpc_provider_disagreed|custody_rpc_quorum_lost|custody_rpc_failed)"' || true)"
  [[ -z "$unexpected" ]] || fail "unexpected fault-phase alert: $unexpected"
elif grep -q 'alert=' "$work/node.log"; then
  fail "unexpected alert: $(grep -m 1 'alert=' "$work/node.log")"
fi

echo "PASS: every deposit credited once after finality; house NAV equals house deposits"
if [[ "${RPC_QUORUM_SCENARIO:-0}" == "1" ]]; then
  echo "PASS: fabricated deposit ignored; one outage credits; two halt; recovery credits once"
fi
