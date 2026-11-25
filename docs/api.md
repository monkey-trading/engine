# API reference

The trading server provides a REST API under `/v1` and a WebSocket stream at `/v1/stream`. This page
describes every route the server serves: the request each one takes, the answer it gives, and the
refusals a client must handle. The [README](../README.md) explains what the server does and how to
run one on your own machine.

- [Conventions](#conventions)
- [Every route](#every-route)
- [Signing requests](#signing-requests)
- [Signed answers](#signed-answers)
- [Limits and refusals](#limits-and-refusals)
- [Markets and prices](#markets-and-prices)
- [Accounts](#accounts)
- [Orders and positions](#orders-and-positions)
- [Funding an account](#funding-an-account)
- [Withdrawals](#withdrawals)
- [MONKEY, distributions and the exit](#monkey-distributions-and-the-exit)
- [The house and the operator](#the-house-and-the-operator)
- [Attestation and the log archive](#attestation-and-the-log-archive)
- [The stream](#the-stream)
- [Operator routes](#operator-routes)

## Conventions

### Connecting

The server listens on the address that `node.listen` sets, which is `127.0.0.1:8080` in
`config/dev.toml`. Configuration keys and commands call a running trading server a node, as this
key does. It accepts HTTP/1.1 over TLS 1.3 and nothing else. TLS ends inside the enclave,
the confidential virtual machine in which the server runs and whose memory the host machine can
neither read nor alter. The enclave generates the TLS key itself, and the key never leaves it. Every
`GET` route also answers `HEAD`, with the same status and headers and no body.

By default the server presents a self-signed certificate for `localhost` and `127.0.0.1`. When the
configuration includes `[api.tls.acme]`, the server presents a certificate for the API's domain from
a public certificate authority, issued for the same key. A production build refuses to start without
that section.

A client that must know it is talking to the enclave pins the TLS key. The enclave's attestation is
a report, signed by the processor, that identifies the code the enclave runs and the keys it holds.
Among those keys is the SHA-256 hash of the TLS certificate's DER-encoded SubjectPublicKeyInfo,
which `GET /v1/attestation` serves as `tls_public_key_sha256`. The following commands pin that hash
with curl and call the health route:

```shell
API=https://127.0.0.1:8080
PIN=$(curl -sk $API/v1/attestation | jq -r '.tls_public_key_sha256[2:]' | xxd -r -p | base64)
curl -s --insecure --pinnedpubkey "sha256//$PIN" $API/v1/health
```

The last command prints `ok`. When the server presents any other key, curl prints nothing and exits
with code 90. The second command reads the hash from the server only to show the form. Against a
live server, take the hash from an attestation that you have verified with
`engine verify-attestation`, as the [README](../README.md) describes.

### Values in requests and answers

Request and answer bodies are JSON, and a request with a body must send
`Content-Type: application/json`.

| Kind of value | Form |
| --- | --- |
| Addresses | `0x` followed by 40 hexadecimal digits. Requests may use either case, and answers use lower case. |
| Prices, quantities and amounts | Decimal strings with at most nine decimal places, such as `"0.5"`. A request may also give a whole number as a JSON integer. No price, quantity or amount that a client supplies may exceed 1,000,000,000. |
| Currency | US dollars, unless the field says otherwise. Fields ending in `_usdc_units` count USDC base units, of which one USDC has 10^6. MONKEY amounts count base units of the 18-decimal token. Both are decimal strings. |
| Times | Milliseconds since the Unix epoch, as JSON integers. The `issued_at` and `expires_at` of a withdrawal authorisation are the exception, in seconds, as the vault reads them. |
| Rates | Integers in basis points, in fields ending in `_bps`. A basis point is one hundredth of one percent. Spread bands give their rate in tenths of a basis point, in `tenth_bps`. |
| Markets | Numbered by `id`, an unsigned 32-bit integer. |
| Hashes and signatures | `0x` followed by hexadecimal digits. |

### The log, seqs and offsets

The server handles every instruction it accepts as a command. It records its commands in its public
log in the order in which it processes them, and a command's seq is its sequence number in that
order, starting from 0. Each command produces events, which describe its effects. An event is a JSON
object whose `type` names it in snake case, such as `fill` or `order_rejected`. A rejection's
`reason` is an object whose `kind` names it, such as
`{"kind": "insufficient_margin", "equity": "…", "required": "…"}`.

At regular intervals the enclave signs a checkpoint. A checkpoint names a seq and the state root at
that seq, which is a Merkle root over the entire state. The log also holds each withdrawal
authorisation that the enclave signs. A message's place in the log is its offset, written
`<seq>:<index>`. Index 0 is the command. Any checkpoint and withdrawal authorisations that the
server wrote at the same seq take the following indexes, in the order in which it wrote them.

### Errors

A refusal has a JSON body of the form `{"error": "<message>"}`, with further fields where this page
says so. The status code tells the client what kind of refusal it is.

| Status | Meaning |
| --- | --- |
| 400 | The path, query or body holds a value the route does not accept, the request's expiry has passed, or the `X-Engine-Challenge` header is malformed. |
| 401 | The signature is missing, or it is not the account's or a live session key's. A revoked session key gets `session key revoked`, and an expired one `session key expired`. |
| 403 | The account may not make this request yet, for example because it has never been credited. A payout reissue that screening refuses carries `{"error": "access refused", "reason": "<reason>"}`. A stream connection from a browser origin that is not allowed is also refused with 403. |
| 404 | No such resource: an account that has never been credited, a proof that no checkpoint holds yet, or a route that this server does not offer. A path that matches no route gets 404 with an empty body. |
| 408 | The client did not send the body within `api.read_timeout`. |
| 409 | The nonce is not the signer's next, with the next nonce in `expected`, or a copy of the same request is still being screened. |
| 413 | The body is larger than `api.max_body_bytes`. |
| 415 | A request with a body did not send `Content-Type: application/json`. |
| 422 | The body is JSON but does not match the route's request, for example because of an unknown or missing field. |
| 429 | The client or the account is over a rate limit. `Retry-After` gives the wait in whole seconds. |
| 500 | The server failed to read its own records. The cause is in its log. |
| 501 | The server cannot offer the route in its current setup, for example a withdrawal request while custody has no finalized checkpoint service to pay it out. |
| 503 | The server cannot answer now. Where it expects to answer soon, it sends `Retry-After`. |

A request that the server refuses with a 4xx status never reaches the log, and it uses no nonce. A
request that reaches the log is answered with status 200, even when the engine rejects it. In that
case the answer's events hold the rejection. A client therefore reads the events of every 200 answer
to learn the outcome.

The bodies of 400 refusals that come from parsing the path or query, and of 415 and 422 refusals,
are plain text rather than JSON.

### Durable answers

Every answer reflects only commands that the log has stored durably. When a read arrives while the
newest batch of commands is still being written to storage, the answer waits for that batch, so no
answer shows a state that a crash could undo.

### Paging

Routes that return a list take a `limit` and a cursor, and return `next`. Passing `next` back as the
cursor gives the following page, until `next` is `null`. Each route below states its own default and
maximum `limit`.

## Every route

| Method | Path | Purpose |
| --- | --- | --- |
| GET | `/v1/health` | Answers `ok` while the server runs |
| GET | `/v1/status` | Whether the server takes orders, and if not, why not |
| GET | `/v1/notice` | The operator's status notice |
| GET | `/v1/markets` | Every market, with its specification, state and pricing |
| GET | `/v1/markets/{id}/marks` | One market's price history |
| GET | `/v1/benchmarks` | Benchmark interest rates and the next financing rollover |
| GET | `/v1/benchmarks/schedule` | The rollover schedule, for the operator's benchmark worker |
| GET | `/v1/accounts/{address}` | An account's balance, margin, positions and working orders |
| GET | `/v1/accounts/{address}/nonce` | A signing key's next nonce |
| GET | `/v1/accounts/{address}/eligibility` | The eligibility class an account has declared |
| GET | `/v1/accounts/{address}/history` | An account's events, newest first |
| GET | `/v1/accounts/{address}/financing/{seq}` | An account's financing charges at the rollovers of one command, with their proof |
| GET | `/v1/accounts/{address}/proof` | A proof of an account's balance and positions |
| GET | `/v1/accounts/{address}/preview` | What a market order of an exact size would do now |
| POST | `/v1/orders` | Places an order |
| POST | `/v1/orders/cancel` | Cancels one working order |
| POST | `/v1/orders/cancel-all` | Cancels every working order of an account |
| POST | `/v1/positions/close-all` | Closes whole positions in the markets named |
| POST | `/v1/positions/levels` | Sets a position's take profit and stop loss |
| POST | `/v1/eligibility` | Records an account's eligibility declaration |
| POST | `/v1/session-keys` | Registers a session key |
| DELETE | `/v1/session-keys` | Revokes a session key |
| GET | `/v1/access/pending/{chain_ref}` | A deposit waiting to be screened |
| GET | `/v1/access/holds/{chain_ref}` | A deposit held by screening |
| POST | `/v1/dev/deposit` | Credits an account without a chain deposit, on a server with simulated custody |
| POST | `/v1/withdrawals` | Requests a withdrawal |
| GET | `/v1/withdrawals` | The public withdrawal queue |
| GET | `/v1/accounts/{address}/withdrawals` | An account's requests in the withdrawal queue |
| GET | `/v1/accounts/{address}/payouts` | An account's unpaid withdrawals and their authorisations |
| POST | `/v1/accounts/{address}/withdrawals/{nonce}/reissue` | Asks for an expired withdrawal authorisation to be signed again |
| GET | `/v1/accounts/{address}/emissions` | A proof of an account's MONKEY entitlement |
| GET | `/v1/accounts/{address}/distributions` | A proof of an account's USDC distributions |
| GET | `/v1/accounts/{address}/exit` | Every proof an account needs for the vault's exit |
| GET | `/v1/monkey` | MONKEY emission, staking and distribution totals |
| GET | `/v1/monkey/activity` | The newest loss allocations, stakes and unstakes |
| GET | `/v1/monkey/distributions` | Closed distribution epochs |
| GET | `/v1/awards` | Bug bounty awards, the queue they are paid from and the cap on each payment |
| GET | `/v1/house` | The house's cash, net asset value, exposure and client positions |
| GET | `/v1/house/positions` | One market's client positions by close-out price and by profit |
| GET | `/v1/house/state` | The trading settings in force, pending notices and the latest checkpoint |
| GET | `/v1/stats/series` | Time series behind the dashboards |
| GET | `/v1/operator` | The operator key in effect, elections and daily turns |
| GET | `/v1/operators` | The operator registry's candidates |
| POST | `/v1/standby/vouch` | Vouches for a rotation-set member's standby key |
| POST | `/v1/admin` | The operator's signed administrative commands |
| GET | `/v1/attestation` | The enclave's attestation |
| GET | `/v1/attestation/collateral` | The processor vendor's certificates and revocation list for the attestation |
| GET | `/v1/archive/{file}` | The log archive's manifest, or one completed log segment |
| GET | `/v1/archive/snapshots/{file}` | The daily snapshots' manifest, or one daily snapshot |
| GET | `/v1/stream` | The WebSocket stream of the log, quotes, status and notices |

## Signing requests

Every request that acts for an account, apart from the simulated deposit, carries the account's
signature or the signature of a key that the account has authorised. The server checks signatures
under EIP-712, the Ethereum standard for signing typed, structured data, which wallets support
through `eth_signTypedData_v4`. Each signed request also carries a nonce and an expiry, so that one
signature authorises one request, once, for a few minutes.

### The domain

Every request is signed under one EIP-712 domain.

| Field | Value |
| --- | --- |
| `name` | `monkeytrading` |
| `version` | `1` |
| `chainId` | The id of the chain that holds the vault, which `custody.chain_id` sets |
| `verifyingContract` | The vault's address, which `custody.vault` sets |

Because the domain names the chain and the vault, a signature made for one deployment is worthless
on another. The local server that `config/dev.toml` configures uses chain 31337 and the zero address
as its vault.

### The `auth` object

Every signed request body, except a withdrawal reissue, carries an `auth` object:

```json
"auth": { "nonce": 7, "expires_at": 1792540800000, "signature": "0x…" }
```

| Field | Meaning |
| --- | --- |
| `nonce` | The signing key's next nonce. |
| `expires_at` | The latest time at which the server may process the request, in milliseconds since the Unix epoch. |
| `signature` | The 65-byte secp256k1 signature `r ‖ s ‖ v`, with `v` equal to 27 or 28, as wallets return it. |

### Nonces

A nonce is a counter that each signing key's requests must use in order, so that each signed request
can take effect only once. `GET /v1/accounts/{address}/nonce` returns `{"next_nonce": <n>}`, the
nonce that the address's next signed request must carry. The answer is available for any address,
including one that has never been credited.

The account's key and each of its session keys count separately. A request that a session key signs
carries the session key's own next nonce, which the same route returns for the session key's
address.

A request whose nonce is not the signer's next is refused with 409 and
`{"error": "invalid nonce", "expected": <n>}`. Once a request reaches the log, it uses its nonce,
even if the engine then rejects it, so the same signed request can never take effect twice.

### Expiry

The expiry bounds how long anyone who holds a signed request, including the host, can wait before
delivering it. It does not bound the order that the request places. A limit or stop order stays
working until it is executed or cancelled.

- When the expiry has already passed on arrival, the server refuses the request with 400
  `request expired at <expires_at>`, and the nonce is not used.
- When the expiry lies more than five minutes after the time at which the server processes the
  request, the request reaches the log and the engine rejects it with the reason `expiry_too_far`,
  which gives the latest expiry it would have accepted. The rejection uses the nonce.
- For an account that has never been credited, an expiry more than five minutes ahead is refused
  with 400 before the log.

### Typed data

Each route signs one EIP-712 type. The fields are signed in the order shown.

| Route | Type | Fields |
| --- | --- | --- |
| `POST /v1/orders`, naming no level | `OrderRequest` | `address account`, `uint64 nonce`, `int64 expiresAt`, `uint64 clientId`, `uint32 market`, `string side`, `int128 qty`, `string kind`, `int128 price`, `bool hasMaxSlippage`, `int64 maxSlippageBps`, `bool reduceOnly` |
| `POST /v1/orders`, naming a take profit or stop loss | `OrderWithLevels` | The fields of `OrderRequest`, then `bool hasTakeProfit`, `int128 takeProfit`, `bool hasStopLoss`, `int128 stopLoss` |
| `POST /v1/positions/levels` | `SetPositionLevels` | `address account`, `uint64 nonce`, `int64 expiresAt`, `uint32 market`, `bool hasTakeProfit`, `int128 takeProfit`, `bool hasStopLoss`, `int128 stopLoss` |
| `POST /v1/orders/cancel` | `CancelRequest` | `address account`, `uint64 nonce`, `int64 expiresAt`, `uint64 orderId` |
| `POST /v1/orders/cancel-all` | `CancelAllRequest` | `address account`, `uint64 nonce`, `int64 expiresAt` |
| `POST /v1/positions/close-all` | `CloseAllRequest` | `address account`, `uint64 nonce`, `int64 expiresAt`, `CloseEntry[] closes`, `bool hasMaxSlippage`, `int64 maxSlippageBps`, where `CloseEntry` is `uint32 market`, `int128 quoted` |
| `POST /v1/withdrawals` | `WithdrawalRequest` | `address account`, `uint64 nonce`, `int64 expiresAt`, `int128 amount` |
| `POST /v1/eligibility` | `EligibilityRequest` | `address account`, `uint64 nonce`, `int64 expiresAt`, `string statement` |
| `POST /v1/session-keys` | `RegisterSessionKey` | `address account`, `uint64 nonce`, `int64 expiresAt`, `address sessionKey`, `int64 sessionExpiresAt` |
| `DELETE /v1/session-keys` | `RevokeSessionKey` | `address account`, `uint64 nonce`, `int64 expiresAt`, `address sessionKey` |
| `POST /v1/admin` | `AdminRequest` | `uint64 nonce`, `int64 expiresAt`, `string command` |

The values in the signed message relate to the request body as follows:

- `nonce` and `expiresAt` are the `auth` object's `nonce` and `expires_at`.
- Prices, quantities and amounts are signed as integers in units of 10^-9, so a quantity of `"0.5"`
  is signed as `500000000`.
- `side` is `buy` or `sell`, and `kind` is `market`, `limit` or `stop`.
- An order's `price` is a market order's `quoted` price, or a limit or stop order's `price`.
- A value that the body leaves out is signed with its `has…` flag set to false and the value 0.
- A close-all signs its `closes` in the order in which the body lists them.
- A declaration's `statement` is the exact sentence of the declared class, given under
  [Eligibility declarations](#eligibility-declarations).
- A session-key registration's `sessionExpiresAt` is the body's `expires_at`, the time at which the
  key itself expires.
- An admin request's `command` is the administrative command's JSON as the engine encodes it.

[`crates/core/tests/fixtures/eip712.json`](../crates/core/tests/fixtures/eip712.json) holds the
typed data and signature of each client request type for a published test key, and
[`crates/core/examples/sign_request.rs`](../crates/core/examples/sign_request.rs) signs a request
body as a client does.

### Admission

Before a signed request reaches the log, the server checks it in the following ways:

- The signature must recover to the account's address, or to a live session key of the account for a
  request that a session key may sign.
- The account must have been credited at least once. An eligibility declaration and a session-key
  request are the exceptions, as [Funding an account](#funding-an-account) describes.
- The nonce must be the signer's next, and the expiry must not have passed.
- The account must be within its request rate, as [Limits and refusals](#limits-and-refusals)
  describes.
- For an order, a close-all, a take profit and stop loss request, a withdrawal or a declaration, the
  server then screens the request, as [Screening](#screening) describes.

A request refused by any of these checks never reaches the log and uses no nonce, so it remains
valid until its expiry. When the engine processes a request, it checks the signature, the nonce and
the expiry again.

When two copies of one signed request arrive together, both can pass these checks. The engine then
processes one copy and rejects the other with the reason `invalid_nonce`. A copy that arrives while
the first is still being screened is refused with 409 `the same request is already being screened`.

### Session keys

A session key is a short-lived key, typically held in a browser, that an account authorises to trade
on its behalf without a wallet prompt for every order. A session key may sign orders, cancels,
cancel-all and close-all requests, and take profit and stop loss requests. A session key can never
move funds. A withdrawal that a session key signs reaches the log and is rejected with the reason
`session_key_scope`.

An account registers a key with `POST /v1/session-keys` and revokes one with
`DELETE /v1/session-keys`. Only the account's own key may sign either request. A session key's
signature on either is refused with 401 `invalid signature`.

| Field | Registration | Revocation | Meaning |
| --- | --- | --- | --- |
| `account` | Required | Required | The account's address. |
| `session_key` | Required | Required | The session key's address. |
| `expires_at` | Required | Not taken | When the key stops working, in milliseconds. It must be in the future and at most 24 hours after the time at which the server processes the registration. |
| `auth` | Required | Required | The account's signature, nonce and request expiry. |

The answer is the events of the command: `session_key_registered`, `session_key_revoked`, or
`session_key_rejected` with its reason. An account may hold at most 10 live keys. An account may
introduce at most 100 new key addresses at once, and the allowance grows back by one address an
hour. Registering an address that the account has used before introduces nothing. A registration
past either limit is rejected with the reason `too_many_session_keys`.

A revoked key's requests are refused with 401 `session key revoked`, and an expired key's with 401
`session key expired`. All of an account's session keys share one request rate, apart from the
account's own, so a key that floods the server cannot stop the account from revoking it.

## Signed answers

The enclave signs every answer under `/v1`, including refusals, so that a client can check that the
answer came from the attested enclave and answers the request it sent. Five routes are not signed:
`/v1/health`, `/v1/attestation`, `/v1/attestation/collateral`, `/v1/archive/{file}` and
`/v1/archive/snapshots/{file}`. The
attestation and the archive carry their own proofs, and the health check carries nothing to rely on.

### The challenge

A client binds an answer to its own request with a challenge: 32 random bytes, sent as 64
hexadecimal characters without `0x` in the `X-Engine-Challenge` header. A request without the header
is answered under 32 zero bytes, which proves where the answer came from but not that it is fresh. A
request whose challenge is not 64 hexadecimal characters is refused with an unsigned 400.

A client sends a new random challenge with every request whose answer it relies on. It verifies the
answer against the challenge, method, path and body that it sent itself, never against the challenge
that the answer echoes.

### Headers

| Header | Content |
| --- | --- |
| `X-Engine-Signature` | The enclave key's 65-byte signature over the digest below, as `0x` hexadecimal. |
| `X-Engine-Time` | The server's time when it answered, in milliseconds since the Unix epoch. |
| `X-Engine-Seq` | The seq of the last command that the log had stored when the server answered, or 0 before the first. |
| `X-Engine-Challenge` | The challenge the answer is signed under, as lower-case hexadecimal. |
| `Cache-Control` | Always `no-store`, so that no cache keeps an answer bound to one request's challenge. The signature does not cover this header. |

The signature covers the keccak-256 hash of the following values, concatenated in this order at the
widths given:

1. The ASCII text `engine.response.v1`.
2. The chain id, as an unsigned 64-bit big-endian integer.
3. The vault's address, 20 bytes.
4. The challenge, 32 bytes.
5. The time, as an unsigned 64-bit big-endian integer.
6. The seq, as an unsigned 64-bit big-endian integer.
7. The HTTP status, as an unsigned 16-bit big-endian integer.
8. The SHA-256 hash of the method, a zero byte, and the path and query exactly as sent.
9. The SHA-256 hash of the request body, which is empty when the request had none.
10. The SHA-256 hash of the answer's body as sent, which is empty for a `HEAD` request.

The signing key's address is the attestation's `enclave_address`.
[`crates/wire`](../crates/wire/README.md) holds these definitions and the checks a client runs, and
[`crates/verify-wasm`](../crates/verify-wasm/README.md) builds the same checks for browsers.

### Verifying an answer

`engine verify-response` checks a saved answer. The following commands send a request with a new
challenge to the local server and verify the answer:

```shell
API=https://127.0.0.1:8080
CHALLENGE=$(od -An -N32 -tx1 /dev/urandom | tr -d ' \n')
curl -sk -D headers -o body -H "X-Engine-Challenge: $CHALLENGE" $API/v1/markets
cargo run -p engine-node -- verify-response \
  --signer "$(curl -sk $API/v1/attestation | jq -r .enclave_address)" \
  --chain-id 31337 --vault 0x0000000000000000000000000000000000000000 \
  --challenge $CHALLENGE --path /v1/markets --headers headers --body body
```

The last command prints `verified: status 200, signed by 0x… at <time> ms, log head seq <seq>`. If a
single byte of the saved body has changed, it prints
`Error: the signature is not the attested key's over these bytes` instead. For a `POST`, add
`--method POST` and `--request-body <file>`, where the file holds the exact body that was sent.
Against a live server, take the `--signer` address from an attestation that you have verified, and
the chain id and vault from the deployment you trade on.

### When an answer is unsigned

The server signs its time only while that time is trustworthy. It checks its clock against signed
readings from independent Roughtime servers. While the latest reading is more than 60 seconds old,
or while the host clock has drifted from it by more than `time.max_drift`, the server signs nothing.
Its answers then go out without `X-Engine-Signature`, and its signed stream connections close.

Some answers are produced before the signing step and are never signed:

- a 429 from the per-client rate limit;
- a 408 or 413 from reading the body;
- the answer to a browser's preflight request;
- a 403 refusal of a stream connection from a browser origin that is not allowed.

A client treats an unsigned answer on a signed route as no answer. It may retry the request.

### Confirming a refusal

A signature proves that the enclave sent an answer, not that the answer belongs to the only delivery
of the request. Someone who relays the traffic can deliver a copy of a signed request first, and the
server then refuses the client's own copy, for example with 409 for its nonce. Both answers verify.

**Before treating an order, a cancel or a withdrawal as refused, read the account again with
`GET /v1/accounts/{address}` or `GET /v1/accounts/{address}/withdrawals` under a new challenge, and
check whether the change took place.** A next nonce higher than the request's does not show that the
request took effect, because a request that the engine rejects also uses its nonce.

## Limits and refusals

### Rate limits

The server counts requests in token buckets. Each bucket holds a number of requests, called its
burst, and refills at a steady rate. A request over a limit is refused with 429 and a `Retry-After`
header that gives the wait in whole seconds.

| Limit | What it counts | Setting |
| --- | --- | --- |
| Per client | Every request from one client address. | `api.per_ip` |
| Per account | Every signed request for an account that has never been credited, or for a credited account whose volume tier names no rate. Every request of the operator, and every payout reissue. | `api.per_account` |
| Per volume tier | Every signed request for a credited account, at its volume tier's rate. | The tier table in the trading settings, and `api.request_bursts` |
| Per account's session keys | Every request that the account's session keys sign, together, at the same rate as the account. | As the account's |
| New connections per client | Connections opened by one client, closed before the TLS handshake when over. | `api.per_ip_connections` |

A client is its connection's source address. An IPv6 client counts by its /64 block. The server
ignores forwarding headers such as `X-Forwarded-For`, which anyone can forge. Behind a pass-through
proxy that the operator lists under `[api.proxy_protocol]`, the client is the address that the
proxy's PROXY protocol v2 header names.

A credited account's request rate depends on its volume tier, which depends on the volume it has
traded over the last 30 days. Each of its buckets holds the number of seconds of its rate that
`api.request_bursts` sets. The production template sets 2 seconds, and a production build refuses
more than 5. The production trading settings set these rates:

| Volume tier | Signed requests a second |
| --- | --- |
| 0 | 2 |
| 1 | 5 |
| 2 | 10 |
| 3 | 20 |

A request over its tier's rate gets a 429 whose body names the tier and its rate, such as
`{"error": "rate limited", "tier": 0, "requests_per_second": 2}`. `GET /v1/accounts/{address}` shows
an account's tier and rate under `volume`.

### Accounts that have never been credited

A wallet that has never been credited can put two kinds of request in the log: an eligibility
declaration, and, once it has declared, session-key registrations and revocations. Those requests
cost the wallet nothing, so the server limits them with budgets apart from the rates above, set in
`[api.fresh_accounts]`. Each such request takes a token from its client's share and from a budget
that all clients share. When either is empty, the request is refused with 429
`too many new wallets are being set up; try again later` and `Retry-After`. A production build
allows at most 60 such requests a minute, with a burst of 120, across all clients, and at most 12 a
minute, with a burst of 6, from one client. A declaration from an account whose deposit is already
waiting for it takes no token from these budgets, but is held to a rate of its own.
[`config/access-source.md`](../config/access-source.md) describes the budgets in full.

### Screening

The server screens orders, close-all requests, take profit and stop loss requests, withdrawals,
payout reissues and declarations before they reach the log. Screening uses the configured access
source's decision about the account and the client's address. The server keeps one decision for each
account and address and uses it for each of their requests while it is fresh, and asks the source
again before it runs out while the account sends requests from that address, so a request with a
fresh decision does not wait for the source. A request with none waits for the source's answer. A
refusal is kept and used in the same way until it runs out. The engine records the decision with
the request, and rejects the request in the log if the decision does not allow it. Cancels,
cancel-all requests and session-key requests are not screened.

A request that waits for the source holds one of a limited number of screening slots while the
source answers. One account's requests, including its session keys', and one client's requests may
hold only a share of the slots. A request past its share is refused with 429 and `Retry-After`.
Declarations may hold at most half of the slots.

Some slots are kept for requests that the account's state shows can reduce risk when the request
arrives: a reduce-only order or a take profit and stop loss request in a market where the account
holds a position, a close-all request naming at least one such market, a withdrawal while the
account has cash it can withdraw, and a payout reissue. An account holds at most one of the kept
slots at a time. Any other request, including a reduce-only order in a market where the account
holds no position, takes one of the other slots.

A request that finds every slot it may take in use is refused with 503 `screening is at capacity`,
or `declaration screening is at capacity` for a declaration, with `Retry-After`. Neither refusal
uses the nonce.

### Size, time and connection limits

| Limit | Refusal | Setting |
| --- | --- | --- |
| Request body size | 413 | `api.max_body_bytes` |
| Time to send the headers, and again the body | 408 for the body; the connection is closed for the headers | `api.read_timeout` |
| Time to finish the TLS handshake | The connection is closed | `api.handshake_timeout` |
| Open connections, in total and from one client | The connection is closed before its TLS handshake | `api.max_connections`, `api.max_connections_per_ip` |
| Open stream connections | 503 `too many stream subscribers` | `api.max_subscribers` |
| Time for a client to take an answer or a stream message | The connection is closed | `api.send_timeout` |

### A server frozen for a move

Before a deployment moves to a new vault, its operator starts the server with `engine run --frozen`,
and the new vault's servers run frozen until the public opening.
A frozen server answers every client `POST` and `DELETE` with 503
`the node is frozen for a move to a new vault and takes no client request`, and the request never
reaches the log. Orders, cancels, closes, levels, session keys, eligibility declarations,
withdrawals, payout reissues and simulated deposits are all refused. Reads, the stream,
`POST /v1/admin` and `POST /v1/standby/vouch` answer as before.

### Browser origins

A web page may call the API only from an origin that `api.allowed_origins` lists, such as
`https://console.example.com`. The list is empty by default, and a production build accepts only
`https` origins. Because the attestation binds the configuration, changing the list requires a new
release.

An answer to a request from a listed origin carries `Access-Control-Allow-Origin` with that origin
and `Vary: Origin`, including on refusals. A listed origin may read `Retry-After` and the four
`X-Engine-*` headers. A preflight `OPTIONS` request to any `/v1` route from a listed origin is
answered with 204, allowing the methods `GET`, `POST`, `DELETE` and `OPTIONS`, the headers
`Content-Type` and `X-Engine-Challenge`, and a cache time of 600 seconds. A preflight counts against
the client's rate like any other request.

Browsers do not apply these rules to WebSockets, so the stream checks the origin itself. A stream
connection whose `Origin` header names an origin that is not listed is refused with 403
`origin not allowed`. A connection without an `Origin` header, as from a program rather than a
browser, is not affected. `/v1/admin` never sends these headers, so no web page can call it.

## Markets and prices

| Method | Path | Purpose |
| --- | --- | --- |
| GET | `/v1/markets` | Every market, with its specification, state and pricing |
| GET | `/v1/markets/{id}/marks` | One market's price history |
| GET | `/v1/status` | Whether the server takes orders, and if not, why not |
| GET | `/v1/notice` | The operator's status notice |
| GET | `/v1/benchmarks` | Benchmark interest rates and the next financing rollover |
| GET | `/v1/benchmarks/schedule` | The rollover schedule, for the operator's benchmark worker |

### Markets

`GET /v1/markets` returns an array with one object per market. Each object holds the market's
specification and its state.

| Field | Meaning |
| --- | --- |
| `id`, `symbol`, `asset_class` | The market's number, its name such as `BTC-USD`, and its asset class. |
| `quote_currency` | The currency its prices are quoted in, present only when it is not US dollars. |
| `schedule` | The market's trading hours. |
| `margin_tiers` | The margin bands. Each band's rate in basis points, `bps`, applies to the part of a position's value up to `up_to` US dollars. The last band's `up_to` is `null`. |
| `spread_tiers` | The spread bands. Each band's rate in tenths of a basis point, `tenth_bps`, applies to the part of a position's value up to `up_to` US dollars. |
| `price_rules` | How the market is priced: `min_sources` venues must have quoted within `max_age_ms`, and agree to within `max_divergence_bps`. |
| `max_slippage_bps` | The furthest a market order's execution price may be from the price the client was quoted. |
| `financing` | The benchmark rate the market's financing follows, its markup in basis points a year, and its day basis. |
| `house_cap_bps`, `house_cap_floor` | The cap on the house's net exposure in the market, as a share of its net asset value with a floor in US dollars. |
| `min_order_notional` | The smallest value, in US dollars, of an order that opens or increases a position. |
| `contracts` | For a market priced from futures, the contracts it rolls through. |
| `markout_floor_bps` | The least value, in basis points as a decimal string, of the typical short-term price move in whose units the toxicity score measures trades. |
| `weekend_margin_bps`, `weekend_margin` | The margin rate over the market's weekend gap, and when it next applies, as described below. Both are absent for a market without a weekend margin. The production trading settings give no market a weekend margin. |
| `min_level_distance_bps` | How far beyond the closing price a take profit or stop loss must lie. |
| `open` | Whether the market's exchange is open now. |
| `reduce_only` | Whether the market accepts only trades that reduce a position, by its own setting or because every market is reduce-only. |
| `mark` | The market's last reference price, or `null` before its first. |
| `fx_mark` | For a market quoted in another currency, the exchange rate its US dollar values use, and `null` otherwise. |
| `stress_level` | The market's stress level, which widens its spreads in turbulent conditions. |
| `pricing` | Whether the market has a price now, and what each of its venues is doing. |

`weekend_margin` holds four fields. `weekend_margin_bps` is the rate. `from` is when new positions
in any market and withdrawals must meet it, two hours before the gap. `close` is when the gap starts
and an account below it is closed out in the market. `reopen` is when the market reopens and its
normal margin applies again.

`pricing` explains a market without a price. It is read from the quotes that the engine held at its
last processed time, `as_of_ms`.

```json
{
  "priced": false,
  "unpriced": "too_few_sources",
  "as_of_ms": 1792504804100,
  "fresh_sources": 1, "min_sources": 2, "max_age_ms": 1000, "max_divergence_bps": 25,
  "sources": [
    { "source": "coinbase", "state": "counts", "last_quote_ms": 1792504804100, "age_ms": 0 },
    { "source": "kraken", "state": "delayed", "last_quote_ms": 1792504800100, "age_ms": 4000 }
  ]
}
```

`unpriced` is present only while the market has no price. It is `too_few_sources` when fewer than
`min_sources` venues have quoted within `max_age_ms`, `diverging` when the fresh quotes are further
apart than `max_divergence_bps`, or `out_of_range` when a futures market's adjusted price is outside
its valid range. `divergence_bps` gives the spread of the fresh quotes once there are enough of
them. A closed market holds no quotes, so it always reads `too_few_sources`, and a client reads
`open` first.

Each source's `state` is one of the following:

| State | Meaning |
| --- | --- |
| `counts` | The source's quote is fresh and inside the reference price. |
| `diverging` | The quote is fresh, but it is one of the two extremes that disagree too far. |
| `stale` | The newest quote is older than `max_age_ms`, and the connection reports nothing wrong. |
| `silent` | The connection is up but has sent no fresh quote for this market. |
| `delayed` | The connection's stream runs too far behind, so its quotes do not count. |
| `down` | The connection is down. |
| `off` | The server holds the connection back, because a secret it needs has not been delivered. |
| `not_expected` | The market takes no quotes from the source now. |
| `never_quoted` | The source is expected to quote the market, and the engine holds no quote from it. |

`last_quote_ms` and `age_ms` are `null` for a source that has not quoted.

### Price history

`GET /v1/markets/{id}/marks` returns one market's reference price history, oldest first, as open,
high, low and close prices over buckets of one width.

| Parameter | Meaning |
| --- | --- |
| `resolution` | Required. The bucket width: `1s`, `1m`, `1h` or `1d`. |
| `from_ms`, `to_ms` | Only buckets that start within this window, both ends included. |
| `limit` | Entries per page, from 1 to 2000. The default is 500. |

The answer holds `market`, `resolution`, `entries`, `next` and `max_limit`. Each entry gives its
bucket's start, `ts`, its prices as decimal strings, and its `provenance`. `log_derived` marks a
bucket computed from the public log. `seeded_from_provider` marks a bucket that the server fetched
from a venue's public candle history to cover the time before its log began. A window wider than one
page continues by passing `next` as `from_ms`. A market that has no history yet answers an empty
page. While a server that has just taken over is still loading its earlier history, the route
answers 503 with `Retry-After`.

### Operating status

`GET /v1/status` tells a client whether the server is processing orders, and if not, why not.

```json
{
  "as_of_ms": 1792504804200,
  "takes_orders": false,
  "holds": [{ "cause": "trusted_time", "since_ms": 1792504801000 }],
  "trusted_time": { "state": "held", "cause": "no_sample", "last_sample_ms": 1792504740000 },
  "venues": [
    { "venue": "coinbase", "connections": 2, "state": "silent",
      "symbols": [{ "symbol": "BTC-USD", "market": 1, "state": "silent" }] }
  ],
  "unpriced_markets": [
    { "market": 1, "symbol": "BTC-USD", "reason": "too_few_sources",
      "fresh_sources": 1, "min_sources": 2 }
  ]
}
```

`takes_orders` is false while any entry of `holds` stands. Each hold names its `cause` and when the
server first saw it, `since_ms`. The causes are as follows:

| Cause | Meaning |
| --- | --- |
| `trusted_time` | The server has no recent signed time reading, or the host clock has drifted beyond its bound. |
| `log_sync_held` | Writing a batch of commands to storage has taken more than 5 seconds. |
| `log_rollback` | A log head posted on chain is ahead of, or differs from, the server's own log, so the server has stopped processing until it restarts. |

A market that is reduce-only, closed or without a price refuses orders of its own, which
`GET /v1/markets` shows; none of these is a hold. `trusted_time.state` is `starting` before the
first reading, `sampled` while readings are current, or `held` with a `cause` of `no_sample` or
`drift`. Each venue's `state`, and each of its symbols', is one of `quoting`, `stale`, `silent`,
`delayed`, `down`, `off` and `not_expected`. A venue shows the state of its worst symbol, counting a symbol
whose market takes no quotes as better than one that quotes, and `connections` gives the number of
connections that carry the venue. A symbol that two connections carry shows the better state of the
two. `unpriced_markets` lists the open markets that have no price, with the reason and source counts
of their `pricing`.

While the server's time is not trustworthy, it signs nothing, so the answer to this route arrives
unsigned. A client takes an unsigned `/v1/status`, or a signed stream that closes, as that state,
and reads the unsigned body for the cause.

### Status notice

`GET /v1/notice` returns the operator's status notice as
`{"notice": {"text": "…", "set_at_ms": 1792504800000}}`, or `{"notice": null}` when there is none.
The notice is part of the engine's state, set and cleared by signed commands in the public log, so
every server shows the same one. Its text is 1 to 280 characters of plain text, with no markup and
no links. A bare domain name is allowed, so a client must not turn the text into links.

### Benchmark rates

`GET /v1/benchmarks` returns the benchmark interest rates that financing follows:

| Field | Meaning |
| --- | --- |
| `next_rollover` | The time of the next daily financing charge. |
| `rollover` | The time zone, `tz`, and the `hour` and `minute` at which the daily charge falls. |
| `required` | The benchmarks that the listed markets use. |
| `history` | By benchmark and date, the rate the engine has applied, as an annual fraction such as `"0.04"` for 4%. |
| `undated` | By benchmark, the rate that applies before its first dated rate, in basis points a year. |
| `protocol_version` | The version of the engine's rules under which the log runs. |

`GET /v1/benchmarks/schedule` serves the operator's benchmark worker, which signs each day's rates.
It returns `{"revision": <n>, "rollover": {…}}`. With `?after=<revision>`, the server holds the
request for up to 5 seconds until the revision changes. Each request holds one of the places that
stream connections use, so the route answers 503 `too many stream subscribers` when they are all in
use.

## Accounts

| Method | Path | Purpose |
| --- | --- | --- |
| GET | `/v1/accounts/{address}` | An account's balance, margin, positions and working orders |
| GET | `/v1/accounts/{address}/nonce` | A signing key's next nonce |
| GET | `/v1/accounts/{address}/eligibility` | The eligibility class an account has declared |
| GET | `/v1/accounts/{address}/history` | An account's events, newest first |
| GET | `/v1/accounts/{address}/financing/{seq}` | An account's financing charges at the rollovers of one command, with their proof |
| GET | `/v1/accounts/{address}/proof` | A proof of an account's balance and positions |
| GET | `/v1/accounts/{address}/preview` | What a market order of an exact size would do now |

Account data is public, as the log it comes from is public. None of these routes needs a signature.

### The account

`GET /v1/accounts/{address}` answers 404 for an account that has never been credited. For any other
account it returns the following fields. All amounts are in US dollars at the current reference
prices.

| Field | Meaning |
| --- | --- |
| `balance` | The account's cash, which is its deposits less its withdrawals, plus its realised profit and loss, less its fees and financing. |
| `unfinalised_deposits` | Deposits credited before the chain has finalised them, which count as margin but cannot be withdrawn yet. Absent when zero. |
| `equity` | The balance plus the unrealised profit and loss of every position. |
| `margin` | The margin requirement at each market's own bands. |
| `initial_margin` | The equity that a trade adding risk, or a withdrawal, must leave. It charges each band at least the account's volume tier's margin floor, and applies weekend margins from two hours before a market's weekend gap. While the protocol parameters name offset groups, it is `portfolio_margin.initial_margin`. |
| `maintenance_margin` | The equity below which the account is closed out, which is `margin` at the close-out level, or, while the protocol parameters name offset groups, `portfolio_margin.maintenance_margin`. |
| `withdrawable` | The cash a withdrawal may take now. |
| `max_leverage` | For every listed market, the most leverage a position opened now could reach. |
| `positions` | By market, each position's signed quantity `qty`, positive for long and negative for short, and its average entry price `entry`. |
| `orders` | By order id, each working order with its market, side, quantity, `kind`, `reduce_only`, `client_id`, and any take profit and stop loss it will attach. |
| `toxicity` | The account's toxicity `score`; `fills`, the decayed count of trades behind the score; the `tier` that applies, or 0 for none; and `add_on_bps`, the spread that the tier adds. |
| `toxicity_bps` | Kept for logs written under earlier rules, and always 0 under the current rules. |
| `closeouts` | For each position, where its market must move for the account to fall below its close-out level, as described below. |
| `close_fees` | By market, the position's opening time `opened_at`, the profit fee `fee_bps` that closing it now would pay on a gain, and `falls_at`, when that rate next falls, or `null` once it has reached the base rate. |
| `volume` | The account's volume tier and its progress, as described below. |
| `weekend_margins` | By market, for each position in a market with a weekend margin: the market's `weekend_margin` fields and `required`, the margin requirement the account must meet over the gap at the current prices. Absent when there is none. The production trading settings give no market a weekend margin, so the production trading server leaves the field out of every account. |
| `levels` | By market, each position's `take_profit` and `stop_loss`. A level that is not set is left out, and the field is absent while no position has one. |
| `portfolio_margin` | Present only while the protocol parameters name offset groups, as described below. |

Each entry of `closeouts` holds `market`, `reach` and `price`. A `reach` of `at` gives in `price`
the market price at which the account would fall below its close-out level, with its other positions
held at their current prices. `due` means the account is already below the level, and `beyond` means
that no price would close it out. `price` is `null` unless `reach` is `at`. The figure is an
estimate, because a close-out may first close a position in another market. For a position in an
offset group that the account holds on both sides, the requirement follows the price through the
group's credit and need not move one way with it, so the server looks for the first price below the
level at 16 evenly spaced prices across the range before narrowing it down; a dip below the level
narrower than their spacing, which recovers before the next, is not seen.

`volume` holds the following fields:

| Field | Meaning |
| --- | --- |
| `tier` | The volume tier at which the account trades now. |
| `matured` | The volume counted over the last 30 UTC days, in US dollars. |
| `pending` | Volume not yet counted, such as the value of a position opened less than 60 seconds ago, or of a close that may still turn out to be a round trip. |
| `progress` | For each tier from 0, the counted volume that the tier's daily cap admits, `capped`, and the number of days that reach that cap, `days`. |
| `best_price_bounds` | By market, where the account's first and cheapest spread band ends, in US dollars. |
| `max_resting_orders` | The most working orders the tier allows, where the tier table names a limit. |
| `requests_per_second` | The tier's request rate, where the tier table names one. |
| `fee_discount_bps`, `financing_discount_bps`, `emissions_boost_bps` | The tier's discount on the profit fee and on the financing markup, and its boost to the account's MONKEY weight, where the tier table names them. |

`portfolio_margin` is the account's requirement set by the risk of the whole account rather than
position by position, which close-outs read. In each offset group, the exposure on
the group's smaller side, long or short, offsets as much on its larger side, and that matched
exposure takes the group's credit: its share of the margin less its worst loss and the cost of
closing it, at most the share the group's floor share leaves. Exposure that offsets nothing keeps
its margin, so a position alone keeps its own requirement. `portfolio_margin` holds `margin`,
`initial_margin` and `maintenance_margin` on the same bases as the fields above, and `groups`, which
gives for each offset group the account holds a member of: its `members`, the loading-weighted
`net_notional` and `matched_notional`, the matched exposure's `worst_loss`, the `binding_direction`
in which a move hurts the unmatched exposure, and the credit `offset`. New risk and withdrawals meet
its `initial_margin`, which charges each band at least the account's volume tier's margin floor, so a
position alone keeps its tier's leverage while a hedged book can pass it. A group takes no credit there
while a member the account holds cannot trade, its market shut or without a price.

### Previewing an order

`GET /v1/accounts/{address}/preview?market={id}&side={buy|sell}&qty={qty}&reduce_only={bool}` answers
what a market order of exactly that size would do now, were it processed next with nothing moved, from
the same pricing and checks an order runs. `reduce_only` is optional and false by default. Screening and
an order's take profit and stop loss are not part of it. The answer is read off the server's copy of the
account, like the account read, and is limited by the client's request rate like every read. It answers
404 for an account that has never been credited, and 400 for a malformed address or parameter.

```json
{"seq": 4211, "account": "0x…", "market": 1, "side": "sell", "qty": "10", "reduce_only": true,
 "fill": {"qty": "10", "price": "99990.2", "mid": "100000", "spread_bps": 0, "increases": false},
 "equity": "60212.1", "initial_margin": "100000", "maintenance_margin": "50000",
 "refused": {"kind": "insufficient_margin", "equity": "60212.1", "required": "100000"}}
```

| Field | Meaning |
| --- | --- |
| `seq` | The last command processed in the state the answer read, or `null` before the first. |
| `account`, `market`, `side`, `qty`, `reduce_only` | The order previewed. |
| `fill` | The fill: its quantity `qty`, which for a reduce-only order is at most the position; its average price `price`, spread included; the reference price `mid`; the spread `spread_bps`; and `increases`, whether it grows the position. `null` when the order cannot be priced, and `refused` says why. |
| `equity`, `initial_margin`, `maintenance_margin` | The account's equity, initial margin and maintenance margin after the fill, the order's market valued at `mid` and every other at its current price. When `fill` is `null`, the account as it is now. |
| `refused` | Why the order would be rejected, as a rejection reason, or `null` when it would be executed. |

While the protocol parameters name offset groups, whether an order is admitted can depend on its size
either way. An account below its initial margin may be refused a small order that hedges part of a
position and admitted a larger one that hedges more, and a hedge that grows past the position it offsets
adds risk of its own. Closing one leg of a hedge can be refused, as described under closing positions.
The preview answers for one size; the executable quotes answer only for the sizes of their steps.

### Eligibility

`GET /v1/accounts/{address}/eligibility` returns the class that an account has declared, with its
statement, as `{"class": "professional", "statement": "I attest that I am a professional client."}`.
Both fields are `null` for an account that has not declared. The route answers for any address.

### History

`GET /v1/accounts/{address}/history` returns an account's events from the server's history index,
newest first, as `{"events": [...], "next": <cursor or null>, "retained_from_ms": <time or null>}`.
Each event carries its `cursor`, the seq of the command that produced it and the event's index among
that command's events, written `<seq>:<index>`. It also carries `ts`, the time at which the command
was processed, and then the event's own fields.

`retained_from_ms` is where the history begins: the index holds every event from that time on, and
none before it. It is the time of the newest command the index holds less `history.retention`, so it
moves forward as commands arrive, and it is `null` until the index holds a command. A log younger than
the retention holds no event before its first command, so its history begins there.

| Parameter | Meaning |
| --- | --- |
| `before`, `after` | Only events strictly before or after this cursor. Passing a page's `next` as `before` gives the next, older page. |
| `kind` | Only events of this `type`. |
| `limit` | Events per page, from 1 to 200. The default is 50. |

`kind` takes any of the following event types: `deposited`, `deposit_rejected`, `deposit_pending`,
`deposit_held`, `custody_deposit_changed`, `custody_deposit_rejected`, `eligibility_attested`,
`eligibility_rejected`, `order_accepted`, `order_rejected`, `order_cancelled`, `fill`,
`position_levels_set`, `position_level_triggered`, `position_levels_removed`, `close_out`,
`bad_debt`, `write_off_repaid`, `financing`, `corporate_dividend`, `corporate_cash_in_lieu`, `withdrawal_queued`,
`withdrawal_queue_updated`, `withdrawal_approved`, `withdrawal_rejected`, `session_key_registered`,
`session_key_revoked`, `session_key_rejected`, `emission_allocated`, `staked`, `unstaked`,
`staking_rejected`, `distribution_allocated`, `operator_share_allocated`,
`operator_share_withheld`, `award_approved` and `award_paid`.

Each daily rollover logs a single `financing_applied` event that covers every position it charges,
and a rollover that charges nothing logs none. That event names the time the rollover charged for,
the number of positions and accounts it charged, the net amount they paid, and the root of a Merkle
tree over each account's charges. The history still lists
each position's charge as a `financing` event. The server derives those events from the rollover
rather than reading them from the log, and gives each one an index past every event the command
logged, so its cursor sorts after them.

The index keeps events for the period that `history.retention` sets, 30 days in production, counted
back from the newest command, and the log keeps every event: an older event is in the public log and
its archive, where a replay finds it. While the index is still taking the commands that the log held
when the server started, the route answers 503 `history is catching up with the log`.

### Financing charges

`GET /v1/accounts/{address}/financing/{seq}` returns the account's financing charges at each
rollover that the command at `seq` ran. Each charge comes with a proof, so a client can check it
against the public log without seeing any other account's charges. The answer holds `seq` and
`rollovers`, one entry for each rollover:

| Field | Meaning |
| --- | --- |
| `event` | Where the rollover's `financing_applied` event sits in the log, written `<seq>:<index>`. |
| `at` | The time the rollover charged for. |
| `root` | The root that `financing_applied` logs over every account's charges. |
| `proof` | The account's charges and their path to `root`, or `null` when the rollover charged the account nothing. |

A proof holds the `account`, its `charges` in market order, each a `market` and an `amount` that is
positive when the account pays, the leaf's `index` among the charged accounts in ascending order,
their `count`, and the `siblings` from the leaf up. `FinancingProof::verify` in `engine-core` checks
a proof against a root, and
[`crates/core/src/financing/commitment.rs`](../crates/core/src/financing/commitment.rs) defines the
leaf and node hashes.

The route answers only once the command is durable in the log. The server keeps the charges of the
newest eight commands that ran a rollover, which is a week of rollovers, and stores older ones for
the period that `history.retention` sets. It answers 404 for a command that is not yet durable, that
charged nothing, or whose charges it no longer holds. A replay of the public log derives those
charges again.

### Proof of an account

`GET /v1/accounts/{address}/proof` returns a proof of the account's balance and positions against
the state root of the latest checkpoint. The answer names that checkpoint's `seq` and `root`, and
holds the account's leaf with its sibling hashes, each position with its proof, and the leaf and
proof of each market in which the account holds a position. For each position, it also holds either
the proof of its take profit and stop loss levels, in `levels`, or a proof that it holds none, in
`no_levels`. A proof that leaves out a position's levels therefore fails to verify.

To check a proof, a client first checks `root` against the signed checkpoint at `seq` in the log,
then checks the proof with `AccountProof::verify` in `engine-core`. An account credited since the
latest checkpoint is not in its root yet, and gets 404 until the next checkpoint.

## Orders and positions

| Method | Path | Purpose |
| --- | --- | --- |
| POST | `/v1/orders` | Places an order |
| POST | `/v1/orders/cancel` | Cancels one working order |
| POST | `/v1/orders/cancel-all` | Cancels every working order of an account |
| POST | `/v1/positions/close-all` | Closes whole positions in the markets named |
| POST | `/v1/positions/levels` | Sets a position's take profit and stop loss |

Every route in this section takes a signed body and answers with the events of the command it
became, in order. Besides the events about the request, the answer can hold events that the engine
produced at the same moment, such as a market opening or a daily financing charge, including events
about other accounts. A client finds its own outcome by `account` and `client_id`.

### Placing an order

`POST /v1/orders` takes the following body:

| Field | Type | Meaning |
| --- | --- | --- |
| `account` | Address | The account placing the order. |
| `client_id` | Unsigned 64-bit integer | A number of the client's choosing, returned in the events about the order. |
| `market` | Market id | The market. |
| `side` | `buy` or `sell` | The order's direction. |
| `qty` | Decimal | The quantity, in units of the market's asset, greater than zero. |
| `kind` | Object | The order type, as described below. |
| `reduce_only` | Boolean, optional | When true, the order may only reduce an existing position. The default is false. |
| `take_profit`, `stop_loss` | Decimal, optional | Levels that the position the order leaves on its side takes, in place of any it has. |
| `auth` | Object | The signature, nonce and expiry. |

`kind` is one of the following objects:

| `kind` | Fields | Behaviour |
| --- | --- | --- |
| `{"kind": "market", "quoted": "…"}` | `quoted`, the price the client was shown, and `max_slippage_bps`, optional | Executes now at the current price. The engine rejects the order with `slippage` when the execution price is worse than `quoted` by more than the slippage limit. The client's `max_slippage_bps` can tighten the market's own limit but not widen it. |
| `{"kind": "limit", "price": "…"}` | `price` | Works until the execution price, including the spread, reaches `price` or better. |
| `{"kind": "stop", "price": "…"}` | `price` | Works until the reference price reaches `price`, then executes at the prevailing price, even if that has gapped. |

An order that the engine accepts produces `order_accepted`, with its `client_id` and the engine's
`order_id`. A market order's `fill` follows in the same answer. A limit or stop order is checked
against the current price at once, so its `fill` can also follow in the same answer. An order that
the engine refuses produces `order_rejected` with its `client_id` and `reason`.

A `fill` event holds `account`, `order_id`, `market`, `side`, `qty`, the execution `price`, the
reference price `index` at the time, the spread paid `spread_bps`, the profit or loss it realised
`realised_pnl` in US dollars, the `profit_fee`, the signed `position` it left, and `liquidation`,
which is true for a close-out. A fill that closes part of a position also gives `hold_ms`, how long
that part was held, and `profit_fee_bps`, the rate a gain on it paid. A fill in a market quoted in
another currency gives `fx_index`, the exchange rate it was converted at.

The most common rejection reasons are these:

| Reason | Meaning |
| --- | --- |
| `access` | Screening refused the account, with the screening `reason`. |
| `unknown_market` | No market has this id. |
| `market_closed` | The market's exchange is closed. |
| `no_price`, `no_fx_price` | The market, or the currency market it converts through, has no price. |
| `slippage` | The execution price was worse than `quoted` by more than the slippage limit. |
| `insufficient_margin` | The trade would leave equity below the initial margin, which a trade that only reduces a position meets only when it raises that margin, as described under closing positions. |
| `weekend_margin` | The trade would leave equity below the weekend margin that applies now. |
| `reduce_only` | The market or the order allows only trades that reduce a position. |
| `house_capacity` | The trade would take the house's exposure past its cap. |
| `close_spread_ceiling` | A reversal's spread would exceed the market's closing spread cap. Closing the position alone stays possible. |
| `tier_order_limit`, `too_many_orders` | The account already has as many working orders as its volume tier, or the trading settings, allow. |
| `level_too_close` | A take profit or stop loss lies closer than the market's minimum distance, as described below. |
| `invalid_order` | The order cannot be accepted as written, with the cause in `reason`, such as `qty out of range` or `below minimum order notional`. |
| `invalid_nonce`, `expired`, `expiry_too_far` | The nonce or expiry failed when the engine processed the request. |

### Cancelling orders

`POST /v1/orders/cancel` takes `account`, the engine's `order_id` and `auth`, and produces
`order_cancelled` with the reason `client`, or `order_rejected` with `client_id` 0 and the reason
`unknown_order`.

`POST /v1/orders/cancel-all` takes `account` and `auth`, and cancels every working order of the
account on one signature and one nonce. It produces one `order_cancelled` for each order, and an
empty array when the account has none.

### Closing positions

`POST /v1/positions/close-all` closes the account's whole position in each market it names, on one
signature, one nonce and one screening. Its body holds `account`, `closes`, an optional
`max_slippage_bps` and `auth`. Each entry of `closes` holds a `market` and `quoted`, the price that
the client was shown for closing.

Each entry runs as a reduce-only market order for the whole position, in the order listed, with the
entry's index in `closes` as its `client_id`. An entry that cannot be executed produces
`order_rejected` with its index, and the other entries still run. A position larger than
1,000,000,000 units closes in parts, and the rest stays open.

A request that names a market twice, or names none, is rejected as a whole. The answer then holds a
single `order_rejected` with `client_id` 0 and the reason `invalid_order`, and no entry runs. A
request that names more markets than are listed is refused with 400
`more closes than listed markets` before its signature is checked.

While the protocol parameters name offset groups, closing one leg of a hedge gives up the group's
credit and can raise the account's initial margin. An ordinary order whose trade only reduces a
position is then checked as a trade adding risk is, and rejected with `insufficient_margin` when it
would leave equity below the margin it raised. Whether a trade raises the margin is judged with every
group's credit kept, so while a member of a group cannot trade, an order closing another member is
checked although `initial_margin`, which credits no such group, falls. A trade that leaves the margin
lower on that basis is never checked.
A close-all entry, a triggered stop or limit order whose trade only reduces a position, including one
triggered as it is placed, a take profit or stop loss, and a close-out are never rejected for that
margin, so a close-all of one market closes a leg that an order could not. A triggered order that turns
a position around is checked on the risk it adds, as any order is. After any trade, an account left below its close-out level is
closed out in the same command.

### Take profit and stop loss

A position can carry a take profit and a stop loss. When the price at which the whole position would
close, including the spread, reaches either level, the engine closes the whole position.

An order sets levels with `take_profit` and `stop_loss`. The position that the order leaves on its
side takes each level the order names, and keeps any level the order leaves out. A limit or stop
order holds its levels until it is executed. A reduce-only order cannot set levels.

`POST /v1/positions/levels` sets the levels of an existing position directly. Its body holds
`account`, `market`, `take_profit`, `stop_loss` and `auth`, and both levels are optional. The
position takes exactly the levels the request names, so a level that the request leaves out is
removed. The request produces `position_levels_set`, or `order_rejected` with `client_id` 0 and one
of these reasons:

| Reason | Meaning |
| --- | --- |
| `level_too_close` | A new or moved level lies within the market's minimum distance of the current closing price. The rejection gives the `level`, the distance `min_distance_bps` and the closing price `reference`. |
| `tier_order_limit`, `too_many_orders` | The position's first level needs a working-order place, and the account has none free. A position's two levels together take one place. |
| `invalid_order` | The account has no position in the market, a level is out of range, or the market has no price to measure from. |

When a level is reached, the engine produces `position_level_triggered`, naming the level and the
order that closed the position, followed by that order's `fill`. Closing or reversing the whole
position, or a close-out, removes both levels with `position_levels_removed`.

## Funding an account

| Method | Path | Purpose |
| --- | --- | --- |
| POST | `/v1/eligibility` | Records an account's eligibility declaration |
| POST | `/v1/session-keys` | Registers a session key |
| DELETE | `/v1/session-keys` | Revokes a session key |
| GET | `/v1/access/pending/{chain_ref}` | A deposit waiting to be screened |
| GET | `/v1/access/holds/{chain_ref}` | A deposit held by screening |
| POST | `/v1/dev/deposit` | Credits an account without a chain deposit, on a server with simulated custody |

Deposits are made in USDC to the vault contract, not through the API. A deposit credits the account
that its vault event names, once the server has observed it on chain and screened it. A new wallet
sets itself up in this order:

1. It signs an eligibility declaration with `POST /v1/eligibility`. The declaration needs no
   deposit.
2. Once it has declared, it may register session keys with `POST /v1/session-keys`.
3. It deposits through the vault. The deposit is credited at its first screening after the
   declaration. A deposit made before the declaration waits, and is credited once the declaration is
   recorded.

Until a deposit credits the account, every other signed request for it, whether signed by the
account or by a session key, is refused with 403 `account has never been credited`.

### Eligibility declarations

`POST /v1/eligibility` takes `account`, `class` and `auth`. The class is `professional` or
`eligible_client`, and the signed statement is the corresponding sentence:

| Class | Statement |
| --- | --- |
| `professional` | `I attest that I am a professional client.` |
| `eligible_client` | `I attest that I am an eligible client.` |

The server screens a declaration as it screens an order. The engine records it only when the
screening evidence is verified, allowed and clear, and produces `eligibility_attested`. Otherwise it
produces `eligibility_rejected` with its reason, which uses the nonce, and the account may declare
again. A declaration from an account that has already declared is refused with 403
`account has already declared`. A session key's signature on a declaration is refused with 401.

A never-credited account's declaration and session-key requests must expire within five minutes, and
are counted against the budgets described under
[Accounts that have never been credited](#accounts-that-have-never-been-credited). A session-key
request from an account that has not declared is refused with 403
`a never-credited account must declare first`.

### Deposits waiting for screening

A deposit that screening has not cleared yet stays pending and is not credited. Its
`deposit_pending` event gives the reason, such as `eligibility_required` when the account has not
declared. Other reasons are that the source classified the account or the payer as unknown, or that
the source could not be reached. The server retries pending deposits by itself.
`GET /v1/access/pending/{chain_ref}` returns one pending deposit's funding input: `account`,
`payer`, `amount` and `chain_ref`, the custody reference of the deposit.

A deposit whose account or payer screening confirms as sanctioned, or whose vault event names no
payer, is held and never credited. `GET /v1/access/holds/{chain_ref}` returns one hold's funding
input and its reason. Both routes answer 404 for a reference they do not hold.

### Simulated deposits

On a server with simulated custody, `POST /v1/dev/deposit` takes `{"account": "0x…", "amount": "…"}`
and credits the account under a new reference, after screening it as a vault deposit. The body is
not signed. On a server with no screening source, as `config/dev.toml` configures, the deposit stays
pending with the reason `source_not_configured`. On a server with real custody, the route answers
404.

## Withdrawals

| Method | Path | Purpose |
| --- | --- | --- |
| POST | `/v1/withdrawals` | Requests a withdrawal |
| GET | `/v1/withdrawals` | The public withdrawal queue |
| GET | `/v1/accounts/{address}/withdrawals` | An account's requests in the withdrawal queue |
| GET | `/v1/accounts/{address}/payouts` | An account's unpaid withdrawals and their authorisations |
| POST | `/v1/accounts/{address}/withdrawals/{nonce}/reissue` | Asks for an expired withdrawal authorisation to be signed again |

A withdrawal goes through three stages:

1. The request takes the amount out of the account's cash and joins a public first-in, first-out
   queue.
2. The queue approves requests in order, as far as the USDC that the vault holds allows. When the
   USDC runs short, the request at the head is approved in parts of at least 10 USDC, and each
   approved part has its own payout nonce.
3. The enclave signs an authorisation for each approval. On a server with real custody, it does so
   once the vault has accepted a checkpoint that covers the approval and the chain has finalised it.

The account then submits the authorisation to the vault's `withdraw` function to be paid.

### Requesting a withdrawal

`POST /v1/withdrawals` takes `account`, `amount` and `auth`, and must be signed by the account's own
key. The amount is rounded down to whole USDC base units, and must be at least 10 USDC and no more
than `withdrawable`. The answer holds `withdrawal_queued`, with the request's `requested_seq`. It
also holds any approval that the queue made at once, as `withdrawal_approved` with its payout
`nonce`, followed by `withdrawal_queue_updated` with the amount still `remaining`. A request that
the engine refuses produces `withdrawal_rejected` with one of these reasons:

| Reason | Meaning |
| --- | --- |
| `insufficient_funds` | The amount exceeds the cash a withdrawal may take, which the rejection gives as `available`. |
| `below_minimum_withdrawal` | The amount is less than 10 USDC. |
| `invalid_amount` | The amount is zero, negative or too large. |
| `payout_unavailable` | The server cannot pay withdrawals, because its custody has no payout path. |
| `session_key_scope` | A session key signed the request. |
| `access` | Screening refused the account. |

### The queue

`GET /v1/withdrawals` returns the queue in request order, and
`GET /v1/accounts/{address}/withdrawals` returns one account's requests in it. Both take `after`, a
request's `requested_seq` from which to continue, and `limit`, from 1 to 200, with a default of 50.
Both return `seq`, the last command the answer reflects, `queue` and `next`.

Each queue entry holds `account`, the requested `amount`, the `remaining` amount not yet approved,
`requested_seq`, `position`, its place in the whole queue counting from 1, and, when it is not zero,
`ticket`, the request's enqueue order counting the requests already completed. Each page scans at
most `limit` requests of the whole queue, so an account's page can be empty and still have a `next`,
which the client must follow.

### Payouts

`GET /v1/accounts/{address}/payouts` returns the account's unpaid approvals, oldest first, with
`seq`, `loaded`, `payouts` and `next`. It takes `after`, a payout nonce, and `limit`, from 1 to 200,
with a default of 50.

Each payout holds `nonce`, `amount` and `withdrawal`, the latest signed authorisation, or `null`
until one is signed. A withdrawal authorisation holds `account`, `amount`, `nonce`, `chain_id`,
`vault`, `signature`, `approved_seq` and `authorization`. The `authorization` object holds the
`signer`, its `signer_epoch`, `issued_at` and `expires_at` in Unix seconds, and the
`checkpoint_seq`, `checkpoint_root` and `siblings` that prove the approval against an accepted
checkpoint. An authorisation is valid for 24 hours.

While the server runs daily operator turns with more than one member, an approval whose next
signature waits for the log check also carries `check`. With `waits` set to `boundary`, it names the
`turn` whose boundary's check will cover the approval, and that boundary's `boundary_ms`. With
`waits` set to `installation`, rotation is off and no turn after the last drawn `turn` is assigned,
so no boundary checks the approval: it waits for the timelock's installation of an operator, which
ends the turns.

An approval stays listed until the server observes the vault's payment, so an approval that the
client has already submitted can still appear. The vault's `usedNonces` shows whether it has been
paid. A nonce missing from the list shows that the approval was paid, but only when `loaded` is true
and the client has followed every `next` from the first page. `loaded` is false while a restarting
server is still loading approvals, and on a server with simulated custody. A restarting server loads
first the approvals whose authorisation was still usable or never signed, and signs new approvals
once those are loaded; one whose authorisation had expired is listed once its turn to load comes.

### Reissuing an expired authorisation

The server never signs an expired authorisation again by itself. The account asks for a new one with
`POST /v1/accounts/{address}/withdrawals/{nonce}/reissue`, where `nonce` is the payout nonce. The
body is not an `auth` object:

```json
{ "replaces_expires_at": 1792540800, "signature": "0x…" }
```

`replaces_expires_at` is the expiry of the latest authorisation, in Unix seconds, or 0 for an
approval that was never signed. The signature is the account's EIP-712 signature, under the usual
domain, of `ReissueRequest(address account,uint64 payoutNonce,uint64 replacesExpiresAt)`. Because it
names the authorisation that it replaces, one signature can be used only once.

The server first checks that it holds the approval unpaid, that the request names the latest
authorisation, and that this authorisation can no longer be used, and refuses a request that fails
any of these before screening it. It then screens the request as it screens a withdrawal and queues
the approval for a new 24-hour authorisation with the same nonce, in approval order. It answers
`{"status": "queued", "resend_after_restart": true}`, and the new authorisation arrives on the
stream and in the payouts route. The queue lives in memory, so after a server restart the client
sends the same signed body again.

| Status | Meaning |
| --- | --- |
| 400 | The latest authorisation is still usable, or the request names an authorisation other than the latest. |
| 401 | The signature is not the account's. |
| 403 | Screening refused the account, with the reason. The approval stays unpaid, and the account may ask again later. |
| 404 | The server holds no unpaid approval under this nonce: it is unknown, already paid, or not yet loaded after a restart. |
| 429 | The account is over its request rate. |
| 503 | The server cannot sign withdrawals now. |

## MONKEY, distributions and the exit

| Method | Path | Purpose |
| --- | --- | --- |
| GET | `/v1/accounts/{address}/emissions` | A proof of an account's MONKEY entitlement |
| GET | `/v1/accounts/{address}/distributions` | A proof of an account's USDC distributions |
| GET | `/v1/accounts/{address}/exit` | Every proof an account needs for the vault's exit |
| GET | `/v1/monkey` | MONKEY emission, staking and distribution totals |
| GET | `/v1/monkey/activity` | The newest loss allocations, stakes and unstakes |
| GET | `/v1/monkey/distributions` | Closed distribution epochs |
| GET | `/v1/awards` | Bug bounty awards, the queue they are paid from and the cap on each payment |

### Claim proofs

`GET /v1/accounts/{address}/emissions` returns the proof of an account's cumulative MONKEY
entitlement, which `MonkeyMinter`'s `claimEmissions` takes. `GET /v1/accounts/{address}/distributions`
returns the proof of an account's cumulative USDC distributions, which the vault's `claimDistribution` takes.
Both answers have the same form.

| Field | Meaning |
| --- | --- |
| `seq`, `root` | The checkpoint the proof is against. |
| `account` | The account. |
| `entitled` | The cumulative amount at that checkpoint, in MONKEY base units or USDC base units. |
| `siblings` | The sibling hashes from the root down to the account's leaf. |
| `accepted_seq` | The latest checkpoint the server has seen the vault record under the signer in force, or `null`. |
| `newest_seq`, `newest_entitled` | The newest checkpoint, and the amount it holds. |
| `claimable` | Whether the vault can accept the proof now. |

The vault pays a distribution, and `MonkeyMinter` mints an emission against the vault's checkpoint,
for the amount that a proof holds less what each has already paid or minted the account; the minter
counts what it minted across every vault it has followed. When `claimable` is
true, the proof is at `accepted_seq`, and its `entitled` can be below `newest_entitled`; a later
claim pays the rest. When `claimable` is false, the proof is at the newest checkpoint, which the
vault has not recorded yet, so the client asks again later rather than keeping the proof. The server
keeps proofs for the 128 accounts that asked most recently, from their first request. An account
gets 404 until the first checkpoint after it was allocated a share.

### The exit

If the enclave stops posting checkpoints for long enough, anyone can start the vault's exit, which
pays each account what the last posted checkpoint shows it is owed.
`GET /v1/accounts/{address}/exit` returns every proof the account needs for the exit, against the
state root of the latest checkpoint:

| Field | Meaning |
| --- | --- |
| `seq`, `root` | The checkpoint. |
| `terms` | The vault deposit cursor and the pending and held amounts that the exit reserves, with their proof. |
| `account` | The account's leaf, positions and markets, as `/proof` serves them, or `null`. |
| `approvals` | Every withdrawal approval the account holds. The vault's `usedNonces` shows which are unpaid. |
| `queued` | The account's requests in the withdrawal queue. |
| `distribution` | The account's distribution proof, or `null`. |
| `pending_deposits` | Deposits for the account still waiting for screening, which the exit refunds to their payers. |
| `unfinalised_credits` | The account's credited deposits that were not yet final, in increasing deposit id. |

A wallet checks the proofs before keeping them. It first checks that the vault's `checkpointRoots`
holds the proof's `root` at its `seq`. While it holds no root there, the checkpoint is still on its way
to the vault, so the wallet keeps its last checked proof and asks again later. Then it checks the
proofs with the vault's `ExitVerifier`:

- `previewClaim` values what the account's equity and its matched credits register, without
  registering anything, before the exit and during it.
- Each unpaid approval and queued remainder that the exit registers adds to that amount.
- The other parts prove through `provesTerms`, `provesApproval`, `provesQueued`,
  `provesDistribution` and `provesReceipt`.

The proofs serve the exit once the vault's latest checkpoint is the proof's `seq`. The route answers
for any address, with `account` set to `null` for an account that the checkpoint does not hold, and
answers 404 before the first checkpoint. `engine exit-proof` builds the same proofs from the
published log after the server has gone.

### MONKEY totals and activity

`GET /v1/monkey` returns `emissions`, the emission schedule and the totals allocated; `staking`, the
MONKEY staked, the number of stakers and the unstaking cooldown; and `distribution`, the
distribution schedule, the house's net asset value, its liquidity floor, `excess_usd`, the amount
the engine would distribute at the last account scan, and the current `epoch`. Each section is
`null` until the operator initialises it. The answer also gives `seq`, `time_ms`, and where its data
begins, as described under [Where dashboard data begins](#where-dashboard-data-begins).

`GET /v1/monkey/activity` returns the newest loss allocations, stakes and unstakes, newest first. It
takes `limit`, from 1 to 200, with a default of 50, and `before`, an entry id from a previous page's
`next`. Ids and `next` are decimal strings. Once older entries have been dropped, `truncated` is
true and `retained_from_ms` names the time of the oldest entry held.

`GET /v1/monkey/distributions` returns the closed distribution epochs, newest first. It takes
`limit`, from 1 to 200, with a default of 50, and `before`, an epoch number. Each epoch gives
`epoch`, the `close_seq` and `close_ms` of the command that closed it, `ended_ms`, the house's
`nav`, the `floor`, `excess_usdc_units`, `allocated_usdc_units`, `awarded_usdc_units`, the part of
the allocation paid to bug bounty awards, `stakers_usdc_units` and the number of `stakers` paid,
`operator_shares`, each operator paid a share with its `amount`, and `skipped`,
the range of earlier epochs that the close distributed nothing for. An epoch closes only once the
chain's finalised staking records have passed its end, so `close_ms` can come well after `ended_ms`.
`retained_from_epoch` names the oldest epoch held, and `truncated` is true once older epochs have
been dropped.

### Bug bounty awards

A bug bounty award is approved only by a passed vote of the rotation set, at the amount the scale
fixes for the finding's severity. An advisory is awarded once, and a second award naming an awarded
advisory is refused when its vote applies. Each epoch close that distributes pays the queue of awards
first, oldest award first, up to the cap, before the operator's share and the stakers' shares. Each
payment is added to the recipient's cumulative USDC distribution, which
`GET /v1/accounts/{address}/distributions` proves and the vault's `claimDistribution` pays.

`GET /v1/awards` returns the awards as the server's durable state holds them:

| Field | Meaning |
| --- | --- |
| `cap` | The most one close pays awards: `excess_bps` of its distributable excess, and at most `max` USDC base units. At launch these are 500 and `"50000000000"`. |
| `queue` | The awards not yet paid in full, oldest first. Each close pays from the front. |
| `paid` | The awards paid in full, oldest first. |
| `paid_total` | What the closes have paid awards in all, in USDC base units. |

Each award gives its `id`, in the order the votes applied them, the `proposal` that approved it, the
`recipient`, the `severity` (`critical`, `high`, `medium` or `low`), the `advisory` it was approved
for, its `amount`, what the closes have `paid` of it and what is still `owed`, all in USDC base
units, and `approved_at`, the sequenced time in milliseconds at which the vote applied it. The
`award_approved` and `award_paid` events in the recipient's history record each approval and each
payment, with the epoch that paid it.

## The house and the operator

| Method | Path | Purpose |
| --- | --- | --- |
| GET | `/v1/house` | The house's cash, net asset value, exposure and client positions |
| GET | `/v1/house/positions` | One market's client positions by close-out price and by profit |
| GET | `/v1/house/state` | The trading settings in force, pending notices and the latest checkpoint |
| GET | `/v1/stats/series` | Time series behind the dashboards |
| GET | `/v1/operator` | The operator key in effect, elections and daily turns |
| GET | `/v1/operators` | The operator registry's candidates |

The house is the engine's own account, which takes the other side of every trade.

### The house

`GET /v1/house` returns the house's `cash`, `nav` and `gross_exposure` after the last command. It
also returns figures from the server's periodic scans of every account: `open_interest`, the value
of every client position, long and short; `open_positions`; `unrealised`, the house's result at the
current prices; `volume` over the last 24 hours, from `volume_since_ms`; and `markets`, each
market's `long`, `short` and `net` client value with position counts, `unrealised` and `volume`.
These come from the latest scan, at most `api.stats_light_interval` old, which `seq` and
`snapshot_ms` date. `open_margin`, the sum of every account's close-out requirement, its portfolio
requirement while the parameters name offset groups, comes from the latest full scan, at most
`api.stats_interval` old, which `margin_seq` and `margin_snapshot_ms` date. The scan figures are `null`
until the first scan.

`GET /v1/house/positions?market=<id>` returns one market's client positions as aggregates that name
no account, as of the latest full scan, which `seq` and `snapshot_ms` date. `market` is required.
`closeout` groups the positions, in US dollars and counts for each side, by how far in basis points
the market must move from its current price to close each account out, estimated as each
account's `closeouts` are, with the price at each
bucket's edges. `due_now` and `beyond_range` count the positions already due and those that no move
within the range reaches. `profit` groups the positions by their profit from entry. The bucket
widths and ranges are in `closeout_bucket_bps`, `closeout_range_bps`, `profit_bucket_bps` and
`profit_range_bps`. The route answers 503 with `Retry-After` until the first full scan.

### Trading settings

`GET /v1/house/state` returns the following fields:

| Field | Meaning |
| --- | --- |
| `seq` | The last command the answer reflects, or `null` on an empty log. |
| `params` | The trading settings in force, with `seq`, the command that set them, or `null` before the first. They include `portfolio_grid`, the offset groups that close-outs read, while they name any. |
| `params_notice` | A pending change to the trading settings, with the time it takes effect, `effective_at`, or `null`. A change to the offset groups' shocks or floor shares appears as its `portfolio_grid`. |
| `listing_notices` | By market, a pending change to the market's `spread_tiers`, `margin_tiers`, `markup_bps`, `weekend_margin_bps`, `price_rules`, `min_level_distance_bps` or futures `contracts`, with `effective_at`. |
| `score_rules` | The toxicity score's fixed constants. |
| `house_loss` | The house's loss to its clients over the trailing day, `loss`, from the start of its window, `since`, and the stress `level` that the loss sets in every market. |
| `governance` | Present while votes of the operator rotation govern the settings. It names the vote contract, each passed change waiting with its `applies_at`, and each restriction in force with its `expires_at`. |
| `pending_ceiling` | A pending change to a market's closing spread cap, with its time. |
| `operator_faults` | The fault proof contract, its penalty, and when each penalised member's penalty started. |
| `checkpoint` | The latest checkpoint, in the form the stream carries it, or `null`. |

`score_rules`, `governance`, `pending_ceiling`, `house_loss` and `operator_faults` are left out when
they do not apply. The other fields are always present, and are `null` or empty when nothing
applies. A noticed change appears in `params_notice` or `listing_notices` until it takes effect, at
least seven days after the notice, or another notice replaces it. It can take effect only within
seven days of its `effective_at`; one left unused after that stays listed, and its change needs a
new notice.

### Series

`GET /v1/stats/series` returns one metric's buckets, oldest first.

| Parameter | Meaning |
| --- | --- |
| `metric` | Required. One of `nav`, `cash`, `withdrawal_queue`, `total_staked`, `total_allocated`, `cumulative_distributed`, `long`, `short`, `volume`, `monkey_per_loss` and `distributed_per_epoch`. |
| `market` | The market, which `long` and `short` require and `volume` accepts. Without it, `volume` covers every market. Every other metric takes no market, and a request that names one gets 400. |
| `resolution` | Required. `1m`, `1h` or `1d`. |
| `from_ms`, `to_ms` | Only buckets within this window. |
| `limit` | Entries per page, from 1 to 2000. The default is 500. |

The answer names the `metric`, its `market` and `resolution`, and its `kind` and `unit`. It gives
`entries`, and `next`, the `from_ms` of the following page or `null` when the window is exhausted.
It also gives `retained_from_ms`, the start of the oldest bucket still held once retention has
pruned older ones, `max_limit`, which is 2000, and where the data begins. `[stats]` in the
configuration sets how long each resolution is kept.

### Where dashboard data begins

The figures behind `/v1/house`, `/v1/house/positions`, `/v1/stats/series` and the `/v1/monkey`
routes are derived from the log and held in memory. A server rebuilds them when it starts. A server
that starts from a recovery snapshot rather than from the whole log holds only what came after that
snapshot. Each answer therefore states where its data begins: `begins_ms`, `begins_seq`, and
`from_genesis`, which is true when the data covers the whole log.

### The operator

The operator runs the trading server, lists markets and publishes benchmark rates, within the limits
that the engine enforces.

`GET /v1/operator` returns the following fields:

| Field | Meaning |
| --- | --- |
| `operator` | The key whose signed administrative commands apply now. |
| `pending_rotation` | A key that takes over at its `effective_at`, or `null`. |
| `rotation_enabled` | Whether the operator registry's elections and daily turns choose the operator. |
| `last_election` | The registry's last election, or `null`. |
| `waiting_elections` | The elections that take effect, each at its own time, before the last one, oldest first; `pending_rotation` is the first one's. Left out while none waits. |
| `rotation_set`, `next_rotation_set` | The rotation set in effect and the next, with its members and `effective_at`. |
| `turn`, `next_turn` | Today's and tomorrow's daily turns, each with `taken`, whether its member has taken it. |
| `turns` | The turn history, newest first, as `records` with `next`, and `truncated` and `retained_from_seq` once older records have been dropped. |
| `api_hostname` | The API hostname that the registry records for the operator in effect, with the finalised block it was read at. Clients reconnect there after a handover. Absent when it cannot be read. |
| `enclave` | This server's enclave address, which signs every answer. |
| `signer` | The vault's signer epoch and the key it names, where the server reads them. |

The turn history takes `before`, a seq from a previous page's `next`, and `limit`, from 1 to 100,
with a default of 20.

`GET /v1/operators` lists the operator registry's approved candidates, at most 100, in registration
order. Each candidate gives its address, `admin_key`, `enclave`, `self_bond`, `release`,
`registration`, `api_hostname`, `handover_endpoint`, `votes`, `eligible` and `role`, which is
`candidate`, `elected` or `operator`. The answer also gives the registry's settings and totals, any
open proposals of the parameter vote, and the finalised block at which the server read them. The
server reads the registry every `api.operators_interval`, 60 seconds by default. The route answers
404 on a server that does not read a registry, and 503 before the first read or once the last read
is too old.

## Attestation and the log archive

| Method | Path | Purpose |
| --- | --- | --- |
| GET | `/v1/health` | Answers `ok` while the server runs |
| GET | `/v1/attestation` | The enclave's attestation |
| GET | `/v1/attestation/collateral` | The processor vendor's certificates and revocation list for the attestation |
| GET | `/v1/archive/{file}` | The log archive's manifest, or one completed log segment |
| GET | `/v1/archive/snapshots/{file}` | The daily snapshots' manifest, or one daily snapshot |

None of these answers is signed, and `/v1/health` answers the text `ok`.

### The attestation

`GET /v1/attestation` returns the attestation the enclave produced when it started:

| Field | Meaning |
| --- | --- |
| `provider` | The kind of enclave, or `mock` for a simulated one. |
| `enclave_address` | The address of the enclave's signing key, which signs every answer, checkpoint and withdrawal authorisation. |
| `config_sha256` | The SHA-256 hash of the configuration file the server loaded. |
| `tls_public_key_sha256` | The SHA-256 hash of the TLS certificate's DER-encoded SubjectPublicKeyInfo. |
| `bundle` | The bytes the processor's report binds, from which the three fields above are read. |
| `report_data` | The 64 bytes of report data in the processor's report. |
| `quote` | The processor's signed report. |

The bundle is 105 bytes: the ASCII text `engine.attestation.v1` in 21 bytes, the enclave address in
20, the configuration hash in 32, and the TLS key hash in 32. Report data bytes 0 to 32 are the
SHA-256 hash of the bundle, and bytes 32 to 64 are zero. A verifier checks the quote, recomputes the
bundle's hash, compares it with the report data, and then reads the fields from the bundle.
`engine verify-attestation` runs these checks. A simulated enclave's quote never verifies.

### Collateral

`GET /v1/attestation/collateral` returns the processor vendor's certificate chain and revocation
list for the attestation's quote, for a verifier that cannot reach the vendor itself: `vcek`,
`intermediate`, `root` and `crl` as hexadecimal DER, and `fetched_at`, when the server fetched them
by the host's clock. The verifier still checks the chain against a vendor root that it pins itself,
so the server cannot forge it. The route answers 404 for a quote that has no collateral, such as a
simulated one, and 503 with `Retry-After` until the first fetch succeeds.
`engine verify-attestation --collateral` takes this document in place of the vendor's service.

### The log archive

The log is stored in segment files. `GET /v1/archive/manifest.jsonl` returns the manifest, with one
JSON line for each completed segment:

```json
{"file": "00000000000000000000.jsonl", "first_seq": 0, "last_seq": 999, "bytes": 568962, "sha256": "f02d…"}
```

`GET /v1/archive/{file}` returns one completed segment that the manifest lists, as JSON lines. A
file that the manifest does not list answers 404. The segment still being written is not in the
archive, and the stream carries it instead. `engine replay` verifies a downloaded archive, and the
server checks each segment against the manifest before serving it.

### Daily snapshots

A server configured with `[daily_snapshots]` keeps the engine's state at the first checkpoint it
writes on each UTC day, by the sequenced time of the instruction the checkpoint covers. A server that
sequences only on some days, as a member of a rotation does, keeps those days' only; an auditor can
combine the snapshots of several servers, since a replay checks each against the log.
`GET /v1/archive/snapshots/manifest.jsonl` returns their manifest, with one JSON line for each
snapshot:

```json
{"file": "00000000000008190311-0000000001.json", "seq": 8190311, "index": 1, "ts": 1795219200412, "root": "0x5e1c…", "bytes": 440817152, "sha256": "9b07…"}
```

`seq` and `index` are the offset of the checkpoint's entry in the log, which names the file, `ts` is
its sequenced time and `root` its state root. `GET /v1/archive/snapshots/{file}` returns one listed
snapshot as a JSON object: `schema` (2), `offset`, `checkpoint`, the checkpoint entry as the log
holds it, `engine`, the engine's state, `state`, the SHA-256 of `engine`'s bytes as the file holds
them, and `signature`, the enclave key's 65-byte signature over the keccak-256 of
`engine.daily-snapshot.v1`, the chain id as 8 bytes and the vault's 20 bytes, `seq` as 8 bytes and
`index` as 4, all big-endian, the root's 32 bytes and `state`'s. A snapshot of schema 1, written
before snapshots were signed, has neither `state` nor `signature`. A file the manifest does not list
answers 404, and so does every request to a server without `[daily_snapshots]`. The server checks
each snapshot's length and hash against the manifest before serving it. `engine replay --snapshots`
takes one only at a checkpoint the log holds, equal to the snapshot's, once the root of the
snapshot's state is that checkpoint's, and only where the replay of the log before it ends in that
state; it skips, and names, one of a checkpoint the log does not hold. A rotation member with an
empty copy starts its replica from one only when its signature covers its state and is by the key
that signed its checkpoint, which the vault recorded.

## The stream

`GET /v1/stream` opens a WebSocket that carries the public log: every command with its events, every
checkpoint, and every withdrawal authorisation, each message once and in offset order. A connection
can also carry signed frames, executable quotes, the operating status and the status notice.

### Parameters

| Parameter | Meaning |
| --- | --- |
| `from` | Starts at the first message of this seq. `from=0` is the whole log. |
| `after` | Resumes after this offset, the `offset` of the last message received. |
| `signed` | `1` sends each message in a signed frame, with heartbeats. It requires `challenge`. |
| `challenge` | 64 hexadecimal characters of fresh randomness, which every frame is signed under. It requires `signed=1`. |
| `status` | `1` also sends the operating status. It requires `signed=1`. |
| `notice` | `1` also sends the status notice. It requires `signed=1`. |
| `heads` | `1` also sends each log head that the enclave signs while it holds a daily operator turn. It requires `signed=1`. |

Without `from` or `after`, the stream starts with the next message. With either, it reads the log
from storage from that point, then continues live. `from` and `after` together are refused with 400,
as is any other value that breaks the rules above. A connection that falls behind reads from storage
again rather than being dropped. A connection that stops reading for `api.send_timeout` is closed,
and the client resumes with `after`.

### Messages

Each message carries its `offset` and a `type`:

| `type` | Content |
| --- | --- |
| `command` | `seq`; `ts`, the time at which the command was processed; `cmd`, the command, tagged by its own `type`; `events`; and `seal`, the enclave's signature over the command, its events and the entry before it. |
| `checkpoint` | `seq`; the state `root`; the enclave's `signature` and its `signer`; and `domain`, the chain and vault the checkpoint commits to. A server that posts its checkpoints to the vault also names `parent`, the vault's checkpoint that this one extends. `deferred_withdrawals` is present and true while withdrawal signatures wait for custody to confirm this checkpoint. |
| `withdrawal` | A signed withdrawal authorisation, in the form the payouts route gives it. |

The first command in a log has seq 0. A command message looks like this:

```json
{"offset": "16:0", "type": "command", "seq": 16, "ts": 1795006113530,
 "cmd": {"type": "house_capital", "amount": "10000000", "chain_ref": "genesis"},
 "events": [{"type": "house_capital_moved", "amount": "10000000", "nav": "10000000"}],
 "seal": {"domain": {"chain_id": 31337, "vault": "0x…"}, "signer": "0x…", "signature": "0x…"}}
```

### Signed frames

With `signed=1`, every message is sent inside a frame that the enclave signs:

```json
{"v": 1, "n": 0, "time": 1792504804200, "msg": "<the message as a JSON string>", "sig": "0x…"}
```

`n` counts the frames on the connection from 0, and `time` is the server's time in milliseconds.
`msg` is the message as a JSON string, so that the signed bytes survive decoding. The signature
covers the keccak-256 hash of the following values, in this order:

1. The ASCII text `engine.frame.v1`.
2. The chain id, as an unsigned 64-bit big-endian integer.
3. The vault's address, 20 bytes.
4. The connection's challenge, 32 bytes.
5. The SHA-256 hash of `GET`, a zero byte, and the path and query of the request that opened the
   connection.
6. The previous frame's digest, or 32 zero bytes for frame 0.
7. `n`, as an unsigned 64-bit big-endian integer.
8. `time`, as an unsigned 64-bit big-endian integer.
9. The SHA-256 hash of `msg`.

Because each frame names the one before it and the request that opened the connection, a frame fits
only in its own place on its own connection. A client closes the connection at the first frame that
fails to verify. `engine verify-frames` checks a saved connection, one frame a line.

The server sends a heartbeat frame when the connection opens and then at least every
`api.heartbeat_interval`, 5 seconds by default. Its message is
`{"type": "heartbeat", "head": "<seq>:<index>"}`, where `head` is the last message the server has
stored, or `null` before the first. A client treats the stream as stale when
no heartbeat has arrived for about two intervals. While the server's time is not trustworthy, it
cannot sign a frame, so it closes the connection, and the client resumes it with `after`. The
exception is a connection with `heads=1` on a server that holds a daily operator turn. It stays
open, skips its heartbeats, and receives each log head bare, outside a frame, since a head carries
its own signature.

With `status=1`, the connection receives the body of `GET /v1/status`, with `"type": "status"`, at
the start and whenever it changes. With `notice=1`, it receives the body of `GET /v1/notice`, with
`"type": "notice"`, in the same way. Both are checked every `api.quote_interval`. With `heads=1`,
the connection also receives each signed log head as a `log_head` message, which members of the
operator rotation relay.

### Executable quotes

A signed connection can ask for one account's executable quotes, the prices at which its orders
would be executed now. The request names the account by address and needs no signature, because
account data is public. The client sends a text message:

```json
{"op": "quotes", "account": "0x…", "markets": [1, 2]}
```

The list may name at most `api.quote_max_markets` distinct markets, four by default, and no more than
four in a production build. Sending the message again replaces the subscription, and an empty
`markets` ends it. The connection then
receives one quote message per market, in signed frames, and a new one for each market whose quote
has changed. The server reads quotes only after the log has moved, and at most once every
`api.quote_interval`, 100 milliseconds by default.

```json
{"type": "quote", "account": "0x…", "market": 1, "open": true, "reduce_only": false,
 "stress_level": 0, "toxicity_bps": 0, "mark": "100000",
 "ask": [{"qty": "…", "price": "…"}], "bid": [{"qty": "…", "price": "…"}], "close": null}
```

`ask` is what a buy pays and `bid` what a sell receives, each a list of steps by size. An order
takes the price of the first step whose `qty` is at least its own, and that price is the average
price for an order of the step's whole size. There is a step at the top of each spread band the
account would reach, and a last step at the largest order the account can open. A step that the
account cannot open carries `declined`, with the reason `insufficient_margin`, `weekend_margin` or
`house_capacity`. A step's `declined` is the answer for an order of exactly the step's size. While the
protocol parameters name offset groups, an order between two steps can be declined when both steps
are admitted, or admitted when both are declined, and the order's own preview, described under the
account routes, gives its answer.
An order sent with a step's price as `quoted` and a slippage limit is executed if nothing has moved,
and is rejected with `slippage` if the price has moved by more than the limit. `toxicity_bps` is the
spread that the account's toxicity adds to every price in the message. `close` is the side, quantity
and price of closing the account's whole position, or of 1,000,000,000 units of a larger one, and
`null` when the account holds no position. It carries `declined` when an order making that close
would be rejected, which a close-all of the market would not be. When the market has no price, `unavailable` gives the
reason as a rejection reason, and `ask` and `bid` are empty.

A quote does not include screening, so an order can still be refused by screening at its quoted
price. A subscription that the server cannot serve, such as one for an account never credited, an
unknown market or too many markets, receives `{"type": "quote_error", "error": "…"}` and no quotes.
Without `signed=1`, the server ignores what the client sends.

## Operator routes

`POST /v1/admin` takes the operator's signed administrative commands, which `engine admin` sends.
Its body is `{"command": {…}, "auth": {…}}`, where `command` is an administrative command tagged by
`action`, and `auth` holds the operator key's signature over the `AdminRequest` type, its nonce and
its expiry. The answer is the command's events, ending in `admin_applied` or `admin_rejected` with
the reason. The server refuses a command that only it may issue with 400, a signature that is not
the operator's with 401, an expired request with 400, and a nonce other than the operator's next
with 409. None of these reaches the log. Browsers can never call the route.

The operator may list a market or change its specification, change the trading settings after
notice, publish benchmark rates, raise a market's stress level, make one market or every market
reduce-only, and set or clear the status notice. No administrative command can stop a client from
reducing or closing a position in a market that is open and has a price.

`POST /v1/standby/vouch` lets a member of the operator rotation obtain the operator's vouch for its
standby key, which the member then registers with the operator schedule. The request carries the
member's quote, and the answer is the vouch, signed by the enclave's key. The route answers 404 on a
server that does not vouch, and 400 naming the cause when it refuses. Trading clients never call it.
