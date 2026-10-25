#!/usr/bin/env bash
# Checks the enclave image build's download retries without building the image:
# - image/retry runs the same command after each failure, at most five times, waiting 15, 30, 60,
#   then 120 seconds, and returns the last failure's exit status;
# - apt with image/apt.conf, in the Dockerfile's pinned Debian image, retries an index that keeps
#   answering 503, then fails `apt-get update`, so image/retry runs it again, instead of exiting 0
#   with the suite left out.
# Needs Docker and python3.
#
# Usage: scripts/check-image-retry.sh

set -euo pipefail

cd "$(dirname "$0")/.."

work=$(mktemp -d)
server_pid=
failed=0

cleanup() {
  if [[ -n "$server_pid" ]]; then
    kill "$server_pid" 2> /dev/null || true
    wait "$server_pid" 2> /dev/null || true
  fi
  rm -rf "$work"
}
trap cleanup EXIT

fail() {
  echo "FAIL: $*" >&2
  failed=1
}

# A download that fails its first $FAILS calls with exit 7, and a sleep that records its argument.
mkdir "$work/bin"
cat > "$work/bin/download" << 'EOF'
#!/bin/sh
printf '%s|' "$@" >> "$WORK/calls"
echo >> "$WORK/calls"
[ "$(wc -l < "$WORK/calls")" -gt "$FAILS" ] || exit 7
EOF
cat > "$work/bin/sleep" << 'EOF'
#!/bin/sh
echo "$1" >> "$WORK/waits"
EOF
chmod +x "$work/bin/download" "$work/bin/sleep"

# Usage: check_retry <failures> <expected exit> <expected calls> <expected waits>
check_retry() {
  local status=0 calls waits sources
  : > "$work/calls"
  : > "$work/waits"
  WORK=$work FAILS=$1 PATH="$work/bin:$PATH" \
    image/retry download https://example.invalid/pinned.tar.xz 'two words' 2> /dev/null || status=$?
  calls=$(wc -l < "$work/calls" | tr -d ' ')
  waits=$(paste -sd ' ' "$work/waits")
  sources=$(sort -u "$work/calls")
  if [[ "$status" != "$2" || "$calls" != "$3" || "$waits" != "$4" \
    || "$sources" != "https://example.invalid/pinned.tar.xz|two words|" ]]; then
    fail "retry after $1 failures: exit $status, $calls calls, waits [$waits], arguments [$sources];" \
      "want exit $2, $3 calls, waits [$4], the same arguments every call"
  else
    echo "ok: retry after $1 failures: exit $status, $calls calls, waits [$waits]"
  fi
}

check_retry 0 0 1 ""
check_retry 1 0 2 "15"
check_retry 4 0 5 "15 30 60 120"
check_retry 5 7 5 "15 30 60 120"

# apt against a server that answers every request with a 503 and a body, which apt counts as
# transient.
image=$(sed -n 's/^FROM --platform=linux\/amd64 \(debian:[^ ]*\) AS tools$/\1/p' image/Dockerfile)
retries=$(sed -n 's/^Acquire::Retries "\([0-9]*\)";$/\1/p' image/apt.conf)
[[ -n "$image" && -n "$retries" ]] || {
  echo "FAIL: no tools image in image/Dockerfile or Acquire::Retries in image/apt.conf" >&2
  exit 1
}
cat > "$work/server.py" << 'EOF'
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


class Unavailable(BaseHTTPRequestHandler):
    def do_GET(self):
        with open(sys.argv[1], "a") as log:
            log.write(self.path + "\n")
        body = b"No healthy backends\n"
        self.send_response(503)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


server = ThreadingHTTPServer(("0.0.0.0", 0), Unavailable)
print(server.server_address[1], flush=True)
server.serve_forever()
EOF
: > "$work/requests"
python3 -I "$work/server.py" "$work/requests" > "$work/port" &
server_pid=$!
for _ in $(seq 50); do
  [[ -s "$work/port" ]] && break
  sleep 0.1
done
port=$(cat "$work/port")
[[ -n "$port" ]] || {
  echo "FAIL: the 503 server did not start" >&2
  exit 1
}

status=0
docker run --rm --platform linux/amd64 --add-host=host.docker.internal:host-gateway \
  -v "$PWD/image/apt.conf:/etc/apt/apt.conf.d/80snapshot:ro" "$image" sh -c "
    rm -f /etc/apt/sources.list.d/*
    echo 'deb [signed-by=/usr/share/keyrings/debian-archive-keyring.pgp] http://host.docker.internal:$port/debian trixie main' \
      > /etc/apt/sources.list
    apt-get update" > "$work/apt.log" 2>&1 || status=$?
fetches=$(grep -cx '/debian/dists/trixie/InRelease' "$work/requests" || true)
if [[ "$status" == 0 || "$fetches" != $((retries + 1)) ]]; then
  fail "apt-get update against a 503 index: exit $status, $fetches fetches of the index;" \
    "want a nonzero exit after $((retries + 1)) fetches"
  tail -n 5 "$work/apt.log" >&2
else
  echo "ok: apt-get update against a 503 index: exit $status after $fetches fetches of the index"
fi

exit "$failed"
