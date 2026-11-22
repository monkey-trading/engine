#!/usr/bin/env bash
# Builds and runs the benchmark guest (ops/runbooks/sev-snp-host.md, "Measure inside the guest"): the
# engine's benchmark and load tests as static x86_64 binaries built with the release profile, with the
# musl libc and allocator of the image's own engine, run inside an SEV-SNP guest that boots the image's
# firmware and kernel, and the same binaries run on the host outside it.
#   build <out-dir> <bench-list>  builds the binaries the list names into <out-dir>/initramfs/bench/,
#                                 then writes <out-dir>/bench-initramfs.cpio.gz (busybox, the binaries
#                                 and an init that runs the list and powers off) and <out-dir>/disk/
#                                 (HEAD's tree, which the tests read at the path they were built at,
#                                 and the list), from which the guest's data disk is made
#   host <out-dir> <cpus>         runs the list on this host, pinned to taskset's CPU list <cpus>,
#                                 writing <out-dir>/host-results/<name>.txt
#
# A bench list has one benchmark a line: its name, then a command that runs a binary under /bench/
# with its arguments, optionally after environment assignments. A binary is named after its test
# target: engine_node for engine-node's library tests, engine_core for engine-core's, and any other
# name for the engine-core integration test of that name, such as hot_path_benchmark. Blank lines and
# lines starting with # are skipped.
#
# Needs a checkout with no uncommitted changes, cargo with the x86_64-unknown-linux-musl target,
# musl-gcc (Debian's musl-tools), a static busybox (BUSYBOX, /bin/busybox from busybox-static), jq,
# GNU cpio and gzip; `host` needs taskset.
#
# Usage: scripts/snp-bench-guest.sh build <out-dir> <bench-list>
#        scripts/snp-bench-guest.sh host <out-dir> <cpus>

set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"

usage() {
  echo "usage: $0 build <out-dir> <bench-list> | host <out-dir> <cpus>" >&2
  exit 2
}
die() {
  echo "$*" >&2
  exit 1
}
# Refuses a checkout with changes or untracked files: the binaries and the guest's tree are HEAD's.
clean_tree() {
  [[ -z "$(git -C "$root" status --porcelain)" ]] || die "commit or stash your changes first: the guest's disk holds HEAD's tree"
}
# The bench list's benchmarks, as `<name> <command>` lines.
benchmarks() { awk '!/^[[:space:]]*(#|$)/' "$1"; }

# Builds the test binaries for `$@` (cargo's package and target options) and copies each into $bench
# under its target's name.
build_tests() {
  CC_x86_64_unknown_linux_musl=musl-gcc cargo test --locked --release --no-run --target x86_64-unknown-linux-musl \
    --manifest-path "$root/Cargo.toml" --message-format=json "$@" |
    jq -r 'select(.reason == "compiler-artifact" and .profile.test and .executable != null) | "\(.target.name) \(.executable)"' |
    while read -r name executable; do
      cp "$executable" "$bench/$name"
    done
}

build() {
  local out="$1" list="$2" busybox="${BUSYBOX:-/bin/busybox}" target core_args=() node=
  clean_tree
  # ldd fails on a static binary, saying so.
  { ldd "$busybox" 2>&1 || true; } | grep -qE 'not a dynamic executable|statically linked' || die "$busybox is not a static binary"
  mkdir -p "$out"
  out="$(cd "$out" && pwd)"
  rm -rf "$out/initramfs" "$out/disk"
  mkdir -p "$out/initramfs/bin" "$out/initramfs/bench" "$out/initramfs/data" "$out/initramfs/dev" \
    "$out/initramfs/proc" "$out/initramfs/sys" "$out/disk/src"
  bench="$out/initramfs/bench"

  for target in $(benchmarks "$list" | grep -oE '/bench/[A-Za-z0-9_]+' | sed 's|^/bench/||' | sort -u); do
    case $target in
      engine_node) node=1 ;;
      engine_core) core_args+=(--lib) ;;
      *) core_args+=(--test "$target") ;;
    esac
  done
  if [[ -n "$node" ]]; then build_tests -p engine-node --lib; fi
  if [[ ${#core_args[@]} -gt 0 ]]; then build_tests -p engine-core "${core_args[@]}"; fi
  for target in $(benchmarks "$list" | grep -oE '/bench/[A-Za-z0-9_]+' | sed 's|^/bench/||' | sort -u); do
    [[ -x "$bench/$target" ]] || die "cargo built no test binary named $target"
  done

  cp "$busybox" "$out/initramfs/bin/busybox"
  cat > "$out/initramfs/init" << 'INIT'
#!/bin/busybox sh
# PID 1 of the benchmark guest: runs each benchmark of /data/bench.list from the source tree's build
# path, its output to the console and to /data/results/<name>.txt, then powers the guest off.
/bin/busybox --install -s /bin
export PATH=/bin
mount -t devtmpfs devtmpfs /dev
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t ext4 /dev/vda /data
ip link set lo up 2> /dev/null || true
src=$(cat /data/src-path)
mkdir -p "${src%/*}" /data/results /data/tmp
ln -s /data/src "$src"
export TMPDIR=/data/tmp HOME=/data
ulimit -n 65536
cd "$src"
{
  echo "clocksource: $(cat /sys/devices/system/clocksource/clocksource0/current_clocksource)"
  echo "vcpus: $(nproc)"
  grep MemTotal /proc/meminfo
  dmesg | grep -E 'SEV: Status|Memory Encryption Features active'
} | tee /data/results/guest.txt
: > /data/results/status.txt
awk '!/^[[:space:]]*(#|$)/' /data/bench.list > /data/tmp/bench.list
while read -r name command; do
  echo "=== $name: $command"
  start=$(date +%s)
  { sh -c "$command" < /dev/null; echo $? > /data/tmp/status; } 2>&1 | tee "/data/results/$name.txt"
  echo "$name exited $(cat /data/tmp/status) after $(($(date +%s) - start)) s" | tee -a /data/results/status.txt
done < /data/tmp/bench.list
sync
poweroff -f
INIT
  chmod 0755 "$out/initramfs/init"
  (cd "$out/initramfs" && find . -print0 | LC_ALL=C sort -z | cpio --quiet --null -o -H newc --owner=0:0) |
    gzip -1 > "$out/bench-initramfs.cpio.gz"

  git -C "$root" archive --format=tar HEAD | tar -x -C "$out/disk/src"
  echo "$root" > "$out/disk/src-path"
  cp "$list" "$out/disk/bench.list"
  echo "built $out/bench-initramfs.cpio.gz and the data disk's files in $out/disk" >&2
}

host() {
  local out="$1" cpus="$2" results name command start status
  clean_tree
  out="$(cd "$out" && pwd)"
  results="$out/host-results"
  mkdir -p "$results" "$out/host-tmp"
  : > "$results/status.txt"
  cd "$(cat "$out/disk/src-path")"
  benchmarks "$out/disk/bench.list" > "$out/host-tmp/bench.list"
  while read -r name command; do
    command=${command//\/bench\//$out/initramfs/bench/}
    echo "=== $name: $command"
    start=$(date +%s)
    status=0
    TMPDIR="$out/host-tmp" taskset -c "$cpus" sh -c "$command" < /dev/null 2>&1 | tee "$results/$name.txt" || status=$?
    echo "$name exited $status after $(($(date +%s) - start)) s" | tee -a "$results/status.txt"
  done < "$out/host-tmp/bench.list"
}

case "${1:-}" in
  build) [[ $# -eq 3 ]] || usage; build "$2" "$3" ;;
  host) [[ $# -eq 3 ]] || usage; host "$2" "$3" ;;
  *) usage ;;
esac
