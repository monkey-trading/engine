#!/usr/bin/env bash
# Checks review-delta.sh on a scratch repository shaped like the engine's: a renumber after main
# gains a version, a site the renumber missed, a line moved between methods, binary fixtures (also
# under a text conversion), bad arguments, and failures. Prints one line per case and exits 1 if
# any case fails.
set -euo pipefail
script="$(cd "$(dirname "$0")" && pwd)/review-delta.sh"
tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1
cd "$tmp"
git init -q . && git config user.name test && git config user.email test@example.com

protocol() { # variants latest gate
  mkdir -p crates/core/src
  {
    echo "pub enum ProtocolVersion {"
    for v in $1; do echo "    $v,"; done
    echo "}"
    echo "impl ProtocolVersion {"
    echo "    pub const LATEST: ProtocolVersion = ProtocolVersion::$2;"
    echo "}"
    echo "fn apply(version: ProtocolVersion) {"
    echo "    common();"
    [ -z "$3" ] || printf '    if version >= ProtocolVersion::%s {\n        new_rule();\n    }\n' \
      "$3"
    echo "}"
  } > crates/core/src/protocol.rs
}
engine() { # the method that settles financing
  {
    echo "impl Engine {"
    echo "    pub fn rollover(&mut self) {"
    echo "        self.common();"
    [ "$1" != rollover ] || echo "        self.settle_financing();"
    echo "    }"
    echo "    pub fn close(&mut self) {"
    echo "        self.common();"
    [ "$1" != close ] || echo "        self.settle_financing();"
    echo "    }"
    echo "}"
  } > crates/core/src/engine.rs
}
log() { # version contents
  mkdir -p crates/core/tests/reference && echo "$2" > "crates/core/tests/reference/$1.jsonl"
}
commit() { git add -A && git commit -qm "$1" && git rev-parse HEAD; }
from() { git checkout -q --detach "$1"; }

protocol "V19 V20" V20 ""; engine none; base=$(commit base)
protocol "V19 V20 V21" V21 V21; engine rollover; log v21 '{"pr":1}'; reviewed=$(commit reviewed)
engine close; moved=$(commit moved)
from "$reviewed"; log v21 '{"pr":2}'; logs_only=$(commit logs-only)
from "$base"; protocol "V19 V20 V21" V21 ""; log v21 '{"main":1}'; main=$(commit main)
protocol "V19 V20 V21 V22" V22 V22; engine rollover; log v22 '{"pr":3}'
renumbered=$(commit renumber)
protocol "V19 V20 V21 V22" V22 V21; missed=$(commit missed-site)
from "$base"; printf '\0\1AAAA' > blob.bin; blob_a=$(commit blob-a)
from "$base"; printf '\0\1BBBB' > blob.bin; blob_b=$(commit blob-b)
from "$main"; printf '\0\1AAAA' > blob.bin; blob_a_on_main=$(commit blob-a-on-main)

failed=0
check() { # name expected-exit args...
  local name=$1 want=$2 got=0; shift 2
  "$script" "$@" > "$name.out" 2> "$name.err" || got=$?
  if [ "$got" = "$want" ]; then echo "ok   $name"; return; fi
  echo "FAIL $name: exit $got, want $want"; failed=1
}
check identical 0 "$base" "$reviewed" "$base" "$reviewed"
check reference-logs-only 0 "$base" "$reviewed" "$base" "$logs_only"
check renumber 0 "$base" "$reviewed" "$main" "$renumbered" V21=V22 v21=v22 V20=V21 v20=v21
check renumber-pairs-in-any-order 0 "$base" "$reviewed" "$main" "$renumbered" \
  V20=V21 V21=V22 v20=v21 v21=v22
check renumber-without-main-latest 1 "$base" "$reviewed" "$main" "$renumbered" V21=V22 v21=v22
check missed-renumber-site 1 "$base" "$reviewed" "$main" "$missed" V21=V22 v21=v22 V20=V21 v20=v21
grep -q '^++.*ProtocolVersion::V21 {$' missed-renumber-site.out ||
  { echo "FAIL missed-renumber-site: the stale V21 gate is not in the residual"; failed=1; }
check line-moved-between-methods 1 "$base" "$reviewed" "$base" "$moved"
check binary-changed 1 "$base" "$blob_a" "$base" "$blob_b"
check binary-same 0 "$base" "$blob_a" "$main" "$blob_a_on_main"
# A text conversion that renders both blobs alike must not hide the change.
echo '*.bin diff=alike' > .git/info/attributes
git config diff.alike.textconv "sh -c 'echo alike'"
check binary-changed-under-textconv 1 "$base" "$blob_a" "$base" "$blob_b"
check bad-ref 3 "$base" no-such-ref "$base" "$reviewed"
check bad-pair 2 "$base" "$reviewed" "$base" "$reviewed" V2.1=V3
check non-label-pair 2 "$base" "$reviewed" "$base" "$reviewed" settle=settle_financing
exit "$failed"
