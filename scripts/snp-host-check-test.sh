#!/usr/bin/env bash
# Runs scripts/snp-host-check.sh against fixture hosts: a Siena host ready for the guests passes and
# names the Genoa certificate chain, and each missing piece fails with its own line. The fixtures
# stand in for /proc, /sys, /dev/cpu/0/cpuid, the kernel log, KVM's SEV features and QEMU. Needs
# python3 and the tools the check uses; touches nothing outside a temporary directory.
#
# Usage: scripts/snp-host-check-test.sh

set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
failures=0

# The CPUID leaf 8000001Fh of an EPYC 8024P with SEV-SNP enabled (AMDESE/AMDSEV issue 246): SME, SEV,
# SEV-ES, SEV-SNP, VMPL, Secure TSC and later features in EAX; C-bit 51, 6 bits of physical address
# reduction and 4 VMPLs in EBX; 1006 encrypted guests; SEV-ES and SEV-SNP below ASID 256.
good_eax=$(((1 << 0) | (1 << 1) | (1 << 3) | (1 << 4) | (1 << 5) | (1 << 8) | (1 << 10) | (1 << 12) | (1 << 13) | (1 << 14) | (1 << 15) | (1 << 16) | (1 << 17) | (1 << 24)))
good_ebx=$((51 | (6 << 6) | (4 << 12)))

# Writes a fixture host under $work/$1 from the variables below.
host() {
  local dir="$work/$1"
  rm -rf "$dir"
  mkdir -p "$dir/proc/sys/kernel" "$dir/sys/module/kvm_amd/parameters" "$dir/dev/cpu/0" "$dir/bin"
  cat > "$dir/proc/cpuinfo" << EOF
processor	: 0
vendor_id	: AuthenticAMD
cpu family	: $family
model		: $model
model name	: AMD EPYC 8224P 24-Core Processor
flags		: fpu tsc constant_tsc nonstop_tsc $flags
EOF
  echo "$osrelease" > "$dir/proc/sys/kernel/osrelease"
  echo "BOOT_IMAGE=/boot/vmlinuz root=/dev/md2 ro $cmdline_extra" > "$dir/proc/cmdline"
  echo "MemTotal:       $mem_kib kB" > "$dir/proc/meminfo"
  echo "$sev_snp" > "$dir/sys/module/kvm_amd/parameters/sev_snp"
  touch "$dir/dev/kvm" "$dir/dev/sev"
  python3 -I - "$dir/dev/cpu/0/cpuid" "$eax" "$ebx" 1006 "$edx" << 'PY'
import struct, sys
with open(sys.argv[1], "wb") as cpuid:
    cpuid.seek(0x8000001F)
    cpuid.write(struct.pack("<4I", *(int(value) for value in sys.argv[2:])))
PY
  printf '%s\n' "$log" > "$dir/dmesg.txt"
  cat > "$dir/bin/qemu" << EOF
#!/usr/bin/env bash
case "\$*" in
  --version) echo "QEMU emulator version $qemu_version" ;;
  "-object sev-snp-guest,help") printf 'sev-snp-guest options:\n  cbitpos=<uint32>\n  kernel-hashes=<bool>\n%s' "$qemu_props" ;;
  *) exit 1 ;;
esac
EOF
  chmod +x "$dir/bin/qemu"
}

# Sets the variables of a host ready for three guests.
defaults() {
  family=25 model=160 flags="sev sev_es sev_snp" osrelease=7.0.0-42-generic cmdline_extra=
  mem_kib=$((192 * 1048576)) sev_snp=Y eax=$good_eax ebx=$good_ebx edx=256 features=0x200
  qemu_version=11.1.92 qemu_props=$'  secure-tsc=<bool>\n'
  log=$'[    0.000000] tsc: Detected 2550.000 MHz processor\n[    4.120000] ccp 0000:02:00.5: SEV-SNP API:1.55 build:21\n[    4.300000] kvm_amd: SEV-SNP enabled (ASIDs 1 - 255)\n[    5.000000] tsc: Refined TSC clocksource calibration: 2549.998 MHz'
}

# Runs the check on host $1; expects exit status $2 and, when given, a line matching $3.
expect() {
  local name="$1" want_status="$2" want_line="${3:-}" status=0 out
  host "$name"
  out=$(SNP_CHECK_ROOT="$work/$name" DMESG="cat $work/$name/dmesg.txt" SNP_CHECK_KVM_FEATURES="$features" \
    QEMU="$work/$name/bin/qemu" bash "$root/scripts/snp-host-check.sh" 2>&1) || status=$?
  if [[ "$status" != "$want_status" ]]; then
    echo "FAIL: $name exited $status, not $want_status" >&2
    echo "$out" >&2
    failures=$((failures + 1))
  elif [[ -n "$want_line" ]] && ! grep -qE "$want_line" <<< "$out"; then
    echo "FAIL: $name printed no line matching '$want_line'" >&2
    echo "$out" >&2
    failures=$((failures + 1))
  else
    echo "ok: $name"
  fi
}

defaults
expect siena_ready 0 '^tee.processor = "Genoa"; verify attestations with --processor Genoa$'
expect siena_ready_firmware 0 '^ok   firmware: SEV-SNP API:1.55 build:21$'

defaults
eax=$((good_eax & ~(1 << 8)))
expect no_secure_tsc 1 '^FAIL cpuid: Secure TSC not supported \(Fn8000_001F_EAX bit 8 is clear\)$'

defaults
family=26 model=2
expect turin 1 '^FAIL processor: AMD family 1Ah'

defaults
osrelease=6.17.9-generic
expect old_kernel 1 '^FAIL kernel: 6.17.9-generic; KVM offers Secure TSC'

defaults
flags="sev sev_es" sev_snp=N
log+=$'\n[    3.900000] AMD-Vi: SNP: IOMMU disabled or configured in passthrough mode, SNP cannot be supported.'
cmdline_extra=iommu=pt
expect snp_off 1 '^FAIL kernel log: AMD-Vi: SNP: IOMMU disabled or configured in passthrough mode'
expect snp_off_cmdline 1 '^FAIL kernel: the command line has iommu=pt'
expect snp_off_param 1 "^FAIL kvm_amd: sev_snp is 'N', not Y"

defaults
edx=1
expect no_asids 1 '^FAIL cpuid: 0 ASIDs for SEV-ES and SEV-SNP guests, under the 3 needed'

defaults
ebx=$((47 | (1 << 6)))
expect other_cbit 1 '^FAIL cpuid: C-bit position 47'

defaults
features=0x20
expect kvm_without_secure_tsc 1 '^FAIL kvm: does not accept SecureTSC \(bit 9\)'

defaults
qemu_version=11.1.3 qemu_props=
expect old_qemu 1 '^FAIL qemu: QEMU emulator version 11.1.3 has no secure-tsc property'

defaults
log=$'[    1.000000] tsc: Detected 800.000 MHz processor'
expect slow_tsc 1 '^FAIL tsc: 800 MHz, under the 1000 MHz'

defaults
mem_kib=$((48 * 1048576))
expect small_memory 0 '^warn memory: 48 GiB, under 3 guests of 16 GiB'

if ((failures)); then
  echo "$failures case(s) failed" >&2
  exit 1
fi
echo "PASS: every fixture host gave the expected verdict"
