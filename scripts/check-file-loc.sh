#!/usr/bin/env bash
# Enforce a maximum line count per Rust source file, per package.

set -euo pipefail

cd "$(dirname "$0")/.."

fail=0

# check_pkg <package-dir> <max-lines> [exempt-path ...]
check_pkg() {
  local pkg=$1 max=$2
  shift 2
  local offenders
  offenders=$(find "$pkg" -name '*.rs' -not -path '*/target/*' -print0 |
    xargs -0 wc -l | awk -v max="$max" '$2 != "total" && $1 > max')
  for exempt in "$@"; do
    offenders=$(echo "$offenders" | awk -v ex="$exempt" '$2 != ex')
  done
  if [[ -n "$offenders" ]]; then
    echo "Rust files in $pkg over $max lines:"
    echo "$offenders"
    fail=1
  fi
}

check_pkg crates/core 500
check_pkg crates/node 500
check_pkg crates/verify 500
check_pkg crates/verify-wasm 500
check_pkg crates/wire 500

if [[ "$fail" -ne 0 ]]; then
  exit 1
fi
echo "All checked Rust files are within their line caps."
