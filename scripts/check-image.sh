#!/usr/bin/env bash
# The enclave image checks that need no SEV-SNP hardware. Boots the image under plain QEMU (no
# SEV-SNP, so no hardware quote) with config/dev.toml on its data disk, then checks by variant:
# - production: the engine refuses the config's mock providers and the guest powers off;
# - dev: /v1/attestation serves a bundle that hashes to its report data and names the enclave
#   address, the config file's SHA-256, and the TLS key, a client pinned to that key connects, and
#   a boot with the log, and the alert outbox that must sit beside it, under / or /dev, not /data,
#   fails on the read-only mount and powers off.
# The config comes from this checkout, so check an image built from it: an engine from another
# commit may not accept this checkout's config/dev.toml. Needs qemu-system-x86_64, mke2fs, curl,
# jq, xxd, and sha256sum.
#
# Usage: scripts/check-image.sh <image-dir>

set -euo pipefail

image=$(cd "${1:?usage: $0 <image-dir>}" && pwd)
cd "$(dirname "$0")/.."

variant=$(jq -r .variant "$image/measurement.json")
cmdline=$(jq -r .cmdline "$image/measurement.json")
port=18080
work=$(mktemp -d)
qemu_pid=
deadline=$((SECONDS + 600))

cleanup() {
  if [[ -n "$qemu_pid" ]]; then
    kill "$qemu_pid" 2> /dev/null || true
    wait "$qemu_pid" 2> /dev/null || true
  fi
  rm -rf "$work"
}
trap cleanup EXIT

fail() {
  echo "FAIL: $1" >&2
  echo "--- guest console (last 40 lines) ---" >&2
  tail -n 40 "$work/console.log" >&2 || true
  exit 1
}

# Boots the image in the background with the config read from stdin on its data disk. The guest's
# API listens on every interface so the forwarded port reaches it; the metrics port stays on
# loopback.
boot() {
  rm -rf "$work/data" "$work/data.img" "$work/console.log"
  mkdir "$work/data"
  awk '/^\[/ { section = $0 } section == "[node]" && /^listen = / { $0 = "listen = \"0.0.0.0:8080\"" } 1' \
    > "$work/data/config.toml"
  mke2fs -q -t ext4 -d "$work/data" "$work/data.img" 64M
  qemu-system-x86_64 -machine "q35,accel=$accel" -m 1G -smp 2 -display none -monitor none -no-reboot \
    -serial "file:$work/console.log" \
    -kernel "$image/vmlinuz" -initrd "$image/initramfs.cpio.gz" -append "$cmdline" \
    -drive "if=virtio,format=raw,file=$work/data.img" \
    -netdev "user,id=net0,hostfwd=tcp:127.0.0.1:$port-:8080" -device virtio-net-pci,netdev=net0,romfile= &
  qemu_pid=$!
}

# Waits for the engine to exit and init to power the guest off, which ends QEMU.
await_poweroff() {
  while kill -0 "$qemu_pid" 2> /dev/null; do
    ((SECONDS < deadline)) || fail "the guest is still running"
    sleep 2
  done
  qemu_pid=
  grep -q 'powering off' "$work/console.log" || fail "the guest stopped without init powering it off"
}

# Another listener on the port would answer in this guest's place.
if (exec 3<> "/dev/tcp/127.0.0.1/$port") 2> /dev/null; then
  fail "port $port is already in use"
fi

accel=tcg
if [[ -w /dev/kvm ]]; then accel=kvm; fi

case $variant in
  production)
    boot < config/dev.toml
    await_poweroff
    grep -q 'a production build refuses mock providers' "$work/console.log" ||
      fail "the engine did not refuse the mock providers"
    echo "ok: the production image refuses mock providers and powers off"
    ;;
  dev)
    boot < config/dev.toml
    api="https://127.0.0.1:$port"
    # The certificate is self-signed, so this skips the CA check; the pinned request below checks it.
    until attestation=$(curl -fsSk --max-time 5 "$api/v1/attestation" 2> /dev/null); do
      kill -0 "$qemu_pid" 2> /dev/null || fail "the guest stopped"
      ((SECONDS < deadline)) || fail "the API did not come up"
      sleep 2
    done
    kill -0 "$qemu_pid" 2> /dev/null || fail "the guest stopped, so another process answered"
    field() { jq -r ".$1" <<< "$attestation" | cut -c3-; }
    bundle=$(field bundle)
    config_hash=$(sha256sum "$work/data/config.toml" | cut -d' ' -f1)
    [[ "$(xxd -r -p <<< "$bundle" | sha256sum | cut -d' ' -f1)$(printf '0%.0s' {1..64})" == "$(field report_data)" ]] ||
      fail "report data is not the bundle's SHA-256 and 32 zero bytes: $attestation"
    tag=$(printf 'engine.attestation.v1' | xxd -p)
    [[ $bundle == "$tag$(field enclave_address)$config_hash$(field tls_public_key_sha256)" ]] ||
      fail "the bundle is not the tag, the address, the config file's SHA-256, and the TLS key hash: $attestation"
    [[ $(field config_sha256) == "$config_hash" ]] || fail "config_sha256 is not the config file's SHA-256"
    pin=$(xxd -r -p <<< "${bundle:146:64}" | base64)
    curl -fsS --max-time 5 --insecure --pinnedpubkey "sha256//$pin" "$api/v1/health" > /dev/null ||
      fail "a client pinned to the bundle's TLS key cannot connect"
    echo "ok: the dev image attests config 0x$config_hash and serves TLS under the key its bundle names"

    kill "$qemu_pid"
    wait "$qemu_pid" 2> /dev/null || true
    # The config refuses an alert outbox that is not beside the log, so it moves with it.
    for parent in "" /dev; do
      boot < <(sed -e "s|^log_dir = .*|log_dir = \"$parent/log\"|" \
        -e "s|^outbox = { dir = \"[^\"]*\"|outbox = { dir = \"$parent/alerts\"|" config/dev.toml)
      await_poweroff
      grep -q 'Read-only file system' "$work/console.log" ||
        fail "the engine did not fail to write under ${parent:-/}"
      echo "ok: the dev image refuses a log directory and alert outbox under ${parent:-/} and powers off"
    done
    ;;
  *)
    fail "unknown variant: $variant"
    ;;
esac
