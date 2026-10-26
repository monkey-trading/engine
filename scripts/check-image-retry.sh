#!/usr/bin/env bash
# Checks the enclave image build's download retries without building the image:
# - image/Dockerfile runs every apt-get update|install, curl, git fetch, git submodule update,
#   cargo fetch, and pip install as `retry <command>`, and cargo build with --frozen, so a later
#   edit that drops the retry or the freeze fails this check instead of the image job on the next
#   source outage; mutation checks confirm the scan catches a dropped retry or --frozen, a
#   download hidden behind an environment assignment or an `env` command, even with their own
#   options, or behind the download command's options before its subcommand, and one hidden
#   behind a comment inside a continuation;
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
# even inside a continuation, so the scan does too), which is then split into the shell segments
# a RUN runs, without splitting inside quotes; leading environment assignments and an `env`
# command do not hide a download, and each tool's own options may precede its subcommand,
# including options that take a separate value, so only the command that starts a segment, once
# normalized, is judged.
scan_dockerfile() {
  python3 -I - "$1" << 'EOF'
import re
import shlex
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


def shell_segments(shell):
    # The operators a RUN chains commands with, split outside quotes so a quoted value is not
    # broken apart (a backslash only escapes inside double quotes).
    segments = []
    current = []
    quote = None
    i = 0
    while i < len(shell):
        ch = shell[i]
        if quote:
            current.append(ch)
            if ch == "\\" and quote == '"' and i + 1 < len(shell):
                current.append(shell[i + 1])
                i += 2
                continue
            if ch == quote:
                quote = None
            i += 1
            continue
        if ch in "'\"":
            quote = ch
            current.append(ch)
            i += 1
            continue
        if ch == "\\" and i + 1 < len(shell):
            current.append(ch)
            current.append(shell[i + 1])
            i += 2
            continue
        pair = shell[i : i + 2]
        if pair in ("&&", "||") or ch in ";|":
            segments.append("".join(current))
            current = []
            i += 2 if pair in ("&&", "||") else 1
            continue
        current.append(ch)
        i += 1
    segments.append("".join(current))
    return segments


# Options that take a value, per tool, so an option's value is never mistaken for the
# subcommand and the subcommand is never mistaken for an option's value.
VALUE_OPTIONS = {
    "apt-get": {"-o", "--option", "-c", "--config-file", "-t", "--target-release",
                "-a", "--host-architecture"},
    "git": {"-C", "-c", "--git-dir", "--work-tree", "--namespace", "--exec-path",
            "--config-env"},
    "submodule": {"-j", "--jobs", "--depth", "--reference"},
    "cargo": {"--manifest-path", "--config", "-Z", "--target", "--target-dir",
              "--features", "--package"},
    "pip": {"-i", "--index-url", "--extra-index-url", "-f", "--find-links", "-r",
            "--requirement", "-c", "--constraint", "-t", "--target", "--prefix",
            "--root", "--src", "--build", "--proxy", "--cert", "--client-cert",
            "--python", "--platform", "--python-version", "--implementation", "--abi",
            "--only-binary", "--no-binary", "--retries", "--timeout", "--trusted-host"},
}
# `env` runs the command that follows it; its own flags, value-taking options (attached,
# separate, or `--opt=value` values), and assignments come first.
ENV_SHORT_VALUES = {"u", "C", "S"}
ENV_SHORT_FLAGS = {"i", "v"}


def split_words(value):
    try:
        return shlex.split(value, posix=True)
    except ValueError:
        return value.split()


def env_operands(tokens):
    # Returns the operand list env runs: its options and assignments are consumed, stopping at
    # the first operand so a `retry` or a benign command there is preserved. A split-string
    # value is word-split into the command env runs, followed by any remaining operands.
    i = 0
    while i < len(tokens):
        option = tokens[i]
        if re.match(r"^[A-Za-z_][A-Za-z0-9_]*=", option):
            i += 1
            continue
        if option == "--":
            return tokens[i + 1:]
        if not option.startswith("-") or option == "-":
            return tokens[i:]
        if option.startswith("--"):
            name, _, attached = option.partition("=")
            if name == "--split-string":
                value = attached if attached else tokens[i + 1] if i + 1 < len(tokens) else ""
                return split_words(value) + tokens[i + (1 if attached else 2):]
            if name in ("--unset", "--chdir"):
                i += 1 if attached else 2 if i + 1 < len(tokens) else 1
                continue
            if name in ("--ignore-environment", "--debug", "--help", "--version"):
                i += 1
                continue
            return tokens[i:]
        cluster = option[1:]
        position = 0
        while position < len(cluster) and cluster[position] in ENV_SHORT_FLAGS:
            position += 1
        if position == len(cluster):
            i += 1
            continue
        letter = cluster[position]
        if letter not in ENV_SHORT_VALUES:
            return tokens[i:]
        attached = cluster[position + 1:]
        if letter == "S":
            value = attached if attached else tokens[i + 1] if i + 1 < len(tokens) else ""
            step = 1 if attached else 2
            return split_words(value) + tokens[i + step:]
        i += 1 if attached else 2 if i + 1 < len(tokens) else 1
    return tokens[i:]


def strip_env(tokens):
    # Leading NAME=value assignments and `env` commands do not change which command a segment
    # runs; strip them (recursively, since a split string may carry its own) and judge the rest.
    while tokens and (re.match(r"^[A-Za-z_][A-Za-z0-9_]*=", tokens[0]) or tokens[0] == "env"):
        if tokens[0] == "env":
            tokens = env_operands(tokens[1:])
        else:
            tokens = tokens[1:]
    return tokens


def skip_options(tokens, i, tool):
    value_options = VALUE_OPTIONS[tool]
    short_values = {name[1] for name in value_options if len(name) == 2}
    while i < len(tokens):
        token = tokens[i]
        if token == "--":
            return i + 1
        if not token.startswith("-") or token == "-":
            break
        if token.startswith("--"):
            i += 1
            if token.split("=", 1)[0] in value_options and "=" not in token \
                    and i < len(tokens):
                i += 1
            continue
        # A short-option cluster: walk it, and let a value-taking option consume the rest of
        # the token as its value or the next token as a separate value.
        i += 1
        for position, letter in enumerate(token[1:], 1):
            if letter in short_values:
                if position == len(token) - 1 and i < len(tokens):
                    i += 1
                break
    return i


def is_download(tokens):
    if not tokens:
        return False
    tool = tokens[0]
    if tool == "curl":
        return True
    if tool not in ("apt-get", "git", "cargo", "pip"):
        return False
    i = skip_options(tokens, 1, tool)
    if i >= len(tokens):
        return False
    if tool == "apt-get":
        return tokens[i] in ("update", "install")
    if tool == "pip":
        return tokens[i] == "install"
    if tool == "cargo":
        return tokens[i] == "fetch"
    if tokens[i] == "fetch":
        return True
    # `git submodule update`, with submodule's own options before `update`.
    if tokens[i] != "submodule":
        return False
    j = skip_options(tokens, i + 1, "submodule")
    return j < len(tokens) and tokens[j] == "update"


def is_unfrozen_build(tokens):
    if not tokens or tokens[0] != "cargo":
        return False
    i = skip_options(tokens, 1, "cargo")
    return i < len(tokens) and tokens[i] == "build" and "--frozen" not in tokens

violations = []
for start, instruction in logical:
    # Instruction keywords are case-insensitive in a Dockerfile.
    run = re.match(r"^RUN\s+(?:--[a-z-]+(?:=\S+)?\s+)*(.*)$", instruction, re.IGNORECASE)
    if not run:
        continue
    for segment in shell_segments(run.group(1)):
        # Tokenize with shell quoting rules so a quoted value with whitespace stays one token;
        # an unbalanced quote cannot build, so fall back to a plain split for it.
        try:
            tokens = shlex.split(segment, posix=True)
        except ValueError:
            tokens = segment.split()
        tokens = strip_env(tokens)
        if is_download(tokens):
            violations.append(f"line {start}: {segment.strip()!r} runs a download without retry")
        elif is_unfrozen_build(tokens):
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
sed 's/retry git fetch -q --depth 1/git -c "http.extraHeader=X-Build: image" fetch -q --depth 1/' \
  image/Dockerfile > "$work/Dockerfile.quoted-option"
if scan_out="$(scan_dockerfile "$work/Dockerfile.quoted-option")"; then
  fail "the Dockerfile scan missed a download behind a quoted option value"
else
  echo "ok: the Dockerfile scan catches a git fetch behind a quoted option value ($scan_out)"
fi
sed 's/retry apt-get update/apt-get -qo Acquire::Retries=0 update/' \
  image/Dockerfile > "$work/Dockerfile.short-cluster"
if scan_out="$(scan_dockerfile "$work/Dockerfile.short-cluster")"; then
  fail "the Dockerfile scan missed a download behind a short-option cluster"
else
  echo "ok: the Dockerfile scan catches an apt-get update behind a short-option cluster ($scan_out)"
fi
sed 's/retry git submodule update/git submodule --jobs 4 update/' \
  image/Dockerfile > "$work/Dockerfile.submodule-options"
if scan_out="$(scan_dockerfile "$work/Dockerfile.submodule-options")"; then
  fail "the Dockerfile scan missed a download behind submodule options"
else
  echo "ok: the Dockerfile scan catches a submodule update behind its own options ($scan_out)"
fi
sed 's/retry pip install --quiet/env PIP_NO_CACHE_DIR=1 pip install --quiet/' \
  image/Dockerfile > "$work/Dockerfile.env-command"
if scan_out="$(scan_dockerfile "$work/Dockerfile.env-command")"; then
  fail "the Dockerfile scan missed a download behind an env command"
else
  echo "ok: the Dockerfile scan catches a pip install behind an env command ($scan_out)"
fi
sed 's/retry pip install --quiet/env -uHTTP_PROXY pip install --quiet/' \
  image/Dockerfile > "$work/Dockerfile.env-attached"
if scan_out="$(scan_dockerfile "$work/Dockerfile.env-attached")"; then
  fail "the Dockerfile scan missed a download behind an env option with an attached value"
else
  echo "ok: the Dockerfile scan catches a pip install behind an attached env option ($scan_out)"
fi
sed "s/&& retry apt-get update/\&\& env -S 'apt-get update'/" \
  image/Dockerfile > "$work/Dockerfile.split-string"
if scan_out="$(scan_dockerfile "$work/Dockerfile.split-string")"; then
  fail "the Dockerfile scan missed a download behind a split string"
else
  echo "ok: the Dockerfile scan catches an apt-get update behind a split string ($scan_out)"
fi
sed 's/&& retry apt-get update/\&\& env -i apt-get update/' \
  image/Dockerfile > "$work/Dockerfile.env-options"
if scan_out="$(scan_dockerfile "$work/Dockerfile.env-options")"; then
  fail "the Dockerfile scan missed a download behind env options"
else
  echo "ok: the Dockerfile scan catches an apt-get update behind env options ($scan_out)"
fi

# A separator inside a quoted assignment value must not split the segment there and hide the
# download that follows the assignment.
sed "s/retry apt-get update/LABEL='a;b' apt-get update/" \
  image/Dockerfile > "$work/Dockerfile.quoted-separator"
if scan_out="$(scan_dockerfile "$work/Dockerfile.quoted-separator")"; then
  fail "the Dockerfile scan missed a download behind a quoted separator"
else
  echo "ok: the Dockerfile scan catches an apt-get update behind a quoted separator ($scan_out)"
fi

# Benign commands whose arguments merely name a download subcommand must not be flagged, and
# neither must a download wrapped in `retry` behind an `env`.
cat > "$work/Dockerfile.benign" << 'BENIGN'
RUN git checkout fetch
RUN git -C /build checkout -q FETCH_HEAD
RUN apt-get moo
RUN apt-get clean
RUN cargo build --frozen --release
RUN echo apt-get update
RUN retry curl -fsSL https://example.invalid/pinned.tar.xz -o pinned.tar.xz
RUN env -i retry curl -fsSL https://example.invalid/pinned.tar.xz -o pinned.tar.xz
RUN env -i echo curl
RUN env -uHTTP_PROXY echo curl
RUN env -S 'retry curl -fsSL https://example.invalid/pinned.tar.xz -o pinned.tar.xz'
BENIGN
if scan_out="$(scan_dockerfile "$work/Dockerfile.benign")"; then
  echo "ok: benign commands are not flagged"
else
  fail "the Dockerfile scan flagged a benign command: $scan_out"
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
