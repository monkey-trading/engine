#!/usr/bin/env bash
# Dev scenario for the House and MONKEY pages' data, on a node with config/dev.toml's mocks (static
# mock mids), a synthetic signed access source, and `api.stats_interval = "1s"`:
#   1. three accounts fund and declare, then trade both ways in two markets (BTC-USD and ETH-USD): a
#      long and a short in each, and one account's round trip in BTC, which pays the spread twice (20 bps here, set by the config);
#   2. GET /v1/house carries the open interest, open margin, positions, and each market's long,
#      short and net those positions give, and GET /v1/house/positions holds the same notional in
#      its close-out and profit buckets and names no account;
#   3. the operator records an emission schedule and a staking config, and one ten-minute MONKEY period
#      closes: GET /v1/monkey/activity lists the losing account's allocation, GET /v1/monkey the
#      totals, and GET /v1/stats/series the NAV, cash, long, short, volume and MONKEY-per-loss buckets;
#   4. `engine verify-response` accepts every one of those answers;
#   5. the node restarts on the same log, and the series, the activity and where each begins match.
# The period is ten minutes of the engine's schedule, so the scenario runs about twelve.
#
# Needs python3, curl, and jq, and the public Roughtime servers config/dev.toml names. API_PORT,
# METRICS_PORT and ACCESS_SOURCE_PORT override the free ports found at start; SCENARIO_ARTIFACTS
# keeps the node log. Uses a private temporary directory.
set -Eeuo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
work="$(mktemp -d)"
free_port() { python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])'; }
api_port="${API_PORT:-$(free_port)}"
metrics_port="${METRICS_PORT:-$(free_port)}"
export ACCESS_SOURCE_PORT="${ACCESS_SOURCE_PORT:-$(free_port)}"
api="https://127.0.0.1:$api_port"
vault="0x0000000000000000000000000000000000000000"
operator_key="4242424242424242424242424242424242424242424242424242424242424242"
# Anvil's published accounts 1 to 3; the node's mock TEE accepts published keys.
keys=(59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d
  5de4111afa1a4b94908f83103eb1f1706367c2e68ca870fc3fb9a804cdab365a
  7c852118294e51e653712a81e05800f419141751be58f605c371e15141b007a6)
accounts=(0x70997970C51812dc3A010C7d01b50e0d17dc79C8 0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC
  0x90F79bf6EB2c4f870365E785982E1f101E93b906)
node_pid=""
access_fixture_pid=""

cleanup() {
  for pid in "$node_pid" "$access_fixture_pid"; do
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
expect() { [[ "$2" == "$3" ]] || fail "$1: expected '$3', got '$2'"; }
# Whether |$1 - $2| <= $3, numbers in decimal.
near() { python3 -c 'import sys; a, b, t = map(float, sys.argv[1:]); sys.exit(0 if abs(a - b) <= t else 1)' "$1" "$2" "$3"; }
expect_near() { near "$2" "$3" "$4" || fail "$1: expected $3 within $4, got $2"; }
calc() { python3 -c "print($1)"; }

step "build the node and the scenario's tools"
cargo build -q --locked -p engine-node --manifest-path "$root/Cargo.toml"
source "$root/scripts/dev-access-fixture.sh"
build_access_tools
engine="$(cargo metadata --format-version 1 --no-deps --manifest-path "$root/Cargo.toml" | jq -r .target_directory)/debug/engine"

step "start the access source, which clears every client"
start_access_fixture

step "write the node config: a one-second account scan, an access source, the dev mocks"
sed -e "s|127.0.0.1:8080|127.0.0.1:$api_port|" \
  -e "s|127.0.0.1:9464|127.0.0.1:$metrics_port|" \
  -e "s|data/dev|$work|g" \
  -e 's|^stats_interval = .*|stats_interval = "1s"|' \
  -e 's|^per_account = .*|per_account = { per_second = 100, burst = 200 }|' \
  -e 's|^spread_tiers = \[{ up_to = "2000000", bps = 0 }, { up_to = "10000000", bps = 3 }, { bps = 8 }\]|spread_tiers = [{ bps = 20 }]|' \
  "$root/config/dev.toml" > "$work/node.toml"
cat >> "$work/node.toml" <<CONFIG

[access]
source = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"
endpoint = "http://127.0.0.1:$ACCESS_SOURCE_PORT"
timeout = "2s"
max_age = "30s"
max_response_bytes = 4096
max_in_flight = 8
CONFIG
for edit in ":$api_port\"" 'stats_interval = "1s"' "$work/log\"" 'spread_tiers = [{ bps = 20 }]'; do
  grep -qF "$edit" "$work/node.toml" || fail "config edit missing: $edit"
done

max_drift="$(awk -F'"' '/^max_drift = / { print $2 }' "$root/config/dev.toml")"
[[ "$max_drift" =~ ^[0-9]+s$ ]] || fail "expected time.max_drift in whole seconds, got '$max_drift'"
sample_within="$(( ${max_drift%s} + 15 ))"

# Runs the node on the config and waits until it sequences and every mock market is priced.
start_node() {
  : > "$work/node.log"
  "$engine" run --config "$work/node.toml" >> "$work/node.log" 2>&1 &
  node_pid=$!
  for _ in $(seq 1 $(( sample_within * 5 ))); do
    grep -q 'first trusted time sample: sequencing started' "$work/node.log" && break
    kill -0 "$node_pid" 2>/dev/null || fail "the node exited"
    sleep 0.2
  done
  grep -q 'first trusted time sample: sequencing started' "$work/node.log" || fail "the node never started sequencing"
  # The mock feeds price the crypto markets whenever they run; the others wait for their hours.
  until [[ "$(curl -sk "$api/v1/markets" 2>/dev/null | jq '[.[]? | select((.id == 1 or .id == 2) and .mark != null)] | length' 2>/dev/null)" == 2 ]]; do
    kill -0 "$node_pid" 2>/dev/null || fail "the node exited"
    sleep 0.3
  done
}
start_node

signer="$(curl -sfk "$api/v1/attestation" | jq -r .enclave_address)"
pin="$(curl -sfk "$api/v1/attestation" | jq -r '.tls_public_key_sha256 | ltrimstr("0x")')"
[[ "$signer" =~ ^0x[0-9a-f]{40}$ && "$pin" =~ ^[0-9a-f]{64}$ ]] || fail "no signer or pin in the attestation"
answer=(--signer "$signer" --chain-id 31337 --vault "$vault")

# GET $1 with a fresh challenge, check `engine verify-response` accepts the answer as received, and
# print its body.
get() {
  local challenge
  challenge="$(od -An -N32 -tx1 /dev/urandom | tr -d ' \n')"
  curl -sfk -D "$work/last.headers" -o "$work/last.body" -H "X-Engine-Challenge: $challenge" "$api$1" \
    || fail "GET $1 failed"
  "$engine" verify-response "${answer[@]}" --challenge "$challenge" --path "$1" \
    --headers "$work/last.headers" --body "$work/last.body" > /dev/null || fail "the answer to GET $1 did not verify"
  cat "$work/last.body"
}
mark() { curl -sfk "$api/v1/markets" | jq -er ".[] | select(.id == $1) | .mark"; }
# Signs `$3`, the unsigned body of request kind `$2`, as account index `$1`.
sign() {
  local nonce expires_at
  nonce="$(curl -sfk "$api/v1/accounts/${accounts[$1]}/nonce" | jq -er .next_nonce)"
  expires_at="$(python3 -c 'import time; print(int(time.time() * 1000) + 60000)')"
  SIGNER_KEY="${keys[$1]}" "$access_signer" 31337 "$vault" "$nonce" "$expires_at" "$2" "$3"
}
post() { curl -sfk --max-time 10 "$api$1" -H 'content-type: application/json' -d "$2"; }
# The mock feeds drift a little between reads, so figures taken from a scan compare to the marks at
# a relative tolerance, and a bucket's figures to the scan's own mark.
rel() { calc "0.003 * $1"; }
# Orders `$4` of market `$3` on side `$2` for account index `$1`, and checks it filled.
trade() {
  local body events
  body="{\"account\":\"${accounts[$1]}\",\"client_id\":$RANDOM,\"market\":$3,\"side\":\"$2\",\"qty\":\"$4\",\"kind\":{\"kind\":\"market\",\"quoted\":\"$(mark "$3")\"}}"
  events="$(post /v1/orders "$(sign "$1" order "$body")")"
  jq -e 'any(.[]; .type == "fill")' <<< "$events" > /dev/null || fail "account $1 $2 $4 of market $3 did not fill: $events"
}

step "fund three accounts and record their eligibility declarations"
deposits=(200000 200000 200000)
for i in 0 1 2; do
  post /v1/dev/deposit "{\"account\":\"${accounts[$i]}\",\"amount\":\"${deposits[$i]}\"}" > /dev/null
  declaration="{\"account\":\"${accounts[$i]}\",\"class\":\"professional\"}"
  events="$(post /v1/eligibility "$(sign "$i" eligibility "$declaration")")"
  jq -e 'any(.[]; .type == "eligibility_attested")' <<< "$events" > /dev/null || fail "no declaration for account $i: $events"
done
for i in 0 1 2; do
  for _ in $(seq 1 100); do
    [[ "$(curl -sk "$api/v1/accounts/${accounts[$i]}" | jq -r '.balance // "0" | tonumber | floor')" -ge 100000 ]] && break
    sleep 0.2
  done
done

step "the operator records the emission schedule and the staking config"
start_at="$(python3 -c 'import datetime; print((datetime.datetime.now(datetime.timezone.utc) + datetime.timedelta(seconds=8)).strftime("%Y-%m-%dT%H:%M:%SZ"))')"
operate() { OPERATOR_KEY="$operator_key" "$engine" admin --config "$work/node.toml" --node "$api" --pin "$pin" "$@" > /dev/null; }
operate init-emissions --start "$start_at" --period-budget 1000000000000000000000
operate init-staking --start "$start_at" --liquidity-floor 1000 --epoch-ms 60000 --cooldown-ms 259200000

step "trade both ways: BTC long 2 and short 1, ETH long 10 and short 5, and a BTC round trip"
trade 0 buy 1 2
trade 1 sell 1 1
trade 2 buy 2 10
trade 1 sell 2 5
sleep 9
# BTC's spread is 20 bps in this config, so the round trip loses far more than the mock index moves.
trade 2 buy 1 1
trade 2 sell 1 1

btc="$(mark 1)"; eth="$(mark 2)"
house="$work/house.json"
for _ in $(seq 1 60); do
  get /v1/house > "$house"
  [[ "$(jq -r '.open_positions // 0' "$house")" == 4 ]] && break
  sleep 1
done

step "GET /v1/house: open interest, margin, positions, and each market's long, short and net"
expect "open positions" "$(jq -r .open_positions "$house")" 4
expect "snapshot seq present" "$(jq -r '.seq != null' "$house")" true
long_btc="$(calc "2 * $btc")"; short_btc="$btc"
long_eth="$(calc "10 * $eth")"; short_eth="$(calc "5 * $eth")"
market() { jq -r ".markets[] | select(.market == $1) | .$2" "$house"; }
expect_near "BTC long" "$(market 1 long)" "$long_btc" "$(rel "$long_btc")"
expect_near "BTC short" "$(market 1 short)" "$short_btc" "$(rel "$short_btc")"
expect_near "BTC net" "$(market 1 net)" "$(calc "$long_btc - $short_btc")" "$(rel "$long_btc")"
expect_near "ETH long" "$(market 2 long)" "$long_eth" "$(rel "$long_eth")"
expect_near "ETH short" "$(market 2 short)" "$short_eth" "$(rel "$short_eth")"
expect_near "ETH net" "$(market 2 net)" "$(calc "$long_eth - $short_eth")" "$(rel "$long_eth")"
total="$(calc "$long_btc + $short_btc + $long_eth + $short_eth")"
expect_near "open interest" "$(jq -r .open_interest "$house")" "$total" "$(rel "$total")"
# Margin is 10% of each position's notional in these markets (the first million), rounded up.
expect_near "open margin" "$(jq -r .open_margin "$house")" "$(calc "$total * 0.1")" "$(rel "$total")"
jq -e '.cash and .nav and .gross_exposure and (.unrealised | tonumber | type == "number") and (.volume | tonumber) > 0' "$house" > /dev/null \
  || fail "the existing fields or the new volume are missing: $(cat "$house")"

step "GET /v1/house/positions: the same notional in buckets, no account named"
positions="$(get "/v1/house/positions?market=1")"
expect "positions market" "$(jq -r .market <<< "$positions")" 1
scan_mark="$(jq -r .mark <<< "$positions")"
expect_near "positions long" "$(jq -r .long.usd <<< "$positions")" "$(calc "2 * $scan_mark")" 1
expect_near "positions short" "$(jq -r .short.usd <<< "$positions")" "$scan_mark" 1
expect "positions long count" "$(jq -r .long.count <<< "$positions")" 1
bucketed="$(jq -r '[.due_now, .beyond_range] + .closeout | map(.long_usd, .short_usd | tonumber) | add' <<< "$positions")"
expect_near "close-out buckets hold the market's notional" "$bucketed" "$(calc "3 * $scan_mark")" 2
profit="$(jq -r '.profit | map(.long_usd, .short_usd | tonumber) | add' <<< "$positions")"
expect_near "profit buckets hold the market's notional" "$profit" "$(calc "3 * $scan_mark")" 2
if grep -qiE '0x[0-9a-f]{40}' <<< "$positions"; then fail "the positions answer names an address: $positions"; fi
expect "an unlisted market" "$(curl -sk -o /dev/null -w '%{http_code}' "$api/v1/house/positions?market=777")" 400

step "wait for the first MONKEY period to close (about ten minutes)"
for _ in $(seq 1 780); do
  [[ "$(get "/v1/monkey/activity?limit=50" | jq '[.entries[] | select(.type == "loss_close")] | length')" -ge 1 ]] && break
  sleep 1
done
activity="$(get "/v1/monkey/activity?limit=50")"
expect "allocations" "$(jq '[.entries[] | select(.type == "loss_close")] | length' <<< "$activity")" 1
expect "the loser" "$(jq -r '.entries[0].account | ascii_downcase' <<< "$activity")" "$(tr 'A-Z' 'a-z' <<< "${accounts[2]}")"
amount="$(jq -r '.entries[0].amount' <<< "$activity")"
[[ "$amount" =~ ^[1-9][0-9]*$ ]] || fail "no MONKEY allocated: $activity"
expect "activity begins at genesis" "$(jq -r .from_genesis <<< "$activity")" true

step "GET /v1/monkey: the schedule, the totals, the distribution state"
monkey="$(get /v1/monkey)"
expect "allocated" "$(jq -r .emissions.allocated <<< "$monkey")" "$amount"
expect "period ms" "$(jq -r .emissions.period_ms <<< "$monkey")" 600000
expect "halving periods" "$(jq -r .emissions.halving_periods <<< "$monkey")" 26280
expect "period budget" "$(jq -r .emissions.period_budget <<< "$monkey")" 1000000000000000000000
expect "the budget is spent or cancelled" "$(jq -r '((.emissions.allocated | tonumber) + (.emissions.cancelled | tonumber)) >= 1e21' <<< "$monkey")" true
expect "stakers" "$(jq -r .staking.stakers <<< "$monkey")" 0
expect "epoch ms" "$(jq -r .distribution.epoch_ms <<< "$monkey")" 60000
expect "floor" "$(jq -r '.distribution.liquidity_floor | tonumber' <<< "$monkey")" 1000
jq -e '.distribution.next_close_ms > .time_ms and .distribution.last_closed_epoch != null' <<< "$monkey" > /dev/null \
  || fail "the distribution state is incomplete: $monkey"

step "GET /v1/stats/series: the buckets the trades left"
series() { get "/v1/stats/series?$1"; }
btc_volume="$(series "metric=volume&market=1&resolution=1m" | jq '[.entries[].value | tonumber] | add')"
expect_near "BTC volume" "$btc_volume" "$(calc "5 * $btc")" "$(calc "0.02 * 5 * $btc")"
all_volume="$(series "metric=volume&resolution=1m" | jq '[.entries[].value | tonumber] | add')"
expect_near "total volume" "$all_volume" "$(calc "5 * $btc + 15 * $eth")" "$(calc "0.02 * (5 * $btc + 15 * $eth)")"
long_series="$(series "metric=long&market=1&resolution=1m")"
expect_near "BTC long series" "$(jq -r '.entries[-1].close' <<< "$long_series")" "$long_btc" "$(rel "$long_btc")"
expect "level kind" "$(jq -r .kind <<< "$long_series")" level
expect "series begins at genesis" "$(jq -r '.from_genesis and .begins_ms > 0' <<< "$long_series")" true
for metric in nav cash withdrawal_queue; do
  [[ "$(series "metric=$metric&resolution=1m" | jq '.entries | length')" -ge 1 ]] || fail "no $metric buckets"
done
per_loss="$(series "metric=monkey_per_loss&resolution=1h")"
[[ "$(jq -r '.entries[0].value | tonumber' <<< "$per_loss")" != 0 ]] || fail "no MONKEY per loss: $per_loss"
expect "per-loss unit" "$(jq -r .unit <<< "$per_loss")" monkey_base_units_per_usd
expect "long without a market" "$(curl -sk -o /dev/null -w '%{http_code}' "$api/v1/stats/series?metric=long&resolution=1m")" 400

step "restart the node on the same log and compare the series, the activity and where they begin"
# Whole minutes at least two minutes old are final: no later command lands in them.
before_ms="$(python3 -c 'import time; print((int(time.time() * 1000) - 120000) // 60000 * 60000)')"
compare=(
  "metric=nav&resolution=1m&to_ms=$before_ms" "metric=cash&resolution=1m&to_ms=$before_ms"
  "metric=volume&market=1&resolution=1m&to_ms=$before_ms" "metric=volume&resolution=1m&to_ms=$before_ms"
  "metric=long&market=1&resolution=1m&to_ms=$before_ms" "metric=short&market=2&resolution=1m&to_ms=$before_ms"
  "metric=withdrawal_queue&resolution=1m&to_ms=$before_ms" "metric=monkey_per_loss&resolution=1h"
)
for i in "${!compare[@]}"; do series "${compare[$i]}" > "$work/before.$i"; done
get "/v1/monkey/activity?limit=50" > "$work/before.activity"
kill "$node_pid"; wait "$node_pid" 2>/dev/null || true; node_pid=""
start_node
for i in "${!compare[@]}"; do
  series "${compare[$i]}" > "$work/after.$i"
  [[ "$(jq '.entries | length' "$work/before.$i")" -ge 1 ]] || fail "nothing to compare for ${compare[$i]}"
  jq -S . "$work/before.$i" > "$work/before.$i.sorted"
  jq -S . "$work/after.$i" > "$work/after.$i.sorted"
  diff "$work/before.$i.sorted" "$work/after.$i.sorted" > /dev/null \
    || fail "the series ${compare[$i]} changed across the restart: $(diff "$work/before.$i.sorted" "$work/after.$i.sorted" | head -n 10)"
done
get "/v1/monkey/activity?limit=50" > "$work/after.activity"
diff <(jq -S .entries "$work/before.activity") <(jq -S .entries "$work/after.activity") > /dev/null \
  || fail "the activity changed across the restart"
expect "begin after the restart" "$(jq -r .begins_ms "$work/after.0")" "$(jq -r .begins_ms "$work/before.0")"

echo "PASS: the dashboard routes answer with the figures the accounts placed, verify, and survive a restart"
