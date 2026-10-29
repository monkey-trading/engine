# engine-verify

## Overview

Verifies an enclave's SEV-SNP attestation against published release manifests. It takes everything
as arguments: the attestation `/v1/attestation` served, the release manifests the caller trusts, the
vendor collateral (the VCEK, intermediate, root, and revocation list, from
`/v1/attestation/collateral` or the vendor's key-distribution service), an optional extra root for a
test or private chain, and the time to check dates at. It does no IO, reads no clock, and builds no
C code, so `engine verify-attestation`, `engine replay --attestation`, the node's collateral relay,
and the browser build (`crates/verify-wasm`) all run this one implementation.

Its errors are `anyhow` chains rather than a typed error: each is a message for a person, which the
CLI prints and the browser build throws word for word. No caller acts on one kind of refusal
differently from another.

## Features

- `verify_attestation`: the quote's certificate chain up to a vendor root pinned in the binary, the
  VCEK bound to the quote's chip id and reported TCB, the root's revocation list current and not
  revoking the intermediate, the report's signature by the VCEK, the report data binding the served
  bundle, and the first release in the list whose launch digest, guest policy, minimum TCB, and
  configuration hashes admit the quote
- The result names the release, the enclave address, the configuration, and the TLS key hash, and
  carries the validity window: the span of times at which every date check passes
- With one release, a refusal says which check failed; with several, it names each release's
  refusal
- A mock quote never verifies; `verify_mock_attestation` reads one for development and returns only
  the bundle it binds
- Signatures on RustCrypto (`p384`, `rsa`): RSASSA-PSS with SHA-384, which the vendor's chains use,
  and ECDSA on P-384 with SHA-384, which the report and the test chain use; any other algorithm is
  refused
- `RelayedCollateral`: the four items as the node serves them, each bounded, and the check the node
  runs on what it fetches before serving it
- `ReleaseManifest`: the release's published TOML, every unknown key refused
- `Attestation` and `AttestationBundle`: the attestation's JSON and the bundle its report data
  hashes, shared with the node that builds them
- `test-fixtures` feature: a stand-in vendor chain, forged reports, and a release manifest, for this
  crate's tests, the node's, and the dev scenarios

## Repository Structure

- `src/lib.rs` — `verify_attestation`, `verify_mock_attestation`, the served-bundle check, and
  release matching
- `src/attestation.rs` — the attestation JSON, the bundle, and the mock quote's marker
- `src/chain.rs` — the pinned roots, the chain's links and dates, the VCEK's binding, and the
  revocation list
- `src/signature.rs` — certificate and revocation-list signatures on RustCrypto
- `src/report.rs` — the SEV-SNP attestation report layout, parsing, and signature
- `src/release.rs` — the release manifest
- `src/collateral.rs` — the relayed collateral document and its checks
- `src/roots/` — the vendor root certificates compiled in
- `src/fixtures.rs` — the `test-fixtures` stand-in chain, reports, and manifest
- `src/tests/mod.rs` — verification, the validity window, the time argument, the mock path, a
  corrupted report signature, and revocation through collateral
- `src/tests/chain.rs` — the vendor's own intermediates, altered signatures, unsupported
  algorithms, and the revocation list's freshness, issuer, and serial scope
- `src/tests/ask-*.pem` — the vendor's intermediates, which those tests verify against the roots
- `src/tests/releases.rs` — release lists: which release is named and how each refuses
- `src/tests/signature.rs` — each branch of the certificate signature check, and the 2048-bit RSA
  minimum at its boundary
- `examples/verifier_fixtures.rs` — writes the attestation fixtures `scripts/dev-wasm-verifier.sh`
  checks with the CLI and the browser build

## Usage

```rust
use engine_verify::{ReleaseManifest, RelayedCollateral, verify_attestation};

let attestation = serde_json::from_str(&attestation_json)?; // GET /v1/attestation
let releases = [ReleaseManifest::from_toml(&current)?, ReleaseManifest::from_toml(&previous)?];
let collateral = RelayedCollateral::from_json(&collateral_json)?; // GET /v1/attestation/collateral
let verified = verify_attestation(&attestation, &releases, &collateral, None, now_unix_secs)?;
println!("{} runs {} with {}", hex::encode(verified.enclave_address), verified.release_name, verified.config_name);
```

## Testing

```bash
cargo test -p engine-verify
```

## Authors

The monkeytrading contributors

## License

Licensed under the Apache License, Version 2.0; see `LICENSE`.
