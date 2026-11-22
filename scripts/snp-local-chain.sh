#!/usr/bin/env bash
# Sets up one SEV-SNP guest's deployment on a local Anvil chain, as a first deployment does on a real
# one (ops/runbooks/sev-snp-host.md, ops/runbooks/deploy-and-rollback.md):
#   prepare   deploys a mock USDC, a timelock with a one-second delay and a vault whose signer is a
#             placeholder; writes the guest's node.toml from config/anvil.toml with real attestation,
#             paths under /data and the chain reached over TLS at <rpc-host>; has the timelock name
#             the release (the image's launch digest and the config's SHA-256); and writes
#             release.toml, a fresh operator key, chain.env and the data disk's files under disk/;
#   register  has the timelock install <enclave-address>, which the guest's verified attestation
#             names, as the vault's signer.
# The chain holds nothing of value, and the keys that sign on it are Anvil's published ones. The
# guest reads the chain only over HTTPS with a publicly trusted certificate, since a node with real
# attestation refuses plain HTTP off its own machine: <rpc-host> is a DNS name for this host whose
# ports RPC_PORTS reach Anvil through a TLS proxy.
#
# Needs anvil running with --slots-in-an-epoch 1, forge, cast, jq, python3 and the contract
# dependencies (`forge soldeer install` in contracts/). RPC is the Anvil URL on this host
# (http://127.0.0.1:8545); RPC_PORTS the proxy's ports for the three custody providers and the
# checkpoint endpoint ("18545 18546 18547 18548"); RELAYER_INDEX the Anvil account that posts this
# guest's checkpoints (3), one per guest; PROCESSOR the guest's tee.processor (Genoa).
#
# Usage: scripts/snp-local-chain.sh prepare <guest-dir> <image-dir> <rpc-host>
#        scripts/snp-local-chain.sh register <guest-dir> <enclave-address>

set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
rpc="${RPC:-http://127.0.0.1:8545}"
read -r -a rpc_ports <<< "${RPC_PORTS:-18545 18546 18547 18548}"
mnemonic="test test test test test test test test test test test junk"
key() { cast wallet private-key --mnemonic "$mnemonic" --mnemonic-index "$1"; }
# Anvil's account 0 deploys the token and the vault, 1 is the placeholder signer, guardian and the
# timelock's proposer, and 2 deploys the timelock.
deployer_key="$(key 0)"
proposer_key="$(key 1)"
proposer="$(cast wallet address --private-key "$proposer_key")"
zero32=0x0000000000000000000000000000000000000000000000000000000000000000

usage() {
  echo "usage: $0 prepare <guest-dir> <image-dir> <rpc-host> | register <guest-dir> <enclave-address>" >&2
  exit 2
}
die() {
  echo "$*" >&2
  exit 1
}

# Schedules `$2` on target `$1` through the timelock in $timelock, waits out its delay and executes it.
timelock_call() {
  local target="$1" data="$2" salt delay op
  salt="$(cast keccak "snp-local-chain $(date +%s) $RANDOM$RANDOM $data")"
  delay="$(cast call "$timelock" 'getMinDelay()(uint256)' --rpc-url "$rpc")"
  cast send "$timelock" 'schedule(address,uint256,bytes,bytes32,bytes32,uint256)' "$target" 0 "$data" "$zero32" "$salt" "$delay" \
    --private-key "$proposer_key" --rpc-url "$rpc" > /dev/null
  op="$(cast call "$timelock" 'hashOperation(address,uint256,bytes,bytes32,bytes32)(bytes32)' "$target" 0 "$data" "$zero32" "$salt" --rpc-url "$rpc")"
  cast rpc evm_increaseTime "$((delay + 1))" --rpc-url "$rpc" > /dev/null
  cast rpc anvil_mine 1 --rpc-url "$rpc" > /dev/null
  [[ "$(cast call "$timelock" 'isOperationReady(bytes32)(bool)' "$op" --rpc-url "$rpc")" == true ]] || die "timelock operation $op is not ready"
  cast send "$timelock" 'execute(address,uint256,bytes,bytes32,bytes32)' "$target" 0 "$data" "$zero32" "$salt" \
    --private-key "$proposer_key" --rpc-url "$rpc" > /dev/null
  # Three more blocks make the call's block finalized under --slots-in-an-epoch 1.
  cast rpc anvil_mine 3 --rpc-url "$rpc" > /dev/null
}

prepare() {
  local guest="$1" image="$2" host="$3"
  local measurement="$image/measurement.json" relayer_key relayer operator_json operator_key public_key
  [[ "$(jq -r .variant "$measurement")" == dev ]] ||
    die "$measurement is not the dev variant's; the production variant refuses this config's mock price feeds"
  [[ ! -e "$guest/chain.env" ]] || die "$guest already holds a prepared guest; use a new directory"
  mkdir -p "$guest/disk"
  guest="$(cd "$guest" && pwd)"
  measurement="$(cd "$image" && pwd)/measurement.json"
  relayer_key="$(key "${RELAYER_INDEX:-3}")"
  relayer="$(cast wallet address --private-key "$relayer_key")"

  echo "deploying the mock USDC, the timelock and the vault on $rpc" >&2
  cd "$root/contracts"
  create() { forge create --rpc-url "$rpc" --private-key "$1" --broadcast --json "${@:2}" | jq -er .deployedTo; }
  usdc="$(create "$deployer_key" test/mocks/MockUSDC.sol:MockUSDC)"
  timelock="$(create "$(key 2)" dependencies/@openzeppelin-contracts-5.7.0/governance/TimelockController.sol:TimelockController \
    --constructor-args 1 "[$proposer]" '[0x0000000000000000000000000000000000000000]' 0x0000000000000000000000000000000000000000)"
  vault="$(create "$deployer_key" src/Vault.sol:Vault \
    --constructor-args "$usdc" "$proposer" "$timelock" "$proposer" 1000000000000000000 2592000 2592000 2592000)"
  cd "$root"

  # The operator's admin key: a fresh one, since a node with real attestation refuses the published
  # test keys. Only `engine admin` reads it.
  operator_json="$(cast wallet new --json)"
  operator_key="$(jq -er '(.data // .)[0].private_key' <<< "$operator_json")"
  public_key="0x04$(cast wallet public-key --private-key "$operator_key" | sed 's/^0x//')"
  (umask 077 && echo "$operator_key" > "$guest/operator.key")

  python3 -I - "$root/config/anvil.toml" "$guest/node.toml" "$host" "$vault" "$usdc" "$relayer" "$public_key" \
    "${PROCESSOR:-Genoa}" "${rpc_ports[@]}" << 'PY'
import sys
from pathlib import Path
source, target, host, vault, usdc, relayer, public_key, processor, *ports = sys.argv[1:]
text = Path(source).read_text()
old_key = text.split('public_key = "', 1)[1].split('"', 1)[0]
for literal, replacement in {
    '[node]\nlisten = "127.0.0.1:8080"': '[node]\nlisten = "0.0.0.0:8080"',
    '"data/anvil/': '"/data/',
    'provider = "mock"\nuse_published_mock_key = true # Public development identity; forbidden with real attestation.':
        'provider = "configfs_tsm"\nprocessor = "%s"\n\n[tee.sealing]\ndir = "/data/keys"\n\n'
        '# Where the operator delivers secrets, such as the Critical alert route.\n'
        '[tee.sealing.secrets]\nlisten = "0.0.0.0:9472"' % processor,
    old_key: public_key,
    'vault = "0xe7f1725E7734CE288F8367e1Bb143E90bb3F0512"': 'vault = "%s"' % vault,
    'usdc = "0x5FbDB2315678afecb367f032d93F642f64180aa3"': 'usdc = "%s"' % usdc,
    '"http://127.0.0.1:8545"': '"https://%s:%s"' % (host, ports[0]),
    '"http://127.0.0.1:8546"': '"https://%s:%s"' % (host, ports[1]),
    '"http://127.0.0.1:8547"': '"https://%s:%s"' % (host, ports[2]),
    'url_env = "ENGINE_ALERTS_CRITICAL_URL"': 'url_secret = "alerts_critical_url"',
    # Real attestation credits a deposit at `latest`, after the confirmations.
    'finality_tag = "safe"': 'finality_tag = "latest"',
    '[metrics]\nlisten = "127.0.0.1:9464"': '[metrics]\nlisten = "0.0.0.0:9464"',
}.items():
    assert text.count(literal) >= 1, literal
    text = text.replace(literal, replacement)
text += """
# The vault names the release that opens the keys; Anvil signs the relayer's transactions.
[checkpoints]
rpc_url = "https://%s:%s"
relayer = "%s"
retry_interval = "5s"
rpc_timeout = "5s"
""" % (host, ports[3], relayer)
Path(target).write_text(text)
PY
  config_hash="0x$(sha256sum "$guest/node.toml" | cut -d' ' -f1)"
  digest="0x$(jq -er .launch_digest "$measurement")"

  echo "naming the release $digest with config $config_hash" >&2
  timelock_call "$vault" "$(cast calldata 'setCurrentRelease(bytes,bytes32)' "$digest" "$config_hash")"
  [[ "$(cast call "$vault" 'currentRelease()(bytes32)' --rpc-url "$rpc")" == "$(cast keccak "$(cast concat-hex "$digest" "$config_hash")")" ]] ||
    die "the vault does not name the release"

  cat > "$guest/release.toml" << EOF
name = "$(basename "$guest")"
launch_digest = "$digest"
policy = "$(jq -er .policy "$measurement")"
guest_features = "$(jq -er .guest_features "$measurement")"

# The lowest reported TCB this release accepts. Zero accepts any; set each value from AMD's latest
# security bulletin for the processor once the first report has shown the host's.
[minimum_tcb]
boot_loader = 0
tee = 0
snp = 0
microcode = 0

[config_hashes]
"node.toml" = "$config_hash"
EOF
  cat > "$guest/chain.env" << EOF
RPC=$rpc
VAULT=$vault
USDC=$usdc
TIMELOCK=$timelock
RELAYER=$relayer
EOF
  cp "$guest/node.toml" "$guest/disk/config.toml"
  touch "$guest/disk/new-key"
  echo "prepared $guest: vault $vault, release named; make its data disk from $guest/disk" >&2
}

register() {
  local guest="$1" enclave="$2" vault signer
  saved() { awk -F= -v name="$1" '$1 == name { print $2 }' "$guest/chain.env"; }
  rpc="$(saved RPC)"
  timelock="$(saved TIMELOCK)"
  vault="$(saved VAULT)"
  echo "installing $enclave as the signer of $vault" >&2
  timelock_call "$vault" "$(cast calldata 'setSigner(address)' "$enclave")"
  signer="$(cast call "$vault" 'checkpointSigner()(address)' --rpc-url "$rpc")"
  lower() { tr '[:upper:]' '[:lower:]' <<< "$1"; }
  [[ "$(lower "$signer")" == "$(lower "$enclave")" ]] || die "the vault's signer is $signer, not $enclave"
  echo "the vault's signer is $signer" >&2
}

case "${1:-}" in
  prepare) [[ $# -eq 4 ]] || usage; prepare "$2" "$3" "$4" ;;
  register) [[ $# -eq 3 ]] || usage; register "$2" "$3" ;;
  *) usage ;;
esac
