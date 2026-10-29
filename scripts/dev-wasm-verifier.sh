#!/usr/bin/env bash
# The browser verifier's dev scenario: build the WebAssembly package (or take one already built),
# write the attestation fixtures for the enclave address engine-wire's vectors are signed by, and
# check every fixture and vector with both the package under Node and the `engine` CLI:
#   1. the test-signed report verifies and names its release, also from a list that starts with the
#      previous release; a mock quote, an unlisted configuration hash, an altered measurement, a
#      corrupted report signature, a TCB below the minimum, a revoked intermediate, a VCEK for
#      another chip, a stale or not yet valid revocation list, and a release list that does not
#      admit the quote fail;
#   2. the verified enclave address is the vectors' signer: each signed response and stream
#      connection ends as its vector records, and answers from any other key, an unattested signer,
#      fail;
#   3. a mock attestation reads only through the development entry point, which says it is a mock.
# Every verdict and message must match the CLI's. Passes with
# `PASS: wasm and native verdicts match on N fixtures`.
#
# Needs Cargo with the wasm32-unknown-unknown target, wasm-bindgen-cli at the version
# scripts/build-wasm-verifier.sh pins, and Node 18 or later; no hardware, vendor account, or
# network.
#
# Usage: scripts/dev-wasm-verifier.sh [package-dir]
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

package="${1:-}"
if [[ -z "$package" ]]; then
  "$root/scripts/build-wasm-verifier.sh" --dev "$work/package"
  package="$work/package"
fi
cargo build --manifest-path "$root/Cargo.toml" -p engine-node --bin engine --locked --quiet
engine="$(cargo metadata --manifest-path "$root/Cargo.toml" --format-version 1 --no-deps \
  | node -e 'process.stdout.write(JSON.parse(require("fs").readFileSync(0)).target_directory)')/debug/engine"

vectors="$root/crates/wire/tests/vectors.json"
signer="$(node -e 'process.stdout.write(JSON.parse(require("fs").readFileSync(process.argv[1])).signer)' "$vectors")"
cargo run --manifest-path "$root/Cargo.toml" -p engine-verify --features test-fixtures --example verifier_fixtures \
  --locked --quiet -- "$work/fixtures" "$signer"

RUST_BACKTRACE=0 RUST_LIB_BACKTRACE=0 node "$root/scripts/dev-wasm-verifier.mjs" \
  "$package" "$engine" "$work/fixtures" "$vectors" "$work/answers"
