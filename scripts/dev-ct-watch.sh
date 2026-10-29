#!/usr/bin/env bash
# The Certificate Transparency watcher against a mock dev node and a loopback stand-in log. The log
# holds three entries: a certificate for the node's attested TLS key, one for an unrelated domain,
# and one for the dev domain with a foreign key. A webhook stand-in records the watcher's pages.
#   1. exactly one unattested_certificate page arrives, for the foreign key;
#   2. after a node restart (a new attested key), a new entry for the new key raises nothing;
#   3. stopping the stand-in log raises ct_log_unreachable.
# Needs python3, OpenSSL 3, curl and jq. No public network, funded account, or live environment is
# used. API_PORT, LOG_PORT, HOOK_PORT, METRICS_PORT and WATCH_METRICS_PORT override the loopback ports;
# SCENARIO_ARTIFACTS keeps logs and the watcher's state.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
work="$(mktemp -d)"
api_port="${API_PORT:-18280}"
log_port="${LOG_PORT:-18281}"
hook_port="${HOOK_PORT:-18282}"
metrics_port="${METRICS_PORT:-19664}"
watch_metrics_port="${WATCH_METRICS_PORT:-19665}"
domain="api.dev.test"
node_pid=""
log_pid=""
hook_pid=""
watch_pid=""

stop() {
  if [[ -n "$1" ]]; then kill "$1" 2>/dev/null || true; wait "$1" 2>/dev/null || true; fi
}
cleanup() {
  stop "$watch_pid"; stop "$node_pid"; stop "$log_pid"; stop "$hook_pid"
  if [[ -n "${SCENARIO_ARTIFACTS:-}" ]]; then
    mkdir -p "$SCENARIO_ARTIFACTS"
    cp "$work"/*.log "$work"/*.jsonl "$work"/*.toml "$work"/watch/state.json "$SCENARIO_ARTIFACTS/" 2>/dev/null || true
  fi
  rm -rf "$work"
}
trap cleanup EXIT
fail() { echo "FAIL: $*" >&2; tail -n 30 "$work"/*.log >&2 || true; exit 1; }
step() { echo "==> $*"; }

pages() { if [[ -f "$work/pages.jsonl" ]]; then wc -l < "$work/pages.jsonl" | tr -d ' '; else echo 0; fi; }
# Waits up to 60 seconds for $1 pages.
wait_for_pages() {
  for _ in $(seq 1 300); do
    kill -0 "$watch_pid" 2>/dev/null || fail "the watcher exited"
    [[ "$(pages)" -ge "$1" ]] && return 0
    sleep 0.2
  done
  fail "no page number $1 arrived: $(cat "$work/pages.jsonl" 2>/dev/null || true)"
}
# Waits up to 30 seconds for jq filter $1 to hold over the watcher's state file.
wait_for_state() {
  for _ in $(seq 1 150); do
    kill -0 "$watch_pid" 2>/dev/null || fail "the watcher exited"
    if jq -e "$1" "$work/watch/state.json" > /dev/null 2>&1; then return 0; fi
    sleep 0.2
  done
  fail "the watcher state never satisfied $1: $(cat "$work/watch/state.json" 2>/dev/null || true)"
}
sha256_of_pubkey() { openssl pkey -pubin -in "$1" -outform DER | openssl dgst -sha256 -r | cut -d' ' -f1; }

start_node() {
  "$engine" run --config "$work/node.toml" > "$work/node-$1.log" 2>&1 &
  node_pid=$!
  for _ in $(seq 1 150); do
    kill -0 "$node_pid" 2>/dev/null || fail "the node exited"
    if curl -sfk --max-time 2 "https://127.0.0.1:$api_port/v1/attestation" > "$work/attestation.json" 2>/dev/null; then return 0; fi
    sleep 0.2
  done
  fail "the node never served its attestation"
}
# Writes the node's TLS public key to $1, as served on its listener.
node_public_key() {
  openssl s_client -connect "127.0.0.1:$api_port" -tls1_3 < /dev/null 2> /dev/null \
    | openssl x509 -pubkey -noout > "$1"
}
# A certificate for $2 (a DNS name) carrying the public key in $1, issued by a dev issuer, to $3.
issue() {
  printf 'subjectAltName=DNS:%s\n' "$2" > "$work/san.cnf"
  openssl x509 -new -force_pubkey "$1" -subj "/CN=$2" -CA "$work/issuer.pem" -CAkey "$work/issuer.key" \
    -days 1 -extfile "$work/san.cnf" -out "$3"
}
add_leaf() { python3 "$root/scripts/dev-ct-log.py" leaf "$2" x509 "$(( $(date +%s) * 1000 ))" "$work/ctlog/leaf-$1.bin"; }

openssl version | grep -q '^OpenSSL 3' || fail "OpenSSL 3 is required (x509 -force_pubkey)"

step "build the node"
(cd "$root" && cargo build -p engine-node --locked --quiet)
engine="$(cargo metadata --format-version 1 --no-deps --manifest-path "$root/Cargo.toml" | jq -r .target_directory)/debug/engine"
python3 - "$api_port" "$log_port" "$hook_port" "$metrics_port" "$watch_metrics_port" <<'PY'
import socket, sys
for port in sys.argv[1:]:
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", int(port)))
PY

step "start a webhook stand-in on 127.0.0.1:$hook_port"
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

step "start a mock dev node with no outside network"
sed -e "s|data/dev|$work|g" \
  -e "s|127.0.0.1:8080|127.0.0.1:$api_port|" \
  -e "s|127.0.0.1:9464|127.0.0.1:$metrics_port|" \
  -e 's|^address = ".*"|address = "127.0.0.1:9"|' \
  "$root/config/dev.toml" > "$work/node.toml"
start_node first
node_public_key "$work/node-first.pub"
[[ "$(sha256_of_pubkey "$work/node-first.pub")" == "$(jq -r '.tls_public_key_sha256 | ltrimstr("0x")' "$work/attestation.json")" ]] \
  || fail "the attested TLS key hash is not the SHA-256 of the served key"

step "issue three certificates and start the stand-in log on 127.0.0.1:$log_port"
openssl ecparam -name prime256v1 -genkey -noout -out "$work/issuer.key"
openssl req -x509 -new -key "$work/issuer.key" -subj "/CN=Dev Issuer" -days 1 -out "$work/issuer.pem"
openssl ecparam -name prime256v1 -genkey -noout -out "$work/log.key"
openssl ec -in "$work/log.key" -pubout -outform DER 2> /dev/null | base64 | tr -d '\n' > "$work/log.pub.b64"
openssl ecparam -name prime256v1 -genkey -noout | openssl ec -pubout -out "$work/foreign.pub" 2> /dev/null
openssl ecparam -name prime256v1 -genkey -noout | openssl ec -pubout -out "$work/unrelated.pub" 2> /dev/null
issue "$work/node-first.pub" "$domain" "$work/attested.pem"
issue "$work/unrelated.pub" "other.dev.test" "$work/unrelated.pem"
issue "$work/foreign.pub" "$domain" "$work/foreign.pem"
mkdir "$work/ctlog"
add_leaf 0 "$work/attested.pem"
add_leaf 1 "$work/unrelated.pem"
add_leaf 2 "$work/foreign.pem"
python3 "$root/scripts/dev-ct-log.py" serve "$log_port" "$work/ctlog" "$work/log.key" &
log_pid=$!

step "write the watcher config"
cat > "$work/release.toml" <<EOF
name = "dev-release"
launch_digest = "0x$(printf '00%.0s' $(seq 1 48))"
policy = "0x30000"
[minimum_tcb]
boot_loader = 0
tee = 0
snp = 0
microcode = 0
[config_hashes]
"node.toml" = "0x$(printf '00%.0s' $(seq 1 32))"
EOF
cat > "$work/watch.toml" <<EOF
domain = "$domain"
state_file = "$work/watch/state.json"
attestation_interval = "2s"
log_interval = "1s"
grace = "10s"
log_stale_after = "10s"
[node]
url = "https://127.0.0.1:$api_port"
release = "$work/release.toml"
processor = "Milan"
[[logs]]
name = "dev-log"
url = "http://127.0.0.1:$log_port"
public_key = "$(cat "$work/log.pub.b64")"
start_index = 0
[logging]
level = "info"
[metrics]
listen = "127.0.0.1:$watch_metrics_port"
[alerts]
episode_quiet = "1h"
[alerts.outbox]
dir = "$work/watch/alerts"
max_pages = 100
max_bytes = 1048576
max_age = "1h"
[alerts.critical]
url_env = "CT_WATCH_ALERTS_CRITICAL_URL"
EOF

step "run the watcher; the foreign-key certificate alerts once its grace ends"
secret="ct-watch-hook-token"
CT_WATCH_ALERTS_CRITICAL_URL="http://127.0.0.1:$hook_port/hooks/$secret" \
  "$engine" ct-watch --config "$work/watch.toml" --allow-mock > "$work/watch.log" 2>&1 &
watch_pid=$!
wait_for_pages 1
curl -sf --max-time 2 "http://127.0.0.1:$watch_metrics_port/metrics" | grep -q '^engine_alert_' \
  || fail "the watcher's metrics listener does not serve the alert counters"
sleep 3
[[ "$(pages)" == 1 ]] || fail "expected one page, got $(pages): $(cat "$work/pages.jsonl")"
foreign_hash="$(sha256_of_pubkey "$work/foreign.pub")"
jq -e --arg hash "$foreign_hash" '.status == "firing" and .severity == "critical"
  and .alert == "unattested_certificate" and .fields.log == "dev-log" and .fields.index == "2"
  and .fields.key_sha256 == $hash and (.fields.issuer | contains("Dev Issuer"))' "$work/pages.jsonl" > /dev/null \
  || fail "unexpected page: $(cat "$work/pages.jsonl")"
wait_for_state '(.logs | to_entries[0].value.next_index) == 3'

step "restart the node: a new attested key, and a certificate for it before the watcher polls"
stop "$node_pid"; node_pid=""
start_node second
node_public_key "$work/node-second.pub"
[[ "$(sha256_of_pubkey "$work/node-second.pub")" != "$(sha256_of_pubkey "$work/node-first.pub")" ]] \
  || fail "the restart kept the TLS key"
issue "$work/node-second.pub" "$domain" "$work/rotated.pem"
add_leaf 3 "$work/rotated.pem"
wait_for_state '(.logs | to_entries[0].value.next_index) == 4 and (.attested | length) == 2'
# Past its grace, the rotated certificate has neither alerted nor been held.
sleep 12
wait_for_state '(.findings | length) == 1'
[[ "$(pages)" == 1 ]] || fail "the rotation raised a page: $(cat "$work/pages.jsonl")"

step "stop the stand-in log"
stop "$log_pid"; log_pid=""
wait_for_pages 2
jq -e -s '.[1] | .status == "firing" and .severity == "critical" and .alert == "ct_log_unreachable"
  and .fields.log == "dev-log"' "$work/pages.jsonl" > /dev/null \
  || fail "unexpected second page: $(cat "$work/pages.jsonl")"

stop "$watch_pid"; watch_pid=""
if grep -q "$secret" "$work/watch.log"; then fail "the watcher logged the webhook URL"; fi
[[ "$(jq -s '[.[] | select(.alert == "unattested_certificate")] | length' "$work/pages.jsonl")" == 1 ]] \
  || fail "more than one unattested_certificate page"

echo "PASS: one alert for the foreign key; restart rotation accepted"
jq -c . "$work/pages.jsonl"
