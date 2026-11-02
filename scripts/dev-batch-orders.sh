#!/usr/bin/env bash
# Dev scenario for batch orders (V44): a dev node with config/dev.toml's mocks and a synthetic signed
# access source, on private temporary data. One account holds resting orders in two markets and a
# position in each, and then, with curl:
#   1. cancel-all cancels every resting order in one request: one `cancel_all` command in the log,
#      one nonce, one order_cancelled per order, and a second cancel-all with nothing resting does
#      nothing;
#   2. a close-all whose ETH entry quotes a price the market has moved past closes BTC, names the
#      slipped entry and the flat one by their list index, and leaves ETH open;
#   3. a close-all for ETH then leaves the account with no orders and no positions;
#   4. a market named twice is refused whole, each request used one nonce, and the public log replays
#      to its roots.
#
# Needs python3, curl, jq, and the public Roughtime servers config/dev.toml names. API_PORT,
# METRICS_PORT, and ACCESS_SOURCE_PORT override 18610, 19910, and 18860; SCENARIO_ARTIFACTS keeps
# the logs.
set -Eeuo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
work="$(mktemp -d)"
api_port="${API_PORT:-18610}"
metrics_port="${METRICS_PORT:-19910}"
source_port="${ACCESS_SOURCE_PORT:-18860}"
vault="0x0000000000000000000000000000000000000000"
# Anvil's published account 1; the node's mock TEE accepts published keys.
trader_key="59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d"
trader="0x70997970C51812dc3A010C7d01b50e0d17dc79C8"
node_pid=""
source_pid=""

cleanup() {
  for pid in "$node_pid" "$source_pid"; do
    if [[ -n "$pid" ]]; then kill "$pid" 2>/dev/null || true; wait "$pid" 2>/dev/null || true; fi
  done
  if [[ -n "${SCENARIO_ARTIFACTS:-}" ]]; then
    mkdir -p "$SCENARIO_ARTIFACTS"
    cp "$work"/*.log "$work"/node.toml "$SCENARIO_ARTIFACTS/" 2>/dev/null || true
  fi
  rm -rf "$work"
}
trap cleanup EXIT
step() { echo "==> $*"; }
fail() { echo "FAIL: $*" >&2; tail -n 40 "$work"/node.log >&2 || true; exit 1; }
trap 'fail "the command at line $LINENO failed"' ERR

for port in "$api_port" "$metrics_port" "$source_port"; do
  if (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null; then fail "port $port is in use"; fi
done

step "build the node and the scenario's tools"
cargo build -q --locked -p engine-node --manifest-path "$root/Cargo.toml"
source "$root/scripts/dev-access-fixture.sh"
build_access_tools
engine="$(cargo metadata --format-version 1 --no-deps --manifest-path "$root/Cargo.toml" | jq -r .target_directory)/debug/engine"

step "start the access source and the node, on private data and ports"
printf '{"mode":"clear","jurisdictions":{},"sanctioned":[]}\n' > "$work/access-fixture.json"
"$access_target/debug/examples/access_source" "127.0.0.1:$source_port" 31337 "$vault" "$work/access-fixture.json" \
  > "$work/access-source.log" 2>&1 &
source_pid=$!
sed -e "s|127.0.0.1:8080|127.0.0.1:$api_port|" \
  -e "s|127.0.0.1:9464|127.0.0.1:$metrics_port|" \
  -e "s|data/dev|$work|g" \
  "$root/config/dev.toml" > "$work/node.toml"
cat >> "$work/node.toml" <<CONFIG

[access]
source = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"
endpoint = "http://127.0.0.1:$source_port"
timeout = "2s"
max_age = "30s"
max_response_bytes = 4096
max_in_flight = 8
CONFIG
for edit in "127.0.0.1:$api_port" "127.0.0.1:$metrics_port" "$work/log"; do
  grep -qF "$edit" "$work/node.toml" || fail "config edit missing: $edit"
done
max_drift="$(awk -F'"' '/^max_drift = / { print $2 }' "$root/config/dev.toml")"
[[ "$max_drift" =~ ^[0-9]+s$ ]] || fail "expected time.max_drift in whole seconds, got '$max_drift'"
sample_within="$(( ${max_drift%s} + 15 ))"
"$engine" run --config "$work/node.toml" > "$work/node.log" 2>&1 &
node_pid=$!
for _ in $(seq 1 $(( sample_within * 5 ))); do
  grep -q 'first trusted time sample: sequencing started' "$work/node.log" && break
  kill -0 "$node_pid" 2>/dev/null || fail "the node exited"
  sleep 0.2
done
grep -q 'first trusted time sample: sequencing started' "$work/node.log" || fail "the node never started sequencing"

api="https://127.0.0.1:$api_port"
expect() { [[ "$2" == "$3" ]] || fail "$1: expected '$3', got '$2'"; }
wait_for() {
  local got=""
  for _ in $(seq 1 150); do
    got="$("$1" "$2")"
    [[ "$got" == "$3" ]] && return 0
    sleep 0.2
  done
  fail "$1 $2: expected $3, got $got"
}
mark_of() { curl --max-time 5 -skf "$api/v1/markets" | jq -r --argjson id "$1" '[.[]? | select(.id == $id) | .mark | values] | first // "none"'; }
marks_set() { [[ "$(mark_of 1)" != none && "$(mark_of 2)" != none ]] && echo yes || echo no; }
wait_for marks_set "" yes
balance() { curl --max-time 5 -sk "$api/v1/accounts/$trader" | jq -r '.balance // "none" | if . == "none" then . else tonumber | floor | tostring end'; }
orders_held() { curl --max-time 5 -sk "$api/v1/accounts/$trader" | jq -r '(.orders // []) | length'; }
positions_held() { curl --max-time 5 -sk "$api/v1/accounts/$trader" | jq -r '[(.positions // {}) | to_entries[] | select((.value.qty | tonumber) != 0) | .key] | sort | join(",")'; }
next_nonce() { curl --max-time 5 -skf "$api/v1/accounts/$trader/nonce" | jq -er .next_nonce; }
signed() { access_signed_body "$trader_key" "$1" "$2"; }
# Posts `$2` to `$1` as the screened-API client and prints the answer; a refusal fails with its body.
post() {
  local out code
  out="$(curl --max-time 10 -sk -w '\n%{http_code}' "$api$1" -H 'content-type: application/json' \
    -H 'x-forwarded-for: 203.0.113.99' -H 'forwarded: for=203.0.113.99' -d "$2")"
  code="${out##*$'\n'}"
  [[ "$code" == 200 ]] || fail "POST $1 answered $code: ${out%$'\n'*}"
  printf '%s\n' "${out%$'\n'*}"
}
# The public log's commands of one type, by their `type`.
logged() { jq -s --arg type "$1" '[.[] | select(.entry == "command") | .cmd | select(.type == $type)] | length' "$work"/log/[0-9]*.jsonl; }

step "fund and declare the account: the deposit waits for the signed declaration, then credits"
events="$(curl --max-time 10 -skf "$api/v1/dev/deposit" -H 'content-type: application/json' \
  -d "{\"account\":\"$trader\",\"amount\":\"100000\"}")"
jq -e 'any(.[]; .type == "deposit_pending")' <<< "$events" > /dev/null || fail "the deposit did not wait: $events"
body="$(signed eligibility "{\"account\":\"$trader\",\"class\":\"professional\"}")"
events="$(post /v1/eligibility "$body")"
jq -e 'any(.[]; .type == "eligibility_attested")' <<< "$events" > /dev/null || fail "the declaration was not recorded: $events"
wait_for balance "" 100000

step "open a BTC long and an ETH short, and rest two orders in each market"
btc="$(mark_of 1)"; eth="$(mark_of 2)"
# Posts a signed order of `$3` in market `$1` on `$2`, and prints the events; `$4` is its kind.
place() {
  local body kind="$4"
  body="{\"account\":\"$trader\",\"client_id\":$RANDOM,\"market\":$1,\"side\":\"$2\",\"qty\":\"$3\",\"kind\":$kind}"
  body="$(signed order "$body")"
  post /v1/orders "$body"
}
btc_market="{\"kind\":\"market\",\"quoted\":\"$btc\"}"
eth_market="{\"kind\":\"market\",\"quoted\":\"$eth\"}"
events="$(place 1 buy 0.05 "$btc_market")"
jq -e 'any(.[]; .type == "fill")' <<< "$events" > /dev/null || fail "the BTC long did not fill: $events"
events="$(place 2 sell 1 "$eth_market")"
jq -e 'any(.[]; .type == "fill")' <<< "$events" > /dev/null || fail "the ETH short did not fill: $events"
half_btc="$(python3 -c 'import sys; print(int(float(sys.argv[1]) / 2))' "$btc")"
half_eth="$(python3 -c 'import sys; print(int(float(sys.argv[1]) / 2))' "$eth")"
for price in "$half_btc" "$((half_btc - 1000))"; do
  limit="{\"kind\":\"limit\",\"price\":\"$price\"}"
  events="$(place 1 buy 0.05 "$limit")"
  jq -e 'length == 1 and .[0].type == "order_accepted"' <<< "$events" > /dev/null || fail "a BTC order did not rest: $events"
done
for price in "$half_eth" "$((half_eth - 100))"; do
  limit="{\"kind\":\"limit\",\"price\":\"$price\"}"
  events="$(place 2 buy 1 "$limit")"
  jq -e 'length == 1 and .[0].type == "order_accepted"' <<< "$events" > /dev/null || fail "an ETH order did not rest: $events"
done
expect "resting orders" "$(orders_held)" 4
expect "open positions" "$(positions_held)" "1,2"

step "cancel all: one request, one cancel event per order, one log entry, one nonce"
nonce="$(next_nonce)"
body="$(signed cancel_all "{\"account\":\"$trader\"}")"
events="$(post /v1/orders/cancel-all "$body")"
jq -e 'length == 4 and all(.[]; .type == "order_cancelled" and .reason == "client")' <<< "$events" > /dev/null \
  || fail "cancel-all did not cancel the four orders: $events"
expect "resting orders after cancel-all" "$(orders_held)" 0
expect "cancel_all log entries" "$(logged cancel_all)" 1
expect "nonce used by cancel-all" "$(( $(next_nonce) - nonce ))" 1
body="$(signed cancel_all "{\"account\":\"$trader\"}")"
events="$(post /v1/orders/cancel-all "$body")"
expect "events of a cancel-all with nothing resting" "$(jq -c . <<< "$events")" "[]"
expect "cancel_all log entries" "$(logged cancel_all)" 2

step "close all: a slipped ETH entry and a flat EUR-USD entry are named by index; BTC closes"
btc="$(mark_of 1)"; eth="$(mark_of 2)"
good_btc="$(python3 -c 'import sys; print(int(float(sys.argv[1]) * 0.99))' "$btc")"
bad_eth="$(python3 -c 'import sys; print(int(float(sys.argv[1]) * 0.9))' "$eth")"
good_eth="$(python3 -c 'import sys; print(int(float(sys.argv[1]) * 1.01))' "$eth")"
nonce="$(next_nonce)"
closes="[{\"market\":2,\"quoted\":\"$bad_eth\"},{\"market\":1,\"quoted\":\"$good_btc\"},{\"market\":10,\"quoted\":\"1\"}]"
body="$(signed close_all "{\"account\":\"$trader\",\"closes\":$closes}")"
events="$(post /v1/positions/close-all "$body")"
jq -e '
  any(.[]; .type == "order_rejected" and .client_id == 0 and .reason.kind == "slippage") and
  any(.[]; .type == "fill" and .market == 1 and (.position | tonumber) == 0) and
  any(.[]; .type == "order_rejected" and .client_id == 2) and
  ([.[] | select(.type == "fill")] | length) == 1' <<< "$events" > /dev/null || fail "close-all outcome: $events"
expect "positions after the first close-all" "$(positions_held)" 2
expect "nonce used by the close-all" "$(( $(next_nonce) - nonce ))" 1
expect "close-all log entries" "$(logged access_close_all)" 1

step "close all again for the remaining ETH short: no orders or positions are left"
body="$(signed close_all "{\"account\":\"$trader\",\"closes\":[{\"market\":2,\"quoted\":\"$good_eth\"}]}")"
events="$(post /v1/positions/close-all "$body")"
jq -e 'any(.[]; .type == "fill" and .market == 2 and (.position | tonumber) == 0)' <<< "$events" > /dev/null \
  || fail "the ETH close did not fill: $events"
expect "resting orders" "$(orders_held)" 0
expect "open positions" "$(positions_held)" ""
expect "close-all log entries" "$(logged access_close_all)" 2

step "a market named twice is refused whole, using its nonce"
nonce="$(next_nonce)"
body="$(signed close_all "{\"account\":\"$trader\",\"closes\":[{\"market\":1,\"quoted\":\"$btc\"},{\"market\":1,\"quoted\":\"$btc\"}]}")"
events="$(post /v1/positions/close-all "$body")"
jq -e 'length == 1 and .[0].type == "order_rejected" and .[0].reason.kind == "invalid_order"' <<< "$events" > /dev/null \
  || fail "a repeated market was not refused: $events"
expect "nonce used by the refused request" "$(( $(next_nonce) - nonce ))" 1

step "stop the node and replay its public log to the signed roots"
kill "$node_pid"; wait "$node_pid" 2>/dev/null || true; node_pid=""
"$engine" replay --log "$work/log" > "$work/replay.log" 2>&1 || { cat "$work/replay.log" >&2; fail "the log did not replay"; }
cat "$work/replay.log"

echo "PASS: cancel-all and close-all are one signed request, one nonce, and one log entry each; the log replays"
