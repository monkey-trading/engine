# Access source and eligibility

From V24, a trading account signs a professional or eligible-client declaration before the engine
credits its first deposit or accepts an order. A new account deposits first: the deposit waits
pending until the account records a declaration, then credits. Declarations, orders and withdrawals
require fresh screening of the account and of the client's peer. US and sanctioned jurisdictions,
sanctioned accounts, and unknown or unavailable classifications are refused. A direct vault call can transfer tokens; the custody adapter applies these checks
before engine credit. Pending and held funds never enter trading balances or house NAV.

Run `./scripts/dev-access-enforcement.sh` for the isolated Anvil scenario. It configures a signed,
loopback-only synthetic source, submits declarations, fills signed orders, tests API refusals and
direct `depositFor` and contract-wallet funding, and restarts with the cursor removed. The fixture
maps test accounts to classifications; it does not perform production geolocation.

## Source configuration

Add one `[access]` section to the node TOML:

```toml
[access]
source = "<screening authority's EVM address>"
endpoint = "<authenticated HTTPS endpoint>"
timeout = "2s"
max_age = "30s"
max_response_bytes = 4096
max_in_flight = 8
```

The durations and concurrency limit above are examples for local testing. Select production bounds
with the approved source. Credential-bearing endpoint URLs are redacted and never recorded in the
public log. TLS uses compiled roots; proxies and redirects are disabled. Plain HTTP is allowed only
on literal loopback addresses with the mock TEE. Real attestation refuses the repository's published
test keys as screening authorities.

An absent section leaves affected actions closed. The source signer and freshness bound are recorded
once in the public log. A later configuration must match them; a mismatch refuses startup before
history repair or log append. Replacing an endpoint cannot replace its recorded signing authority.
The first policy may be recorded when configuring a log that previously had none. Rotating the
source's signing key or changing the freshness bound therefore needs a fresh log.

## Request and response contract

The node sends one JSON POST per screening action:

```json
{
  "request_hash": "<32-byte hex digest>",
  "account": "<beneficiary address, or null for house funding>",
  "payer": "<actual funding address for a deposit, otherwise null>",
  "peer": "<actual API socket peer IP, or null for custody>"
}
```

The source returns a `Screening` object from `engine_core::access`, with exactly these fields:

| Field | Meaning |
| --- | --- |
| `source` | Recorded screening authority address |
| `request_hash` | Exact request digest from the query |
| `observed_at` | Milliseconds since Unix epoch; the oldest underlying classification's decision time |
| `account`, `payer` | Exact subjects from the query, including nulls |
| `jurisdiction` | `allowed`, `united_states`, `sanctioned`, or `unknown` |
| `account_status`, `payer_status` | Each `clear`, `sanctioned`, or `unknown` |
| `signature` | Authority's 65-byte EIP-712 signature, hex, with recovery byte 27 or 28 |

`observed_at` is evidence freshness, not retrieval time. The source must not refresh it by fetching
an old decision. The engine rejects evidence stamped more than one second after the sequenced
time (`MAX_EVIDENCE_LEAD_MS`, the same lead venue quotes get, since the host clock that stamps
commands may lag the source's) and evidence older than the recorded bound at sequenced time,
including time spent waiting in queues. It verifies every required disposition
independently. Custody checks payer and beneficiary sanctions; API orders and withdrawals check
account sanctions and the actual peer's jurisdiction. Forwarded and X-Forwarded-For headers cannot
change that peer. A proxy needs a separate authenticated trust model before it can supply geography.
The socket peer itself comes from the host's network, which a host relay or NAT can rewrite to an
allowed address. Jurisdiction screening is therefore not yet bounded against the host; binding it
to an assertion the host cannot alter is a promotion requirement.

Use `engine_core::eip712::screening_hash` to sign the response. The type is
`AccessScreening(string decision)` under the recorded deployment domain: name `monkeytrading`,
version `1`, custody chain id, and vault address. `decision` is canonical compact JSON with fields in
this order: `source`, `request_hash`, `observed_at`, `account`, `payer`, `jurisdiction`, `account_status`,
`payer_status`. It excludes `signature`. The request hash binds the signed client action, including
nonce and expiry, or the full deposit input, including its chain reference, amount and funding address.
The independent wallet vectors are in `crates/core/tests/fixtures/access-eip712.json`.

The source contract is a gateway interface, not a selected commercial provider or legal country
list. Selecting the data sources, jurisdiction classifications, freshness policy, signing authority
and credentials, and obtaining counsel's launch sign-off, remain promotion requirements. Bind the
node configuration into verified attestation before relying on host-supplied configuration.

## Signed declarations and refusals

POST `/v1/eligibility` with `account`, `class` (`professional` or `eligible_client`) and mandatory
`auth`. The exact signed statement is respectively “I attest that I am a professional client.” or
“I attest that I am an eligible client.” It adds no further qualification claims. Use the existing
recorded EIP-712 deployment domain, account nonce and expiry; declarations require signatures even
in development logs that recorded unsigned client auth. From V24, other client actions also require
signatures. Cancellation remains available without a geography restriction.

`engine-core`'s `sign_request` example accepts `eligibility` with the unsigned JSON body. Read the next
nonce at `/v1/accounts/{address}/nonce` and the recorded class at `/v1/accounts/{address}/eligibility`.
Eligibility is separate from funded account state. A never-credited account can declare only once a
deposit for it is pending, and only a declaration the engine could record is admitted: the account
has not declared yet, and the expiry is at most five minutes ahead. The API screens a declaration
like an order, with the account and the transport peer, and the engine records it only on verified,
fresh, allowed and clear evidence. A declaration from a US or sanctioned jurisdiction, or with
unknown, unavailable, stale, mismatched or forged evidence, is refused in the public log with its
reason (`eligibility_rejected`) and uses its nonce; the account's deposit stays pending and is not
retried. The account may declare again, each attempt counted against its rate, its client's share of
fresh-account admissions and the shared budget, and the next retry after a recorded declaration
credits the deposit.

Policy refusals are sequenced and durable before the API returns their rejection events. They use
the authenticated signer's nonce, preventing replay from the public log after conditions change.
Invalid authentication, stale API nonces, rate limits and transport failures do not create a claimed
account action. A never-credited account may sign only a declaration, and only once a deposit for it is
pending; its other requests, and a declaration without one, are refused with 403 before the log. An otherwise allowed EVM withdrawal receives `payout_unavailable` until on-chain
payouts exist; no collateral is debited. An unhealthy reconciliation guard also prevents approval.
Screening at placement covers a resting limit or stop order until it fills or is cancelled; it is
not screened again when it triggers. Orders resting from before V24 were admitted under the rules in
force when they were placed and are settled the same way. Liquidation and client cancellation remain
available.

## Pending retries, holds, reconciliation and monitoring

A sanctioned payer cannot fund an innocent beneficiary through `depositFor`, or fund the house.
The vault's payer-bearing `Deposited` event authenticates its caller, including contract wallets;
the outer transaction sender is not used. Legacy trader deposit events name no payer, so they cannot
be screened and are held under V24 (`payer_missing`). The same custody reference cannot be both
credited and held.

Only a confirmed sanction of the account or payer, or a missing payer, holds a deposit for good. Every
other refusal is curable, so the deposit stays uncredited and durably pending: the account has not
declared yet, the source classifies the account or payer as unknown, or the source gives no verified
decision because it is unavailable or unconfigured, or its evidence is malformed, not signed by the
recorded authority, stale, or about another request. `GET /v1/access/pending/{chain_ref}` exposes its
immutable funding input. A separate supervised worker screens at most eight pending deposits per
turn, in a fair circular scan, every five seconds, skipping deposits whose account has not declared.
A screening that fails, or that still classifies a subject as unknown, changes nothing and is not
sequenced. When a sequenced retry's evidence still fails verification, the turn ends and the wait
doubles, up to five minutes; the next turn that leaves no sequenced retry pending resets it to five
seconds. The engine indexes pending deposits by beneficiary and keeps the retryable ones in their
own ordered set, so neither a turn nor declaration admission scans the backlog. Each HTTP call keeps the configured
deadline and size bounds. The ingestion cursor can advance after the pending command is durable;
its progress is independent of retry IO. Both mock and EVM custody resume this work from the public
log after restart or cursor loss. Verified clear evidence for a declared account credits the deposit
once; a confirmed sanction moves it into a permanent hold, which has no automatic release path.

`GET /v1/access/holds/{chain_ref}` exposes one hold's funding input and reason. The corresponding
sequenced command carries its signed screening evidence, or a bounded typed source failure. No raw
IP, endpoint, credential or raw provider response enters the public log. Replay reconstructs holds
and declarations and checks the same source proofs and state roots.

Pending and held collateral are reserved in checkpoint reconciliation, including the vault deposit count, while
remaining outside trading cash and house NAV. Cursor loss, process restart, and a later clear source
decision cannot release sanctioned funds. There is no release or refund command; counsel-directed
handling needs its own approved implementation. Keep the public log and cursor for investigation.
A plain token transfer to the vault has no vault deposit event and appears as unexplained collateral
to reconciliation, rather than as an attributable deposit.

`engine_access_pending_deposits` reports the current durable backlog, including after recovery.
A successful retry transfers reserves into cash or a hold without recounting the vault deposit.
`access_deposit_held` is a Critical operator alert. `access_source_failed` is a Warning that resolves
after verified fresh source evidence is available. `access_deposit_retry_failed` is a Warning raised
while a retry turn's screening fails or its evidence does not verify, and resolved by the next turn
that does not fail. Refusal counters include `eligibility_rejected`
and `deposit_held` in `engine_rejections_total`; API policy reasons appear under `reason="access"`.
Use the public event for the exact reason. API screening has bounded concurrency; custody retains
independent bounded screening capacity for ingestion and retries. A declaration from a never-credited account takes one token from its client's
share, at the per-account rate, and one from a budget all clients share, at the per-IP rate. Fresh
signing keys cannot multiply that budget, and no one client can take all of it.

## Promotion and rollback

Public launch requires counsel sign-off. Provider selection, approved jurisdiction and sanctions
classifications, production source credentials, and verified data freshness are promotion requirements.
Supply those inputs through the signed interface; the committed fixtures contain synthetic local policy.
Handle sanctioned collateral only as counsel directs.

Deploy a vault emitting the payer-bearing event before accepting new trader funding under V24.
The vault is immutable; an existing deployment continues emitting legacy events. Changing the vault
or chain changes the recorded signature domain and requires a fresh log and an explicitly planned
custody transition. This feature performs no deployment, migration, release or refund.

V24 activates at a recorded sequence on the first boot of a V24 build. Earlier commands retain
their earlier roots and events, and all historical reference logs stay byte-for-byte unchanged.
A build predating V24 cannot replay an activated log; rollback requires an approved log and custody
plan, not regeneration of reference fixtures or deletion of funded history.
