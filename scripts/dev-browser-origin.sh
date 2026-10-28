#!/usr/bin/env bash
# Dev scenario for the browser-origin allowlist, on a node with config/dev.toml's mocks and
# `api.allowed_origins = ["http://localhost:3000"]`, checked with curl:
#   1. a preflight and a GET /v1/markets from the listed origin carry that exact origin back;
#   2. the same from http://evil.test carry no CORS headers;
#   3. a stream upgrade from the evil origin gets 403, and from the listed origin 101;
#   4. /v1/admin gets no CORS headers and no successful preflight, even from the listed origin;
#   5. a request the rate limiter refuses with 429 still carries the listed origin.
#
# Needs curl and jq, and the public Roughtime servers config/dev.toml names. API_PORT and
# METRICS_PORT override 18380 and 19764; SCENARIO_ARTIFACTS keeps the node log.
set -Eeuo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
work="$(mktemp -d)"
api_port="${API_PORT:-18380}"
metrics_port="${METRICS_PORT:-19764}"
listed="http://localhost:3000"
evil="http://evil.test"
base="https://127.0.0.1:$api_port"
node_pid=""

cleanup() {
  if [[ -n "$node_pid" ]]; then kill "$node_pid" 2>/dev/null || true; wait "$node_pid" 2>/dev/null || true; fi
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

(cd "$root" && cargo build -p engine-node --locked --quiet)
engine="$(cargo metadata --format-version 1 --no-deps --manifest-path "$root/Cargo.toml" | jq -r .target_directory)/debug/engine"

max_drift="$(awk -F'"' '/^max_drift = / { print $2 }' "$root/config/dev.toml")"
[[ "$max_drift" =~ ^[0-9]+s$ ]] || fail "expected time.max_drift in whole seconds, got '$max_drift'"
sample_within="$(( ${max_drift%s} + 15 ))"

step "write the node config with one allowed origin and a small request burst"
sed -e "s|127.0.0.1:8080|127.0.0.1:$api_port|" \
  -e "s|127.0.0.1:9464|127.0.0.1:$metrics_port|" \
  -e "s|data/dev|$work|g" \
  -e 's|^per_ip = .*|per_ip = { per_second = 1, burst = 40 }|' \
  "$root/config/dev.toml" |
  awk -v origin="$listed" '{ print } /^max_subscribers = / { printf "allowed_origins = [\"%s\"]\n", origin }' \
  > "$work/node.toml"
for edit in ":$api_port\"" "burst = 40" "allowed_origins = [\"$listed\"]"; do
  grep -qF "$edit" "$work/node.toml" || fail "config edit missing: $edit"
done

"$engine" run --config "$work/node.toml" > "$work/node.log" 2>&1 &
node_pid=$!
for _ in $(seq 1 $(( sample_within * 5 ))); do
  grep -q 'first trusted time sample: sequencing started' "$work/node.log" && break
  kill -0 "$node_pid" 2>/dev/null || fail "the node exited"
  sleep 0.2
done
grep -q 'first trusted time sample: sequencing started' "$work/node.log" || fail "the node never started sequencing"

# The response header $1 (case-insensitive) of a curl call, empty when absent. The arguments after
# $1 are the request's.
header() {
  local name="$1"; shift
  curl -sk -D - -o /dev/null "$@" | tr -d '\r' | awk -F': ' -v n="$(tr 'A-Z' 'a-z' <<< "$name")" 'tolower($1) == n { print $2 }'
}
status() { curl -sk -o /dev/null -w '%{http_code}' "$@"; }
expect() { [[ "$2" == "$3" ]] || fail "$1: expected '$3', got '$2'"; }
# A WebSocket upgrade with Origin $1, which curl cuts off after the status line.
upgrade_status() {
  curl -sk -o /dev/null -w '%{http_code}' --max-time 3 -H "Origin: $1" -H 'Connection: Upgrade' \
    -H 'Upgrade: websocket' -H 'Sec-WebSocket-Version: 13' -H 'Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==' \
    "$base/v1/stream" || true
}

step "the listed origin gets its own origin back on a preflight and a read"
expect "preflight status" "$(status -X OPTIONS -H "Origin: $listed" -H 'Access-Control-Request-Method: POST' "$base/v1/orders")" 204
expect "preflight origin" "$(header Access-Control-Allow-Origin -X OPTIONS -H "Origin: $listed" -H 'Access-Control-Request-Method: POST' "$base/v1/orders")" "$listed"
expect "preflight headers" "$(header Access-Control-Allow-Headers -X OPTIONS -H "Origin: $listed" -H 'Access-Control-Request-Method: POST' "$base/v1/orders")" "Content-Type, X-Engine-Challenge"
expect "read status" "$(status -H "Origin: $listed" "$base/v1/markets")" 200
expect "read origin" "$(header Access-Control-Allow-Origin -H "Origin: $listed" "$base/v1/markets")" "$listed"
expect "read vary" "$(header Vary -H "Origin: $listed" "$base/v1/markets")" "Origin"

step "another origin gets no CORS headers"
expect "evil read status" "$(status -H "Origin: $evil" "$base/v1/markets")" 200
expect "evil read origin" "$(header Access-Control-Allow-Origin -H "Origin: $evil" "$base/v1/markets")" ""
expect "evil preflight origin" "$(header Access-Control-Allow-Origin -X OPTIONS -H "Origin: $evil" -H 'Access-Control-Request-Method: POST' "$base/v1/orders")" ""
expect "evil preflight status" "$(status -X OPTIONS -H "Origin: $evil" -H 'Access-Control-Request-Method: POST' "$base/v1/orders")" 405

step "the stream refuses an unlisted origin before the upgrade"
expect "evil upgrade" "$(upgrade_status "$evil")" 403
expect "listed upgrade" "$(upgrade_status "$listed")" 101

step "the operator's admin route is closed to browsers"
expect "admin preflight status" "$(status -X OPTIONS -H "Origin: $listed" -H 'Access-Control-Request-Method: POST' "$base/v1/admin")" 405
expect "admin preflight origin" "$(header Access-Control-Allow-Origin -X OPTIONS -H "Origin: $listed" -H 'Access-Control-Request-Method: POST' "$base/v1/admin")" ""
expect "admin post origin" "$(header Access-Control-Allow-Origin -X POST -H "Origin: $listed" -H 'Content-Type: application/json' -d '{}' "$base/v1/admin")" ""

step "a rate-limited refusal still carries the listed origin"
limited=""
for _ in $(seq 1 80); do
  response="$(curl -sk -D - -o /dev/null -H "Origin: $listed" "$base/v1/health" | tr -d '\r')"
  if [[ "$(head -n 1 <<< "$response")" == *" 429"* ]]; then limited="$response"; break; fi
done
[[ -n "$limited" ]] || fail "the rate limiter never answered 429"
grep -qix "access-control-allow-origin: $listed" <<< "$limited" || fail "the 429 carries no allowed origin: $limited"

echo "PASS: browser origins"
