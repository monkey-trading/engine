# engine-wire

The definition of the trading server's signed answers, shared by the server and every client that
checks them.

The enclave's key signs each API response, and each frame of a signed stream connection.
`engine-wire` defines the digests those signatures cover, the checks a client runs on them, and a
set of known-answer vectors. The server uses the crate to sign its answers.
`engine verify-response`, `engine verify-frames` and the
[browser verifier](../verify-wasm/README.md) use it to check them.
The crate performs no input or output and needs no async runtime, so it builds for WebAssembly.

## Usage

| Item | Purpose |
| --- | --- |
| `response_digest` | The keccak-256 digest that a response's signature covers: the domain tag `engine.response.v1`, the chain id, the vault, the client's challenge, the server's time, the sequence number of the log's head, the status, and SHA-256 hashes of the method and path, the request body and the response body |
| `frame_digest` | The digest that a stream frame's signature covers: the domain tag `engine.frame.v1`, the chain id, the vault, the connection's challenge and opening request, the previous frame's digest, the frame's position `n`, the time, and a SHA-256 hash of the message |
| `verify_response` | Checks an `X-Engine-Signature` against the attested enclave address |
| `verify_received` | Checks a response from its status, its `X-Engine-*` header values as received and its body. It refuses a response that lacks a signature, a time or a sequence number, and one that echoes a challenge other than the one sent. |
| `FrameVerifier` | Checks a connection's frames in order from `n` equal to 0. It refuses a dropped, repeated, reordered or inserted frame, and every frame after the first one that fails. `verify_text` takes a frame's JSON exactly as received. |
| `SignedFrame` | The stream's envelope, `{"v":1,"n":…,"time":…,"msg":"…","sig":"0x…"}` |
| `Connection` | The challenge and opening request that every frame on a connection is bound to |
| `parse_challenge` | Accepts a challenge of exactly 64 hexadecimal characters, as the server does |

A client checks a response and a stream connection as follows:

```rust
use engine_wire::{Connection, Deployment, FrameVerifier, Response, verify_response};

let deployment = Deployment { chain_id: 31337, vault: [0; 20] };
let response = Response {
    challenge,          // the 32 bytes this client sent in X-Engine-Challenge
    time_ms,            // X-Engine-Time
    head_seq,           // X-Engine-Seq
    status: 200,
    method: "GET",
    path_and_query: "/v1/markets",
    request_body: b"",  // the body this client sent
    body: &body,
};
verify_response(&attested_address, &deployment, &response, &signature)?;

let connection = Connection::new(challenge, "/v1/stream?signed=1&challenge=…"); // as sent
let mut frames = FrameVerifier::new(attested_address, deployment, connection);
frames.verify(&serde_json::from_str(&text)?)?; // each frame in the order received
```

A client verifies an answer with the challenge, request and body that it sent itself, and never with
the challenge that the response echoes. It sends a fresh random challenge with each request or
connection that it relies on.

A signature binds the bytes of a request and its challenge, but not a single delivery of them.
Whoever relays the traffic can send the server a copy of a request, and the server answers each
copy. A verified refusal of a request that changes state, such as an order or a withdrawal, may
therefore be the answer to a second copy that arrived after the first was executed. Before showing
such a refusal, a client confirms it with a signed read of the account under a fresh challenge, sent
after the refusal arrived. An account's next nonce does not prove that the request was executed,
because a request refused for its expiry also uses its nonce.

The server signs its host's clock only while that clock is within its drift bound of trusted time,
so a signed time is accurate to within that bound. Without trusted time, the server signs nothing.

## Layout

| Path | Contents |
| --- | --- |
| `src/` | The digests, the envelope and the checks, with unit tests of each field a signature covers, malformed signatures and challenges, and frame sequences |
| `tests/` | The known-answer vectors in `vectors.json`, signed by the published development enclave key, and the test that replays them and derives each signature again |

## Testing

```shell
cargo test -p engine-wire
```

The browser verifier's scenario replays the same vectors under Node.js, as its
[README](../verify-wasm/README.md#testing) describes.

## License

Licensed under the Apache License, Version 2.0, whose text is in [LICENSE](../../LICENSE).
