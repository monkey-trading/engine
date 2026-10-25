#!/usr/bin/env bash
# Print what changed in a PR's patch since its review loop ended.
#
#   review-delta.sh <reviewed-base> <reviewed-head> <new-base> <new-head> [OLD=NEW ...]
#
# Compares the reviewed patch (reviewed-base...reviewed-head) with the new one
# (new-base...new-head), so changes on main between the two bases drop out. Each
# OLD=NEW pair is a protocol-version label (V<n> or v<n>), renamed in the reviewed
# patch with all pairs at once: the PR's renumber (V21=V23 v21=v23) and, if main has
# gained versions, main's latest (V20=V22 v20=v22). A site the renumber missed keeps
# its old label and shows. Reference logs (crates/*/tests/reference/*.jsonl), index
# lines, and hunk line numbers are ignored. The comparison keys each hunk to its file
# and the nearest preceding Rust item line, so a line moved within one item does not
# show; the residual's hunk headers name the file.
#
# Exit 0 with no output: the patches match. Exit 1: the residual is printed.
# Exit 2: bad arguments. Exit 3: git, sed, or diff failed.
set -Eeuo pipefail
trap 'exit 3' ERR
export LC_ALL=C
[ $# -ge 4 ] || { sed -n '2,17p' "$0"; exit 2; }
rb=$1 rh=$2 nb=$3 nh=$4; shift 4
for pair in "$@"; do
  [[ $pair =~ ^[Vv][0-9]+=[Vv][0-9]+$ ]] || { echo "not a version label pair: $pair" >&2; exit 2; }
done
tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT
echo '*.rs diff=rust' > "$tmp/attributes"

patch() { # base head out
  git -c core.attributesFile="$tmp/attributes" diff --no-color --no-ext-diff --no-textconv \
    --no-renames --binary -U0 "$1...$2" -- ':(top)' \
    ':(top,exclude,glob)crates/*/tests/reference/*.jsonl' |
    sed -E -e '/^index /d' -e 's/^@@ [^@]* @@/@@/' > "$3"
}
rename() { # file pattern replacement; sed -i is not portable between BSD and GNU
  sed "s/$2/$3/g" "$1" > "$tmp/renamed"
  mv "$tmp/renamed" "$1"
}
patch "$rb" "$rh" "$tmp/reviewed"
patch "$nb" "$nh" "$tmp/new"
# Through placeholders, so that V20=V21 V21=V22 does not chain.
tag=
for pair in "$@"; do tag+=$'\002'; rename "$tmp/reviewed" "${pair%%=*}" $'\001'"$tag"$'\001'; done
tag=
for pair in "$@"; do tag+=$'\002'; rename "$tmp/reviewed" $'\001'"$tag"$'\001' "${pair#*=}"; done
status=0
diff -u -F '^diff --git' -L reviewed -L new "$tmp/reviewed" "$tmp/new" > "$tmp/residual" ||
  status=$?
[ "$status" -ne 0 ] || exit 0
[ "$status" -eq 1 ] || exit 3
cat "$tmp/residual"
exit 1
