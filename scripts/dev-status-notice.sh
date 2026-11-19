#!/usr/bin/env bash
# Dev scenario for the operator's status notice (protocol V78), on a node with config/dev.toml's mocks:
#   1. with no notice, GET /v1/notice answers `null`, and the answer verifies with `engine verify-response`;
#   2. `engine admin set-notice` sets one and the signed answer carries its text and the time it was set;
#   3. a signed stream opened with `notice=1` sends a frame at its start and on each change, set, replaced
#      and cleared, and every frame verifies with `engine verify-frames`;
#   4. text a notice may not hold (empty, over 280 characters, markup, a link) is refused before it is
#      signed, and `clear-notice` with none in force is refused by the engine;
#   5. a notice set again survives a restart of the node with the time it was set, and clears;
#   6. the log replays.
# Passes with `PASS: the notice is set, streamed, kept across a restart and cleared; every answer verifies`.
#
# Needs curl, jq, and python3, which reads the WebSocket stream. API_PORT and METRICS_PORT override
# 18290 and 19590.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
work="$(mktemp -d)"
api_port="${API_PORT:-18290}"
metrics_port="${METRICS_PORT:-19590}"
api="https://127.0.0.1:$api_port"
operator_key="4242424242424242424242424242424242424242424242424242424242424242"
node_pid=""
capture_pid=""

cleanup() {
  [[ -n "$capture_pid" ]] && kill "$capture_pid" 2>/dev/null || true
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

# Appends each text message of the stream at `$1` (path and query) to `$2`, one a line, flushed as it
# arrives, until `$3` of them are notice frames. The node's certificate is self-signed, so this skips
# the CA check as `curl -k` does.
capture() {
  python3 -I - 127.0.0.1 "$api_port" "$@" <<'PY'
import base64, json, os, socket, ssl, struct, sys

host, port, path, out, wanted = sys.argv[1], int(sys.argv[2]), sys.argv[3], sys.argv[4], int(sys.argv[5])
context = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
context.check_hostname = False
context.verify_mode = ssl.CERT_NONE
sock = context.wrap_socket(socket.create_connection((host, port), timeout=60), server_hostname=host)
key = base64.b64encode(os.urandom(16)).decode()
sock.sendall((f"GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n"
              f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n").encode())
stream = sock.makefile("rb")
status = stream.readline()
if b" 101 " not in status:
    sys.exit(f"the upgrade was refused: {status!r}")
while stream.readline() not in (b"\r\n", b""):
    pass


def read(n):
    data = stream.read(n)
    if len(data) != n:
        sys.exit("the stream ended")
    return data


seen, message = 0, b""
with open(out, "a") as file:
    while seen < wanted:
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
                text = message.decode()
                message = b""
                file.write(text + "\n")
                file.flush()
                if json.loads(json.loads(text)["msg"]).get("type") == "notice":
                    seen += 1
PY
}

# The number of notice frames in `$1`.
notice_frames() {
  [[ -s "$1" ]] || { echo 0; return; }
  jq -s '[.[] | .msg | fromjson | select(.type == "notice")] | length' "$1"
}

# Waits until `$1` holds `$2` notice frames.
wait_for_frames() {
  for _ in $(seq 1 150); do
    [[ "$(notice_frames "$1")" -ge "$2" ]] && return 0
    kill -0 "$capture_pid" 2>/dev/null || break
    sleep 0.2
  done
  fail "the stream sent fewer than $2 notice frames: $(cat "$1" 2>/dev/null)"
}

step "build the node"
[ -n "${ENGINE_PREBUILT:-}" ] || (cd "$root" && cargo build -p engine-node --locked --quiet)
engine="$(cargo metadata --format-version 1 --no-deps --manifest-path "$root/Cargo.toml" | jq -r .target_directory)/debug/engine"

step "write the node config"
sed -e "s|^listen = \"127.0.0.1:8080\"|listen = \"127.0.0.1:$api_port\"|" \
  -e "s|^listen = \"127.0.0.1:9464\"|listen = \"127.0.0.1:$metrics_port\"|" \
  -e "s|^log_dir = .*|log_dir = \"$work/log\"|" \
  -e "s|^dir = \"data/dev/history\"|dir = \"$work/history\"|" \
  -e "s|dir = \"data/dev/alerts\"|dir = \"$work/alerts\"|" \
  "$root/config/dev.toml" > "$work/node.toml"
for edit in ":$api_port\"" ":$metrics_port\"" "$work/log\"" "$work/history\""; do
  grep -qF "$edit" "$work/node.toml" || fail "config edit missing: $edit"
done
chain_id="$(awk -F' = ' '/^chain_id = / { print $2; exit }' "$work/node.toml")"
vault="$(awk -F'"' '/^vault = / { print $2; exit }' "$work/node.toml")"

# The first trusted time sample may wait max_drift for a slow Roughtime server; genesis follows it.
max_drift="$(awk -F'"' '/^max_drift = / { print $2 }' "$root/config/dev.toml")"
[[ "$max_drift" =~ ^[0-9]+s$ ]] || fail "expected time.max_drift in whole seconds, got '$max_drift'"
ready_within="$(( ${max_drift%s} + 15 ))"

# Waits until the node signs an answer. A restarted node serves its recovered markets before its first
# trusted time sample, and signs only once trusted time bounds the host clock, so an answer asked for
# earlier is unsigned.
wait_until_signed() {
  local started
  started="$(date +%s)"
  until curl -sk -D "$work/signed.headers" -o /dev/null -H "X-Engine-Challenge: $(challenge)" "$api/v1/markets" 2>/dev/null \
    && grep -qi '^x-engine-time:' "$work/signed.headers"; do
    (( $(date +%s) - started <= ${ready_within} )) || fail "no signed answer within ${ready_within}s of boot"
    kill -0 "$node_pid" 2>/dev/null || fail "the node exited"
    sleep 0.2
  done
}

# Runs the node on the config and waits until it serves its markets and signs, then reads what a client pins.
start_node() {
  "$engine" run --config "$work/node.toml" >> "$work/node.log" 2>&1 &
  node_pid=$!
  local started
  started="$(date +%s)"
  until [[ "$(curl -sk "$api/v1/markets" 2>/dev/null | jq 'if type == "array" then length else 0 end' 2>/dev/null)" =~ ^[1-9] ]]; do
    (( $(date +%s) - started <= ready_within )) || fail "no markets within ${ready_within}s of boot"
    kill -0 "$node_pid" 2>/dev/null || fail "the node exited"
    sleep 0.2
  done
  wait_until_signed
  signer="$(curl -sfk "$api/v1/attestation" | jq -r .enclave_address)"
  pin="$(curl -sfk "$api/v1/attestation" | jq -r '.tls_public_key_sha256 | ltrimstr("0x")')"
  [[ "$signer" =~ ^0x[0-9a-f]{40}$ && "$pin" =~ ^[0-9a-f]{64}$ ]] || fail "no signer or pin in the attestation"
  answer=(--signer "$signer" --chain-id "$chain_id" --vault "$vault")
}

stop_node() {
  kill -TERM "$node_pid"
  wait "$node_pid" || fail "node exited with an error"
  node_pid=""
}

# `engine admin` as the operator.
operate() { OPERATOR_KEY="$operator_key" "$engine" admin --config "$work/node.toml" --node "$api" --pin "$pin" "$@"; }

# GET /v1/notice with a fresh challenge, checked with `engine verify-response`; prints the body.
read_notice() {
  local sent
  sent="$(challenge)"
  curl -sfk -D "$work/headers" -o "$work/body" -H "X-Engine-Challenge: $sent" "$api/v1/notice" \
    || fail "GET /v1/notice failed"
  "$engine" verify-response "${answer[@]}" --challenge "$sent" --path /v1/notice \
    --headers "$work/headers" --body "$work/body" > /dev/null || fail "the notice answer did not verify"
  cat "$work/body"
}

step "run the node and wait for genesis"
: > "$work/node.log"
start_node

step "with no notice the signed answer is null"
[[ "$(read_notice | jq -c .)" == '{"notice":null}' ]] || fail "unexpected answer: $(cat "$work/body")"

step "open a signed stream with notice=1: its first notice frame is null"
streamed="$(challenge)"
path="/v1/stream?signed=1&notice=1&challenge=$streamed"
: > "$work/frames"
capture "$path" "$work/frames" 4 &
capture_pid=$!
wait_for_frames "$work/frames" 1

step "set a notice: the signed answer carries its text and the time it was set"
first="Security maintenance: withdrawals are paused."
operate set-notice "$first" > /dev/null || fail "set-notice failed"
read="$(read_notice)"
[[ "$(jq -r .notice.text <<< "$read")" == "$first" ]] || fail "the notice is not the one set: $read"
set_at="$(jq -r .notice.set_at_ms <<< "$read")"
[[ "$set_at" =~ ^[0-9]+$ ]] || fail "no set time: $read"
wait_for_frames "$work/frames" 2

step "replace it, then clear it: a frame follows each change"
second="Withdrawals have resumed. Trading was not affected."
operate set-notice "$second" > /dev/null || fail "replacing the notice failed"
[[ "$(read_notice | jq -r .notice.text)" == "$second" ]] || fail "the notice was not replaced"
wait_for_frames "$work/frames" 3
operate clear-notice > /dev/null || fail "clear-notice failed"
[[ "$(read_notice | jq -c .)" == '{"notice":null}' ]] || fail "the notice was not cleared"
wait "$capture_pid" || fail "the stream capture failed"
capture_pid=""

step "the stream's frames verify and carry null, the text, its replacement and null"
jq -se '[.[] | .msg | fromjson | select(.type == "notice") | .notice.text // null] == [null, $first, $second, null]' \
  --arg first "$first" --arg second "$second" "$work/frames" > /dev/null \
  || fail "unexpected notice frames: $(cat "$work/frames")"
"$engine" verify-frames "${answer[@]}" --challenge "$streamed" --path "$path" --frames "$work/frames" \
  || fail "the stream did not verify"

step "text a notice may not hold is refused before it is signed"
long="$(printf 'a%.0s' $(seq 1 281))"
operator_address="$(curl -sfk "$api/v1/operator" | jq -r .operator)"
nonce() { curl -sfk "$api/v1/accounts/$operator_address/nonce" | jq -r .next_nonce; }
nonce_before="$(nonce)"
[[ "$nonce_before" =~ ^[0-9]+$ ]] || fail "no operator nonce: $nonce_before"
for text in "" "$long" "<b>paused</b>" "see https://example.com" "$(printf 'two\xe2\x80\xa8lines')"; do
  if operate set-notice "$text" > "$work/refused" 2>&1; then
    fail "the notice '${text:0:30}' was signed"
  fi
  # The client's own check names the text; an engine refusal would say it rejected the command.
  grep -q "the notice" "$work/refused" && ! grep -q "rejected the command" "$work/refused" \
    || fail "unexpected refusal of '${text:0:30}': $(cat "$work/refused")"
done
[[ "$(nonce)" == "$nonce_before" ]] || fail "a refused notice spent the operator's nonce"
[[ "$(read_notice | jq -c .)" == '{"notice":null}' ]] || fail "a refused notice reached the node"
step "clearing with no notice in force is refused by the engine"
if operate clear-notice > "$work/refused" 2>&1; then fail "a clear with no notice in force applied"; fi
grep -q "no notice is in force" "$work/refused" || fail "unexpected refusal: $(cat "$work/refused")"

step "a notice survives a restart, with the time it was set"
operate set-notice "$first" > /dev/null || fail "set-notice failed"
before="$(read_notice)"
stop_node
start_node
after="$(read_notice)"
[[ "$(jq -c .notice <<< "$after")" == "$(jq -c .notice <<< "$before")" ]] \
  || fail "the notice changed across the restart: $before then $after"
[[ "$(jq -r .notice.text <<< "$after")" == "$first" ]] || fail "the notice is not the one set: $after"

step "clear it after the restart"
operate clear-notice > /dev/null || fail "clear-notice failed"
[[ "$(read_notice | jq -c .)" == '{"notice":null}' ]] || fail "the notice was not cleared"

step "stop the node and replay its log"
stop_node
"$engine" replay --log "$work/log" > "$work/replay.log" 2>&1 || { cat "$work/replay.log" >&2; fail "the log did not replay"; }

echo "PASS: the notice is set, streamed, kept across a restart and cleared; every answer verifies"
