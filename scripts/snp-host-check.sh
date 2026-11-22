#!/usr/bin/env bash
# Checks, without changing anything, that this host can launch the enclave image as an AMD SEV-SNP
# guest with Secure TSC (ops/runbooks/sev-snp-host.md): the processor and its generation, the CPUID
# bits for SEV-SNP and Secure TSC, the C-bit position the launch flags name, the SEV-SNP guests the
# firmware leaves room for, a kernel whose KVM offers Secure TSC, SEV-SNP initialised with the IOMMU
# on, the TSC, and a QEMU whose sev-snp-guest object has `secure-tsc`. Prints one line per check
# (`ok`, `warn` or `FAIL`), then the processor name the guest's `tee.processor` and
# `engine verify-attestation --processor` take, and exits 1 if any check failed.
#
# Run it as root: it reads /dev/cpu/0/cpuid (`modprobe cpuid` first), /dev/kvm and the kernel log.
# Needs python3. GUESTS (3) is the number of SEV-SNP guests the host must run at once, GUEST_MEMORY_GIB
# (16) the memory each takes, and QEMU the QEMU binary (qemu-system-x86_64).
#
# For scripts/snp-host-check-test.sh only: SNP_CHECK_ROOT puts every /proc, /sys and /dev path under a
# fixture tree, DMESG names a command that prints the kernel log, and SNP_CHECK_KVM_FEATURES stands in
# for the value /dev/kvm reports.
#
# Usage: sudo scripts/snp-host-check.sh

set -euo pipefail

root="${SNP_CHECK_ROOT:-}"
guests="${GUESTS:-3}"
guest_memory_gib="${GUEST_MEMORY_GIB:-16}"
qemu="${QEMU:-qemu-system-x86_64}"
read -r -a dmesg_cmd <<< "${DMESG:-dmesg}"
failed=0
processor=

ok() { echo "ok   $*"; }
warn() { echo "warn $*"; }
fail() {
  echo "FAIL $*"
  failed=1
}

cpuinfo_field() { awk -F': *' -v key="$1" '$1 ~ "^"key"[[:space:]]*$" { print $2; exit }' "$root/proc/cpuinfo"; }
has_flag() { [[ " $(cpuinfo_field flags) " == *" $1 "* ]]; }

# --- The processor -------------------------------------------------------------------------------
vendor=$(cpuinfo_field vendor_id)
family=$(cpuinfo_field 'cpu family')
model=$(cpuinfo_field model)
if [[ "$vendor" != AuthenticAMD ]]; then
  fail "processor: vendor is '${vendor:-unknown}', not AuthenticAMD"
elif [[ "$family" == 25 ]] && ((model <= 0x0f)); then
  processor=Milan
  ok "processor: AMD family 19h model $(printf '%02xh' "$model"), Milan"
elif [[ "$family" == 25 ]] && { ((model >= 0x10 && model <= 0x1f)) || ((model >= 0xa0 && model <= 0xaf)); }; then
  # Genoa, Bergamo and Siena share the key distribution service's Genoa certificate chain.
  processor=Genoa
  ok "processor: AMD family 19h model $(printf '%02xh' "$model"), Genoa, Bergamo or Siena"
elif [[ "$family" == 26 ]]; then
  fail "processor: AMD family 1Ah (Turin or later): engine verify-attestation does not support its certificates"
else
  fail "processor: AMD family ${family:-?} model ${model:-?} is not an SEV-SNP generation this check knows"
fi

# --- CPUID Fn8000_001F: what the processor supports and what its firmware set aside --------------
# Prints EAX EBX ECX EDX of the leaf, read through the cpuid driver at the leaf's offset.
cpuid_leaf() {
  python3 -I - "$root/dev/cpu/0/cpuid" "$1" <<'PY'
import os, struct, sys
fd = os.open(sys.argv[1], os.O_RDONLY)
print(*struct.unpack("<4I", os.pread(fd, 16, int(sys.argv[2], 0))))
PY
}
if [[ ! -e "$root/dev/cpu/0/cpuid" ]]; then
  fail "cpuid: $root/dev/cpu/0/cpuid is missing; run 'modprobe cpuid' and run this check as root"
elif ! read -r eax ebx _ edx < <(cpuid_leaf 0x8000001f 2> /dev/null); then
  fail "cpuid: cannot read leaf 8000001Fh from $root/dev/cpu/0/cpuid; run this check as root"
else
  bit() { (($1 >> $2 & 1)); }
  for feature in 1:SEV 3:SEV-ES 4:SEV-SNP "8:Secure TSC"; do
    if bit "$eax" "${feature%%:*}"; then
      ok "cpuid: ${feature#*:} supported (Fn8000_001F_EAX bit ${feature%%:*})"
    else
      fail "cpuid: ${feature#*:} not supported (Fn8000_001F_EAX bit ${feature%%:*} is clear)"
    fi
  done
  cbitpos=$((ebx & 0x3f))
  if ((cbitpos == 51)); then
    ok "cpuid: C-bit position 51, as the launch flags' cbitpos=51"
  else
    fail "cpuid: C-bit position $cbitpos; scripts/snp-launch.sh and image/README.md launch with cbitpos=51"
  fi
  # SEV-ES and SEV-SNP guests take ASIDs 1 to MinSevNoEsAsid - 1, which the BIOS sets.
  snp_asids=$((edx > 0 ? edx - 1 : 0))
  if ((snp_asids >= guests)); then
    ok "cpuid: $snp_asids ASIDs for SEV-ES and SEV-SNP guests, at least the $guests needed"
  else
    fail "cpuid: $snp_asids ASIDs for SEV-ES and SEV-SNP guests, under the $guests needed; raise the BIOS's SEV-ES ASID space limit"
  fi
fi

# --- The kernel ----------------------------------------------------------------------------------
release=$(cat "$root/proc/sys/kernel/osrelease")
IFS=.- read -r major minor _ <<< "$release"
if ((major > 6 || (major == 6 && minor >= 18))); then
  ok "kernel: $release, at least 6.18, whose KVM offers Secure TSC to SEV-SNP guests"
else
  fail "kernel: $release; KVM offers Secure TSC to SEV-SNP guests from Linux 6.18"
fi
if has_flag sev_snp; then
  ok "kernel: SEV-SNP enabled (the sev_snp flag in /proc/cpuinfo)"
else
  fail "kernel: no sev_snp flag in /proc/cpuinfo; enable SMEE and SEV-SNP in the BIOS"
fi
read -r -a cmdline < "$root/proc/cmdline"
for word in "${cmdline[@]}"; do
  case "$word" in
    iommu=pt | iommu.passthrough=1 | amd_iommu=off | iommu=off) fail "kernel: the command line has $word, and SEV-SNP needs the IOMMU in translated mode" ;;
  esac
done
snp_param="$root/sys/module/kvm_amd/parameters/sev_snp"
if [[ "$(cat "$snp_param" 2> /dev/null)" == Y ]]; then
  ok "kvm_amd: sev_snp=Y"
else
  fail "kvm_amd: sev_snp is '$(cat "$snp_param" 2> /dev/null || echo missing)', not Y; load kvm_amd with SEV-SNP enabled in the BIOS"
fi
for device in kvm sev; do
  if [[ -e "$root/dev/$device" ]]; then ok "device: /dev/$device present"; else fail "device: /dev/$device missing"; fi
done

# --- The kernel log ------------------------------------------------------------------------------
if ! log=$("${dmesg_cmd[@]}" 2> /dev/null); then
  fail "kernel log: '${dmesg_cmd[*]}' failed; run this check as root"
  log=
fi
if firmware=$(grep -o 'SEV-SNP API:[0-9]*\.[0-9]* build:[0-9]*' <<< "$log" | tail -n 1); then
  ok "firmware: $firmware"
else
  warn "firmware: no 'SEV-SNP API' line in the kernel log, so its version is unknown"
fi
while IFS= read -r line; do
  if [[ -n "$line" ]]; then fail "kernel log: ${line#*] }"; fi
done < <(grep -E 'SNP cannot be supported|SEV-SNP disabled|SEV: failed' <<< "$log" || true)
if asids=$(grep -o 'SEV-SNP enabled (ASIDs [0-9]* - [0-9]*)' <<< "$log" | tail -n 1); then
  ok "kvm_amd: $asids"
fi
if mhz=$(grep -oE 'tsc: (Refined TSC clocksource calibration:|Detected) [0-9]+' <<< "$log" | tail -n 1 | grep -oE '[0-9]+$'); then
  # KVM launches a Secure TSC guest at the host's TSC frequency, and the guest refuses under 1000 MHz.
  if ((mhz >= 1000)); then
    ok "tsc: $mhz MHz, at least the 1000 MHz the guest requires"
  else
    fail "tsc: $mhz MHz, under the 1000 MHz the guest requires"
  fi
else
  warn "tsc: no frequency in the kernel log"
fi
if has_flag constant_tsc && has_flag nonstop_tsc; then
  ok "tsc: constant_tsc and nonstop_tsc"
else
  fail "tsc: the host's TSC is not invariant (constant_tsc and nonstop_tsc in /proc/cpuinfo)"
fi

# --- KVM: the SEV features it accepts for a guest's VMSA -----------------------------------------
# KVM_GET_DEVICE_ATTR on /dev/kvm, group KVM_X86_GRP_SEV (1), attribute KVM_X86_SEV_VMSA_FEATURES (0).
kvm_vmsa_features() {
  if [[ -n "${SNP_CHECK_KVM_FEATURES:-}" ]]; then
    echo "$SNP_CHECK_KVM_FEATURES"
    return
  fi
  python3 -I - <<'PY'
import ctypes, fcntl, os, struct
KVM_GET_DEVICE_ATTR = 0x4018AEE2  # _IOW(KVMIO, 0xe2, struct kvm_device_attr)
value = ctypes.c_uint64(0)
fd = os.open("/dev/kvm", os.O_RDWR)
fcntl.ioctl(fd, KVM_GET_DEVICE_ATTR, struct.pack("<IIQQ", 0, 1, 0, ctypes.addressof(value)))
print(value.value)
PY
}
if ! features=$(kvm_vmsa_features 2> /dev/null); then
  fail "kvm: cannot read the SEV features KVM accepts from /dev/kvm; run this check as root"
elif ((features >> 9 & 1)); then
  ok "kvm: accepts SecureTSC (bit 9) in a guest's SEV features ($(printf '0x%x' "$features"))"
else
  fail "kvm: does not accept SecureTSC (bit 9) in a guest's SEV features ($(printf '0x%x' "$features")); it needs SEV-SNP initialised and a Secure TSC processor"
fi

# --- QEMU ----------------------------------------------------------------------------------------
if ! version=$("$qemu" --version 2> /dev/null | head -n 1); then
  fail "qemu: $qemu not found; build QEMU as ops/runbooks/sev-snp-host.md describes"
elif "$qemu" -object sev-snp-guest,help 2> /dev/null | grep -q 'secure-tsc'; then
  ok "qemu: $version, whose sev-snp-guest object has secure-tsc"
else
  fail "qemu: $version has no secure-tsc property on its sev-snp-guest object"
fi

# --- Memory --------------------------------------------------------------------------------------
total_kib=$(awk '$1 == "MemTotal:" { print $2 }' "$root/proc/meminfo")
need_gib=$((guests * guest_memory_gib + 8))
if ((total_kib / 1048576 >= need_gib)); then
  ok "memory: $((total_kib / 1048576)) GiB, at least $guests guests of $guest_memory_gib GiB and 8 GiB for the host"
else
  warn "memory: $((total_kib / 1048576)) GiB, under $guests guests of $guest_memory_gib GiB and 8 GiB for the host"
fi

echo
if [[ -n "$processor" ]]; then
  echo "tee.processor = \"$processor\"; verify attestations with --processor $processor"
fi
if ((failed)); then
  echo "FAILED: this host cannot launch the image as an SEV-SNP guest with Secure TSC" >&2
  exit 1
fi
echo "PASSED: this host can launch the image as an SEV-SNP guest with Secure TSC"
