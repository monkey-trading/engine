#!/usr/bin/env bash
# The attestation verifier's dev scenario: a stand-in vendor key-distribution service on loopback
# issues a test certificate chain, and a test-signed SEV-SNP report verifies against a release
# manifest and names the release; a mock quote, an unlisted configuration, an altered measurement,
# and an unattested checkpoint signer fail. A hand-built log whose checkpoints and signed
# withdrawal one enclave key signed also replays through `engine replay --attestation` without a
# `--signer` pin, and the same command with a mismatched `--signer` fails. The node relays the
# stand-in's collateral at `/v1/attestation/collateral`; with the stand-in stopped,
# `engine verify-attestation --collateral <the route>` still verifies, and a served VCEK for
# another chip or a stale revocation list fails. Needs only Cargo: no
# hardware, vendor account, or network beyond loopback is involved.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
(cd "$root" && cargo build -p engine-node --locked --quiet)
CARGO_INCREMENTAL=0 cargo test --locked --manifest-path "$root/Cargo.toml" -p engine-node --lib \
  verify::tests -- --ignored --nocapture
