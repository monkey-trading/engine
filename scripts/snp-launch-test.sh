#!/usr/bin/env bash
# Checks the QEMU command scripts/snp-launch.sh builds, with --dry-run, on a fixture image: the launch
# flags image/README.md gives for an SEV-SNP host, taken from measurement.json, and the refusals of an
# image whose files do not match its SHA256SUMS, of a measurement without Secure TSC, and of a vCPU
# count that would change the launch digest. Needs jq and sha256sum; touches nothing outside a
# temporary directory.
#
# Usage: scripts/snp-launch-test.sh

set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
failures=0
launch="$root/scripts/snp-launch.sh"

# An image directory as scripts/build-image.sh writes it, with stand-in bytes.
image="$work/image"
mkdir "$image"
for file in OVMF.fd vmlinuz initramfs.cpio.gz; do echo "stand-in $file" > "$image/$file"; done
cat > "$image/measurement.json" << 'EOF'
{
  "cmdline": "console=ttyS0 ip=dhcp panic=-1 no-kvmclock",
  "guest_features": "0x201",
  "launch_digest": "000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000",
  "policy": "0x30000",
  "variant": "dev",
  "vcpu_type": "EPYC-v4",
  "vcpus": 4,
  "vmm_type": "QEMU"
}
EOF
(cd "$image" && sha256sum OVMF.fd initramfs.cpio.gz measurement.json vmlinuz > SHA256SUMS)
touch "$work/data.img" "$work/log.img" "$work/bench.cpio.gz"

check() {
  local name="$1" want="$2" got
  shift 2
  if got=$("$@" 2> /dev/null) && [[ "$got" == "$want" ]]; then
    echo "ok: $name"
  else
    printf 'FAIL: %s\n  want: %s\n  got:  %s\n' "$name" "$want" "$got" >&2
    failures=$((failures + 1))
  fi
}
refused() {
  local name="$1" why="$2" got
  shift 2
  if got=$("$@" 2>&1); then
    echo "FAIL: $name was not refused: $got" >&2
    failures=$((failures + 1))
  elif ! grep -q -- "$why" <<< "$got"; then
    echo "FAIL: $name was refused without '$why': $got" >&2
    failures=$((failures + 1))
  else
    echo "ok: $name"
  fi
}

# image/README.md's command, with the README's own file names replaced by the fixture's paths.
# shellcheck disable=SC2054 # QEMU's options separate their values with commas.
readme=(
  qemu-system-x86_64 -enable-kvm -cpu EPYC-v4 -smp 4 -m 16G -nographic -no-reboot
  -machine q35,confidential-guest-support=sev0
  -object sev-snp-guest,id=sev0,policy=0x30000,cbitpos=51,reduced-phys-bits=1,kernel-hashes=on,secure-tsc=on
  -bios "$image/OVMF.fd" -kernel "$image/vmlinuz" -initrd "$image/initramfs.cpio.gz"
  -append "console=ttyS0 ip=dhcp panic=-1 no-kvmclock"
  -drive "if=none,id=data,format=raw,file=$work/data.img"
  -device virtio-blk-pci,drive=data,disable-legacy=on,iommu_platform=on
  -drive "if=none,id=log,format=raw,file=$work/log.img"
  -device virtio-blk-pci,drive=log,disable-legacy=on,iommu_platform=on
  -netdev user,id=net0,hostfwd=tcp:127.0.0.1:8081-:8080,hostfwd=tcp:0.0.0.0:9472-:9472
  -device virtio-net-pci,netdev=net0,disable-legacy=on,iommu_platform=on,romfile=
)
check "the launch flags from measurement.json" "$(printf '%q ' "${readme[@]}")" \
  "$launch" --dry-run --memory 16G --forward 8081:8080 --forward 0.0.0.0:9472:9472 "$image" "$work/data.img" "$work/log.img"

# The benchmark guest has no log disk: its own init mounts only the data disk (readme[26..29] are the log's).
bench=("${readme[@]:0:26}" "${readme[@]:30}")
bench[5]=8
bench[19]="$work/bench.cpio.gz"
bench[27]=user,id=net0
check "the benchmark guest's initramfs and vCPUs" "$(printf '%q ' "${bench[@]}")" \
  "$launch" --dry-run --memory 16G --initrd "$work/bench.cpio.gz" --smp 8 "$image" "$work/data.img"

refused "a vCPU count without --initrd" "needs --initrd" "$launch" --dry-run --smp 8 "$image" "$work/data.img" "$work/log.img"
refused "a malformed forward" "--forward takes" "$launch" --dry-run --forward 8081 "$image" "$work/data.img" "$work/log.img"
refused "a missing data disk" "no data disk" "$launch" --dry-run "$image" "$work/missing.img" "$work/log.img"
refused "a missing log disk" "no log disk" "$launch" --dry-run "$image" "$work/data.img" "$work/missing.img"
refused "no log disk" "usage:" "$launch" --dry-run "$image" "$work/data.img"

echo "altered" >> "$image/vmlinuz"
refused "a kernel that is not the image's" "do not match its SHA256SUMS" "$launch" --dry-run "$image" "$work/data.img" "$work/log.img"
(cd "$image" && echo "stand-in vmlinuz" > vmlinuz)

sed -i.orig 's/"0x201"/"0x1"/' "$image/measurement.json"
(cd "$image" && sha256sum OVMF.fd initramfs.cpio.gz measurement.json vmlinuz > SHA256SUMS)
refused "a measurement without Secure TSC" "not 0x201" "$launch" --dry-run "$image" "$work/data.img" "$work/log.img"

if ((failures)); then
  echo "$failures case(s) failed" >&2
  exit 1
fi
echo "PASS: the launch command matches the image's measurement and the README's flags"
