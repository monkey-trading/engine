#!/usr/bin/env bash
# The attestation verifier's dev scenario: a stand-in vendor key-distribution service on loopback
# issues a test certificate chain, and a test-signed SEV-SNP report verifies against a release
# manifest and names the release; a mock quote, an unlisted configuration, an altered measurement,
# and an unattested checkpoint signer fail. Needs only Cargo: no hardware, vendor account, or
# network beyond loopback is involved.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
CARGO_INCREMENTAL=0 cargo test --locked --manifest-path "$root/Cargo.toml" -p engine-node --lib \
  verify::tests::dev_scenario_attestation_verifier -- --ignored --exact --nocapture
