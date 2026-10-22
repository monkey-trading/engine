#!/usr/bin/env bash
# Builds the enclave image from a commit: the kernel, the initramfs with the engine, OVMF, and the
# SEV-SNP launch measurement, written to <out-dir> with their SHA-256 sums. Only committed files go
# in, and SOURCE_DATE_EPOCH is the commit time, so the same commit gives the same bytes anywhere.
#
# Usage: scripts/build-image.sh [--ref <commit>] [--variant production|dev] [--no-cache] <out-dir>

set -euo pipefail

ref=HEAD
variant=production
no_cache=
out=

while [[ $# -gt 0 ]]; do
  case $1 in
    --ref) ref=$2; shift 2 ;;
    --variant) variant=$2; shift 2 ;;
    --no-cache) no_cache=--no-cache; shift ;;
    -*) echo "unknown option: $1" >&2; exit 2 ;;
    *) out=$1; shift ;;
  esac
done
if [[ -z "$out" ]]; then
  echo "usage: $0 [--ref <commit>] [--variant production|dev] [--no-cache] <out-dir>" >&2
  exit 2
fi
mkdir -p "$out"
out=$(cd "$out" && pwd)
cd "$(dirname "$0")/.."

commit=$(git rev-parse --verify "$ref^{commit}")
epoch=$(git log -1 --format=%ct "$commit")
echo "building the $variant image from $commit (SOURCE_DATE_EPOCH=$epoch)" >&2

git archive --format=tar "$commit" |
  docker buildx build ${no_cache:+"$no_cache"} \
    --platform linux/amd64 \
    --file image/Dockerfile \
    --build-arg SOURCE_DATE_EPOCH="$epoch" \
    --build-arg VARIANT="$variant" \
    --output "type=local,dest=$out" \
    -

cat "$out/SHA256SUMS"
