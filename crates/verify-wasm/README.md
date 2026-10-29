# engine-verify-wasm

## Overview

The engine's verifier for browsers. A page that may talk only to the node's origin verifies the
node's attestation against the release manifests it pins, then every signed response and stream
frame it relies on, with the Rust code `engine verify-attestation`, `engine verify-response`, and
`engine verify-frames` run: `engine-verify` and `engine-wire`, built for WebAssembly with
wasm-bindgen. The page passes bodies and header values exactly as it received them and never
recomputes a digest itself.

## Features

- `verifyAttestation(attestation, manifests, collateral, nowMs, extraRootPem?)`: the
  `/v1/attestation` JSON, the pinned `release.toml` texts (the current release and the previous one
  across an upgrade), the `/v1/attestation/collateral` JSON, and the browser's clock; returns the
  release, configuration, enclave address, TLS key hash, and the validity window
  (`validFromMs`, `validUntilMs`) a second clock, such as the node's signed time, must fall inside
- A mock quote never verifies; `verifyMockAttestation(attestation)` is the development entry point,
  and its result says `mock: true`
- `ExpectedSigner(signer, chainId, vault, challenge)`: the verified enclave address, the deployment,
  and the challenge a request or connection sent
- `verifyResponse(expected, method, pathAndQuery, requestBody, status, signature, time, seq,
  echoedChallenge, body)`: the request as sent, the status, the four `X-Engine-*` header values
  (`null` when absent), and the body as received
- `FrameVerifier(expected, pathAndQuery).verifyFrame(text)`: each frame's text in the order
  received; returns the message, and once one fails every later one does
- A failed check throws its message as a string, the same message the CLI prints

## Repository Structure

- `src/lib.rs` — the exported functions and classes
- `src/tests.rs` — the exports run natively against the fixtures and `engine-wire`'s vectors

## Usage

`scripts/build-wasm-verifier.sh <out-dir>` builds the package: `engine_verify_bg.wasm`, its glue
`engine_verify.js` (an ES module), the TypeScript types, `package.json`, and `SHA256SUMS`. The
release build uses the pinned Rust toolchain and wasm-bindgen CLI and maps build paths to fixed
names, so a commit builds to the same bytes on any x86_64 Linux machine; CI builds it twice, from
two checkout paths, and compares. Each release publishes the package with its
`SHA256SUMS` and `release.toml` (`image/README.md`). A console installs the package from the
release and checks the `.wasm` against the SHA-256 it pins before using it.

```js
import init, { ExpectedSigner, FrameVerifier, verifyAttestation, verifyResponse } from 'engine-verify';

await init();
const attested = verifyAttestation(attestationText, [currentRelease, previousRelease], collateralText, Date.now());
const challenge = randomHex(32);
const response = await fetch('/v1/markets', { headers: { 'X-Engine-Challenge': challenge } });
const body = new Uint8Array(await response.arrayBuffer());
const h = (name) => response.headers.get(name);
const expected = new ExpectedSigner(attested.enclaveAddress, 31337n, vault, challenge);
verifyResponse(expected, 'GET', '/v1/markets', new Uint8Array(), response.status,
  h('x-engine-signature'), h('x-engine-time'), h('x-engine-seq'), h('x-engine-challenge'), body);
```

## Testing

```bash
cargo test -p engine-verify-wasm
./scripts/dev-wasm-verifier.sh
```

The dev scenario builds the package and checks it under Node against the CLI on every attestation
fixture and signed-answer vector; it needs the wasm32-unknown-unknown target, wasm-bindgen-cli at the
pinned version, and Node 18 or later.

## Authors

The monkeytrading contributors

## License

Licensed under the Apache License, Version 2.0; see `LICENSE`.
