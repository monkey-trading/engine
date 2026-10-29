#!/usr/bin/env bash
# Dev scenario for the client address behind a pass-through proxy, on a node with config/dev.toml's
# mocks, a synthetic signed access source, and `[api.proxy_protocol] trusted = ["::1/128"]`. A
# loopback forwarder stands in for the proxy: it connects from ::1, prepends a PROXY protocol v2
# header naming the client its port stands for, and relays the bytes without reading them. Checked
# with curl:
#   1. two client addresses behind the forwarder are limited separately, and a request limit spent by
#      one leaves the other served;
#   2. an account whose declaration comes from an address the source places in the United States is
#      refused in the public log, then admitted from an allowed address, and its order is refused
#      from the US address and filled from the allowed one, whatever X-Forwarded-For says;
#   3. a connection from outside the trusted range that sends a header is closed, and so is one from
#      the forwarder that sends none.
#
# Needs python3, curl, and jq, and the public Roughtime servers config/dev.toml names. API_PORT,
# METRICS_PORT, ACCESS_SOURCE_PORT, and FORWARDER_PORT override 18480, 19864, 18846, and 18490 (the
# forwarder takes that port and the next five); SCENARIO_ARTIFACTS keeps the logs.
set -Eeuo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
work="$(mktemp -d)"
api_port="${API_PORT:-18480}"
metrics_port="${METRICS_PORT:-19864}"
source_port="${ACCESS_SOURCE_PORT:-18846}"
fwd="${FORWARDER_PORT:-18490}"
vault="0x0000000000000000000000000000000000000000"
# Anvil's published account 1; the node's mock TEE accepts published keys.
trader_key="59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d"
trader="0x70997970C51812dc3A010C7d01b50e0d17dc79C8"
node_pid=""
source_pid=""
forwarder_pid=""

cleanup() {
  for pid in "$node_pid" "$source_pid" "$forwarder_pid"; do
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

# The forwarder's ports, each the proxy in front of one client address.
allowed_a=$fwd; allowed_b=$((fwd + 1)); us=$((fwd + 2)); spare_a=$((fwd + 3)); spare_b=$((fwd + 4)); headerless=$((fwd + 5))
allowed_a_ip="203.0.113.7"; allowed_b_ip="203.0.113.9"; us_ip="198.51.100.5"

step "build the node and the scenario's tools"
cargo build -q --locked -p engine-node --manifest-path "$root/Cargo.toml"
source "$root/scripts/dev-access-fixture.sh"
build_access_tools
engine="$(cargo metadata --format-version 1 --no-deps --manifest-path "$root/Cargo.toml" | jq -r .target_directory)/debug/engine"

step "start the access source, which places $us_ip in the United States and the other client addresses nowhere"
printf '{"mode":"clear","jurisdictions":{},"sanctioned":[],"peers":{"%s":"allowed","%s":"allowed","%s":"united_states"}}\n' \
  "$allowed_a_ip" "$allowed_b_ip" "$us_ip" > "$work/access-fixture.json"
"$access_target/debug/examples/access_source" "127.0.0.1:$source_port" 31337 "$vault" "$work/access-fixture.json" \
  > "$work/access-source.log" 2>&1 &
source_pid=$!

step "write the node config: dual-stack listener, proxy range ::1, access source, a small request burst"
sed -e "s|127.0.0.1:8080|[::]:$api_port|" \
  -e "s|127.0.0.1:9464|127.0.0.1:$metrics_port|" \
  -e "s|data/dev|$work|g" \
  -e 's|^per_ip = .*|per_ip = { per_second = 2, burst = 40 }|' \
  "$root/config/dev.toml" > "$work/node.toml"
cat >> "$work/node.toml" <<EOF

[api.proxy_protocol]
trusted = ["::1/128"]

[access]
source = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"
endpoint = "http://127.0.0.1:$source_port"
timeout = "2s"
max_age = "30s"
max_response_bytes = 4096
max_in_flight = 8
EOF
for edit in "[::]:$api_port" "burst = 40" 'trusted = ["::1/128"]'; do
  grep -qF "$edit" "$work/node.toml" || fail "config edit missing: $edit"
done

step "start the forwarder: each port prepends the header for its client; the last prepends none"
python3 - "$fwd" "$api_port" "$allowed_a_ip" "$allowed_b_ip" "$us_ip" 203.0.113.50 203.0.113.51 none <<'PY' > "$work/forwarder.log" 2>&1 &
import asyncio, random, socket, struct, sys

first_port, node_port, *claims = int(sys.argv[1]), int(sys.argv[2]), *sys.argv[3:]
SIGNATURE = b"\r\n\r\n\x00\r\nQUIT\n"


def header(client):
    # Version 2, command PROXY, TCP over IPv4: the client, then the proxy-side address.
    addresses = socket.inet_aton(client) + socket.inet_aton("192.0.2.1")
    ports = struct.pack("!HH", random.randint(1024, 65535), 443)
    return SIGNATURE + bytes([0x21, 0x11]) + struct.pack("!H", 12) + addresses + ports


async def pipe(reader, writer):
    try:
        while data := await reader.read(65536):
            writer.write(data)
            await writer.drain()
    except OSError:
        pass
    finally:
        writer.close()


def forward(client):
    async def handle(reader, writer):
        upstream_reader, upstream_writer = await asyncio.open_connection("::1", node_port)
        if client != "none":
            upstream_writer.write(header(client))
        await asyncio.gather(pipe(reader, upstream_writer), pipe(upstream_reader, writer))

    return handle


async def main():
    for offset, client in enumerate(claims):
        await asyncio.start_server(forward(client), "127.0.0.1", first_port + offset)
    await asyncio.Event().wait()


asyncio.run(main())
PY
forwarder_pid=$!

step "start the node"
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

direct="https://127.0.0.1:$api_port"
via() { echo "https://127.0.0.1:$1"; }
status() { curl -sk --max-time 5 -o /dev/null -w '%{http_code}' "$@"; }
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
marked() { curl --max-time 5 -sk "$direct/v1/markets" | jq -r '[.[]? | select(.id == 1) | .mark | values] | first // "none"'; }
mark_set() { [[ "$(marked)" == none ]] && echo no || echo yes; }
wait_for mark_set "" yes

# A signed request body for the trader: kind, then the unsigned body. The signing nonce and
# expiry come from the node and the clock.
signed() {
  local nonce expires_at
  nonce="$(curl --max-time 5 -skf "$direct/v1/accounts/$trader/nonce" | jq -er .next_nonce)"
  expires_at="$(python3 -c 'import time; print(int(time.time() * 1000) + 60000)')"
  SIGNER_KEY="$trader_key" "$access_signer" 31337 "$vault" "$nonce" "$expires_at" "$1" "$2"
}
# Posts `$3` to path `$2` behind forwarder port `$1`. The forwarding headers claim loopback and an
# allowed address; only the address in the proxy's header counts.
post_via() {
  curl --max-time 10 -skf "$(via "$1")$2" -H 'content-type: application/json' \
    -H 'x-forwarded-for: 127.0.0.1' -H 'forwarded: for=203.0.113.7' -d "$3"
}
trader_balance() {
  curl --max-time 5 -sk "$direct/v1/accounts/$trader" | jq -r '.balance // "none" | if . == "none" then . else tonumber | floor | tostring end'
}

step "an unclaimed account's deposit waits for a declaration; one from a US address is refused in the log"
events="$(curl --max-time 10 -skf "$direct/v1/dev/deposit" -H 'content-type: application/json' \
  -d "{\"account\":\"$trader\",\"amount\":\"1000\"}")"
jq -e 'any(.[]; .type == "deposit_pending")' <<< "$events" > /dev/null || fail "the deposit did not wait: $events"
declaration="{\"account\":\"$trader\",\"class\":\"professional\"}"
events="$(post_via "$us" /v1/eligibility "$(signed eligibility "$declaration")")"
jq -e 'length == 1 and .[0].type == "eligibility_rejected" and .[0].reason.reason == "united_states"' <<< "$events" \
  > /dev/null || fail "a declaration behind the proxy from a US address was not refused: $events"
expect "balance after the refused declaration" "$(trader_balance)" none

step "the same declaration from an allowed address is recorded and the deposit credited"
events="$(post_via "$allowed_a" /v1/eligibility "$(signed eligibility "$declaration")")"
jq -e 'any(.[]; .type == "eligibility_attested")' <<< "$events" > /dev/null || fail "an allowed declaration failed: $events"
wait_for trader_balance "" 1000

step "an order is refused from the US address and filled from an allowed one"
mark="$(marked)"
order="{\"account\":\"$trader\",\"client_id\":1,\"market\":1,\"side\":\"buy\",\"qty\":\"0.01\",\"kind\":{\"kind\":\"market\",\"quoted\":\"$mark\"}}"
events="$(post_via "$us" /v1/orders "$(signed order "$order")")"
jq -e 'any(.[]; .type == "order_rejected" and .reason.kind == "access" and .reason.reason == "united_states")' \
  <<< "$events" > /dev/null || fail "an order from the US address was not refused: $events"
events="$(post_via "$allowed_b" /v1/orders "$(signed order "$order")")"
jq -e 'any(.[]; .type == "fill")' <<< "$events" > /dev/null || fail "an order from an allowed address did not fill: $events"

step "clients behind one proxy have their own request limits"
limited=""
for i in $(seq 1 120); do
  if [[ "$(status "$(via "$spare_a")/v1/health")" == 429 ]]; then limited=$i; break; fi
done
[[ -n "$limited" ]] || fail "a client behind the proxy was never limited"
expect "the limited client, again" "$(status "$(via "$spare_a")/v1/health")" 429
expect "another client behind the same proxy" "$(status "$(via "$spare_b")/v1/health")" 200
expect "a direct client" "$(status "$direct/v1/health")" 200
expect "the first client, from the proxy's own address" "$(status "$(via "$allowed_a")/v1/health")" 200

step "a header from outside the trusted range is refused, and a trusted peer without one is too"
python3 - "$api_port" "$us_ip" <<'PY'
import socket, struct, sys

port, client = int(sys.argv[1]), sys.argv[2]
header = (
    b"\r\n\r\n\x00\r\nQUIT\n"
    + bytes([0x21, 0x11])
    + struct.pack("!H", 12)
    + socket.inet_aton(client)
    + socket.inet_aton("192.0.2.1")
    + struct.pack("!HH", 40000, 443)
)
# From 127.0.0.1, which is not the proxy: the header is not read as one, and the connection ends.
for payload in (header, b"PROXY TCP4 %s 192.0.2.1 40000 443\r\n" % client.encode()):
    with socket.create_connection(("127.0.0.1", port), timeout=5) as conn:
        conn.sendall(payload)
        try:
            data = conn.recv(4096)
        except ConnectionResetError:
            data = b""
        # An alert at most, then the end of the stream; never a response.
        assert b"HTTP/" not in data, data
        conn.settimeout(5)
        try:
            while conn.recv(4096):
                pass
        except ConnectionResetError:
            pass
PY
if curl -sk --max-time 5 -o /dev/null "$(via "$headerless")/v1/health"; then fail "a trusted peer without a header was served"; fi
expect "the forwarder's other ports" "$(status "$(via "$allowed_a")/v1/health")" 200
grep -q 'alert="api_proxy_header_refused"' "$work/node.log" || fail "a trusted peer's missing header raised no alert"
if grep -Eq "$us_ip|$allowed_a_ip" "$work"/log/[0-9]*.jsonl; then fail "a client address leaked into the public log"; fi

echo "PASS: per-client limits and screening use the proxied address; forged headers are refused"
