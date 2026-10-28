# engine-wire

## Overview

The definition of the node's signed answers, shared by the node that signs them and every client
that checks them. The attested enclave key signs each API response over the request it answers, the
client's challenge, the node's time and log head, and the status and body; and each frame of a
signed stream connection over its place on the connection, the node's time, and the message. This
crate holds those digests, the checks a client runs on them, and known-answer vectors. It does no IO
and needs no runtime, so a browser verifier can build it for WebAssembly.

## Features

- `response_digest`: keccak-256 over `engine.response.v1`, the chain id, the vault, the challenge,
  the time, the log head's seq, the status, SHA-256 of the method and path, and SHA-256 of the body
- `frame_digest`: keccak-256 over `engine.frame.v1`, the chain id, the vault, the challenge, the
  frame's place `n`, the time, and SHA-256 of the message
- `verify_response`: checks an `X-Engine-Signature` against the attested address
- `FrameVerifier`: checks a connection's frames in order from `n` 0, refusing a dropped, repeated,
  or reordered frame, and every frame after the first that fails
- `SignedFrame`: the stream envelope, `{"v":1,"n":…,"time":…,"msg":"…","sig":"0x…"}`
- `parse_challenge`: exactly 64 hex characters, as the node accepts them
- Header names and domain tags as constants
- `tests/vectors.json`: valid responses and connections, and one refusal of each kind, every
  digest and signature re-derived by `tests/vectors.rs`

## Repository Structure

- `src/lib.rs` — the digests, the envelope, and the checks
- `src/tests.rs` — each field a signature covers, malformed signatures, challenges, and frame
  sequences
- `tests/vectors.json` — known answers, signed by the published mock enclave key
- `tests/vectors.rs` — replays the vectors and re-derives each signature from how it was made

## Usage

```rust
use engine_wire::{Deployment, FrameVerifier, Response, verify_response};

let deployment = Deployment { chain_id: 31337, vault: [0; 20] };
let response = Response {
    challenge,          // the 32 bytes this client sent in X-Engine-Challenge
    time_ms,            // X-Engine-Time
    head_seq,           // X-Engine-Seq
    status: 200,
    method: "GET",
    path_and_query: "/v1/markets",
    body: &body,
};
verify_response(&attested_address, &deployment, &response, &signature)?;

let mut frames = FrameVerifier::new(attested_address, deployment, challenge);
frames.verify(&serde_json::from_str(&text)?)?; // each frame in the order received
```

A client verifies with the challenge it sent, never the one a response echoes, and sends a fresh
random one with each request or connection it relies on.

## Testing

```bash
cargo test -p engine-wire
```

## Authors

The monkeytrading contributors

## License

Licensed under the Apache License, Version 2.0; see `LICENSE`.
