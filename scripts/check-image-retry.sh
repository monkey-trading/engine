#!/usr/bin/env bash
# Checks the enclave image build's download retries without building the image:
# - image/Dockerfile runs every apt-get update|install, curl, git fetch, git submodule update,
#   cargo fetch, and pip install as `retry <command>`, and cargo build with --frozen, so a later
#   edit that drops the retry or the freeze fails this check instead of the image job on the next
#   source outage; mutation checks confirm the scan catches a dropped retry or --frozen, a
#   download hidden behind an environment assignment (bare or via `env`) or its command options,
#   even options taking a separate value, and one hidden behind a comment inside a continuation;
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

# Scans a Dockerfile and fails on any apt-get update|install, curl, git fetch, git submodule
# update, cargo fetch, or pip install not run as `retry <command>`, and on a cargo build without
# --frozen. Continuation lines are joined into their instruction (BuildKit strips comment lines
# even inside a continuation, so the scan does too), which is then split into the shell segments a
# RUN runs; leading environment assignments, an `env` command, and the command's own options
# before its subcommand — even options taking a separate value — do not hide a download, so only
# the command that starts a segment, once normalized, is judged.
scan_dockerfile() {
  python3 -I - "$1" << 'EOF'
import re
import sys

path = sys.argv[1]
physical = open(path, encoding="utf-8").read().splitlines()

logical = []
buffer = ""
start = 0
for number, line in enumerate(physical, 1):
    stripped = line.strip()
    if stripped.startswith("#"):
        continue
    if not buffer:
        if not stripped:
            continue
        start = number
    if stripped.endswith("\\"):
        buffer += stripped[:-1] + " "
        continue
    buffer += stripped
    logical.append((start, buffer))
    buffer = ""

env_assignment = re.compile(
    r"^[A-Za-z_][A-Za-z0-9_]*=(?:'[^']*'|\"[^\"]*\"|\S*)\s+"
)
# After environment assignments are stripped, the download a segment starts with, allowing the
# command's own options before its subcommand, including options that take a separate value
# (a token that is neither an option nor the subcommand is an option's value, so scanning for
# the subcommand without a fixed position finds it).
download = re.compile(
    r"^(?:apt-get(?:(?!\s+(?:update|install)\b)\s+\S+)*\s+(?:update|install)"
    r"|curl\b"
    r"|git(?:(?!\s+submodule\b)\s+\S+)*\s+submodule(?:(?!\s+update\b)\s+\S+)*\s+update"
    r"|git(?:(?!\s+fetch\b)\s+\S+)*\s+fetch"
    r"|cargo(?:(?!\s+fetch\b)\s+\S+)*\s+fetch"
    r"|pip(?:(?!\s+install\b)\s+\S+)*\s+install)\b"
)
build = re.compile(r"^cargo(?:(?!\s+build\b)\s+\S+)*\s+build\b")

violations = []
for start, instruction in logical:
    # Instruction keywords are case-insensitive in a Dockerfile.
    run = re.match(r"^RUN\s+(?:--[a-z-]+(?:=\S+)?\s+)*(.*)$", instruction, re.IGNORECASE)
    if not run:
        continue
    for segment in re.split(r"&&|\|\||;|\|", run.group(1)):
        command = segment.strip()
        while True:
            stripped_env = env_assignment.sub("", command)
            # `env` runs the command that follows it, with its own assignments.
            if not stripped_env:
                break
            if re.match(r"^env\s+", stripped_env):
                stripped_env = re.sub(r"^env\s+", "", stripped_env, count=1)
            if stripped_env == command:
                break
            command = stripped_env
        if download.match(command):
            violations.append(f"line {start}: {segment.strip()!r} runs a download without retry")
        elif build.match(command) and "--frozen" not in command.split():
            violations.append(f"line {start}: {segment.strip()!r} builds without --frozen")

for violation in violations:
    print(violation)
sys.exit(1 if violations else 0)
EOF
}

if ! scan_out="$(scan_dockerfile image/Dockerfile)"; then
  echo "FAIL: image/Dockerfile has a download outside image/retry or an unfrozen cargo build:" >&2
  echo "$scan_out" >&2
  exit 1
fi
echo "ok: every image/Dockerfile download runs under retry and cargo build is --frozen"

# Mutation check: the scan must catch a later edit that drops a retry or the --frozen.
sed 's/retry apt-get update/apt-get update/' image/Dockerfile > "$work/Dockerfile.no-retry"
if scan_out="$(scan_dockerfile "$work/Dockerfile.no-retry")"; then
  fail "the Dockerfile scan missed a download with retry removed"
else
  echo "ok: the Dockerfile scan catches a removed retry ($scan_out)"
fi
sed 's/cargo build --frozen/cargo build/' image/Dockerfile > "$work/Dockerfile.no-frozen"
if scan_out="$(scan_dockerfile "$work/Dockerfile.no-frozen")"; then
  fail "the Dockerfile scan missed a cargo build without --frozen"
else
  echo "ok: the Dockerfile scan catches a dropped --frozen ($scan_out)"
fi

# The scan must also catch a download disguised so it no longer starts a segment with the
# command: an environment assignment or `env` in front of it, or its options moved before its
# subcommand.
sed 's/retry apt-get install -y --no-install-recommends/DEBIAN_FRONTEND=noninteractive apt-get -y install --no-install-recommends/' \
  image/Dockerfile > "$work/Dockerfile.shuffled"
if scan_out="$(scan_dockerfile "$work/Dockerfile.shuffled")"; then
  fail "the Dockerfile scan missed a download behind an environment assignment and moved options"
else
  echo "ok: the Dockerfile scan catches a disguised apt-get install ($scan_out)"
fi
sed 's/retry git fetch -q --depth 1/git -C \/build -c protocol.version=2 fetch -q --depth 1/' \
  image/Dockerfile > "$work/Dockerfile.git-options"
if scan_out="$(scan_dockerfile "$work/Dockerfile.git-options")"; then
  fail "the Dockerfile scan missed a download behind options taking a separate value"
else
  echo "ok: the Dockerfile scan catches a git fetch behind value-taking options ($scan_out)"
fi
sed 's/retry pip install --quiet/env PIP_NO_CACHE_DIR=1 pip install --quiet/' \
  image/Dockerfile > "$work/Dockerfile.env-command"
if scan_out="$(scan_dockerfile "$work/Dockerfile.env-command")"; then
  fail "the Dockerfile scan missed a download behind an env command"
else
  echo "ok: the Dockerfile scan catches a pip install behind an env command ($scan_out)"
fi

# BuildKit strips comment lines even inside a continuation, so a comment there cannot be used to
# split the scan's instruction early and hide a download; drop the retry from the apt install that
# follows an inserted comment and the scan must still catch it.
awk '
  /^ && retry apt-get install -y --no-install-recommends/ {
    if (!done) {
      print "# a comment inside the continuation"
      done = 1
    }
    sub(/retry apt-get install/, "apt-get install")
  }
  { print }
' image/Dockerfile > "$work/Dockerfile.comment"
if scan_out="$(scan_dockerfile "$work/Dockerfile.comment")"; then
  fail "the Dockerfile scan missed a download behind a comment in a continuation"
else
  echo "ok: the Dockerfile scan catches a download behind a continuation comment ($scan_out)"
fi

# A download whose first $FAILS calls fail, call N with exit 10 + N, and a sleep that records its
# argument.
mkdir "$work/bin"
cat > "$work/bin/download" << 'EOF'
#!/bin/sh
printf '%s|' "$@" >> "$WORK/calls"
echo >> "$WORK/calls"
call=$(wc -l < "$WORK/calls" | tr -d ' ')
[ "$call" -gt "$FAILS" ] || exit $((10 + call))
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
check_retry 5 15 5 "15 30 60 120"

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
