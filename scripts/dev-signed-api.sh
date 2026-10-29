#!/usr/bin/env bash
# Dev scenario for signed API answers, on a node with config/dev.toml's mocks, a quiet log (no mock
# quotes, an hourly tick) and one-second stream heartbeats:
#   1. read the enclave address from /v1/attestation (a mock one here; a real client verifies the
#      attestation first);
#   2. GET /v1/markets with a fresh challenge verifies with `engine verify-response`; the same body
#      with one byte changed fails, and the same response checked under another challenge fails;
#   3. a signed stream from the start of the log, 20 frames, verifies with `engine verify-frames`
#      with n 0..19; the same frames with one removed fail at the gap, and checked as the answer to
#      a request from another start fail;
#   4. a live signed stream on the quiet log gets a verifying heartbeat at least every interval.
# Passes with `PASS: responses and frames verify; tampering, replay and gaps fail`.
#
# Needs curl, jq, and python3, which reads the WebSocket stream. API_PORT and METRICS_PORT override
# 18280 and 19564.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
work="$(mktemp -d)"
api_port="${API_PORT:-18280}"
metrics_port="${METRICS_PORT:-19564}"
api="https://127.0.0.1:$api_port"
node_pid=""

cleanup() {
  [[ -n "$node_pid" ]] && kill "$node_pid" 2>/dev/null || true
  rm -rf "$work"
}
trap cleanup EXIT

step() { echo "==> $*"; }
fail() {
  echo "FAIL: $*" >&2
  [[ -f "$work/node.log" ]] && tail -n 40 "$work/node.log" >&2
  exit 1
}

# 32 random bytes as 64 hex characters.
challenge() { od -An -N32 -tx1 /dev/urandom | tr -d ' \n'; }

# Saves the first `$2` text messages of the stream at `$1` (path and query) to `$3`, one a line,
# and each one's arrival in milliseconds after the upgrade to `$4`. The node's certificate is
# self-signed, so this skips the CA check as `curl -k` does.
capture() {
  python3 -I - 127.0.0.1 "$api_port" "$@" <<'PY'
import base64, os, socket, ssl, struct, sys, time

host, port, path, count, frames_out, arrivals_out = sys.argv[1], int(sys.argv[2]), sys.argv[3], int(sys.argv[4]), sys.argv[5], sys.argv[6]
context = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
context.check_hostname = False
context.verify_mode = ssl.CERT_NONE
sock = context.wrap_socket(socket.create_connection((host, port), timeout=30), server_hostname=host)
key = base64.b64encode(os.urandom(16)).decode()
sock.sendall((f"GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n"
              f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n").encode())
stream = sock.makefile("rb")
status = stream.readline()
if b" 101 " not in status:
    sys.exit(f"the upgrade was refused: {status!r}")
while stream.readline() not in (b"\r\n", b""):
    pass
opened = time.monotonic()


def read(n):
    data = stream.read(n)
    if len(data) != n:
        sys.exit("the stream ended")
    return data


frames, arrivals, message = [], [], b""
while len(frames) < count:
    first, second = read(2)
    length = second & 0x7F
    if length == 126:
        length = struct.unpack(">H", read(2))[0]
    elif length == 127:
        length = struct.unpack(">Q", read(8))[0]
    if second & 0x80:
        sys.exit("a server frame is never masked")
    payload, opcode = read(length), first & 0x0F
    if opcode == 8:
        sys.exit("the node closed the stream")
    if opcode in (0, 1):
        message += payload
        if first & 0x80:
            frames.append(message.decode())
            arrivals.append(round((time.monotonic() - opened) * 1000))
            message = b""
with open(frames_out, "w") as out:
    out.write("\n".join(frames) + "\n")
with open(arrivals_out, "w") as out:
    out.write("\n".join(map(str, arrivals)) + "\n")
PY
}

step "build the node"
(cd "$root" && cargo build -p engine-node --locked --quiet)
engine="$(cargo metadata --format-version 1 --no-deps --manifest-path "$root/Cargo.toml" | jq -r .target_directory)/debug/engine"

step "write the node config: no mock quotes, an hourly tick, one-second heartbeats"
sed -e "s|^listen = \"127.0.0.1:8080\"|listen = \"127.0.0.1:$api_port\"|" \
  -e "s|^listen = \"127.0.0.1:9464\"|listen = \"127.0.0.1:$metrics_port\"|" \
  -e "s|^log_dir = .*|log_dir = \"$work/log\"|" \
  -e "s|^dir = \"data/dev/history\"|dir = \"$work/history\"|" \
  -e "s|dir = \"data/dev/alerts\"|dir = \"$work/alerts\"|" \
  -e 's|^tick = .*|tick = "1h"|' \
  -e 's|^heartbeat_interval = .*|heartbeat_interval = "1s"|' \
  -e 's|mid = "\([0-9.]*\)" }|mid = "\1", stop_after = "0s" }|' \
  "$root/config/dev.toml" > "$work/node.toml"
for edit in ":$api_port\"" ":$metrics_port\"" "$work/log\"" "$work/history\"" 'tick = "1h"' 'heartbeat_interval = "1s"'; do
  grep -qF "$edit" "$work/node.toml" || fail "config edit missing: $edit"
done
[[ "$(grep -c 'stop_after = "0s"' "$work/node.toml")" == "$(grep -c 'mid = "' "$work/node.toml")" ]] \
  || fail "not every mock market is silenced"
chain_id="$(awk -F' = ' '/^chain_id = / { print $2; exit }' "$work/node.toml")"
vault="$(awk -F'"' '/^vault = / { print $2; exit }' "$work/node.toml")"

# The first trusted time sample may wait max_drift for a slow Roughtime server; genesis follows it.
max_drift="$(awk -F'"' '/^max_drift = / { print $2 }' "$root/config/dev.toml")"
[[ "$max_drift" =~ ^[0-9]+s$ ]] || fail "expected time.max_drift in whole seconds, got '$max_drift'"
ready_within="$(( ${max_drift%s} + 15 ))"

step "run the node and wait for genesis"
"$engine" run --config "$work/node.toml" > "$work/node.log" 2>&1 &
node_pid=$!
started="$(date +%s)"
until [[ "$(curl -sk "$api/v1/markets" 2>/dev/null | jq 'if type == "array" then length else 0 end' 2>/dev/null)" =~ ^[1-9] ]]; do
  (( $(date +%s) - started <= ready_within )) || fail "no markets within ${ready_within}s of boot"
  kill -0 "$node_pid" 2>/dev/null || fail "the node exited"
  sleep 0.2
done

step "read the enclave address from the attestation"
signer="$(curl -sfk "$api/v1/attestation" | jq -r .enclave_address)"
[[ "$signer" =~ ^0x[0-9a-f]{40}$ ]] || fail "no enclave address in the attestation: $signer"
answer=(--signer "$signer" --chain-id "$chain_id" --vault "$vault")

step "GET /v1/markets with a fresh challenge verifies"
sent="$(challenge)"
curl -sfk -D "$work/headers" -o "$work/body" -H "X-Engine-Challenge: $sent" "$api/v1/markets" \
  || fail "GET /v1/markets failed"
"$engine" verify-response "${answer[@]}" --challenge "$sent" --path /v1/markets \
  --headers "$work/headers" --body "$work/body" || fail "the response did not verify"

step "the body with one byte changed fails"
python3 -I - "$work/body" "$work/body.changed" <<'PY'
import sys

body = bytearray(open(sys.argv[1], "rb").read())
body[len(body) // 2] ^= 0x01
open(sys.argv[2], "wb").write(body)
PY
if out="$("$engine" verify-response "${answer[@]}" --challenge "$sent" --path /v1/markets \
  --headers "$work/headers" --body "$work/body.changed" 2>&1)"; then
  fail "a changed body verified"
fi
grep -q "not the attested key's" <<< "$out" || fail "unexpected refusal of a changed body: $out"

step "the same response checked under another challenge fails"
grep -v -i '^x-engine-challenge:' "$work/headers" > "$work/headers.unechoed"
for headers in "$work/headers" "$work/headers.unechoed"; do
  if out="$("$engine" verify-response "${answer[@]}" --challenge "$(challenge)" --path /v1/markets \
    --headers "$headers" --body "$work/body" 2>&1)"; then
    fail "the response verified under another challenge"
  fi
  grep -qE "echoes another challenge|not the attested key's" <<< "$out" || fail "unexpected refusal: $out"
done

step "a signed stream of 20 frames from the start of the log verifies"
streamed="$(challenge)"
from_start="/v1/stream?from=0&signed=1&challenge=$streamed"
capture "$from_start" 20 "$work/frames" "$work/frame-arrivals" || fail "the signed stream could not be read"
jq -se 'map(.n) == [range(0; 20)]' "$work/frames" > /dev/null || fail "frames are not n 0..19"
"$engine" verify-frames "${answer[@]}" --challenge "$streamed" --path "$from_start" --frames "$work/frames" \
  || fail "the stream did not verify"

step "the same frames fail as the answer to a request from another start"
if out="$("$engine" verify-frames "${answer[@]}" --challenge "$streamed" --path "${from_start/from=0/from=5}" \
  --frames "$work/frames" 2>&1)"; then
  fail "frames verified as the answer to another start"
fi
grep -q "not the attested key's" <<< "$out" || fail "unexpected refusal of another start: $out"

step "the same frames with one removed fail at the gap"
sed '10d' "$work/frames" > "$work/frames.gap"
if out="$("$engine" verify-frames "${answer[@]}" --challenge "$streamed" --path "$from_start" \
  --frames "$work/frames.gap" 2>&1)"; then
  fail "frames with a gap verified"
fi
grep -q "line 10" <<< "$out" && grep -q "frame 10 arrived where frame 9 was due" <<< "$out" \
  || fail "unexpected refusal of a gap: $out"

step "a live signed stream on the quiet log gets a heartbeat each interval"
live="$(challenge)"
live_only="/v1/stream?signed=1&challenge=$live"
capture "$live_only" 3 "$work/beats" "$work/beat-arrivals" || fail "the live signed stream could not be read"
jq -se 'all(.[]; .msg | fromjson | .type == "heartbeat" and (.head | test("^[0-9]+:[0-9]+$")))' "$work/beats" \
  > /dev/null || fail "a live frame on the quiet log is not a heartbeat: $(cat "$work/beats")"
"$engine" verify-frames "${answer[@]}" --challenge "$live" --path "$live_only" --frames "$work/beats" \
  || fail "the heartbeats did not verify"
# At most the one-second interval apart, with a second for a loaded machine.
awk 'NR == 1 && $1 > 2000 { exit 1 } NR > 1 && $1 - last > 2000 { exit 1 } { last = $1 }' "$work/beat-arrivals" \
  || fail "a heartbeat came late: $(tr '\n' ' ' < "$work/beat-arrivals")ms after the upgrade"

step "stop the node"
kill -TERM "$node_pid"
wait "$node_pid" || fail "node exited with an error"
node_pid=""

echo "PASS: responses and frames verify; tampering, replay and gaps fail"
