#!/usr/bin/env bash
# An isolated offline node pages when loopback Roughtime servers remain unavailable. Kill it
# during webhook retries, start the receiver, then restart on the same private data directory.
# Needs python3, curl and jq. No public network, funded account, or live environment is used.
# API_PORT, METRICS_PORT and HOOK_PORT override the loopback ports; SCENARIO_ARTIFACTS keeps logs.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
work="$(mktemp -d)"
api_port="${API_PORT:-18180}"
metrics_port="${METRICS_PORT:-19564}"
hook_port="${HOOK_PORT:-18199}"
node_pid=""
hook_pid=""

cleanup() {
  if [[ -n "$node_pid" ]]; then kill "$node_pid" 2>/dev/null || true; wait "$node_pid" 2>/dev/null || true; fi
  if [[ -n "$hook_pid" ]]; then kill "$hook_pid" 2>/dev/null || true; wait "$hook_pid" 2>/dev/null || true; fi
  if [[ -n "${SCENARIO_ARTIFACTS:-}" ]]; then
    mkdir -p "$SCENARIO_ARTIFACTS"
    cp "$work"/*.log "$work"/*.jsonl "$work"/*.json "$SCENARIO_ARTIFACTS/" 2>/dev/null || true
  fi
  rm -rf "$work"
}
trap cleanup EXIT
fail() { echo "FAIL: $*" >&2; tail -n 30 "$work"/*.log >&2 || true; exit 1; }
metric() { curl --max-time 2 -sf "http://127.0.0.1:$metrics_port/metrics" | awk -v name="$1" '$1 == name {print $2}' || true; }
wait_for() {
  for _ in $(seq 1 450); do
    kill -0 "$node_pid" 2>/dev/null || fail "node exited"
    [[ "$(metric "$1")" == "$2" ]] && return 0
    sleep 0.2
  done
  fail "metric $1 never reached $2"
}

[ -n "${ENGINE_PREBUILT:-}" ] || (cd "$root" && cargo build -p engine-node --locked --quiet)
engine="$(cargo metadata --format-version 1 --no-deps --manifest-path "$root/Cargo.toml" | jq -r .target_directory)/debug/engine"
python3 - "$api_port" "$metrics_port" "$hook_port" <<'PY'
import socket, sys
for port in sys.argv[1:]:
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", int(port)))
PY
sed -e "s|data/dev|$work|g" \
  -e "s|127.0.0.1:8080|127.0.0.1:$api_port|" \
  -e "s|127.0.0.1:9464|127.0.0.1:$metrics_port|" \
  -e 's|^address = ".*"|address = "127.0.0.1:9"|' \
  "$root/config/dev.toml" > "$work/node.toml"
printf '\n[alerts.critical]\nurl_env = "ENGINE_ALERTS_CRITICAL_URL"\n' >> "$work/node.toml"
start_node() {
  ENGINE_ALERTS_CRITICAL_URL="http://127.0.0.1:$hook_port/dev-outbox-token" \
    "$engine" run --config "$work/node.toml" > "$work/$1.log" 2>&1 &
  node_pid=$!
}

echo "==> withhold trusted time until its Critical page is durably pending and retrying"
start_node before
wait_for 'engine_alert_pages_pending{severity="critical"}' 1
# Confirm an actual failed send before SIGKILL; the pending gauge alone cannot prove mid-retry.
for _ in $(seq 1 100); do
  failed="$(metric 'engine_alert_attempts_failed_total{severity="critical"}')"
  [[ -n "$failed" && "$failed" -ge 1 ]] && break
  sleep 0.1
done
[[ -n "$failed" && "$failed" -ge 1 ]] || fail "no retry observed"
kill -KILL "$node_pid"
wait "$node_pid" 2>/dev/null || true
node_pid=""
python3 - "$work/alerts/outbox.sqlite" "$work/pending.json" <<'PY'
import json, sqlite3, sys
with sqlite3.connect(sys.argv[1]) as db:
    rows = db.execute("select body from pages order by id").fetchall()
assert len(rows) == 1, rows
page = json.loads(rows[0][0])["page"]
assert page["alert"] == "trusted_time_unavailable" and page["severity"] == "critical", page
with open(sys.argv[2], "w") as out:
    json.dump(page, out)
PY

echo "==> start the receiver and restart on the same data directory"
python3 - "$hook_port" "$work/pages.jsonl" "$work/receiver.ready" <<'PY' &
import http.server, sys
port, out, ready = int(sys.argv[1]), sys.argv[2], sys.argv[3]
class Hook(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        body = self.rfile.read(int(self.headers["Content-Length"]))
        with open(out, "ab") as pages:
            pages.write(body + b"\n")
        self.send_response(200)
        self.end_headers()
    def log_message(self, *args): pass
server = http.server.HTTPServer(("127.0.0.1", port), Hook)
open(ready, "w").close()
server.serve_forever()
PY
hook_pid=$!
for _ in $(seq 1 100); do [[ -f "$work/receiver.ready" ]] && break; sleep 0.1; done
[[ -f "$work/receiver.ready" ]] || fail "receiver did not start"
start_node after
wait_for 'engine_alert_deliveries_total{severity="critical",outcome="sent"}' 1
wait_for 'engine_alert_pages_pending{severity="critical"}' 0
# Metrics are served for the process's whole life, so they can answer before the API listens.
for _ in $(seq 1 150); do
  curl --max-time 2 -sfk -o /dev/null "https://127.0.0.1:$api_port/v1/health" && break
  sleep 0.2
done
archive_status="$(curl --max-time 2 -sk -o /dev/null -w '%{http_code}' \
  "https://127.0.0.1:$api_port/v1/archive/outbox.sqlite")"
[[ "$archive_status" == 404 ]] || fail "private outbox archive request answered $archive_status"
kill -TERM "$node_pid"
wait "$node_pid" || fail "restart did not shut down cleanly"
node_pid=""
python3 - "$work/pending.json" "$work/pages.jsonl" "$work/alerts/outbox.sqlite" <<'PY'
import json, sqlite3, sys
expected = json.load(open(sys.argv[1]))
pages = [json.loads(line) for line in open(sys.argv[2])]
assert pages == [expected], pages
with sqlite3.connect(sys.argv[3]) as db:
    assert db.execute("select count(*) from pages").fetchone()[0] == 0
print("PASS: one accepted page after SIGKILL/restart; original episode key, payload and route retained; durable row removed; private outbox excluded from archive")
PY
if grep -q 'dev-outbox-token' "$work"/*.log; then fail "webhook credential leaked"; fi
