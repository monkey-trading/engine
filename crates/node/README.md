# engine-node

## Overview

The node that runs `engine-core` inside a TEE (an Intel TDX or AMD SEV-SNP confidential VM). It
sequences every command into a public, append-only log, signs state-root checkpoints with an
attested enclave key, serves the client API, streams the log live, indexes each account's history
from the log, and talks to custody and price feeds. For its operator it serves Prometheus metrics
and posts alerts to webhooks. The same binary replays a published log and verifies its checkpoints.

## Features

- Dated publisher ingestion with a separate signing worker, startup synchronization, and per-date
  rollover catch-up under V18
- Single-owner sequencer on a dedicated OS thread: commands are logged and flushed before any client sees their effect
- Genesis all or nothing: the engine checks every genesis command at config load and again before
  any is written, and recovery refuses a log whose genesis stopped short of its closing checkpoint
- Signed checkpoints and withdrawals, bound to the chain id and vault, from an attested key
- Strict checkpoint signature verification in replay and recovery, with explicit domains for early logs
- Optional checkpoint relaying, signed-parent vault commitments, startup rollback checks, and collateral
  reconciliation that pauses withdrawal signatures while trading continues
- Attestation through Linux configfs-tsm, covering TDX and SEV-SNP, or a clearly marked mock
- Trusted time: every stamp is checked against signed Roughtime readings that independent servers
  agree on, and nothing, genesis included, is stamped before the first; sequencing halts with a
  Critical alert when the host clock is beyond the drift bound, or 60 seconds after the last
  sample, and resumes once a sample is within the bound
- REST API for orders, cancels, withdrawals, and reads, plus a WebSocket stream of the log
- Orders, cancels, and withdrawals signed by the account (EIP-712), checked before they are
  sequenced; `auth = "none"` trusts the account field and runs only with the mock TEE and custody
- The latest protocol version and the client auth activated at boot, at the log's next seq, after
  giving parameters a log set before `V11` the config's toxicity notional weighting
- Stream offsets of `<seq>:<index>`: a subscriber starts or resumes at any offset, reads the log
  from disk, then goes live, getting each message once; one that falls behind resumes from disk
- Segmented log: completed segments and a manifest of their seq ranges and SHA-256 hashes are a
  public archive served over HTTP
- TLS terminated in the enclave, with a key generated there and its hash in the attestation
- Rate limits on requests per client IP and per account and on new connections per client IP,
  caps on request size, connections, and stream subscribers, and read timeouts, all set in `[api]`
- `engine replay` to verify a published log, or a downloaded archive, against the open-source
  engine, reading it a segment at a time; a missing log, or one with no command or no checkpoint,
  fails
- Account history: every event that names an account, indexed in SQLite from the log and paged
  newest first by a `<seq>:<index>` cursor, with a stated retention; it survives restarts, and
  rebuilds from the log when it is missing, damaged, or does not match the log
- `engine admin` to sign an admin command with the operator's key, read from a file or
  `OPERATOR_KEY`, and submit it over TLS pinned to the node's attested key; the node holds only the
  operator's public key, recorded in the log, and refuses a request before the log unless it carries
  the operator's signature, next nonce, and an unexpired expiry
- Proofs of an account's balance and positions, and of its positions' marks, against the last
  checkpoint's state root, from protocol `V9`
- Supervised tasks: any task stopping stops the node, and SIGTERM drains to a final checkpoint
- Live crypto prices from Coinbase, Kraken, and Bitstamp over WebSocket: one supervised task per
  venue, quotes stamped with the venue's own time, and reconnects with backoff
- Alerts for refused venue messages (at most one a minute per venue, counting the rest), venue
  outages, symbols with no fresh quote, and venue times well ahead of the host clock
- A `house_deficit` alert when house NAV falls below zero, at most one a minute while it keeps
  crossing zero
- EVM custody: each vault deposit credited once after a strict majority of at least three RPC
  providers agrees, at a configurable finality tag and confirmations, with a resumable cursor
- Mock custody and a mock price feed for local development
- Prometheus metrics on a separate operator address, from a registry the node owns, with no
  per-account labels or values
- Alert routing: Critical and Warning alert lines are posted as JSON to a webhook per severity,
  once when an episode starts and once when it is resolved

## Repository Structure

- `src/main.rs` — the `engine` binary: `run`, `replay`, and `admin`
- `src/lib.rs` — module index
- `src/admin/mod.rs` — `engine admin`: the operator key, the commands it signs, and submission
- `src/admin/benchmark_feed.rs` — the separate signed publisher worker and rollover scheduling
- `src/admin/benchmark_watch.rs` — bounded schedule notifications and reconnection for the worker
- `src/benchmarks/` — official CSV fetching (`mod.rs`), exact decimal parsing (`parse.rs`), compact
  schedule notifications (`schedule.rs`), and tests
- `src/admin/tls.rs` — the TLS connection to a node, pinned to its attested key
- `src/admin/tests.rs` — `engine admin` tests: key sources, commands, signing, and pins
- `src/node/mod.rs` — startup, task supervision, and shutdown
- `src/node/tests.rs` — startup, history recovery, and incomplete-genesis tests
- `src/telemetry.rs` — the log subscriber, which feeds alert lines to the router, and the metrics
  registry, set up before the node runs
- `src/sequencer/mod.rs` — the sequencer, checkpoints, and metrics
- `src/sequencer/messages.rs` — sequencer requests, stream messages, and signed withdrawals
- `src/sequencer/checkpoints.rs` — checkpoint creation, custody reconciliation, and signature release
- `src/sequencer/payouts.rs` — durable unpaid delivery, finalized claim evidence, expiry, and epoch reissues
- `src/sequencer/payout_tests.rs` — release order across claim pages, durability, restart, expiry, and rotation
- `src/sequencer/withdrawal_scenario/` — actual Anvil/TLS/stream/custody crash and FIFO scenarios with local signed time
- `src/withdrawals.rs` — vault EIP-712 digests and exact approval inclusion verification
- `src/api/withdrawal_queue.rs` — public FIFO pagination and account queue positions
- `src/node/signer_history.rs` — finalized accepted checkpoint identities before recovery writes
- `src/replay_audit.rs` and `src/replay_audit/` — read-only custody-backed historical signer audits and tests
- `src/log/segments/durability_tests.rs` — failed-fsync stream publication regression
- `src/sequencer/checkpoints/accounting_tests.rs` — live/recovery accounting oracles and gated 100k-account capture scaling
- `src/sequencer/checkpoint_tests.rs` — confirmed-prefix signature gating and restart tests
- `src/sequencer/fixture_tests.rs` — test-only signing of copies of placeholder protocol fixtures
- `src/sequencer/recovery.rs` — pure recovery validation and exact authenticated checkpoint-tail restoration
- `src/sequencer/replay.rs` — log replay, which recovery and `engine replay` share
- `src/sequencer/required.rs` — client auth, the operator, protocol activation, and genesis, which
  wait for trusted time
- `src/sequencer/genesis.rs` — the genesis commands, the check that the engine accepts every one
  before any is written, and how far a replayed log got through genesis
- `src/sequencer/alerts.rs` — the alerts the sequencer raises: engine rejections an operator must
  look at, benchmarks with no rate, and house NAV below zero
- `src/sequencer/alerts/retirement_tests.rs` — benchmark retirement, catalogue changes, and resolution state
- `src/sequencer/alerts/retirement_webhook_tests.rs` — retirement pages through the durable webhook router
- `src/sequencer/alerts/undated_tests.rs` — scalar fallback, independent incidents, and durable webhook recovery
- `src/sequencer/benchmark_schedule.rs` — changed rollover notifications and publisher synchronization when due
- `src/sequencer/tests/benchmark_schedule.rs` — schedule changes, durable watches, and due-time synchronization
- `src/sequencer/tests/mod.rs` — sequencer, activation, recovery, and replay tests
- `src/sequencer/tests/replay.rs` — root, sequence, and pre-event log replay tests
- `src/sequencer/tests/benchmarks.rs` — publisher startup prerequisites and legacy-log refusal
- `src/sequencer/tests/benchmark_recovery.rs` — publisher synchronization after trusted-time halts
- `src/sequencer/tests/genesis.rs` — genesis tests: a rejected genesis writes nothing, and a log
  whose genesis did not apply in full is refused at recovery
- `src/sequencer/tests/operator.rs` — recovery, activation, and replay with the operator the log
  records
- `src/sequencer/batch_tests.rs` — batching: a batch ends after `MAX_BATCH` requests of any kind
- `src/sequencer/margin_tests.rs` — the margin and solvency dev scenario on the dev genesis (a
  close-out after the rollover's financing, and a withdrawal limited at a stale market's last
  mark), and a genesis with no house capital that trades and alerts on a house deficit
- `src/sequencer/metrics_tests.rs` — the sequencer's metrics and alert-line tests
- `src/log/mod.rs` — the JSON-lines log format, stream offsets, and the single-file reader
- `src/log/segments.rs` — segment files, rotation, the manifest, and the recovery read, a segment
  at a time, with its checks
- `src/log/reader.rs` — reads the log from any offset while it is being written, up to the last
  entry the writer has flushed
- `src/log/checked.rs` — reads a completed segment in chunks checked against its listing
- `src/log/tests/mod.rs` — log format, segment, and writer tests
- `src/log/tests/reader.rs` — reader tests: offsets, following the writer, flushed entries only,
  and the checks on what it reads
- `src/api/mod.rs` — REST routes, request signature checks, and the request span
- `src/api/admin.rs` — the operator's admin route, checked before it is sequenced
- `src/api/benchmark_schedule.rs` — capped, bounded watches of the durable rollover schedule
- `src/api/benchmark_tests.rs` — deterministic publisher/TLS/API restart and financing dev scenario
- `src/api/benchmark_tests/recovery.rs` — publisher retries during an in-process trusted-time halt
- `src/api/benchmark_tests/retention.rs` — failed publisher refreshes and calendar changes after restart
- `src/api/benchmark_tests/schedule.rs` — actual worker rescheduling, in-flight nonce fencing, and financing
- `src/api/benchmark_tests/schedule/reconnect.rs` — watch-only transport failure during a publisher fetch
- `src/api/admin_tests.rs` — admin route tests, and the operator channel dev scenario through
  `engine admin`
- `src/api/stream/mod.rs` — the WebSocket stream: backfill from disk, then live, under the
  subscriber cap, reading what the subscriber sends
- `src/api/stream/tests.rs` — feed, backfill, and subscriber tests
- `src/api/archive.rs` — the archive routes: the manifest and completed segments
- `src/api/auth_tests.rs` — signed-request API tests: signatures, nonces, expiries, refused
  accounts, and racing requests
- `src/api/history.rs` — the account history route: its parameters and limits
- `src/api/proof_tests.rs` — account proof API tests, including the state tree dev scenario: 100k
  accounts, checkpoint work, a proof checked against its logged root, and older logs replayed
- `src/api/tests.rs` — API tests, including the dev scenarios of the resumable stream and archive,
  and of account history, and `Retry-After`
- `src/history/mod.rs` — the history index: which events name an account, the startup check
  against the log and the rebuild when it fails, and the supervised task that follows the log into
  the index
- `src/history/store.rs` — the index's SQLite schema, its writer, its page reader, and its discard
- `src/history/tests/mod.rs` — history index tests: pages, filters, retention, following the log,
  and restarts
- `src/history/tests/recovery.rs` — startup checks against the log, readiness to serve, and
  failures
- `src/history/tests/rebuild.rs` — power losses and other damage: an index left behind the log
  resumes, and one ahead of it, contradicting it, damaged, or pruned under another retention is
  rebuilt
- `src/test_support.rs` — the shared test harness, including the capture of alerts and their levels
- `src/test_tracing.rs` — the tracing setup the test collectors share: one process-wide
  interested subscriber, so no callsite can be cached uninteresting and a collector never
  silently misses an alert or line
- `src/tee.rs` — the enclave key, signatures, and attestation providers
- `src/trusted_time/mod.rs` — the trusted time task: queries the Roughtime servers, backs off
  failing ones, and turns agreeing replies into a sample
- `src/trusted_time/gate.rs` — the sequencer's check of each stamp against the latest sample
- `src/trusted_time/tests/` — agreement, round, back-off, gate, and alert key tests, and the
  offline dev scenario
- `src/roughtime/mod.rs` — the Roughtime client: builds requests and verifies signed replies, for
  version 1 (RFC 10049) and the final drafts' 0x8000000c
- `src/roughtime/message.rs` — the Roughtime wire format
- `src/roughtime/testing.rs` — a Roughtime server for tests that signs with its own keys
- `src/roughtime/tests.rs` — client tests over recorded and locally signed replies
- `src/roughtime/fixtures/replies.json` — replies recorded from the servers in `config/dev.toml`
- `src/tls/mod.rs` — the TLS key and certificate, and the server loop that applies the connection
  caps and timeouts
- `src/tls/tests/mod.rs` — the API end to end over TLS: key pinning, limits, caps, and the harness
- `src/tls/tests/stream.rs` — the stream end to end over TLS: the subscriber and message caps
- `src/limits.rs` — request rate limits, connection caps, and the stream subscriber cap
- `src/config/mod.rs` — the node config schema and loading
- `src/config/benchmarks.rs` — publisher endpoints, cadence, timeout, and catalogue coverage
- `src/config/checkpoints.rs` — checkpoint relayer config and validation
- `src/config/custody.rs` — the `[custody.evm]` section, independent RPC providers, redacted
  URLs, and custody checks
- `src/config/telemetry.rs` — the metrics listener and alert webhook config, and the redacting
  `WebhookUrl`
- `src/config/tests.rs` — config loading and validation tests
- `src/config/mock_key_tests.rs` — published mock-only signer opt-in and hardware refusal regressions
- `src/config/custody_tests.rs` — independent provider configuration checks
- `src/config/telemetry_tests.rs` — metrics and alert config tests: the Critical route, the dev
  config's telemetry, and webhook URLs
- `src/config/validation.rs` — the checks a parsed config must also pass
- `src/config/test_keys.rs` — the published test keys from `config/test-keys.toml`, which a node
  with real attestation refuses, and their tests
- `src/checkpoints/mod.rs` — posting, retry, signed-parent commitment checks, and collateral reconciliation
- `src/checkpoints/accounting.rs` — bounded custody totals derived from computed engine events
- `src/checkpoints/lifecycle.rs` — pinned signer/epoch, cumulative paid amounts, and approval nonce-use evidence
- `src/checkpoints/lifecycle_tests.rs` — paid cash reconciliation, bounded claim polling, and rotated authorities
- `src/checkpoints/reconciliation.rs` — finalized cash reconciliation against deposits less authenticated payouts
- `src/checkpoints/rpc/observation.rs` — same-block balance, epoch, paid total, and bounded claim reads
- `src/checkpoints/rpc/authority.rs` — accepted historical signer identities for rotated nonce journals
- `src/checkpoints/rpc/submission/tests/lifecycle.rs` — canonical pinned history and retired journal recovery
- `src/checkpoints/accounting/tests.rs` — cash/reference recomputation oracles and arithmetic boundaries
- `src/checkpoints/signature.rs` — shared strict checkpoint signature and domain verification
- `src/checkpoints/rpc.rs` — pinned-block vault reads and transaction relay through a signing RPC
- `src/checkpoints/rpc/tests.rs` — sampled ancestry and durable relayer request regressions
- `src/checkpoints/rpc/submission.rs` — bounded locked crash-durable nonce/hash reservations
- `src/checkpoints/rpc/submission/posting.rs` — persisted reservation and exact transaction retry transitions
- `src/checkpoints/rpc/submission/progress.rs` — canonical finalized obsolete nonce-consumer proofs
- `src/checkpoints/rpc/submission/tests.rs` — response-loss, restart, identity, and reservation regressions
- `src/checkpoints/rpc/submission/tests/fakes.rs` — isolated relay and canonical transaction RPC fixtures
- `src/checkpoints/rpc/submission/tests/nonce_progress.rs` — free, obsolete, and resurfaced nonce regressions
- `src/checkpoints/setup.rs` — finalized chain anchor lookup and pending-inclusion startup wait
- `src/checkpoints/tests.rs` — signature mutations, rollback, retry, and reconciliation tests
- `src/checkpoints/scenario.rs` — isolated Anvil posting, tampering, truncation, shortfall, and reorg checks
- `src/history/preflight.rs` — immutable history planning before authorized SQLite mutation
- `src/history/preflight/tests.rs` — private WAL resume and malformed geometry byte-preservation regressions
- `src/node/tests/custody_preflight.rs` — local custody refusal preserves log/history across image states
- `src/node/preflight.rs` — chain validation before writable history recovery
- `src/node/preflight/tests.rs` — refused startup preserves history, WAL, and log bytes
- `src/sequencer/replay/auth_tests.rs` — trusted immediate/deferred completion evidence regressions
- `src/sequencer/replay/accounting_tests.rs` — historical fixture and outstanding-work accounting oracles
- `src/custody/mod.rs` — the custody providers (mock and EVM) and the USDC-to-`Fixed` conversion
- `src/custody/evm.rs` — the deposit watcher: the chain and token check, range reads from blocks
  proven to lead to the tagged final block, the completeness check, and stall alerts
- `src/custody/chain.rs` — the chain of block hashes proven to lead to the tagged block, and the
  headers of an interrupted proof, kept apart until it ends there
- `src/custody/watchdog.rs` — alerts for a stale endpoint head, a final block that stops, and
  reads that keep trailing the final block
- `src/custody/stall.rs` — what a poll did, the watcher's run loop, and the alert each kind of
  stall raises
- `src/custody/sizing.rs` — read sizes that adapt to a provider's limits
- `src/custody/credit.rs` — crediting one deposit once, and the sequencer round trips it needs
- `src/custody/quorum.rs` — strict-majority reads at common block heights, provider alerts, and
  a Critical alert while quorum is lost
- `src/custody/rpc.rs` — the vault over JSON-RPC, trusting only compiled-in TLS roots, with
  errors scrubbed of the endpoint URL
- `src/custody/cursor.rs` — the deposit cursor kept between restarts
- `src/custody/tests/mod.rs` — deposit watcher tests against a fake chain and a real sequencer
- `src/custody/tests/alerts.rs` — the watcher's alerts over time, in paused time
- `src/custody/tests/cursor.rs` — the deposit cursor across restarts, and which cursors a log
  refuses
- `src/custody/tests/fakes.rs` — the fake chain, test sequencer, log capture, and fixtures
- `src/custody/tests/quorum.rs` — fabrication, disagreement, lag, outages, and quorum recovery
- `src/custody/tests/quorum_diagnostics.rs` — malformed header exclusion, transport independence, and retained watchdog progress
- `src/custody/tests/rpc.rs` — the RPC adapter's log decoding, deadlines, and URL scrubbing
- `src/custody/tests/reorg.rs` — reorgs and forked answers against the proven chain
- `src/feeds/mod.rs` — the feed index and the mock feed
- `src/feeds/venue/mod.rs` — the venue task: connect, subscribe, sequence quotes, alert, reconnect
- `src/feeds/venue/validation.rs` — the checks every venue's books pass before they are sequenced
- `src/feeds/venue/confirm.rs` — confirming with the engine which markets the venue is quoting,
  holding back markets it does not list, and settling a dropped connection's last quotes
- `src/feeds/venue/quotes.rs` — the quotes held between ticks until the host clock reaches them,
  and the rule that decides whether the quote the engine holds counts as the venue quoting
- `src/feeds/venue/setup.rs` — the TLS client config and reconnect backoff every connection shares
- `src/feeds/venue/tests.rs` — venue task unit tests: alert throttling and silence windows on
  paused time, held quotes against the engine's rule, kept books for unsubscribed symbols, the
  freshness rule, and backoff
- `src/feeds/coinbase.rs`, `src/feeds/bitstamp.rs` — venue adapters
- `src/feeds/kraken/mod.rs` — the Kraken adapter: a kept order book verified against Kraken's
  checksum; `src/feeds/kraken/tests.rs` replays recorded book messages against it
- `src/feeds/tests/mod.rs` — venue tasks against loopback venues: what they sequence, refuse,
  and alert, and how they reconnect
- `src/feeds/tests/fake.rs` — the loopback venues, the feed harness with an engine that applies
  what the feed sequences, and the wait for the next alert
- `src/feeds/tests/late.rs` — venue tasks against a sequencer that answers late: when a quote
  counts as the venue quoting, and how a connection's last quotes are settled after it ends
- `src/feeds/tests/mock.rs` — the mock feed stopping one market at its `stop_after`
- `src/feeds/tests/scenario.rs` — the dev scenario through the sequencer and engine
- `src/throttle.rs` — holds a repeating alert to one a minute, counting the ones it held back
- `src/clock.rs` — the wall and monotonic clocks the sequencer stamps and checks with
- `src/metrics/mod.rs` — the metrics registry, event counting, and the `/metrics` route
- `src/metrics/tests.rs` — metrics tests
- `src/monitor/mod.rs` — samples queue depth, the house, markets, and quote freshness into
  metrics, and raises `market_no_price`, `market_unfed`, and `sequencer_stalled`
- `src/monitor/tests.rs` — monitor tests
- `src/alerts/mod.rs` — the tracing layer that picks up alert lines, the router, and the page
  format
- `src/alerts/episodes.rs` — one page per episode start and end
- `src/alerts/webhook.rs` — webhook routes, the TLS client, and one delivery task per route, with
  retries
- `src/alerts/outbox.rs` — the single-owner durable SQLite outbox and disk/row validation
- `src/alerts/outbox/tests.rs` — restart, bounds, corruption and storage-failure tests
- `src/alerts/tests.rs` — alert routing tests
- `examples/reference_log.rs` — writes a protocol version's node reference log from engine-core's
- `tests/reference_logs.rs` — replays every committed node reference log, events included
- `tests/usd_settlement.rs` — the USD settlement dev scenario on the committed dev config: Germany 40
  valued at EUR-USD's index while EUR-USD moves, then refusing orders while EUR-USD is stale
- `tests/reference/` — one node-format reference log per protocol version, never regenerated
- `tests/replay.rs` — replays a log the node wrote before protocol versions, kept in
  `tests/fixtures/`, and checks that `engine replay` fails on a log it cannot verify
- `tests/production_genesis.rs` — loads `config/production-genesis.toml` as a dev node's genesis,
  checks the launch values and quote currencies, and steps it through weeks with holidays and early
  closes

## Usage

```bash
# Run a local node with mock TEE, custody, and feeds (API on https://127.0.0.1:8080)
cargo run -p engine-node -- run --config config/dev.toml

# Call it pinned to the attested TLS key: report data bytes 32..64 are the SHA-256 of the
# certificate's DER SubjectPublicKeyInfo
API=https://127.0.0.1:8080
PIN=$(curl -sk $API/v1/attestation | jq -r '.report_data[66:]' | xxd -r -p | base64)
curl -s --insecure --pinnedpubkey "sha256//$PIN" $API/v1/health

# Price BTC-USD and ETH-USD from live venues instead, leaving the other markets unpriced: set
# feeds.provider = "venues" in a copy
cargo run -p engine-node -- run --config <copy of config/dev.toml>

# Verify a node's log directory, a downloaded archive, or a single log file
cargo run -p engine-node -- replay --log data/dev/log

# Sign and submit admin commands as the operator, over TLS pinned to the attested key (hex). The key
# is read from --key-file, OPERATOR_KEY_FILE, or OPERATOR_KEY; config/dev.toml names the public key
# of the local test key 0x4242…42 (32 bytes of 0x42). A market or parameters file holds the keys of a
# [[genesis.markets]] entry or of [genesis.params] at its top level, without the table header.
export OPERATOR_KEY=0x4242424242424242424242424242424242424242424242424242424242424242
PIN=$(curl -sk $API/v1/attestation | jq -r .tls_public_key_sha256)
cargo run -p engine-node -- admin --pin $PIN set-reduce-only true             # every market
cargo run -p engine-node -- admin --pin $PIN set-reduce-only false --market 2
cargo run -p engine-node -- admin --pin $PIN set-stress-floor 1 --market 1
cargo run -p engine-node -- admin --pin $PIN set-benchmark-rate SOFR 430
cargo run -p engine-node -- admin --pin $PIN upsert-market market.toml        # no table header
cargo run -p engine-node -- admin --pin $PIN set-params params.toml           # no table header

# Download the archive: the manifest, then each segment it lists (`--retry` waits out a 429)
curl -kf --retry 5 --create-dirs -o archive/manifest.jsonl https://127.0.0.1:8080/v1/archive/manifest.jsonl
jq -r '.file' archive/manifest.jsonl |
  xargs -I{} curl -kf --retry 5 -o archive/{} https://127.0.0.1:8080/v1/archive/{}

# Run the vault deposit scenario: Anvil, the vault, and a node with EVM custody
./scripts/dev-vault-deposits.sh

# Also fabricate a deposit at one provider, stop two providers, and recover quorum
RPC_QUORUM_SCENARIO=1 ./scripts/dev-vault-deposits.sh

# Run checkpoint posting, tampered replay, truncation, and shortfall checks on isolated Anvil
./scripts/dev-checkpoints.sh
```

EVM custody requires at least three entries in `custody.evm.providers`, each with a unique
`name` and a distinct credential-bearing `rpc_url`. Use independently operated services in
production; configuration cannot prove operator independence. Each URL must use HTTPS unless it
is a literal loopback address and must serve state back to `custody.evm.start_block` (an archive
node). The old single `custody.evm.rpc_url` config is rejected and must be migrated before restart.
A strict majority (two of three, three of four) must agree on identity, headers, deposit sets and
counts. Each poll admits only providers agreeing with its identity majority, which the watcher
checks against the configured chain and vault token. All subsequent reads exclude other providers;
a failed identity vote clears admission. The threshold stays fixed against all configured providers,
including those excluded or unavailable. Recovery requires a new successful identity poll. Finality is checked at the highest height a majority reports final, comparing its hash at
that same height; head differences alone do not prevent agreement. Empty log sets count only when
the answering provider also returns the range's last block.

A failed or dissenting provider raises `custody_rpc_provider_failed` or
`custody_rpc_provider_disagreed`, keyed by provider name and operation. Loss of quorum immediately
raises Critical `custody_rpc_quorum_lost` and pauses crediting while trading continues. Operations
retry on the next poll; recovery resolves the matching alerts. Credentials never appear in alerts.
The local scenario uses three proxies to one Anvil process, for fault injection only; the proxy
ports default to 18545–18547 and can be overridden with `RPC_A_PORT`, `RPC_B_PORT`, `RPC_C_PORT`.
`SCENARIO_LOG_DIR` preserves its logs. `config/anvil.toml` expects local endpoints on 8545–8547 when
run directly; the scenario rewrites these to its proxy ports.

The reserved V22 withdrawal rules reserve an accepted request from trading cash, rounding down to
whole USDC base units and retaining any smaller residual in the account. A public FIFO queue
approves its head partially when cash is short; each part has its own payout nonce. Trading losses
trigger another queue pass but do not create vault tokens. New deposits and house capital add
physical payout capacity. The node signs only after the approval is synced and included in a
finalized accepted checkpoint; a logged signature remains unpaid until the vault reports its used
nonce. Expired approvals are reissued with the same nonce and a new 24-hour authorization.

`GET /v1/withdrawals?limit=50&after=<request-seq>` returns `seq`, `queue` and `next`.
`GET /v1/accounts/<address>/withdrawals` filters the same FIFO and reports each request's global
one-based `position`. Both endpoints bound their scan to 1–200 requests; an account page may be
empty and still have `next`, which clients must follow. Queue records include the reserved amount,
remaining amount and original request sequence; queue changes also appear in the public log and
account history. `POST /v1/withdrawals` retains the signed client request format. Its later
`withdrawal` stream message carries the exact approved amount and nonce, signer epoch, issue/expiry
seconds, accepted checkpoint and approval proof needed by `Vault.withdraw`.

On-chain checkpoint posting uses an optional `[checkpoints]` section, required with hardware EVM
custody. `node.checkpoint_every` sets the periodic frequency; every approved withdrawal also forces
a checkpoint. Each root is durable locally before the relayer sends it, and the node releases no
withdrawal signature until its approval is covered by a confirmed on-chain prefix and collateral
reconciles at custody's configured `finality_tag` minus `confirmations`. Latest inclusion is
tracked separately so delayed finality never causes duplicate submission. Startup waits for a
pending inclusion to reach that boundary (or disappear in a reorg), bounded by
`custody.evm.stall_critical_after`, before selecting its rollback anchor. Recovery then restores every outstanding durable signed
checkpoint and its original accounting before signing descendants; an unmined signature remains
live even when both chain views still show its parent. Permissionless relays matching that local
chain reconcile the covered prefix, including work beyond the receive queue. The queue is bounded; exhaustion stops the node with the durable log available for
recovery. Failed posts retry after `retry_interval` and emit `checkpoint_post_failed`. A conflicting
posted root or signer stops the node with a Critical alert. A collateral mismatch emits Critical
`collateral_mismatch` and pauses withdrawals; ordinary trading commands continue. Deposits not yet
credited also defer withdrawal release until a checkpoint includes their credits; a mismatch
lasting `custody.evm.stall_critical_after` raises a Critical alert.

```toml
[checkpoints]
rpc_url = "http://127.0.0.1:8545"
relayer = "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266"
retry_interval = "2s"
rpc_timeout = "10s"
```

The endpoint must support `eth_sendTransaction`, `eth_getTransactionByHash` and latest/pending
`eth_getTransactionCount` for a funded relayer dedicated to this log. The node durably reserves an
explicit nonce in `node.log_dir/checkpoint-submission.json` before sending, reuses it after response
loss or restart, and polls known transactions. Keep this sidecar with the log; startup refuses a
live reservation absent from authenticated recovered work before touching history. The companion
`.lock` and `.json.tmp` files serialize posting and atomic replacement. These files provide crash
durability, not cryptographic protection against host deletion or rollback, and never authorize
withdrawal release. Unidentified external pending work, an unrelated nonce consumer, or a stuck
fee policy pauses posting for operator intervention; no automatic fee bump or cancel is sent.
After actual finalized vault progress makes an older reservation obsolete, the next checkpoint can
reuse its still-free nonce. A bounded, signature-checked `reused_through` checkpoint records that
obsolete prefix. If a dropped predecessor later consumes the reused nonce, canonical finalized
transaction history must prove it was an authorized obsolete vault call before the successor can
move to the next free nonce. Foreign nonce consumers still refuse. The endpoint must support
historical account-nonce reads and full consuming-block transactions; unavailable history leaves
the reservation intact. These nonce proofs never authorize withdrawal release.
It signs and
pays gas for relays; only the attested enclave's signature authorizes a vault commitment. Reads
pin signer, root, deposit count, and USDC balance to one canonical block hash. This RPC's chain view
is trusted pending independent verification. The immutable `Vault` constructor signer must match
the enclave key. The shipped dev and Anvil configs explicitly set `tee.use_published_mock_key = true`,
using the public development signer listed in `config/test-keys.toml` so actual process restarts
retain independent checkpoint and withdrawal authority. This option defaults false and is refused
with real attestation, including runtime callers which skipped config validation. It must never
protect real funds. All signature, domain and changed-authority checks still apply; logs created
under another key require that retained authority, with no migration or self-named signer fallback.
Real key persistence and rotation remain separate work: a fresh different key fails closed. Before any history SQLite open-for-write, schema creation,
WAL recovery, or repair, the node authenticates the replay and finalized anchor. History preflight
uses a read-only private WAL index without changing the original WAL/SHM, then rechecks actual
progress after authorization. Valid WAL-backed history resumes; damaged history is repaired only
after authorization. The Anvil test provisions its generated signer before deploying
the vault; this is a local harness, not a production key-provisioning workflow. Withdrawals still
have no on-chain payout path, so reconciliation counts every approval as unpaid. Payout settlement
must subtract confirmed payouts when that feature is added.

Checkpoint work captures fixed-size node accounting derived from computed engine events during
live sequencing and authenticated replay. Liabilities equal cumulative successful deposits plus
positive or negative house-capital moves; approvals transfer cash to unpaid liabilities. Only a
matching successful credit counts its `0x` reference, including accepted zero-capital moves.
Accounting overflow refuses further sequencing or replay. This cache adds no trusted metadata:
future snapshot recovery must rebuild or verify it once against authenticated cash, unpaid
approvals, and credited references before use, retaining exact outstanding checkpoint accounting
or falling back to full replay.

New on-chain checkpoints use `engine.checkpoint.v2`, signing the domain tag, chain id (u64 big
endian), vault (20 bytes), sequence (u64 big endian), root (32 bytes), parent-present byte,
parent sequence (u64 big endian), and parent root (32 bytes). The vault atomically compares the
signed parent with its latest commitment; two same-key instances cannot publish divergent
extensions of one parent. An absent parent is encoded as false/zero/zero. Historical checkpoints
keep the original `engine.checkpoint.v1` digest and remain replayable, but cannot authorize v2
vault posts. Domains come from the checkpoint or the log's recorded operator/client authentication. For earlier genuine logs, supply
`replay --chain-id <id> --vault <address>`; recovery uses the configured deployment and refuses a
conflicting recorded domain. Logs containing withdrawal release records require `replay --signer <address>` from an independent
trust source. Both immediate and deferred completions must match the pending approval and carry a
valid withdrawal signature for the trusted deployment before replay consumes it. The signer flag
also pins checkpoint signers to an address
obtained from an independently verified attestation or the vault. Protocol reference fixtures
contain placeholder enclave signatures; fixture tests check their roots/events separately, while `engine replay` rejects their placeholders.

A vault deposit credits the trading account its event names: the caller for `deposit`, or the
account `depositFor` names. The vault refuses deposits under 10 USDC or over 1e9 USDC. A trading
account must be able to sign requests with its own key, since the engine accepts no contract
signatures: a contract wallet funds a key that signs for it through `depositFor`, and a balance
credited to a contract account can be recovered only through the exit (#48).

The adapter trusts a strict majority of the configured independent RPC providers. The chain of
block hashes up to the agreed finality block and the vault's agreed deposit count further bound
those answers. A compromised provider cannot fabricate a credited deposit by itself. It takes the
vault, its token, and the finality tag from the host-supplied config, and keeps its cursor on host storage. Reorganisations
of Arbitrum One that undo a block after its batch reached Ethereum, the finality it waits for, are
outside its threat model.

Endpoints: `GET /v1/health`, `GET /v1/attestation`, `GET /v1/markets`, `GET /v1/house`,
`GET /v1/accounts/{address}`, `GET /v1/accounts/{address}/nonce`,
`GET /v1/accounts/{address}/history`, `GET /v1/accounts/{address}/proof`,
`POST /v1/orders`, `POST /v1/orders/cancel`,
`POST /v1/withdrawals` (mock custody only until withdrawals are paid on-chain), `POST /v1/admin`
(the operator's), `POST /v1/dev/deposit` (mock custody only), `GET /v1/stream` (WebSocket), and
`GET /v1/archive/{file}` (`manifest.jsonl` or a segment it lists).
A client over its rate gets 429 with `Retry-After`, a body over `api.max_body_bytes` gets 413, a
body not received within `api.read_timeout` gets 408, and a subscriber over `api.max_subscribers`
gets 503. A connection over a connection cap or over its client's connection rate is closed before
its TLS handshake. A client is its socket peer IP, with IPv6 counted by its /64; forwarding headers
are ignored.

`/v1/stream` is live from now by default. `?from=<seq>` starts at the first message of `seq`
(`from=0` is the whole log), and `?after=<seq>:<index>` resumes after the `offset` of the last
message received. Each message carries its `offset`: the command of a seq is index 0, then its
withdrawals, then any checkpoint. A command logged before events were recorded in the log streams
without its `events` key.

The log is a directory (`node.log_dir`) of segment files named for their first seq, plus
`manifest.jsonl`, which gets one line per completed segment with its seq range, size, and SHA-256.
The node loads the manifest at recovery, after checking it against the segment files, and then holds
it in memory, adding each segment it completes. The archive serves that copy, not the file on disk.
Backfill reads up to 256 messages at a time and keeps them until the subscriber takes them. It
checks a completed segment against the manifest in one pass the first time it reads from it, then
checks each chunk it reads, and refuses any other segment the manifest does not list. The segment
still being written is read from disk unchecked, and replay against the signed checkpoints is the
check for it. Backfill yields nothing past the last entry the sequencer has flushed, which the
writer publishes with its manifest after each batch, so no part of a batch still being written
reaches a subscriber from disk. While it reads a batch, a subscriber holds one of 16 permits and one
1 MiB chunk of a segment. If a new segment cannot be created, the current one keeps growing and the
node warns (`log_rotation_failed`) until it can. A log written as one file before segments existed
continues as the first segment: move it to `<log_dir>/00000000000000000000.jsonl`. Every segment is
kept, since recovery replays the log from genesis. It replays as it reads, one segment and one line
at a time, so its memory does not grow with the log, and replays nothing of a completed segment
before checking it against the manifest; `engine replay` reads the log the same way.

With `auth = "eip712"`, each order, cancel, and withdrawal body carries
`"auth": { "nonce": <next nonce>, "expires_at": <ms>, "signature": "0x…" }`: the account's EIP-712
signature under the domain of `custody.vault` on `custody.chain_id` (see `engine-core`'s
`sign_request` example). `expires_at` is the latest time, in milliseconds since the Unix epoch, at
which the request itself is accepted; a limit or stop order it places rests until it fills or is
cancelled. A missing or invalid signature is refused with 401, an expired request with 400, a
signed request from an account that has never been credited with 403, and a nonce other than the
account's next with 409 and the `expected` nonce; none of these reaches the log. A request
refused with 403 or 409 uses no nonce, so it stays valid until its `expires_at`: resubmit that same
request once its nonce is next, or let it expire before signing different content at that nonce.
An expiry more than five minutes ahead is sequenced and rejected with `expiry_too_far`, which uses
the nonce. Two requests that race for one nonce are both sequenced, and the engine rejects the
second with `invalid_nonce`.

The first start records `auth`, and under `eip712` the custody chain id and vault, in the log;
changing any of them needs a fresh log, removed while the node is stopped. The history index in
`history.dir` then no longer matches the log, so the node rebuilds it from the new one.

The first start also sequences `[genesis]`, all or nothing. Loading the config refuses a genesis
value the engine would reject, or a market id, market symbol, or benchmark name listed twice.
Recovery refuses (`log_recovery_failed`) a log whose genesis did not apply in full: one that stops
short of the checkpoint after the house capital credit, as a crash during the first start leaves
it, or one that holds a genesis command the engine rejected, or genesis run again before its first
run finished, as earlier builds left it. When nothing follows genesis in the log, remove
`node.log_dir` and `history.dir` while the node is stopped and start again; when commands follow,
keep the log for an operator to examine.

`/v1/accounts/{address}/history` returns `{"events": [...], "next": ...}`: the account's events,
newest first. Each event carries its `cursor`, `<seq>:<index>`, which is the seq of the command that
produced it and its index among that command's events; then `ts`, the command's sequenced time; then
the event's own fields. `?before=<cursor>` and `?after=<cursor>` bound the page, both exclusive.
Passing a page's `next` as `before` gives the next, older page, until `next` is null. `?kind=` takes
one event `type`: `deposited`, `deposit_rejected`, `withdrawal_approved`, `withdrawal_rejected`,
`order_accepted`, `order_rejected`, `order_cancelled`, `fill`, `close_out`, `bad_debt`, or
`financing`. `?limit=` is 1 to 200, 50 by default. A page is one search of a SQLite index that reads
at most `limit + 1` entries, and at most 16 pages are read at once. Until the index has taken every
command the log held when the node started, the route answers 503 (`history is catching up with the
log`). History is public, like the log.

The history index is a SQLite file in `history.dir`, derived from the log alone. A supervised task
follows the log as a stream subscriber does, from disk and then live, and writes on its own thread,
so the sequencer never waits for it; once caught up, history trails the log by the commands queued
for the writer, and reads from disk whatever it falls behind the live log by. Each write stores the
account events of a batch of commands, keyed by cursor, and the last command taken, in one
transaction. Events whose command is older than `history.retention`, counted back from the newest
command in the index, leave it; the log keeps them. The node reads its log once at startup,
replaying it as it reads, and before recovery writes anything, the index checks that this read held
the last command the index took, unchanged, and resumes after it. Since the index is derived from the log,
an index that fails that check is discarded with a Warning (`history_index_discarded`) and rebuilt
from the log's segments, and the route answers 503 until the rebuild has taken the log's tip. The
log ends before that command after a power loss, since neither the log nor the index syncs every
write, or after the log was rolled back or truncated; it holds another command there after it was
replaced. An index is rebuilt the same way when SQLite's integrity check finds its file damaged,
when the file holds another schema, or when `history.retention` has changed since its events were
pruned. The node refuses to start (`history_recovery_failed`) only when it cannot open a fresh
index. The check compares that one command, so a log forged to end with it passes; a host able to
forge the log can rewrite the index as well. SQLite opens the file in defensive mode, with triggers
and views off, its schema untrusted, and cell sizes checked, and every write and read refuses,
inside its own transaction, a schema this code did not create. Removing `history.dir` while the node
is stopped rebuilds the index too. Commands logged before events were recorded add nothing to
history.

`engine admin` reads the node config (`--config`, or `CONFIG_PATH`) for the operator's public key
and the custody chain id and vault, refuses a key that is not the operator's, takes the operator's
next nonce from `GET /v1/accounts/{operator address}/nonce`, and posts
`{ "command": <admin command>, "auth": { "nonce", "expires_at", "signature" } }` to
`POST /v1/admin` on `--node` (default `https://127.0.0.1:8080`). It connects over TLS 1.3 only when
the node's certificate key hashes to `--pin`, the attestation's `tls_public_key_sha256`, so a client
that has checked the quote reaches the attested enclave. The signature is EIP-712 over the
command's JSON, its nonce, and an expiry two and a half minutes ahead, under the same domain as
client requests. It prints the signed request and the events the engine produced, and fails when
the engine answers `admin_rejected`. The node refuses a command only it sequences (client auth, the
operator, protocol activation) with 400, a signature that is not the operator's with 401, an
expired request with 400, and a nonce other than the operator's next with 409, and none of these
reaches the log. The operator may list a market or replace its spec and calendar, replace the
protocol parameters, set a benchmark rate, set a stress floor, which raises a market's stress level
but never lowers it below the automatic one, and make one market or every market reduce-only, where
new risk is refused and trades that reduce or close a position still fill. The engine also refuses
parameters whose stress and toxicity spreads would price a closing trade at the price itself. Once
any deposit or house capital is credited, the engine refuses an unsigned operator command, so only
genesis, which credits its house capital last, runs unsigned. The first start under protocol `V8`
records `operator.public_key`, with the custody chain id and vault, in the log; changing it needs a
fresh log, and `engine replay` prints the operator a log records. A node whose `tee.provider` is not
`mock` refuses to start with an operator key that `config/test-keys.toml` lists, in either SEC1
encoding: the repository publishes those keys' private keys for local nodes.

`/v1/accounts/{address}/proof` returns, against the state root of the last checkpoint, whose `seq`
and `root` it names: the account's leaf (balance, withdrawal nonce, toxicity score with, from `V11`,
its fraction of a bp and the notional of history behind it, and number of open positions) with its sibling hashes; each position (its market, qty, and entry) with its leaf's
siblings; and the leaf and siblings of each market it holds a position in. Check `root` against that
checkpoint's signed entry, then check the proof with `engine-core`'s `AccountProof::verify` or its
`verify_proof` example. An account credited since the last checkpoint is not in its root yet, and
gets 404 until the next one. Each checkpoint logs, at `debug`, how long its root took and how many
leaves and tree nodes it hashed.

The operator listener, `metrics.listen` (`127.0.0.1:9464` in `config/dev.toml`), serves
`GET /metrics` in Prometheus text format. `ops/grafana/engine-node.json` is a Grafana dashboard
over it, showing sequencer health and the quote freshness of each venue the config feeds (one
that has not quoted reads as stale from the node's start), and
`ops/prometheus/engine-node-rules.yml` holds alerting rules for the operator's Prometheus that page
on what the node cannot page itself: a scrape that stops, and alert pages that fail or are dropped.

The node also watches itself every second. An open market that a configured feed prices raises the
Critical `market_no_price` once it has had no usable price for `alerts.no_price_after` (its own
index, or for a market quoted in another currency the FX index it converts at), and a
sequencer that answers no sample for as long raises `sequencer_stalled`. A market that no configured
feed prices, such as the non-crypto markets of `config/dev.toml` with `feeds.provider = "venues"`,
cannot trade; the node names each such market once, in its own Warning `market_unfed`, and never
pages for them.

An alert is a log line at `error` (Critical) or `warn` (Warning) carrying `alert = "<name>"`; a line
carrying `alert_resolved = "<name>"` and the same fields ends it. With `[alerts.critical]` or
`[alerts.warning]` configured, the node posts each episode's start and end as JSON to that
severity's webhook:

```json
{
  "status": "firing",
  "severity": "critical",
  "alert": "market_no_price",
  "key": "market_no_price{market=1}",
  "summary": "BTC-USD has had no usable price for 5s: only 0 fresh sources, need 2",
  "fields": { "market": "1" },
  "context": "",
  "ts_ms": 1792503583932
}
```

`status` is `firing` or `resolved`, and `key` is the same for both, so a receiver can match a
resolve to its alert and drop duplicates. `context` names the spans the line was logged in, such
as `command{id=42}`. Repeats of a key do not page again until it has been quiet for
`alerts.episode_quiet`. Each severity's webhook delivers its oldest pending page first. The
sequencer only tries a bounded channel send; disk writes and webhook retries run on supervised
workers. A page is committed with SQLite `synchronous=FULL` before its first POST, and removed
only after 2xx acceptance, permanent refusal (redirects and 4xx except 408/429), or its age/retry
budget expires. Retries start at 1s and double to a minute. Each run retries for at most an hour;
`alerts.outbox.max_age` bounds the entire lifetime, including queueing and time across restarts.
Shutdown leaves committed pending pages for the next start, in their original route order.

`alerts.outbox` is required: `dir` must be a sibling of `node.log_dir` on the private data disk,
`max_pages` bounds retained rows across both routes, `max_bytes` bounds their combined serialized
payload bytes, and `max_age` bounds their age. Dev configs use 2048 pages, 16 MiB, and one hour;
one serialized row is additionally capped at 64 KiB. SQLite reuses freed pages. Its database file is capped at
`ceil(max_bytes / page_size) + 4 * max_pages + 16` SQLite pages, including indexes and retained
free pages; the temporary rollback journal requires
additional space for at most those database pages plus journal record headers. No outbox file enters the public
log archive. Both directories must resolve to distinct siblings. Keep one node per outbox; an
exclusive SQLite lock refuses another owner. Back up and restore the outbox with the log.

Capacity refusal leaves older pages intact, counts `engine_alert_outbox_overflows_total`, and
forgets the refused episode so a repeat can retry admission. The external Prometheus rule
`EngineAlertOutboxOverflow` pages a separate receiver; line-queue drops and permanent delivery
failures have their own rules. Storage errors stop the supervised node; corrupt data, changed
bounds that exclude retained rows, or removing a route with pending pages refuse startup without
discarding pages. Operators must retain the route/bounds until those pages drain.

Delivery is at least once: a crash after receiver acceptance but before durable deletion may
repeat the same page. Receivers can deduplicate `(key, status, ts_ms)`; `key` alone also joins an
alert to its resolve and later episodes. Episode suppression remains in memory. An alert line
still waiting for the router when the process is killed can be lost before becoming a durable
page. SQLite schema/integrity checks and page bounds reject damaged input; they do not detect
host deletion or rollback. Integrity-protected, rollback-resistant storage is still required to
bound a hostile host. Across restarts, retention uses host wall time; a backward clock refuses
future-dated rows, but trusted time is not sealed with the outbox. The webhook URL remains a
host-supplied credential, and HTTP client logs stay at `warn` regardless of `logging.level`.

## Configuration

`CONFIG_PATH`: path to the node config file, used when `run` or `admin` is given no config flag.

`OPERATOR_KEY`: the operator's private key in hex, for `admin` when no `--key-file` is given.

`OPERATOR_KEY_FILE`: a file holding the operator's private key in hex, for `admin`.

The alert webhook URLs are credentials, so config names the environment variable that holds each
(`alerts.critical.url_env`, `alerts.warning.url_env`) rather than the URL. A URL must be HTTPS
unless it is HTTP to a loopback address, and the node refuses to start when a named variable is
unset. A node whose TEE or custody provider is not `mock` refuses to start without
`[alerts.critical]`, since its Critical alerts would otherwise page no one.

### Dated benchmark worker

Append `config/benchmark-publishers.toml` to a complete node config to enable publisher ingestion.
On a fresh node, omit fixed `genesis.benchmarks` entries. They are manual rates effective on the
startup date and can take precedence over delayed publisher observations; production genesis
leaves them unset.
The sources cover SOFR (NY Fed), ESTR (ECB), SONIA (Bank of England), AONIA (RBA), and TONA (BOJ).
Endpoints use the official publishers' certificate chains against compiled-in roots; redirects and
proxies are disabled. Loopback HTTP endpoints are accepted only with a mock TEE.

Start the node with this config, verify its attestation, then run a separate operator service:

```sh
engine admin --config /path/to/node.toml --pin <attested-tls-hash> sync-benchmarks --watch
```

Give `OPERATOR_KEY_FILE` to that worker alone. The node retains only the operator public key.
Without `--watch`, the command fetches and signs one snapshot. The config's interval must be at
most 24 hours; the example polls hourly to retry failures and pick up publications before rollover.
Watch mode also schedules a fetch before the next rollover, reserving every publisher timeout
and the three bounded calls to the node. Scheduling refuses unrepresentable time budgets.
The worker shares the operator nonce with other admin clients and reads it before fetching each
snapshot. A signed admin change during the fetch invalidates that nonce; the old attempt cannot
install its observations under the new schedule. Watch mode retries a refused or failed submission
within the shorter of the configured interval and the node-call timeout.

With `[benchmarks]` present, each startup refuses ordinary sequencing until an authorized snapshot
has attempted every listed benchmark. Trusted time still gates genesis, activation, and the snapshot.
If trusted-time admission halts across an unpaid rollover, recovery waits for another complete signed
snapshot before ordinary or queued commands can charge catch-up. Publisher failures still use the
missing/stale policy below. A brief halt with no unpaid rollover resumes without an extra snapshot.
An accepted change to the rollover timezone or time also requires a fresh complete attempt.
Ordinary sequencing continues before the newly selected rollover; if it becomes due first,
financing waits for the attempt. Rejected or unchanged schedules do not re-arm this requirement.
The worker follows compact durable revisions through `GET /v1/benchmarks/schedule?after=<revision>`;
changes trigger an immediate fetch and re-arm preflight, even inside the new fetch window.
Watches share the API subscriber cap, wait within the node-call timeout, and refresh on reconnection.
A recovered log with a rollover schedule must already record client auth, the operator, and toxicity
notional weighting. Publisher synchronization refuses older logs missing those activation prerequisites
before writing any command; migrate such logs separately before configuring the publisher worker.
The public `GET /v1/benchmarks` returns the next rollover and retained rate history, so the worker
fetches histories covering downtime before a missed rollover is charged. A restart regenerates the
TLS key: verify its attestation and restart the worker with the new pin before sequencing resumes.

Each signed snapshot logs effective dates and annual fractions, retaining sub-basis-point precision.
Each rollover selects the latest observation on or before its local date. Weekdays mean Monday to
Friday; an observation older than five weekdays raises `benchmark_rate_stale` at Critical severity
and still applies. With no observation or inherited rate, `benchmark_rate_missing` is Critical and
that benchmark accrues no financing. An inherited undated rate still applies and raises
`benchmark_rate_undated` until a dated observation covers the rollover. Applying that fallback
resolves the independent missing-rate incident immediately. Fetch errors retain previously
sequenced history; the separate worker logs `benchmark_fetch_failed` and retries at its next interval.
When an accepted market update removes the final use of a benchmark, its alerts resolve after that
command's rollover events, including incidents raised before the node restarted.

Monitor both processes. A configured node waiting for its worker logs `benchmark_sync_pending`;
the worker must run after every restart. SONIA's free database has the publisher's publication delay.
For long outages, size `api.max_body_bytes` for the signed history (the protocol allows 16,384
observations per snapshot); a rejected or lost HTTP submission leaves startup synchronization pending.
After catch-up, histories retain predecessors needed by unpaid rollovers and any legal calendar
change. Older observations are removed only after a retained predecessor supersedes them for all
those dates. Previously charged dates are not billed again when a publisher revises a rate.

Protocol activation is one-way: a build predating V18 cannot replay a log that activated it.
Run the deterministic dev scenario with
`cargo test -p engine-node dev_scenario_benchmark_worker` and protocol coverage with
`cargo test -p engine-core --test benchmarks --test benchmark_retention`; both run in the existing
crate CI lanes.

## Testing

```bash
cargo test -p engine-node

# Alert outbox SIGKILL/restart scenario, offline with a loopback receiver
./scripts/dev-alert-outbox.sh

# The trusted time dev scenario, offline against loopback Roughtime servers
cargo test -p engine-node trusted_time::tests::scenario

# Dev scenario: a mock feed stops, the market goes stale, and a local webhook gets one page
./scripts/dev-metrics-alerts.sh
```

## Authors

The monkeytrading contributors

## License

Licensed under the Apache License, Version 2.0; see `LICENSE`.

## Sequencer CPU binding

The `engine run` entry point reserves the last CPU in its initial Linux affinity mask before
creating the Tokio runtime. The main thread, runtime workers and blocking workers inherit a mask
excluding that CPU; the sequencer alone binds back to it on its dedicated thread. Linux startup
requires at least two allowed CPUs and refuses failed mask reads or updates. The calling thread's
original mask is restored after runtime shutdown. Other platforms retain a dedicated thread and
log that strict CPU pinning requires Linux.

The public `run_dedicated` entry point owns this runtime setup. The asynchronous `run` entry point
is retained for embedded/test callers that supply their own runtime; it does not reserve a CPU
away from that runtime's existing workers.

Node-owned thread exclusion does not isolate the CPU from unrelated host processes or kernel
work. Production scheduler/cgroup isolation must keep competing host work off the logged CPU.
macOS development measurements provide no Linux CPU-isolation evidence. Linux CI checks worker
mask inheritance, sequencer binding, calling-mask restoration and single-CPU refusal.

The sequencer owns an OS thread and reports its result through a supervised channel. Dropping its
supervisor requests cancellation at an async boundary, including startup-error and drain-timeout
paths. Its affinity cannot leak into Tokio's reusable blocking pool. Runtime teardown uses the
non-waiting shutdown path after node/alert draining, so a blocked storage syscall cannot extend
that drain deadline through runtime destruction. On a fatal drain error the CLI returns an error
and exits, terminating any remaining OS threads. An embedded caller must likewise terminate its
process after an uncancellable drain failure; a blocked thread is not safe to restart beside.
