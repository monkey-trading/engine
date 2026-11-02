#!/usr/bin/env bash
# Dev scenario for restarting from checkpoint snapshots, on a node with config/dev.toml's mocks,
# checkpoints every 100 commands, and `[snapshots]` added:
#   1. run the node, take its enclave address from the attestation line, and pin it;
#   2. sequence 400 dev deposits, let history catch up, stop the node, and restart it from its
#      newest paired cache;
#   3. sequence 3,600 more (a tenfold log and history), and stop and restart again the same way;
#   4. corrupt the newest cache and restart once more.
# Passes when every restart recovers from a cache and replays at most one checkpoint interval,
# history serves every deposit once as soon as the API answers, the dashboard record says it begins
# after the cache (`from_genesis` false), the corrupt cache raises a
# `snapshot_rejected` Warning and recovery uses an earlier cache, and `engine replay` verifies every
# checkpoint the node wrote, the ones after the fallback included. Restart times are printed, not
# gated.
#
# Needs python3, curl, and jq. API_PORT and METRICS_PORT override 18280 and 19664;
# SCENARIO_ARTIFACTS keeps the node logs.
set -Eeuo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
work="$(mktemp -d)"
api_port="${API_PORT:-18280}"
metrics_port="${METRICS_PORT:-19664}"
account="0x3737373737373737373737373737373737373737"
interval=100
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
fail() { echo "FAIL: $*" >&2; tail -n 40 "$work"/*.log >&2 || true; exit 1; }
trap 'fail "the command at line $LINENO failed"' ERR

(cd "$root" && cargo build -p engine-node --locked --quiet)
engine="$(cargo metadata --format-version 1 --no-deps --manifest-path "$root/Cargo.toml" | jq -r .target_directory)/debug/engine"
python3 - "$api_port" "$metrics_port" <<'PY'
import socket, sys
for port in sys.argv[1:]:
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", int(port)))
PY

max_drift="$(awk -F'"' '/^max_drift = / { print $2 }' "$root/config/dev.toml")"
[[ "$max_drift" =~ ^[0-9]+s$ ]] || fail "expected time.max_drift in whole seconds, got '$max_drift'"
sample_within="$(( ${max_drift%s} + 15 ))"

step "write the node config with [snapshots] and no signer pin yet"
sed -e "s|127.0.0.1:8080|127.0.0.1:$api_port|" \
  -e "s|127.0.0.1:9464|127.0.0.1:$metrics_port|" \
  -e "s|data/dev|$work|g" \
  -e "s|^segment_commands = .*|segment_commands = $interval|" \
  -e "s|^checkpoint_every = .*|checkpoint_every = $interval|" \
  -e 's|^per_ip = .*|per_ip = { per_second = 100000, burst = 100000 }|' \
  "$root/config/dev.toml" > "$work/node.toml"
printf '\n[snapshots]\ndir = "%s/snapshots"\nretain = 3\nmax_bytes = 67108864\ntrusted_signers = []\n' \
  "$work" >> "$work/node.toml"
for edit in ":$api_port\"" ":$metrics_port\"" "$work/log\"" "checkpoint_every = $interval" "per_second = 100000"; do
  grep -qF "$edit" "$work/node.toml" || fail "config edit missing: $edit"
done

start_node() {
  "$engine" run --config "$work/node.toml" > "$work/$1.log" 2>&1 &
  node_pid=$!
  started="$(now_ms)"
}
stop_node() {
  kill -TERM "$node_pid"
  wait "$node_pid" || fail "the node exited with an error"
  node_pid=""
}
now_ms() { python3 -c 'import time; print(time.time_ns() // 1000000)'; }
elapsed_ms() { echo $(( $(now_ms) - started )); }
wait_for_log() {
  for _ in $(seq 1 $(( sample_within * 5 ))); do
    grep -q "$2" "$work/$1.log" && return 0
    kill -0 "$node_pid" 2>/dev/null || fail "the node exited"
    sleep 0.2
  done
  fail "$1: no '$2' within ${sample_within}s"
}
# The history route answers 503 until history has taken the log the node recovered.
history_status() {
  curl -sk -o /dev/null -w '%{http_code}' "https://127.0.0.1:$api_port/v1/accounts/$account/history?limit=1" || true
}
wait_for_history() {
  for _ in $(seq 1 600); do
    [[ "$(history_status)" == 200 ]] && return 0
    kill -0 "$node_pid" 2>/dev/null || fail "the node exited"
    sleep 0.05
  done
  fail "history never served"
}
# Every history event of the account, paged with the cursors the route returns.
history_events() {
  python3 - "$api_port" "$account" <<'PY'
import http.client, json, ssl, sys
port, account = int(sys.argv[1]), sys.argv[2]
conn = http.client.HTTPSConnection("127.0.0.1", port, context=ssl._create_unverified_context())
count, before = 0, None
while True:
    path = f"/v1/accounts/{account}/history?limit=200" + (f"&before={before}" if before else "")
    conn.request("GET", path)
    response = conn.getresponse()
    body = json.loads(response.read())
    assert response.status == 200, body
    count += len(body["events"])
    before = body.get("next")
    if before is None:
        break
print(count)
PY
}
# Sequences `$1` dev deposits over one keep-alive connection.
deposit() {
  python3 - "$api_port" "$account" "$1" <<'PY'
import http.client, json, ssl, sys
port, account, n = int(sys.argv[1]), sys.argv[2], int(sys.argv[3])
conn = http.client.HTTPSConnection("127.0.0.1", port, context=ssl._create_unverified_context())
for _ in range(n):
    conn.request("POST", "/v1/dev/deposit", json.dumps({"account": account, "amount": "100"}),
                 {"Content-Type": "application/json"})
    response = conn.getresponse()
    body = response.read()
    assert response.status == 200, body
PY
}
# The recovery line's `snapshot_seq` and command count, as "<snapshot_seq> <commands>".
recovery() {
  local line
  line="$(grep 'recovered from log' "$work/$1.log" | tail -n 1)"
  [[ -n "$line" ]] || fail "$1: no recovery line"
  local seq commands
  seq="$(sed -n 's/.*snapshot_seq=\([0-9]*\).*/\1/p' <<< "$line")"
  commands="$(sed -n 's/.*commands=\([0-9]*\).*/\1/p' <<< "$line")"
  echo "${seq:-none} $commands"
}
# Where the dashboard record begins, as "<from_genesis> <begins_seq>", once a command has reached it.
dashboard_begins() {
  local begins
  for _ in $(seq 1 100); do
    begins="$(curl -sk --max-time 5 "https://127.0.0.1:$api_port/v1/stats/series?metric=cash&resolution=1d" \
      | jq -r 'select(.begins_seq != null) | "\(.from_genesis) \(.begins_seq)"')"
    [[ -n "$begins" ]] && { echo "$begins"; return 0; }
    sleep 0.2
  done
  fail "the dashboard record never began"
}
# A recovery from a cache replays only the suffix, so the record says it does not begin at genesis.
expect_suffix_begin() {
  local from_genesis begins_seq
  read -r from_genesis begins_seq <<< "$(dashboard_begins)"
  [[ "$from_genesis" == false ]] || fail "the dashboard record claims the genesis after a recovery from cache $1"
  (( begins_seq > $1 )) || fail "the dashboard record begins at $begins_seq, not after cache $1"
}
newest_cache() { find "$work/snapshots" -maxdepth 1 -name '[0-9]*-[0-9]*.json' | sort | tail -n 1; }
# A burst can outrun history; the scenario restarts a node whose history has caught up.
wait_for_events() {
  for _ in $(seq 1 1200); do
    [[ "$(history_events 2>/dev/null || echo 0)" == "$1" ]] && return 0
    kill -0 "$node_pid" 2>/dev/null || fail "the node exited"
    sleep 0.5
  done
  fail "history never reached $1 events"
}

step "boot once to read the enclave address and pin it"
start_node boot
wait_for_log boot 'enclave, config, and TLS key attested'
enclave="$(sed -n 's/.*enclave=\(0x[0-9a-fA-F]\{40\}\).*/\1/p' "$work/boot.log" | head -n 1)"
[[ -n "$enclave" ]] || fail "no enclave address in the attestation line"
wait_for_log boot 'first trusted time sample: sequencing started'
stop_node
sed -i.bak "s|^trusted_signers = \[\]|trusted_signers = [\"$enclave\"]|" "$work/node.toml"
grep -qF "trusted_signers = [\"$enclave\"]" "$work/node.toml" || fail "pin not written"

deposits=0
for target in 400 4000; do
  step "sequence dev deposits up to $target, then stop"
  start_node "grow-$target"
  wait_for_log "grow-$target" 'first trusted time sample: sequencing started'
  deposit $(( target - deposits ))
  deposits="$target"
  wait_for_events "$target"
  stop_node

  step "restart from the newest cache with $target deposits in the log"
  start_node "restart-$target"
  wait_for_history
  restart_ms="$(elapsed_ms)"
  line="$(recovery "restart-$target")"
  read -r seq commands <<< "$line"
  [[ "$seq" != none ]] || fail "restart with $target deposits replayed the whole log"
  suffix=$(( commands - seq - 1 ))
  (( suffix <= interval )) || fail "restart replayed $suffix commands after the cache"
  expect_suffix_begin "$seq"
  served="$(history_events)"
  [[ "$served" == "$target" ]] || fail "history served $served of $target deposits"
  echo "restart scenario: deposits=$target log_commands=$commands restart_ms=$restart_ms suffix_commands=$suffix"
  stop_node
done

step "corrupt the newest cache and restart"
newest="$(newest_cache)"
newest_seq="$(basename "$newest" | cut -c1-20 | sed 's/^0*//')"
printf '{"payload":' > "$newest"
start_node fallback
wait_for_history
line="$(recovery fallback)"
read -r seq commands <<< "$line"
[[ "$seq" != none ]] || fail "the fallback replayed the whole log"
(( seq < newest_seq )) || fail "recovery used cache $seq, not one before the corrupt $newest_seq"
grep -q 'WARN.*alert="snapshot_rejected"' "$work/fallback.log" || fail "no snapshot_rejected Warning"
expect_suffix_begin "$seq"
served="$(history_events)"
[[ "$served" == "$deposits" ]] || fail "history served $served of $deposits deposits after the fallback"
stop_node

step "audit the whole log, including the checkpoints written after the fallback"
"$engine" replay --log "$work/log" --signer "$enclave" | tee "$work/replay.log"
grep -q '^verified: ' "$work/replay.log" || fail "replay did not verify"

echo "PASS: each restart used a cache and a bounded suffix, and the corrupt cache fell back to an earlier one"
