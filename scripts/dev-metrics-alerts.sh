#!/usr/bin/env bash
# Dev scenario for metrics and alert routing, on a node with config/dev.toml's mocks:
#   1. start a local webhook receiver and route Critical alerts to it;
#   2. run a node with its metrics listener, alerts.no_price_after at 3 seconds, and the mock feed for
#      BTC-USD (market 1) set to stop time.max_drift + 15 seconds after boot: nothing is sequenced
#      before the first trusted time sample, and a Roughtime round waits up to time.max_drift for
#      its slowest server;
#   3. wait up to time.max_drift + 5 seconds for the first trusted time sample, then for both
#      markets to be priced, then for BTC-USD to go without a usable price, then stop the node.
# Passes when the metrics show BTC-USD open but unpriced, with every venue's BTC-USD quote stale and
# ETH-USD still fresh; the receiver got exactly one page, a firing market_no_price for market 1; and
# the node never logged the webhook URL.
#
# Needs python3, curl, and jq. API_PORT, METRICS_PORT, and HOOK_PORT override 18080, 19464, and
# 18099.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
work="$(mktemp -d)"
api_port="${API_PORT:-18080}"
metrics_port="${METRICS_PORT:-19464}"
hook_port="${HOOK_PORT:-18099}"
secret="dev-secret-token"
node_pid=""
hook_pid=""

cleanup() {
  [[ -n "$node_pid" ]] && kill "$node_pid" 2>/dev/null || true
  if [[ -n "$hook_pid" ]]; then
    kill "$hook_pid" 2>/dev/null || true
    wait "$hook_pid" 2>/dev/null || true
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

# The value of one sample, by its full name and labels, e.g. engine_market_priced{market="1"}.
metric() {
  curl -sf "http://127.0.0.1:$metrics_port/metrics" | awk -v name="$1" '$1 == name { print $2 }' || true
}
priced() { metric "engine_market_priced{market=\"$1\"}"; }
pages() { if [[ -f "$work/pages.jsonl" ]]; then wc -l < "$work/pages.jsonl" | tr -d ' '; else echo 0; fi; }

# Polls `$1 $2` until it prints `$3`, for up to 30 seconds.
wait_for() {
  local got=""
  for _ in $(seq 1 150); do
    got="$("$1" "$2")"
    [[ "$got" == "$3" ]] && return 0
    sleep 0.2
  done
  fail "$1 $2: expected $3, got $got"
}

# Seconds since the newest quote from `venue` for `market`, by the node's metrics.
quote_age() {
  local ts
  ts="$(metric "engine_quote_timestamp_seconds{market=\"$1\",venue=\"$2\"}")"
  [[ -n "$ts" ]] || fail "no quote timestamp for market $1 from $2"
  awk -v ts="$ts" -v now="$(date +%s)" 'BEGIN { printf "%d", now - ts }'
}

step "build the node"
(cd "$root" && cargo build -p engine-node --locked --quiet)
engine="$(cargo metadata --format-version 1 --no-deps --manifest-path "$root/Cargo.toml" | jq -r .target_directory)/debug/engine"

step "start a webhook receiver on 127.0.0.1:$hook_port"
python3 - "$hook_port" "$work/pages.jsonl" <<'PY' &
import http.server
import sys

port, out = int(sys.argv[1]), sys.argv[2]


class Hook(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        body = self.rfile.read(int(self.headers["Content-Length"]))
        with open(out, "ab") as pages:
            pages.write(body + b"\n")
        self.send_response(200)
        self.end_headers()

    def log_message(self, *args):
        pass


http.server.HTTPServer(("127.0.0.1", port), Hook).serve_forever()
PY
hook_pid=$!

# The first sample may wait max_drift for a slow server, and a lookup on top: allow 5 seconds past
# it, and keep BTC-USD quoting 10 seconds beyond that, since quotes before the sample are refused.
max_drift="$(awk -F'"' '/^max_drift = / { print $2 }' "$root/config/dev.toml")"
[[ "$max_drift" =~ ^[0-9]+s$ ]] || fail "expected time.max_drift in whole seconds, got '$max_drift'"
sample_within="$(( ${max_drift%s} + 5 ))"
stop_after="$(( sample_within + 10 ))"

step "write the node config"
sed -e "s|^listen = \"127.0.0.1:8080\"|listen = \"127.0.0.1:$api_port\"|" \
  -e "s|^listen = \"127.0.0.1:9464\"|listen = \"127.0.0.1:$metrics_port\"|" \
  -e "s|^log_dir = .*|log_dir = \"$work/log\"|" \
  -e "s|^dir = \"data/dev/history\"|dir = \"$work/history\"|" \
  -e "s|dir = \"data/dev/alerts\"|dir = \"$work/alerts\"|" \
  -e 's|^no_price_after = .*|no_price_after = "3s"|' \
  -e "s|{ market = 1, mid = \"100000\" }|{ market = 1, mid = \"100000\", stop_after = \"${stop_after}s\" }|" \
  "$root/config/dev.toml" > "$work/node.toml"
printf '\n[alerts.critical]\nurl_env = "ENGINE_ALERTS_CRITICAL_URL"\n' >> "$work/node.toml"
for edit in ":$api_port\"" ":$metrics_port\"" "$work/log\"" "$work/history\"" 'no_price_after = "3s"' "stop_after = \"${stop_after}s\""; do
  grep -qF "$edit" "$work/node.toml" || fail "config edit missing: $edit"
done

step "run the node; BTC-USD quotes stop ${stop_after}s after boot"
ENGINE_ALERTS_CRITICAL_URL="http://127.0.0.1:$hook_port/hooks/$secret" \
  "$engine" run --config "$work/node.toml" > "$work/node.log" 2>&1 &
node_pid=$!
started="$(date +%s)"
until grep -q 'first trusted time sample: sequencing started' "$work/node.log"; do
  (( $(date +%s) - started <= sample_within )) || fail "no trusted time sample within ${sample_within}s of boot"
  kill -0 "$node_pid" 2>/dev/null || fail "the node exited"
  sleep 0.2
done
echo "first trusted time sample $(( $(date +%s) - started ))s after boot"
wait_for priced 1 1
wait_for priced 2 1
[[ "$(pages)" == 0 ]] || fail "a page arrived while every market was priced"

step "wait for BTC-USD to go unpriced and page"
# Polls are bounded from the moment its quotes stop, whenever the trusted time sample came.
until_stop="$(( started + stop_after - $(date +%s) ))"
if (( until_stop > 0 )); then sleep "$until_stop"; fi
wait_for priced 1 0
wait_for pages "" 1
# Further samples see the same episode: no second page.
sleep 3
[[ "$(metric 'engine_market_open{market="1"}')" == 1 ]] || fail "BTC-USD is not open"
[[ "$(priced 2)" == 1 ]] || fail "ETH-USD lost its price"
for venue in mock-a mock-b mock-c; do
  age="$(quote_age 1 "$venue")"
  (( age >= 5 )) || fail "market 1 quote from $venue is only ${age}s old"
  age="$(quote_age 2 "$venue")"
  (( age <= 2 )) || fail "market 2 quote from $venue is ${age}s old"
done

step "stop the node"
kill -TERM "$node_pid"
wait "$node_pid" || fail "node exited with an error"
node_pid=""

[[ "$(pages)" == 1 ]] || fail "expected exactly one page, got $(pages)"
jq -e '.status == "firing" and .severity == "critical" and .alert == "market_no_price"
  and .key == "market_no_price{market=1}" and .fields == {"market": "1"}' "$work/pages.jsonl" > /dev/null \
  || fail "unexpected page: $(cat "$work/pages.jsonl")"
grep -q 'BTC-USD has had no usable price' "$work/node.log" || fail "the alert is not in the node log"
if grep -q "$secret" "$work/node.log"; then fail "the node logged the webhook URL"; fi

echo "PASS: one page for BTC-USD, and the metrics show its venues stale"
jq -c . "$work/pages.jsonl"
