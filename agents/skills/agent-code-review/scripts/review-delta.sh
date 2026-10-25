#!/usr/bin/env bash
# Print what changed in a PR's patch since its review loop ended.
#
#   review-delta.sh <reviewed-base> <reviewed-head> <new-base> <new-head> [OLD=NEW ...]
#
# Compares the reviewed patch (reviewed-base...reviewed-head) with the new one
# (new-base...new-head), so changes on main between the two bases drop out. Each
# OLD=NEW pair is a protocol-version renumber (e.g. V21=V23 v21=v23): it is undone
# in the new patch before comparing. Reference logs (crates/*/tests/reference/*.jsonl)
# are ignored; CI replays them. Hunk line numbers and index lines are ignored.
#
# Exit 0 with no output: the new patch is the reviewed one plus the renumber, so no
# review round is needed. Exit 1: the residual is printed; review only that. Exit 2:
# bad arguments. Any other exit: git failed.
set -euo pipefail
[ $# -ge 4 ] || { sed -n '2,14p' "$0"; exit 2; }
rb=$1 rh=$2 nb=$3 nh=$4; shift 4
for pair in "$@"; do
  [[ $pair =~ ^[A-Za-z0-9_]+=[A-Za-z0-9_]+$ ]] || { echo "bad OLD=NEW pair: $pair" >&2; exit 2; }
done
tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT

patch() { # base head out
  git diff --no-color --no-ext-diff --no-renames -U0 "$1...$2" -- ':(top)' \
    ':(top,exclude,glob)crates/*/tests/reference/*.jsonl' |
    sed -E -e '/^index /d' -e 's/^@@ .* @@/@@/' > "$3"
}
patch "$rb" "$rh" "$tmp/reviewed"
patch "$nb" "$nh" "$tmp/new"
# sed without -i, which takes its suffix differently in BSD and GNU sed.
for pair in "$@"; do
  old=${pair%%=*} new=${pair#*=}
  sed "s/${new}/${old}/g" "$tmp/new" > "$tmp/undone"
  mv "$tmp/undone" "$tmp/new"
done
if diff -u -L reviewed -L new "$tmp/reviewed" "$tmp/new" > "$tmp/residual"; then
  exit 0
fi
cat "$tmp/residual"
exit 1
