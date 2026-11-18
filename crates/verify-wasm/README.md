# engine-verify-wasm

The attestation and signed-answer checks of the `engine` command-line tools, compiled to
WebAssembly for web browsers.

A web page that talks only to the trading server can use this package to check the server for
itself. The page first verifies the server's attestation against the release manifests it pins.
It then verifies every signed response and stream frame it relies on. The checks are the same Rust
code that `engine verify-attestation`, `engine verify-response` and `engine verify-frames` run:
[`engine-verify`](../verify/README.md) and [`engine-wire`](../wire/README.md), built with
wasm-bindgen. The page passes bodies and header values exactly as it received them, and never
computes a digest itself.

## Usage

`scripts/build-wasm-verifier.sh` builds the npm package, which a web page imports as
`engine-verify`:

```js
import init, { ExpectedSigner, verifyAttestation, verifyResponse } from 'engine-verify';

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

`randomHex` stands for any source of 32 random bytes in hexadecimal, such as
`crypto.getRandomValues`. The package exports the following:

| Export | Purpose |
| --- | --- |
| `verifyAttestation(attestation, manifests, collateral, nowMs, extraRootPem?)` | Checks the `/v1/attestation` JSON against the pinned `release.toml` texts, using the `/v1/attestation/collateral` JSON and the browser's clock. It returns the release, the configuration, the enclave address, the TLS key's hash and the validity window (`validFromMs`, `validUntilMs`). A second clock, such as the server's signed time, must fall inside that window. |
| `verifyMockAttestation(attestation)` | Reads a development server's simulated attestation, which never passes `verifyAttestation`. Its result says `mock: true`. |
| `ExpectedSigner(signer, chainId, vault, challenge)` | The verified enclave address, the deployment, and the challenge that a request or connection sent |
| `verifyResponse(expected, method, pathAndQuery, requestBody, status, signature, time, seq, echoedChallenge, body)` | Checks a response from the request as sent, the status, the four `X-Engine-*` header values (`null` when absent) and the body as received |
| `FrameVerifier(expected, pathAndQuery).verifyFrame(text)` | Checks each frame's text in the order received and returns the message. Once one frame fails, every later frame fails too. |

A failed check throws its message as a string, which is the same message the command-line tools
print.

The package holds `engine_verify_bg.wasm`, its JavaScript module `engine_verify.js`, the TypeScript
types, `package.json` and `SHA256SUMS`. The build uses a pinned Rust toolchain and wasm-bindgen,
and it maps build paths to fixed names, so a commit builds to the same bytes on any x86_64 Linux
machine.
Each release publishes the package with its `SHA256SUMS` and the release's `release.toml`, as the
[image's release steps](../../image/README.md#release) describe. A web page should check the
`.wasm` file against the SHA-256 hash it pins before using it.

## Layout

| Path | Contents |
| --- | --- |
| `src/lib.rs` | The exported functions and classes |
| `src/tests.rs` | The exports run natively against the attestation fixtures and the signed-answer vectors |

## Testing

```shell
cargo test -p engine-verify-wasm
scripts/build-wasm-verifier.sh out/verifier
./scripts/dev-wasm-verifier.sh out/verifier
```

The first command runs the exports natively. The second builds the package into `out/verifier`; it
needs rustup with Rust 1.99.1 and its `wasm32-unknown-unknown` target, and wasm-bindgen-cli
0.2.129. It fails if the package is larger than 1.5 MB gzipped. The third runs the package under
Node 18 or later against every attestation fixture and signed-answer vector, and compares its
verdicts with those of the `engine` command-line tools. The check has passed when it prints
`PASS: wasm and native verdicts match on <n> fixtures`.

## License

Licensed under the Apache License, Version 2.0. The licence text is in
[`LICENSE`](../../LICENSE).
