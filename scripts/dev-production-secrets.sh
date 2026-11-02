#!/usr/bin/env bash
# Delivering production secrets into the attested enclave, under the mock TEE (its quotes prove
# nothing and its sealing is a clearly labelled public stand-in): every value here is an obviously
# fake FAKE-… string. A node on config/anvil.toml's chain names three RPC provider keys and two alert
# webhook URLs as delivered secrets, and a vendor key no bundle ever holds, with the environment
# holding decoys for what the config no longer reads from it:
#   1. with nothing sealed the node holds, serving only its delivery listener;
#   2. a bundle for another release or config, and one the operator did not sign, are refused;
#   3. the bundle is encrypted to the node's attested one-time key and delivered; the node goes on
#      starting, a deposit credits through the keyed providers, and the webhook is paged;
#   4. a restart opens the sealed bundle with no delivery, and both are in use again;
#   5. a rotation reaching the running node is sealed at once and says it needs a restart, what runs
#      keeps its keys until then, and afterwards only the new key and webhook are used;
#   6. a host's older sealed copy shows in the attested version and a new delivery repairs it, and a
#      sealed file that no longer opens is an alert and a hold until the operator delivers again.
# Passes when each of those holds, the log replays, and no value appears in the node's logs, its
# public log, its other data, its metrics, or a page.
#
# Needs anvil, forge, cast, curl, jq, and python3, and the contract dependencies (`forge soldeer
# install` in contracts/). Runs on private temporary data and its own free ports, with no real secret;
# a synthetic signed access source clears the deposits, as the vault deposit scenario's does.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
work="$(mktemp -d)"
mkdir -p "$work/operator" "$work/stand-ins"
# Anvil's default test mnemonic: account 0 deploys, 1 trades, 2 funds the timelock's deployment.
mnemonic="test test test test test test test test test test test junk"
key() { cast wallet private-key --mnemonic "$mnemonic" --mnemonic-index "$1"; }
deployer_key="$(key 0)"
trader_key="$(key 1)"
house_key="$(key 2)"
trader="$(cast wallet address --private-key "$trader_key" | tr '[:upper:]' '[:lower:]')"
usdc_units=1000000
digest="$(printf '5a%.0s' $(seq 1 48))"
node_pid=""
access_fixture_pid=""
pids=()

read -r anvil_port api_port metrics_port listen_port hook_port rpc_a rpc_b rpc_c ACCESS_SOURCE_PORT <<< "$(python3 -I - <<'PY'
import socket
probes = [socket.socket() for _ in range(9)]
for probe in probes:
    probe.bind(("127.0.0.1", 0))
print(*(probe.getsockname()[1] for probe in probes))
PY
)"
rpc="http://127.0.0.1:$anvil_port"
api="https://127.0.0.1:$api_port"

export ACCESS_SOURCE_PORT

cleanup() {
  [[ -n "$node_pid" ]] && kill "$node_pid" 2>/dev/null || true
  [[ -n "$access_fixture_pid" ]] && kill "$access_fixture_pid" 2>/dev/null || true
  for pid in "${pids[@]}"; do kill "$pid" 2>/dev/null || true; done
  if [[ -n "${SCENARIO_LOG_DIR:-}" ]]; then
    mkdir -p "$SCENARIO_LOG_DIR"
    cp "$work"/*.log "$SCENARIO_LOG_DIR/" 2>/dev/null || true
  fi
  rm -rf "$work"
}
trap cleanup EXIT

step() { echo "==> $*"; }
fail() {
  echo "FAIL: $*" >&2
  [[ -f "$work/node.log" ]] && grep -vE ' (TRACE|DEBUG) ' "$work/node.log" | tail -n 40 >&2
  exit 1
}

# Polls `$1` (a shell function name) until it prints `$2`, for up to 40 seconds.
wait_for() {
  local got=""
  for _ in $(seq 1 200); do
    got="$("$1")"
    [[ "$got" == "$2" ]] && return 0
    sleep 0.2
  done
  fail "$1: expected $2, got $got"
}

trader_balance() { curl -sk "$api/v1/accounts/$trader" | jq -r '.balance // "none"'; }
pending_deposits() { curl -sf "http://127.0.0.1:$metrics_port/metrics" | awk '$1 == "engine_access_pending_deposits" { print $2 }'; }
secret_missing_alerts() { grep -c 'ERROR.*alert="secret_missing"' "$work/node.log" || true; }
health() { curl -sk -o /dev/null -w '%{http_code}' "$api/v1/health" || true; }
mine() { cast rpc anvil_mine "$(printf '0x%x' "$1")" --rpc-url "$rpc" >/dev/null; }
send() { cast send "$@" --rpc-url "$rpc" >/dev/null; }
deposit() { send "$vault" "deposit(uint256)" $(($1 * usdc_units)) --private-key "$trader_key"; mine 3; }

# The pages a path of the webhook stand-in received, as `<alert> <status>` lines.
pages() { jq -r --arg path "/hooks/$1" 'select(.path == $path) | "\(.body.alert) \(.body.status)"' "$work/stand-ins/pages.jsonl" 2>/dev/null || true; }
paged() { pages "$hook_path" | grep -cx "$page" || true; }
wait_for_page() {
  hook_path="$1" page="$2"
  for _ in $(seq 1 200); do
    [[ "$(paged)" != 0 ]] && return 0
    sleep 0.2
  done
  fail "$1 was never paged $2: $(pages "$1" | tr '\n' ';')"
}

# Starts the node, `$@` its extra flags, without waiting for its API.
launch() {
  # Decoys: the config reads none of these, and a value from them must never be used.
  MASSIVE_API_KEY="FAKE-environment-decoy" ENGINE_ALERTS_CRITICAL_URL="http://127.0.0.1:$hook_port/hooks/FAKE-environment-decoy" \
    "$engine" run --config "$work/node.toml" --mock-launch-digest "$digest" "$@" >>"$work/node.log" 2>&1 &
  node_pid=$!
}
wait_up() {
  for _ in $(seq 1 300); do
    kill -0 "$node_pid" 2>/dev/null || fail "node exited at startup"
    [[ "$(health)" == 200 ]] && return 0
    sleep 0.2
  done
  fail "node did not come up"
}
stop_node() {
  kill -TERM "$node_pid"
  wait "$node_pid" || fail "node did not shut down cleanly"
  node_pid=""
}
wait_listening() {
  for _ in $(seq 1 150); do
    kill -0 "$node_pid" 2>/dev/null || fail "node exited while holding"
    python3 -I -c "import socket,sys; socket.create_connection(('127.0.0.1', $listen_port), 1).close()" 2>/dev/null && return 0
    sleep 0.2
  done
  fail "the delivery listener did not come up"
}

# `engine admin`, on `$config`, keeping what it prints.
admin() {
  # shellcheck disable=SC2086 # admin_flags is empty or one flag.
  "$engine" admin --config "${config:-$work/node.toml}" --key-file "${key_file:-$work/operator/operator.key}" \
    ${admin_flags:-} "$@" >>"$work/tool.log" 2>&1
}
deliver() { admin deliver-secrets --to "127.0.0.1:$listen_port" --release "${release:-$work/operator/release.toml}" --secrets-file "$1" --allow-mock; }
status() { admin secrets-status --to "127.0.0.1:$listen_port" --release "$work/operator/release.toml" --allow-mock; }
# Fails unless the last thing the tool printed holds `$1`.
tool_said() { tail -n 8 "$work/tool.log" | grep -q "$1" || fail "the tool did not say $1: $(tail -n 8 "$work/tool.log")"; }
refused() {
  local why="$1"; shift
  if "$@"; then fail "a delivery that should be refused was not: $why"; fi
  tool_said "$why"
}

# Writes a bundle at version `$1`, with keys and webhook paths tagged `$2`.
bundle() {
  cat >"$work/operator/$2.toml" <<TOML
version = $1
[secrets]
rpc_a_key = "FAKE-rpc-a-$2"
rpc_b_key = "FAKE-rpc-b-$2"
rpc_c_key = "FAKE-rpc-c-$2"
alerts_critical_url = "http://127.0.0.1:$hook_port/hooks/FAKE-critical-$2"
alerts_warning_url = "http://127.0.0.1:$hook_port/hooks/FAKE-warning-$2"
TOML
}
# Lists the keys tagged `$@` as valid at each keyed provider, as a rotation's overlap does.
provider_keys() {
  local letter tag
  for letter in a b c; do
    : >"$work/stand-ins/keys-$letter"
    for tag in "$@"; do echo "FAKE-rpc-$letter-$tag" >>"$work/stand-ins/keys-$letter"; done
  done
}
# The keys that have reached the providers since the last call, one line per distinct key.
keys_seen() {
  local letter
  for letter in a b c; do [[ -f "$work/stand-ins/hits-$letter" ]] && sed 's#^/v2/##' "$work/stand-ins/hits-$letter"; done | sort -u
}
forget_keys_seen() { rm -f "$work"/stand-ins/hits-*; }

step "Build the node"
cargo build -q --locked -p engine-node --manifest-path "$root/Cargo.toml"
source "$root/scripts/dev-access-fixture.sh"
build_access_tools
engine="$(cargo metadata --format-version 1 --no-deps --manifest-path "$root/Cargo.toml" | jq -r .target_directory)/debug/engine"

step "Start Anvil and deploy the mock USDC and the vault"
anvil --port "$anvil_port" --slots-in-an-epoch 1 --silent &
pids+=("$!")
for _ in $(seq 1 50); do cast block-number --rpc-url "$rpc" >/dev/null 2>&1 && break; sleep 0.2; done
cd "$root/contracts"
create() { forge create --rpc-url "$rpc" --private-key "$deployer_key" --broadcast --json "$@" | jq -r '.deployedTo'; }
usdc="$(create test/mocks/MockUSDC.sol:MockUSDC)"
governance="$(forge create --rpc-url "$rpc" --private-key "$house_key" --broadcast --json \
  dependencies/@openzeppelin-contracts-5.7.0/governance/TimelockController.sol:TimelockController \
  --constructor-args 1 "[$trader]" '[0x0000000000000000000000000000000000000000]' \
  0x0000000000000000000000000000000000000000 | jq -r '.deployedTo')"
vault="$(create src/Vault.sol:Vault --constructor-args "$usdc" "$trader" "$governance" "$trader" 1000000000000000000 \
  2592000 2592000 2592000)"
cd "$root"
grep -qi "vault = \"$vault\"" config/anvil.toml || fail "vault deployed at $vault, not the address config/anvil.toml names"
send "$usdc" "mint(address,uint256)" "$trader" $((1000000 * usdc_units)) --private-key "$deployer_key"
send "$usdc" "approve(address,uint256)" "$vault" $((1000000 * usdc_units)) --private-key "$trader_key"

start_access_fixture

step "Start the keyed RPC providers and the webhook stand-in"
provider_keys v1
for pair in "a:$rpc_a" "b:$rpc_b" "c:$rpc_c"; do
  python3 -I "$root/scripts/dev-rpc-proxy.py" --port "${pair#*:}" --target "$rpc" \
    --keys-file "$work/stand-ins/keys-${pair%%:*}" --hits-file "$work/stand-ins/hits-${pair%%:*}" >/dev/null 2>&1 &
  pids+=("$!")
done
python3 -I - "$hook_port" "$work/stand-ins/pages.jsonl" >/dev/null 2>&1 <<'PY' &
import http.server, json, sys
port, out = int(sys.argv[1]), sys.argv[2]
class Hook(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["content-length"])))
        with open(out, "a") as pages:
            pages.write(json.dumps({"path": self.path, "body": body}) + "\n")
        self.send_response(200)
        self.end_headers()
    def log_message(self, *_):
        pass
http.server.HTTPServer(("127.0.0.1", port), Hook).serve_forever()
PY
pids+=("$!")

step "Write a config that names every secret and holds none, and the release that lists it"
python3 -I - "$root/config/anvil.toml" "$work" "$rpc_a" "$rpc_b" "$rpc_c" "$api_port" "$metrics_port" "$listen_port" >"$work/node.toml" <<'PY'
import re, sys
from pathlib import Path
source, work, rpc_a, rpc_b, rpc_c, api, metrics, listen = sys.argv[1:]
text = Path(source).read_text()
for literal, replacement in {
    "data/anvil": work,
    "127.0.0.1:8080": "127.0.0.1:" + api,
    "127.0.0.1:9464": "127.0.0.1:" + metrics,
    '"http://127.0.0.1:8545"': '"http://127.0.0.1:%s/v2/{rpc_a_key}"' % rpc_a,
    '"http://127.0.0.1:8546"': '"http://127.0.0.1:%s/v2/{rpc_b_key}"' % rpc_b,
    '"http://127.0.0.1:8547"': '"http://127.0.0.1:%s/v2/{rpc_c_key}"' % rpc_c,
    'url_env = "ENGINE_ALERTS_CRITICAL_URL"': 'url_secret = "alerts_critical_url"\n[alerts.warning]\nurl_secret = "alerts_warning_url"',
    'level = "info"': 'level = "trace"',
    'use_published_mock_key = true # Public development identity; forbidden with real attestation.':
        'use_published_mock_key = false\n[tee.sealing]\ndir = "%s/keys"\n[tee.sealing.secrets]\nlisten = "127.0.0.1:%s"' % (work, listen),
    'provider = "mock"\ninterval': 'provider = "venues"\ninterval',
}.items():
    assert literal in text, literal
    text = text.replace(literal, replacement)
vendor = """
[feeds.venues]
timeout = "10s"
reconnect_min = "1s"
reconnect_max = "10s"
stale_after = "10s"
min_quote_spacing = "100ms"
[[feeds.venues.connections]]
venue = "massive"
url = "wss://socket.massive.com/forex"
api_key_secret = "massive_api_key"
symbols = [{ market = 1, symbol = "C.EURUSD" }]

"""
text = text.replace("# Trusted time. The sequencer", vendor + "# Trusted time. The sequencer", 1)
print(text, end="")
PY
write_access_config "$work/node.toml"
grep -q FAKE "$work/node.toml" && fail "the config holds a value"
release_toml() {
  cat <<TOML
name = "dev-production-secrets"
launch_digest = "0x$1"
policy = "0x0"
[minimum_tcb]
boot_loader = 0
tee = 0
snp = 0
microcode = 0
[config_hashes]
"node.toml" = "0x$2"
TOML
}
sha256() { python3 -I -c 'import hashlib,sys; print(hashlib.sha256(open(sys.argv[1],"rb").read()).hexdigest())' "$1"; }
config_hash="$(sha256 "$work/node.toml")"
release_toml "$digest" "$config_hash" >"$work/operator/release.toml"
printf '0x%s\n' "$(printf '42%.0s' $(seq 1 32))" >"$work/operator/operator.key"
bundle 1 v1

step "With nothing sealed the node holds, serving only its delivery listener"
launch --new-key
wait_listening
[[ "$(health)" != 200 ]] || fail "the API serves without the chain's keys"
wait_for secret_missing_alerts 6
status
tool_said "sealed version none, running version none, lacking \[alerts_critical_url, alerts_warning_url, massive_api_key, rpc_a_key, rpc_b_key, rpc_c_key\]"

step "A bundle for another release or config, and one the operator did not sign, are refused"
release_toml "$(printf '6b%.0s' $(seq 1 48))" "$config_hash" >"$work/operator/other-code.toml"
release="$work/operator/other-code.toml" refused "launch measurement" deliver "$work/operator/v1.toml"
release_toml "$digest" "$(printf 'ab%.0s' $(seq 1 32))" >"$work/operator/other-list.toml"
release="$work/operator/other-list.toml" refused "lists no config" deliver "$work/operator/v1.toml"
cp "$work/node.toml" "$work/operator/other.toml"
echo "# another config" >>"$work/operator/other.toml"
release_toml "$digest" "$(sha256 "$work/operator/other.toml")" >"$work/operator/other-listed.toml"
config="$work/operator/other.toml" release="$work/operator/other-listed.toml" \
  refused "the enclave runs config" deliver "$work/operator/v1.toml"
printf '0x%s\n' "$(printf '77%.0s' $(seq 1 32))" >"$work/operator/stranger.key"
key_file="$work/operator/stranger.key" admin_flags="--rotated" \
  refused "not the operator key in effect" deliver "$work/operator/v1.toml"
status
tool_said "sealed version none"
[[ "$(health)" != 200 ]] || fail "a refused bundle ended the hold"

step "The bundle is encrypted to the node's attested key and delivered, and the node goes on starting"
deliver "$work/operator/v1.toml"
tool_said "sealed: version 1"
tool_said "these integrations stay off: massive_api_key"
wait_up

step "A deposit credits through the keyed providers, and the delivered webhook is paged"
deposit 1000
wait_for pending_deposits 1
access_attest "$trader_key"
wait_for trader_balance 1000
[[ "$(keys_seen | tr '\n' ' ')" == "FAKE-rpc-a-v1 FAKE-rpc-b-v1 FAKE-rpc-c-v1 " ]] || fail "providers saw $(keys_seen)"
wait_for_page FAKE-critical-v1 "secret_missing firing"

step "The vendor whose key no bundle holds stays off, and nothing fell back to the environment"
grep -q 'ERROR.*alert="secret_missing".*secret=massive_api_key' "$work/node.log" || fail "no alert for the vendor key"
grep -q "starting venue feed" "$work/node.log" && fail "the vendor connection started without its delivered key"
grep -q "FAKE-environment-decoy" "$work/stand-ins/pages.jsonl" && fail "a page went to the environment's webhook"

step "A restart opens the sealed bundle with no delivery, and both are in use again"
stop_node
cp "$work"/keys/*.secrets.json "$work/operator/sealed-v1.json"
sealed_file="$(ls "$work"/keys/*.secrets.json)"
# A fresh outbox, so the alert that stays open pages again.
rm -rf "$work/alerts"
: >"$work/stand-ins/pages.jsonl"
forget_keys_seen
launch
wait_up
deposit 100
wait_for trader_balance 1100
[[ "$(keys_seen | tr '\n' ' ')" == "FAKE-rpc-a-v1 FAKE-rpc-b-v1 FAKE-rpc-c-v1 " ]] || fail "providers saw $(keys_seen)"
wait_for_page FAKE-critical-v1 "secret_missing firing"
status
tool_said "sealed version 1, running version 1"

step "A rotation reaching the running node is sealed, says it needs a restart, and changes nothing that runs"
provider_keys v1 v2
bundle 2 v2
deliver "$work/operator/v2.toml"
tool_said "sealed: version 2"
tool_said "restart required"
wait_for_page FAKE-warning-v1 "secrets_restart_required firing"
status
tool_said "sealed version 2, running version 1"
forget_keys_seen
deposit 10
wait_for trader_balance 1110
[[ "$(keys_seen | tr '\n' ' ')" == "FAKE-rpc-a-v1 FAKE-rpc-b-v1 FAKE-rpc-c-v1 " ]] || fail "what runs changed its keys: $(keys_seen)"

step "After a restart only the rotated key and webhook are used, with the old key revoked"
stop_node
provider_keys v2
forget_keys_seen
: >"$work/stand-ins/pages.jsonl"
launch
wait_up
deposit 10
wait_for trader_balance 1120
[[ "$(keys_seen | tr '\n' ' ')" == "FAKE-rpc-a-v2 FAKE-rpc-b-v2 FAKE-rpc-c-v2 " ]] || fail "providers saw $(keys_seen)"
wait_for_page FAKE-warning-v2 "secrets_restart_required resolved"
status
tool_said "sealed version 2, running version 2"

step "A host's older sealed copy shows in the attested version, and a new delivery repairs it"
stop_node
cp "$work/operator/sealed-v1.json" "$sealed_file"
provider_keys v1 v2 v3
launch
wait_up
status
tool_said "sealed version 1, running version 1"
bundle 3 v3
deliver "$work/operator/v3.toml"
tool_said "sealed: version 3"
tool_said "restart required"

step "A sealed file that no longer opens is an alert and a hold, until the operator delivers again"
stop_node
echo "the host altered this" >"$sealed_file"
launch
wait_listening
status
tool_said "sealed version none"
grep -q 'ERROR.*alert="secrets_unreadable"' "$work/node.log" || fail "no alert for the sealed file"
[[ "$(health)" != 200 ]] || fail "the API serves without the chain's keys"
deliver "$work/operator/v3.toml"
tool_said "sealed: version 3"
wait_up
provider_keys v3
forget_keys_seen
deposit 10
wait_for trader_balance 1130
[[ "$(keys_seen | tr '\n' ' ')" == "FAKE-rpc-a-v3 FAKE-rpc-b-v3 FAKE-rpc-c-v3 " ]] || fail "providers saw $(keys_seen)"
curl -sf "http://127.0.0.1:$metrics_port/metrics" >"$work/metrics.log" || fail "the metrics did not answer"
stop_node

step "The log replays"
"$engine" replay --log "$work/log" >"$work/replay.log" 2>&1 || fail "the log does not replay: $(tail -n 5 "$work/replay.log")"

step "No value appears in the node's logs, its public log, its other data, its metrics, or a page"
grep -q "sealed a delivered secrets bundle" "$work/node.log" || fail "the node log holds none of the delivery's lines"
for path in node.log tool.log replay.log metrics.log node.toml log history alerts keys custody-cursor.json; do
  if grep -rqa FAKE "$work/$path"; then fail "a value appears in $path: $(grep -rla FAKE "$work/$path" | head -n 3)"; fi
done
jq -e 'select(.body | tostring | contains("FAKE"))' "$work/stand-ins/pages.jsonl" >/dev/null && fail "a page's body holds a value"

echo "PASS: delivered, sealed, restarted, rotated, restored, and repaired, with no value outside the enclave"
