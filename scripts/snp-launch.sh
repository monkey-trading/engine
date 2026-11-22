#!/usr/bin/env bash
# Launches the enclave image as an AMD SEV-SNP guest with Secure TSC (ops/runbooks/sev-snp-host.md).
# The vCPU count and type, guest policy and kernel command line come from the image's
# measurement.json, which the launch digest was computed for, so the guest's attestation report
# carries measurement.json's launch_digest. The image's files are checked against its SHA256SUMS
# first. QEMU runs in the foreground with the guest's serial console on standard output, and exits
# when the guest powers off, which it does when the engine exits.
#
# Usage: scripts/snp-launch.sh [options] <image-dir> <data-disk> <log-disk>
# The log disk is the guest's second virtio disk, which init mounts at /data/log, apart from the data disk's
# device. With --initrd, whose own init mounts what it needs, the data disk alone is attached.
#   --memory <size>          guest memory in QEMU's syntax (4G); the launch digest does not cover it
#   --forward <host>:<guest> forward TCP from [address:]port on the host (address 127.0.0.1 unless
#                            given) to the guest's port; repeatable
#   --initrd <file>          boot this initramfs instead of the image's, as the benchmark guest does;
#                            the launch digest then differs from measurement.json
#   --smp <count>            with --initrd: this many vCPUs instead of measurement.json's
#   --dry-run                print the QEMU command instead of running it
# QEMU names the QEMU binary (qemu-system-x86_64), which needs the sev-snp-guest object's secure-tsc
# property (scripts/snp-host-check.sh checks it).

set -euo pipefail

usage() {
  echo "usage: $0 [--memory <size>] [--forward [<address>:]<port>:<guest-port>]... [--initrd <file> [--smp <count>]] [--dry-run] <image-dir> <data-disk> [<log-disk>]" >&2
  exit 2
}

memory=4G
forwards=()
initrd=
smp=
dry_run=
args=()
while [[ $# -gt 0 ]]; do
  case $1 in
    --memory) memory=${2:?}; shift 2 ;;
    --forward) forwards+=("${2:?}"); shift 2 ;;
    --initrd) initrd=${2:?}; shift 2 ;;
    --smp) smp=${2:?}; shift 2 ;;
    --dry-run) dry_run=1; shift ;;
    -*) echo "unknown option: $1" >&2; usage ;;
    *) args+=("$1"); shift ;;
  esac
done
want=3
[[ -z "$initrd" ]] || want=2
[[ ${#args[@]} -eq $want ]] || usage
image=${args[0]}
disk=${args[1]}
log_disk=${args[2]-}
[[ -z "$smp" || -n "$initrd" ]] || { echo "--smp changes the launch digest, so it needs --initrd" >&2; exit 2; }
[[ -f "$disk" ]] || { echo "no data disk at $disk" >&2; exit 1; }
if [[ -z "$initrd" && ! -f "$log_disk" ]]; then
  echo "no log disk at $log_disk" >&2
  exit 1
fi

# Every file the launch reads must be the one the image's build hashed.
(cd "$image" && sha256sum --quiet -c SHA256SUMS) || { echo "the files in $image do not match its SHA256SUMS" >&2; exit 1; }

measurement="$image/measurement.json"
field() { jq -er ".$1" "$measurement"; }
[[ "$(field vmm_type)" == QEMU ]] || { echo "measurement.json is for VMM $(field vmm_type), not QEMU" >&2; exit 1; }
# SEV_FEATURES 0x201 is SNPActive and SecureTSC, which secure-tsc=on gives each vCPU.
[[ "$(field guest_features)" == 0x201 ]] || { echo "measurement.json's guest_features are $(field guest_features), not 0x201" >&2; exit 1; }
vcpus=$(field vcpus)
vcpu_type=$(field vcpu_type)
policy=$(field policy)
cmdline=$(field cmdline)

hostfwd=
for forward in ${forwards[@]+"${forwards[@]}"}; do
  IFS=: read -r -a parts <<< "$forward"
  case ${#parts[@]} in
    2) hostfwd+=",hostfwd=tcp:127.0.0.1:${parts[0]}-:${parts[1]}" ;;
    3) hostfwd+=",hostfwd=tcp:${parts[0]}:${parts[1]}-:${parts[2]}" ;;
    *) echo "--forward takes [<address>:]<port>:<guest-port>, not $forward" >&2; exit 2 ;;
  esac
done

if [[ -n "$initrd" ]]; then
  echo "booting $initrd: the launch digest will not be measurement.json's" >&2
fi
log_drive=()
# shellcheck disable=SC2054 # QEMU's options separate their values with commas.
if [[ -n "$log_disk" ]]; then
  log_drive=(-drive "if=none,id=log,format=raw,file=$log_disk"
    -device virtio-blk-pci,drive=log,disable-legacy=on,iommu_platform=on)
fi
# shellcheck disable=SC2054 # QEMU's options separate their values with commas.
command=(
  "${QEMU:-qemu-system-x86_64}" -enable-kvm -cpu "$vcpu_type" -smp "${smp:-$vcpus}" -m "$memory" -nographic -no-reboot
  -machine q35,confidential-guest-support=sev0
  -object "sev-snp-guest,id=sev0,policy=$policy,cbitpos=51,reduced-phys-bits=1,kernel-hashes=on,secure-tsc=on"
  -bios "$image/OVMF.fd" -kernel "$image/vmlinuz" -initrd "${initrd:-$image/initramfs.cpio.gz}"
  -append "$cmdline"
  -drive "if=none,id=data,format=raw,file=$disk"
  -device virtio-blk-pci,drive=data,disable-legacy=on,iommu_platform=on
  ${log_drive[@]+"${log_drive[@]}"}
  -netdev "user,id=net0$hostfwd"
  -device virtio-net-pci,netdev=net0,disable-legacy=on,iommu_platform=on,romfile=
)
if [[ -n "$dry_run" ]]; then
  printf '%q ' "${command[@]}"
  echo
  exit 0
fi
exec "${command[@]}"
