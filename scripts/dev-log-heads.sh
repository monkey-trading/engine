#!/usr/bin/env bash
# Dev scenario for on-chain log heads (#541), on a local Anvil chain with a node on a config
# derived from config/anvil.toml:
#   1. posts: with [log_heads] configured, the node's poster releases its newest log head on chain;
#   2. clean restart: a node recovering a log that holds the posted head starts normally;
#   3. truncation refused: a log cut below the posted head is refused at startup with Critical
#      `log_rollback`, naming the missing range;
#   4. runtime freeze: a second copy on the same (published mock) enclave key posting a head this
#      node never released freezes sequencing: Critical `log_rollback` and the node stops.
#
# The vault is deployed with the published mock enclave key (0x5975c1…, config/test-keys.toml) as
# its checkpoint signer — the same identity the node's mock TEE signs log heads with — so the
# LogHeads contract's signature check passes.
#
# Needs anvil, forge, cast, cargo, curl, jq, and python3, plus the contract dependencies (`forge
# soldeer install` in contracts/) and the public Roughtime servers config/anvil.toml names. Set
# ENGINE_PREBUILT=1 to skip the cargo build, as CI's scenario lane does. ANVIL_PORT,
# RPC_{A,B,C}_PORT, API_PORT, and METRICS_PORT override 19545, 19546-19548, 19443, and 19464.
# SCENARIO_LOG_DIR keeps the logs.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
work="$(mktemp -d)"
anvil_port="${ANVIL_PORT:-19545}"
api_port="${API_PORT:-19443}"
metrics_port="${METRICS_PORT:-19464}"
rpc_a_port="${RPC_A_PORT:-19546}"
rpc_b_port="${RPC_B_PORT:-19547}"
rpc_c_port="${RPC_C_PORT:-19548}"
rpc="http://127.0.0.1:$anvil_port"
api="https://127.0.0.1:$api_port"
# Anvil's default test mnemonic: account 0 deploys, 3 relays checkpoints, 4 relays log heads,
# 5 relays the overtaking post of the fourth leg.
mnemonic="test test test test test test test test test test test junk"
key() { cast wallet private-key --mnemonic "$mnemonic" --mnemonic-index "$1"; }
mine() { cast rpc anvil_mine "$(printf '0x%x' "$1")" --rpc-url "$rpc" >/dev/null; }
deployer_key="$(key 0)"
house_key="$(key 2)"
checkpoint_relayer="0x90F79bf6EB2c4f870365E785982E1f101E93b906"
heads_relayer="0x15d34AAf54267DB7D7c367839AAf71A00a2C6A65"
junk_relayer_key="$(key 5)"
# The published mock enclave identity (32 bytes of 0x43, config/test-keys.toml): the vault's
# checkpoint signer and the key whose log-head signatures LogHeads verifies.
mock_key="0x4343434343434343434343434343434343434343434343434343434343434343"
mock_enclave="0x5975c152fE58cDcB7E25586A3c9b994A16dbB615"
anvil_pid=""
node_pid=""
proxy_pids=()

# A closed local port stands in for the required Critical route; the alert-routing scenario tests
# delivery separately.
export ENGINE_ALERTS_CRITICAL_URL="http://127.0.0.1:9/alerts"

cleanup() {
  [[ -n "$node_pid" ]] && { kill "$node_pid" 2>/dev/null || true; wait "$node_pid" 2>/dev/null || true; }
  [[ -n "$anvil_pid" ]] && { kill "$anvil_pid" 2>/dev/null || true; wait "$anvil_pid" 2>/dev/null || true; }
  for pid in ${proxy_pids[@]+"${proxy_pids[@]}"}; do
    kill "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
  done
  if [[ -n "${SCENARIO_LOG_DIR:-}" ]]; then
    mkdir -p "$SCENARIO_LOG_DIR"
    cp "$work"/*.log "$SCENARIO_LOG_DIR/" 2>/dev/null || true
    cp "$work"/*.toml "$SCENARIO_LOG_DIR/" 2>/dev/null || true
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

# Fails on an alert in the node log that is not in $1 (a `|` list), except a Warning a retry clears
# (an RPC provider that has not caught up): that one is tolerated and logged when it resolves
# within ALERT_CLEAR_SECONDS, and fails when it persists past it.
alert_clear_seconds="${ALERT_CLEAR_SECONDS:-5}"
audit_alerts() {
  local out status=0 deadline=$((SECONDS + alert_clear_seconds + 2))
  while :; do
    out="$(python3 -I "$root/scripts/dev-alert-audit.py" audit "$work/node.log" --allow "$1" \
      --transient 'custody_rpc_provider_failed|custody_rpc_provider_disagreed|custody_rpc_failed|custody_chain_unsettled' \
      --window "$alert_clear_seconds")" && status=0 || status=$?
    # Young and still open: give it the rest of its window to clear.
    if [[ "$status" == 2 && "$SECONDS" -lt "$deadline" ]]; then sleep 1; continue; fi
    break
  done
  grep '^tolerated' <<< "$out" || true
  [[ "$status" == 0 ]] || fail "unexpected alert $2: $out"
}

for port in "$anvil_port" "$rpc_a_port" "$rpc_b_port" "$rpc_c_port" "$api_port" "$metrics_port"; do
  if (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null; then fail "port $port is in use"; fi
done

step "Build the node (skipped when ENGINE_PREBUILT names CI's artifact build)"
[ -n "${ENGINE_PREBUILT:-}" ] || cargo build -q --locked -p engine-node --manifest-path "$root/Cargo.toml"
# Wherever Cargo put it: `CARGO_TARGET_DIR` or `build.target-dir` may move the target directory.
engine="$(cargo metadata --format-version 1 --no-deps --manifest-path "$root/Cargo.toml" | jq -r .target_directory)/debug/engine"

step "Start Anvil; with one slot per epoch its safe block trails the head by one"
anvil --port "$anvil_port" --slots-in-an-epoch 1 --silent &
anvil_pid=$!
anvil_up=""
for _ in $(seq 1 50); do
  kill -0 "$anvil_pid" 2>/dev/null || fail "anvil exited at startup"
  cast block-number --rpc-url "$rpc" >/dev/null 2>&1 && anvil_up=yes && break
  sleep 0.2
done
[[ -n "$anvil_up" ]] || fail "anvil did not come up"

step "Deploy the mock USDC, the vault (checkpoint signer: the mock enclave key), and LogHeads"
cd "$root/contracts"
# `--constructor-args` takes every argument after it, so the contract and its arguments go last.
create() { forge create --rpc-url "$rpc" --private-key "$deployer_key" --broadcast --json "$@" | jq -r '.deployedTo'; }
usdc="$(create test/mocks/MockUSDC.sol:MockUSDC)"
# A separate local deployer preserves the token/Vault addresses at account 0's nonces 0/1, exactly
# as scripts/dev-vault-deposits.sh deploys them, so config/anvil.toml's addresses still hold.
governance="$(forge create --rpc-url "$rpc" --private-key "$house_key" --broadcast --json \
  dependencies/@openzeppelin-contracts-5.7.0/governance/TimelockController.sol:TimelockController \
  --constructor-args 1 "[0x70997970C51812dc3A010C7d01b50e0d17dc79C8]" '[0x0000000000000000000000000000000000000000]' \
  0x0000000000000000000000000000000000000000 | jq -r '.deployedTo')"
# The guardian is the timelock's proposer. The MONKEY period budget, 1 MONKEY, is a development
# value, not an approved production parameter. The exit timings are thirty days each, past
# anything this waits for. Unlike dev-vault-deposits.sh, the checkpoint signer is the published
# mock enclave identity, the key the node's mock TEE signs log heads with.
vault="$(create src/Vault.sol:Vault --constructor-args "$usdc" "$mock_enclave" "$governance" \
  0x70997970C51812dc3A010C7d01b50e0d17dc79C8 1000000000000000000 2592000 2592000 2592000)"
logheads="$(create src/LogHeads.sol:LogHeads --constructor-args "$vault")"
cd "$root"
# The node reads the vault at the finalized tag, which trails Anvil's head by two blocks with one
# slot per epoch: advance past the deployments so the tag names a block that holds their code.
mine 16
grep -qi "vault = \"$vault\"" config/anvil.toml || fail "vault deployed at $vault, not the address config/anvil.toml names"
grep -qi "usdc = \"$usdc\"" config/anvil.toml || fail "USDC deployed at $usdc, not the address config/anvil.toml names"
signer="$(cast call "$vault" "checkpointSigner()(address)" --rpc-url "$rpc" | tr '[:upper:]' '[:lower:]')"
[[ "$signer" == "$(tr '[:upper:]' '[:lower:]' <<< "$mock_enclave")" ]] \
  || fail "the vault's checkpoint signer is $signer, not the mock enclave key"
echo "vault $vault, log heads $logheads, checkpoint signer $signer"

step "Start three local RPC proxies for the custody quorum"
for port in "$rpc_a_port" "$rpc_b_port" "$rpc_c_port"; do
  python3 "$root/scripts/dev-rpc-proxy.py" --port "$port" --target "$rpc" >"$work/proxy-$port.log" 2>&1 &
  proxy_pids+=("$!")
  for _ in $(seq 1 100); do
    kill -0 "${proxy_pids[${#proxy_pids[@]}-1]}" 2>/dev/null || fail "proxy exited"
    cast block-number --rpc-url "http://127.0.0.1:$port" >/dev/null 2>&1 && break
    sleep 0.1
  done
done

step "Write the node's config: anvil.toml's rewrites plus [checkpoints] and [log_heads]"
# Rewrite original config tokens once so destination ports cannot cascade into later matches.
python3 - "$root/config/anvil.toml" "$work" "$rpc_a_port" "$rpc_b_port" "$rpc_c_port" "$api_port" "$metrics_port" >"$work/anvil.toml" <<'PY_CONFIG'
import re
import sys
from pathlib import Path

source, work, rpc_a, rpc_b, rpc_c, api, metrics = sys.argv[1:]
replacements = {
    "data/anvil": work,
    "127.0.0.1:8545": "127.0.0.1:" + rpc_a,
    "127.0.0.1:8546": "127.0.0.1:" + rpc_b,
    "127.0.0.1:8547": "127.0.0.1:" + rpc_c,
    "127.0.0.1:8080": "127.0.0.1:" + api,
    "127.0.0.1:9464": "127.0.0.1:" + metrics,
    # Small segments give the truncation leg a listed prefix to cut back to.
    "segment_commands = 1000": "segment_commands = 2",
}
pattern = "|".join(map(re.escape, replacements))
print(re.sub(pattern, lambda match: replacements[match[0]], Path(source).read_text()), end="")
PY_CONFIG
cat >> "$work/anvil.toml" <<CONFIG

# The checkpoint endpoint funds the checkpoint relayer's posts; the log-head poster borrows its
# transport and deadlines. Both relayers are Anvil accounts the endpoint manages.
[checkpoints]
rpc_url = "http://127.0.0.1:$anvil_port"
relayer = "$checkpoint_relayer"
retry_interval = "2s"
rpc_timeout = "5s"

[log_heads]
contract = "$logheads"
relayer = "$heads_relayer"
interval = "10s"
CONFIG
for edit in "127.0.0.1:$api_port" "127.0.0.1:$metrics_port" "$work/log" "segment_commands = 2" "[log_heads]" "$logheads"; do
  grep -qF "$edit" "$work/anvil.toml" || fail "config edit missing: $edit"
done

step "The alert audit tolerates a quick-clearing provider warning and fails a persisting one"
python3 -I "$root/scripts/dev-alert-audit.py" selftest

# Leg 3's truncation: the sound cut a host could pass off as its own recovery. $1 is the log
# directory, $2 the posted head's seq; prints the seq the cut log now ends at.
cat > "$work/cut_log.py" <<'PY_CUT'
import json
import sys
from pathlib import Path

log = Path(sys.argv[1])
posted = int(sys.argv[2])
manifest = log / "manifest.jsonl"
listed = [json.loads(line) for line in manifest.read_text().splitlines() if line.strip()]
files = sorted(p.name for p in log.glob("[0-9]*.jsonl"))
# The newest segment is still being written, so the manifest does not list it: drop it whole.
if len(files) > len(listed):
    (log / files[-1]).unlink()
    files.pop()
# Then drop completed segments from the end, with their manifest lines, until the manifest's last
# entry ends below the posted head.
while listed and listed[-1]["last_seq"] >= posted:
    entry = listed.pop()
    (log / entry["file"]).unlink()
    files.pop()
manifest.write_text("".join(json.dumps(entry) + "\n" for entry in listed))
remaining = listed[-1]["last_seq"] if listed else 0
print(remaining)
PY_CUT

# Log-head signatures: the `engine.log-head.v1` digest abi.encodePacked builds, byte-for-byte as
# the node and LogHeads.sol build it. $1 chain id, $2 vault, $3 epoch, $4 seq, $5 link (the
# cumulative command-prefix digest), $6 parent seq, $7 parent link.
cat > "$work/log_head_digest.py" <<'PY_DIGEST'
import sys

prefix = b"engine.log-head.v1"
chain_id = int(sys.argv[1]).to_bytes(8, "big")
vault = bytes.fromhex(sys.argv[2].removeprefix("0x"))
epoch = int(sys.argv[3]).to_bytes(8, "big")
seq = int(sys.argv[4]).to_bytes(8, "big")
link = bytes.fromhex(sys.argv[5].removeprefix("0x"))
parent_seq = int(sys.argv[6]).to_bytes(8, "big")
parent_link = bytes.fromhex(sys.argv[7].removeprefix("0x"))
sys.stdout.write("0x" + (prefix + chain_id + vault + epoch + seq + link + parent_seq + parent_link).hex())
PY_DIGEST

# Starts the node on the scenario's config, logging to $1 (default node.log).
start_node() {
  curl -sk "$api/v1/health" >/dev/null 2>&1 && fail "port $api_port already serves an API"
  # The previous run's last checkpoint post may not yet be visible at the finalized tag, which
  # trails Anvil's head by two blocks; the checkpoint setup waits for that boundary before log
  # recovery. Mine past it so startup proceeds.
  mine 3
  "$engine" run --config "$work/anvil.toml" >>"${1:-$work/node.log}" 2>&1 &
  node_pid=$!
  for _ in $(seq 1 100); do
    kill -0 "$node_pid" 2>/dev/null || fail "node exited at startup"
    curl -skf "$api/v1/health" >/dev/null && return 0
    sleep 0.2
  done
  fail "node did not come up"
}

stop_node() {
  kill -TERM "$node_pid"
  wait "$node_pid" || fail "node did not shut down cleanly"
  node_pid=""
}

vault_epoch() { cast call "$vault" "signerEpoch()(uint64)" --rpc-url "$rpc"; }
# The newest head the LogHeads contract holds for the vault's current signer epoch: seq on the
# first line, the cumulative command-prefix digest on the second. The vault's first signer epoch
# is 1 (0 is reserved).
posted_head() { cast call "$logheads" "heads(uint64)(uint64,bytes32)" "$(vault_epoch)" --rpc-url "$rpc"; }
posted_seq() { posted_head | head -n 1 | tr -d '[:space:]'; }
# The greatest seq the log on disk holds.
last_log_seq() { jq -s 'map(.seq) | max' "$work"/log/[0-9]*.jsonl; }

step "Leg 1 (posts): the node starts and its poster releases the log's head on chain"
start_node
posted=""
for _ in $(seq 1 450); do
  seq_now="$(posted_seq)"
  [[ "$seq_now" =~ ^[0-9]+$ && "$seq_now" -gt 0 ]] && posted="$seq_now" && break
  kill -0 "$node_pid" 2>/dev/null || fail "node exited while posting its head"
  sleep 0.2
done
[[ -n "$posted" ]] || fail "the poster did not post a log head within 90s"
# The poster reads the chain at a majority-agreed latest block: the post is visible to its next
# read without advancing the chain.
echo "PASS: leg 1 (posts): the poster released log head seq $posted on chain at $logheads"
leg1_head="$posted"

step "Leg 2 (clean restart accepts): a node recovering a log that holds the posted head starts"
stop_node
start_node
curl -skf "$api/v1/health" >/dev/null || fail "the restarted node does not serve"
# The restart accepted the log: the on-chain head was within it. The head keeps moving (mock feed
# prices sequence commands), so watch the poster advance the on-chain head across two of its
# posting intervals rather than chase an exact value.
before="$(posted_seq)"
sleep 25
after="$(posted_seq)"
[[ "$before" =~ ^[0-9]+$ && "$after" =~ ^[0-9]+$ && "$after" -gt "$before" ]] \
  || fail "the restarted node's poster did not advance the on-chain head ($before -> $after)"
echo "PASS: leg 2 (clean restart accepts): the recovered log holds the posted head; the node serves"
# A healthy run may post a newer head than the first release (the restart sequences commands of
# its own); the truncation leg cuts below whatever is posted now.
stop_node
# A SIGTERM can land mid-checkpoint-post (the checkpoint poster is not what this scenario tests;
# dev-checkpoints.sh covers it), raising a shutdown-race checkpoint_post_failed Warning.
audit_alerts "checkpoint_post_failed" "after the clean restarts"
posted="$(posted_seq)"
[[ "$posted" =~ ^[0-9]+$ && "$posted" -gt 0 ]] || fail "no posted head to truncate below: '$posted'"
cp -R "$work/log" "$work/log-full"

step "Leg 3 (truncation refused): a log cut below the posted head is refused, naming the range"
# The sound cut a host could pass off as its own recovery: drop the unlisted open tail, then drop
# listed segments, with their manifest lines, until the manifest's last entry is below the posted
# head. Recovery accepts exactly this layout; it no longer holds the posted head.
cut_at="$(python3 "$work/cut_log.py" "$work/log" "$posted")"
[[ "$cut_at" =~ ^[0-9]+$ ]] || fail "the cut did not leave a parseable manifest"
[[ "$cut_at" -lt "$posted" ]] || fail "the cut left seq $cut_at, not below the posted head $posted"
echo "log cut to seq $cut_at; the posted head names $((cut_at + 1))..=$posted"
mine 3
"$engine" run --config "$work/anvil.toml" >"$work/node3.log" 2>&1 &
node_pid=$!
refused=""
for _ in $(seq 1 150); do
  kill -0 "$node_pid" 2>/dev/null || { refused=yes; break; }
  curl -sk "$api/v1/health" >/dev/null 2>&1 && fail "the truncated log started serving"
  sleep 0.2
done
[[ -n "$refused" ]] || fail "the node did not exit on the truncated log"
if wait "$node_pid"; then fail "the refused node exited zero"; fi
node_pid=""
grep -q 'alert="log_rollback"' "$work/node3.log" || fail "no Critical log_rollback in the refusal: $(tail -n 5 "$work/node3.log")"
grep -q "seqs $((cut_at + 1))..=$posted" "$work/node3.log" \
  || fail "the refusal did not name the range $((cut_at + 1))..=$posted: $(tail -n 5 "$work/node3.log")"
echo "PASS: leg 3 (truncation refused): Critical log_rollback names seqs $((cut_at + 1))..=$posted; the node refuses"

step "Leg 4 (runtime freeze): a second copy on the same key overtakes; sequencing freezes"
rm -rf "$work/log"
cp -R "$work/log-full" "$work/log"
start_node "$work/node4.log"
ours="$(last_log_seq)"
[[ "$ours" =~ ^[0-9]+$ ]] || fail "no seq in the restored log"
# A head this node never released: seq ours+1000 over a junk cumulative prefix, extending the real
# confirmed head as its parent (only a first post of the epoch may name none) — far enough ahead
# that this log's own head, which keeps moving on mock feed prices (~13 commands a second), cannot
# overtake it before the poster's next read — signed by the published mock enclave key (the
# vault's checkpoint signer), relayed by any funded account. The digest is
# keccak256(abi.encodePacked("engine.log-head.v1", chainid, vault, epoch, seq, link, parentSeq,
# parentLink)), built byte-for-byte as the node and LogHeads.sol build it; cast wallet sign
# --no-hash signs the raw digest, the same form the enclave key signs.
junk_link="0x$(printf '11%.0s' $(seq 1 32))"
junk_seq=$((ours + 1000))
parent_seq="$(posted_seq)"
parent_link="$(posted_head | tail -n 1 | tr -d '[:space:]')"
data="$(python3 "$work/log_head_digest.py" 31337 "$vault" "$(vault_epoch)" "$junk_seq" "$junk_link" "$parent_seq" "$parent_link")"
digest="$(cast keccak "$data")"
signature="$(cast wallet sign --no-hash --private-key "$mock_key" "$digest")"
[[ "$signature" =~ ^0x[0-9a-f]{130}$ ]] || fail "no 65-byte signature for the overtaking head"
cast send "$logheads" "post(uint64,bytes32,uint64,bytes32,bytes)" "$junk_seq" "$junk_link" "$parent_seq" "$parent_link" "$signature" \
  --private-key "$junk_relayer_key" --rpc-url "$rpc" >/dev/null
# The poster reads at a majority-agreed latest block: the overtaking head is visible to its next
# read without advancing the chain.
frozen=""
for _ in $(seq 1 150); do
  kill -0 "$node_pid" 2>/dev/null || { frozen=yes; break; }
  sleep 0.2
done
[[ -n "$frozen" ]] || fail "the node did not stop after the overtaking head"
if wait "$node_pid"; then fail "the frozen node exited zero"; fi
node_pid=""
grep -q 'alert="log_rollback"' "$work/node4.log" || fail "no Critical log_rollback in the freeze: $(tail -n 5 "$work/node4.log")"
grep -Eq "over seqs [0-9]+\.\.=$junk_seq([^0-9]|\$)" "$work/node4.log" \
  || fail "the freeze did not name the range up to $junk_seq: $(tail -n 5 "$work/node4.log")"
echo "PASS: leg 4 (runtime freeze): an on-chain head at seq $junk_seq over this log's $ours froze sequencing; the node stopped"

step "Summary"
echo "PASS: leg 1 (posts): the poster released log head seq $leg1_head on chain"
echo "PASS: leg 2 (clean restart accepts): a log holding the posted head restarts and serves"
echo "PASS: leg 3 (truncation refused): a log cut below the posted head is refused, naming the range"
echo "PASS: leg 4 (runtime freeze): a second copy on the same key freezes sequencing with Critical log_rollback"
echo "All legs passed; every process this scenario started is stopped."
