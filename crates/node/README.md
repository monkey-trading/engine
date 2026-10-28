# engine-node

## Overview

The node that runs `engine-core` inside a TEE (an Intel TDX or AMD SEV-SNP confidential VM). It
sequences every command into a public, append-only log, signs state-root checkpoints with an
attested enclave key, serves the client API, streams the log live, indexes each account's history
from the log, and talks to custody and price feeds. For its operator it serves Prometheus metrics
and posts alerts to webhooks. The same binary replays a published log and verifies its checkpoints.

## Features

- Signed eligibility and fail-closed API/custody screening from V24, with public refusal reasons
  and reserved, uncredited sanctioned deposits
- Dated publisher ingestion with a separate signing worker, startup synchronization, and per-date
  rollover catch-up under V18
- Single-owner sequencer on a dedicated OS thread: commands are logged and synced to disk before any
  client sees their effect
- Genesis all or nothing: the engine checks every genesis command at config load and again before
  any is written, and recovery refuses a log whose genesis stopped short of its closing checkpoint
  or is not the config's, and one whose genesis no current config can give (rules newer than the
  log refuse it at load), which needs a fresh log
- Signed checkpoints and withdrawals, bound to the chain id and vault, from an attested key
- Strict checkpoint signature verification in replay and recovery, with explicit domains for early logs
- Command-origin seals for new entries, checked against the configured deployment and recovery signer;
  historical unsigned commands require an authorized covering checkpoint before writable recovery. A
  command after the last authenticated checkpoint that the current enclave key did not seal stops
  recovery for operator repair. Each hardware enclave start generates a new key, and keys that
  survive a restart are #28's: until then a hardware node cannot restart, after a crash or a drained
  shutdown, because the vault's signer must be the enclave's key and the old key's commands and
  checkpoints after the last one the vault accepted cannot be authenticated under a new key. With a
  key that survives restarts, a drained shutdown ends the log at a checkpoint and restarts normally
- Optional checkpoint relaying, signed-parent vault commitments, startup rollback checks, and collateral
  reconciliation that pauses withdrawal signatures while trading continues
- Attestation through Linux configfs-tsm, covering TDX and SEV-SNP, or a clearly marked mock: the
  report data is the SHA-256 of a served bundle of the enclave address, the config file's SHA-256,
  and the TLS key's hash
- A `production` cargo feature that refuses mock TEE, custody, and feed providers at config load
- Trusted time: every stamp is checked against signed Roughtime readings that independent servers
  agree on, and nothing, genesis included, is stamped before the first; sequencing halts with a
  Critical alert when the host clock is beyond the drift bound, or 60 seconds after the last
  sample, and resumes once a sample is within the bound
- REST API for orders, cancels, withdrawals, and reads, plus a WebSocket stream of the log
- Orders, cancels, and withdrawals signed by the account (EIP-712), checked before they are
  sequenced; before V24, `auth = "none"` trusts the account field with mock TEE and custody; V24
  client actions and eligibility declarations require signatures in every mode
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
- Optional recovery caches (`[snapshots]`): restart from the newest signed checkpoint cache and the
  log suffix after it, with history and the segment catalog in proof-checked databases
- `engine admin` to sign an admin command with the operator's key, read from a file or
  `OPERATOR_KEY`, and submit it over TLS pinned to the node's attested key; the node holds only the
  operator's public key, recorded in the log, and refuses a request before the log unless it carries
  the operator's signature, next nonce, and an unexpired expiry
- Proofs of an account's balance and positions, and of its positions' marks, against the last
  checkpoint's state root, from protocol `V9`
- MONKEY emissions from protocol `V19`: `engine admin init-emissions` records the schedule once,
  and `GET /v1/accounts/{address}/emissions` serves an account's cumulative entitlement with its proof
  against the last checkpoint's root, for the vault's `claimEmissions`; with `[checkpoints]`, the
  node reads the period budget the vault's MONKEY cap was derived from, refuses a schedule with any
  other, and refuses to start on a log that records one
- MONKEY staking from protocol `V25`: `engine admin init-staking` or a genesis `staking` entry
  records the liquidity floor, epoch length, start, and unstake cooldown once; each epoch the
  engine distributes house NAV above the floor to stakers pro-rata, and
  `GET /v1/accounts/{address}/distributions` serves an account's cumulative USDC distribution with
  its proof against the last checkpoint's root, for the vault's `claimDistribution`; checkpoint
  reconciliation counts the USDC the vault has paid in distributions against the ledger
- Supervised tasks: any task stopping stops the node, and SIGTERM drains to a final checkpoint
- Live crypto prices from Coinbase, Kraken, and Bitstamp over WebSocket: one supervised task per
  venue connection, quotes stamped with the venue's own time, and reconnects with backoff
- Live FX, index and commodity futures, and US share prices from market-data vendors, two per
  market: Massive (FX, futures, and stocks feeds) and TraderMade over WebSocket with their API
  keys, and Databento's live gateway over TCP with challenge-response authentication; quotes for
  a futures contract carry its name, and a feed holds back quotes stamped or sent while their
  market is closed and quotes for a contract the market does not take
- Alerts for refused venue messages (at most one a minute per venue, counting the rest, with any API
  key redacted), venue outages, symbols with no fresh quote while their market is open, and venue
  times well ahead of the host clock
- A `house_deficit` alert when house NAV falls below zero, at most one a minute while it keeps
  crossing zero
- EVM custody: a strict majority of at least three RPC providers admits each deposit; from V20,
  sequencer confirmation credits trading collateral, Ethereum finality unlocks withdrawal principal,
  and canonical reversals debit once with a Critical alert. From V25, the same poll mirrors the
  staking contract's finalised `Staked`/`UnstakeCompleted` events (`custody.evm.staking`) into
  `Stake`/`Unstake` commands once each, holding them until `InitStaking` has applied; its event
  reads narrow after an RPC size or timeout failure on a range of their own, apart from deposits'
- Mock custody and a mock price feed for local development
- Prometheus metrics on a separate operator address, from a registry the node owns, with no
  per-account labels or values
- Alert routing: Critical and Warning alert lines are posted as JSON to a webhook per severity,
  once when an episode starts and once when it is resolved

## Repository Structure

- `src/main.rs` — the `engine` binary: `run`, `replay`, `verify-attestation`, and `admin`
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
- `src/node/preflight/unscanned.rs` — unprovable prelaunch anchors refuse writable startup before changing log or history
- `src/node/preflight/tests/origin.rs` — integrity refusal preserves absent, corrupt, and WAL-backed history
- `src/telemetry.rs` — the log subscriber, which feeds alert lines to the router, and the metrics
  registry, set up before the node runs
- `src/sequencer/mod.rs` — the sequencer, checkpoints, and metrics
- `src/sequencer/messages.rs` — sequencer requests, stream messages, and signed withdrawals
- `src/sequencer/checkpoints.rs` — checkpoint creation, custody reconciliation, and signature release
- `src/sequencer/payouts.rs` — durable unpaid delivery, finalized claim evidence, epoch reissues, and reissue requests
- `src/sequencer/payout_schedule.rs` — complete unpaid approval order and bounded epoch and requested reissue scheduling
- `src/sequencer/payout_tests.rs` — release order across claim pages, durability, restart, expiry without reissue, and rotation
- `src/sequencer/payout_tests/` — paginated replacement signature order, paused claim acknowledgement, and sealed approval recovery
- `src/sequencer/withdrawal_scenario/` — actual Anvil/TLS/stream/custody crash and FIFO scenarios with local signed time
- `src/withdrawals.rs` — vault EIP-712 digests and exact approval inclusion verification
- `src/api/withdrawal_queue.rs` — public FIFO pagination and account queue positions
- `src/api/reissue.rs` — a holder's request to sign an expired approval again
- `src/node/signer_history.rs` — finalized accepted roots and immutable signer epochs before recovery writes
- `src/replay_audit.rs` and `src/replay_audit/` — read-only custody-backed historical signer audits and tests
- `src/log/segments/durability_tests.rs` — failed-fsync stream publication regression
- `src/test_support/written.rs` — shared in-memory test output capture
- `src/sequencer/checkpoints/accounting_tests.rs` — live/recovery accounting oracles and gated 100k-account capture scaling over migrated credits and fresh holds
- `src/sequencer/checkpoint_tests.rs` — confirmed-prefix signature gating and restart tests
- `src/sequencer/checkpoint_tests/unvalidated.rs` — worker observations preserve signing holds until collateral is compared, and an unproven finalized prefix pauses withdrawals until coverage is proven
- `src/sequencer/checkpoint_tests/bootstrap.rs` — recovered empty anchors reconcile while nonempty unscanned checkpoints wait
- `src/sequencer/fixture_tests.rs` — test-only signing of copies of placeholder protocol fixtures,
  and logs whose genesis an older protocol applied but no current config can give
- `src/sequencer/recovery.rs` — pure recovery validation and exact authenticated checkpoint-tail restoration
- `src/sequencer/origin.rs` — command-origin coverage shared by replay and writable recovery
- `src/sequencer/replay.rs` — log replay, which recovery and `engine replay` share
- `src/sequencer/replay/snapshot.rs` — replay restored from a verified state snapshot
- `src/sequencer/replay/authorities.rs` — accepted historical signers for checkpoints a retired key signed
- `src/sequencer/legacy.rs` — verified legacy activation inputs and pre-writer signature eligibility
- `src/sequencer/legacy/approvals.rs` — authenticated approval prefixes, bounded receipt proofs and per-obligation release authorization
- `src/sequencer/legacy/approvals/tests.rs` — exact proof retention, terminal/mismatched receipts and uncredited history
- `src/sequencer/legacy/tests.rs` — ledger tampering, unrelated funds, deferred reservations and historical replay provenance
- `src/sequencer/legacy/tests/activated.rs` — activated recovery refusal, funding-prefix release and restart/nonce isolation
- `src/sequencer/snapshots.rs` — the checkpoint capture the sequencer hands the history owner
- `src/sequencer/snapshots/paired_recovery.rs` — paired-cache recovery: cache, history head, and
  catalog boundary checked together before the suffix replays
- `src/sequencer/snapshots/context_tests.rs` — exact cached custody work and required context fields
- `src/sequencer/snapshots/custody_tests.rs` — cached canonical deposit coverage and refused legacy provenance
- `src/sequencer/history_rebuild.rs` — history rebuilt from the log for migration and repair, bound
  to the replayed command prefix
- `src/snapshot/mod.rs` — signed snapshot envelopes, size limits, atomic publication, and retention
- `src/snapshot/tests.rs` — snapshot authentication, deployment binding, retention, and corruption
- `src/snapshot/context.rs` — required recovery accounting, signed base witnesses, and concrete checkpoint work
- `src/snapshot/head.rs` — authenticated complete history-head records and bounded metadata publication
- `src/snapshot/head/tests.rs` — head provenance, replay-prefix matching, and interrupted-publication regressions
- `src/snapshot/paired.rs` — the record binding one checkpoint's cache, history head, and catalog boundary
- `src/snapshot/sealing.rs` — the bounded checkpoint capture and its completion on the history owner
- `src/snapshot/payloads.rs` — authenticated cache payloads reused while their files are unchanged
- `src/snapshot/pins.rs` — root leases for every version a retained cache or head names
- `src/snapshot/references.rs` — the verified inventory of retained caches and heads that authorizes
  generation removal
- `src/snapshot/lifecycle.rs` — the per-node gate over generation allocation, publication, and pruning
- `src/snapshot/catalog_heads.rs` — signed catalog-head files, their retention, and their root leases
- `src/snapshot/generations/` — history and catalog database generations: allocation, the
  retirement journal (`disk.rs`), removal, and their tests
- `src/authenticated/` — packed immutable proof storage, checked JMT reads/updates, leases, and reclamation
- `src/log/catalog.rs` — proof-checked dense segment metadata and exact checkpoint locators
- `src/log/catalog_access.rs` — read-only proof access to the catalog store
- `src/log/catalog_commit.rs` — staged catalog writes that become visible in one commit
- `src/log/catalog_head.rs` — signed catalog heads and their verification
- `src/log/catalog_owner.rs` — the single catalog writer: commits, maintenance, and head publication
- `src/log/catalog_prefix.rs` — the running catalog prefix commitment
- `src/log/catalog_seal.rs` — the exact checkpoint boundary captured for a paired cache
- `src/log/commitment.rs` — commitments to the complete typed command/event prefix
- `src/log/suffix.rs` — recovery that replays only the suffix after a catalog-located checkpoint
- `src/log/writer.rs` — appends, flushes, segment rotation, catalog folds, and catalog collection
- `src/log/view.rs` — manifest accessors over full listings and the lazy catalog
- `src/log/archive.rs` — completed-segment bytes for the archive, checked against the catalog
- `src/log/append_integrity.rs` — reopening the active segment for append only after its prefix rechecks
- `src/log/authenticated_tests/` — catalog, suffix, continuation, active-segment, stream, and
  publication tests
- `src/node/paired_tests/` — paired recovery through node startup: restart and fallback, crash
  recovery, the restart dev scenario (`restart.rs`), the on-chain anchor preflight (`anchor.rs`),
  custody receipts and their history across a cache restart (`custody.rs`), publication faults,
  public history pages, and the harness
- `src/sequencer/required.rs` — client auth, the operator, protocol activation, and genesis, which
  wait for trusted time
- `src/sequencer/catch_up.rs` — the `Tick`s that finish a V33 catch-up before a command
- `src/sequencer/genesis.rs` — the genesis commands, the check that the engine accepts every one
  before any is written, and how far a replayed log got through genesis and which genesis it ran
- `src/sequencer/alerts.rs` — the alerts the sequencer raises: engine rejections an operator must
  look at, benchmarks with no rate, and house NAV below zero
- `src/sequencer/alerts/retirement_tests.rs` — benchmark retirement, catalogue changes, and resolution state
- `src/sequencer/alerts/retirement_webhook_tests.rs` — retirement pages through the durable webhook router
- `src/sequencer/alerts/undated_tests.rs` — scalar fallback, independent incidents, and durable webhook recovery
- `src/sequencer/benchmark_schedule.rs` — changed rollover notifications and publisher synchronization when due
- `src/sequencer/tests/benchmark_schedule.rs` — schedule changes, durable watches, and due-time synchronization
- `src/sequencer/tests/mod.rs` — sequencer, activation, recovery, and replay tests
- `src/sequencer/tests/replay.rs` — root, sequence, and pre-event log replay tests
- `src/sequencer/tests/catch_up.rs` — a command after an outage of more than one catch-up batch
- `src/sequencer/tests/benchmarks.rs` — publisher startup prerequisites and legacy-log refusal
- `src/sequencer/tests/benchmark_recovery.rs` — publisher synchronization after trusted-time halts
- `src/sequencer/tests/genesis.rs` — genesis tests: a rejected genesis writes nothing, and a log
  whose genesis did not apply in full, is not the config's, or is one no current config can give,
  is refused at recovery
- `src/sequencer/tests/operator.rs` — recovery, activation, and replay with the operator the log
  records
- `src/sequencer/batch_tests.rs` — batching: a batch ends after `MAX_BATCH` requests of any kind
- `src/sequencer/margin_tests.rs` — the margin and solvency dev scenario on the dev genesis (a
  close-out after the rollover's financing, and a withdrawal limited at a stale market's last
  mark), and a genesis with no house capital that trades and alerts on a house deficit
- `src/sequencer/metrics_tests.rs` — the sequencer's metrics and alert-line tests
- `src/log/mod.rs` — the JSON-lines log format, stream offsets, and the single-file reader
- `src/log/command_seal.rs` — domain-separated canonical command/event seals and predecessor links
- `src/log/segments.rs` — segment files, rotation, the manifest, and the recovery read, a segment
  at a time, with its checks
- `src/log/reader.rs` — reads the log from any offset while it is being written, up to the last
  entry the writer has flushed
- `src/log/checked.rs` — reads a completed segment in chunks checked against its listing
- `src/log/tests/mod.rs` — log format, segment, and writer tests
- `src/log/tests/durability.rs` — writer durability and rotation: per-batch sync, reopened segments,
  staging, and opening the log while a writer rotates
- `src/log/tests/reader.rs` — reader tests: offsets, following the writer, flushed entries only,
  and the checks on what it reads
- `src/api/mod.rs` — REST routes, read handlers and the request span
- `src/api/state.rs` — request authentication, nonce checks and durable sequencer round trips
- `src/api/actions.rs` — signed client actions and real-peer access screening
- `src/api/access.rs` — reads of one custody hold, one pending deposit, and an account's signed eligibility
- `src/api/access_tests.rs` — deposit-first declarations, peer provenance and public access refusals
- `src/api/access_tests/admission.rs` — admission before sequencing: never-credited signers declare only against a pending deposit, within per-client and shared budgets; expired requests and exhausted screening capacity refuse without a nonce
- `src/api/access_tests/declarations.rs` — a declaration screened like an order: public refusals that leave its deposit pending until an allowed one is recorded
- `src/access.rs`, `src/access/tests.rs` — bounded signed-source IO and failure-path tests
- `src/api/admin.rs` — the operator's admin route, checked before it is sequenced
- `src/api/distributions.rs` — an account's cumulative USDC distribution with its proof against the
  last checkpoint's root, for the vault's `claimDistribution`
- `src/api/benchmark_schedule.rs` — capped, bounded watches of the durable rollover schedule
- `src/api/benchmark_tests.rs` — deterministic publisher/TLS/API restart and financing dev scenario
- `src/api/benchmark_tests/recovery.rs` — publisher retries during an in-process trusted-time halt
- `src/api/benchmark_tests/retention.rs` — failed publisher refreshes and calendar changes after restart
- `src/api/benchmark_tests/schedule.rs` — actual worker rescheduling, in-flight nonce fencing, and financing
- `src/api/benchmark_tests/schedule/reconnect.rs` — watch-only transport failure during a publisher fetch
- `src/api/admin_tests.rs` — admin route tests, and the operator channel dev scenario through
  `engine admin`
- `src/api/admin_tests/corporate.rs` — signed corporate-action TLS/API dev scenario, including
  prior-close holdings, a dividend followed by a split, restart and public-log replay
- `src/api/stream/mod.rs` — the WebSocket stream: backfill from disk, then live, under the
  subscriber cap, reading what the subscriber sends
- `src/api/stream/tests.rs` — feed, backfill, and subscriber tests
- `src/api/archive.rs` — the archive routes: the manifest and completed segments
- `src/api/auth_tests.rs` — signed-request API tests: signatures, nonces, expiries, refused
  accounts, and racing requests
- `src/api/history.rs` — the account history route: its parameters and limits
- `src/api/proof_tests.rs` — account proof and emission entitlement API tests, including the state
  tree dev scenario: 100k accounts, checkpoint work, a proof checked against its logged root, and
  older logs replayed
- `src/api/tests.rs` — API tests, including the dev scenarios of the resumable stream and archive,
  and of account history, and `Retry-After`
- `src/api/tests/catalog_archive.rs` — archive responses over the lazy catalog
- `src/history/mod.rs` — the history index: which events name an account, the startup check
  against the log and the rebuild when it fails, and the supervised task that follows the log into
  the index
- `src/history/store.rs` — the index's SQLite schema, its writer, its page reader, and its discard
- `src/history/authenticated/` — dense account/kind lists, complete cursor pages, chronological expiry,
  state proofs, and their tests
- `src/history/reader.rs` — legacy/authenticated page dispatch, version leases, and corruption-to-repair signals
- `src/history/reader/tests.rs` — damaged pages withhold the generation; busy storage and bad requests do not
- `src/history/rebuild.rs` — exact typed-prefix witnesses for log-derived migration and repair
- `src/history/startup.rs` — the authenticated history plan: read-only until preflight, then activation
- `src/history/runtime/` — the supervised history owner: indexing, checkpoint sealing, and repair
  (`mod.rs`), completing a capture into the paired cache (`capture.rs`), and signed heads,
  retained-version pins, and collection (`heads.rs`)
- `src/history/tests/mod.rs` — history index tests: pages, filters, retention, following the log,
  and restarts
- `src/history/tests/recovery.rs` — startup checks against the log, readiness to serve, and
  failures
- `src/history/tests/rebuild.rs` — power losses and other damage: an index left behind the log
  resumes, and one ahead of it, contradicting it, damaged, or pruned under another retention is
  rebuilt
- `src/test_support/mod.rs` — the shared test harness, including the capture of alerts and their
  levels
- `src/test_support/events.rs` — one sample of every engine event
- `src/test_support/log_view.rs` — publishes a log directory's manifest for tests
- `src/test_tracing.rs` — the tracing setup the test collectors share: one process-wide
  interested subscriber, so no callsite can be cached uninteresting and a collector never
  silently misses an alert or line
- `src/tee.rs` — the enclave key, signatures, the attestation bundle, and attestation providers
- `src/verify/mod.rs` — attestation verification against a release manifest: the quote, its
  certificate chain, the report data's bundle binding, and the checkpoint-signer check
- `src/verify/report.rs` — the SEV-SNP attestation report layout, parsing, and signature
  verification
- `src/verify/chain.rs` — the VCEK chain up to the pinned vendor roots and the key-distribution
  service client that issues it
- `src/verify/release.rs` — the release manifest of launch digest, policy, minimum TCB, and
  configuration hashes
- `src/verify/roots/` — the vendor root certificates compiled into the verifier
- `src/verify/tests/mod.rs` — verification tests and the loopback dev scenario
- `src/verify/tests/replay_dispatch.rs` — the dev-scenario regression replaying a signed log
  through `engine replay --attestation` with and without a `--signer` pin
- `src/verify/tests/fixtures.rs` — the stand-in vendor chain, forged reports, and the release
  manifest
- `src/verify/tests/revocation.rs` — the root-issued revocation list's serial matching: enforced
  against the intermediate, never the VCEK
- `src/verify/tests/stub.rs` — the key-distribution service stand-ins
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
- `src/config/mod.rs` — the node config schema, loading, and the config file's hash
- `src/config/benchmarks.rs` — publisher endpoints, cadence, timeout, and catalogue coverage
- `src/config/checkpoints.rs` — checkpoint relayer config and validation
- `src/config/feeds.rs` — the `[feeds]` section: the mock feed, the venues and vendors with their
  redacted API keys, and the checks on their endpoints, credentials, and symbols
- `src/config/feeds_tests.rs` — vendor connection config tests
- `src/config/access.rs` — redacted source configuration, recorded identity and freshness bounds
- `src/config/custody.rs` — the `[custody.evm]` section, independent RPC providers, redacted
  URLs, and custody checks
- `src/config/telemetry.rs` — the metrics listener and alert webhook config, and the redacting
  `WebhookUrl`
- `src/config/tests.rs` — config loading and validation tests
- `src/config/mock_key_tests.rs` — published mock-only signer opt-in and hardware refusal regressions
- `src/config/custody_tests.rs` — independent provider configuration checks
- `src/config/production_tests.rs` — the config file's hash, and a production build's refusal of
  mock providers
- `src/config/telemetry_tests.rs` — metrics and alert config tests: the Critical route, the dev
  config's telemetry, and webhook URLs
- `src/config/validation.rs` — the checks a parsed config must also pass, and the production
  build's refusal of mocks
- `src/config/test_keys.rs` — the published test keys from `config/test-keys.toml`, which a node
  with real attestation refuses, and their tests
- `src/checkpoints/mod.rs` — posting, retry, signed-parent commitment checks, and collateral reconciliation
- `src/checkpoints/accounting.rs` — bounded custody totals and held reserves derived from computed engine events
- `src/checkpoints/accounting/access_tests.rs` — actual pending and hold outputs against independent reconciliation totals
- `src/checkpoints/lifecycle.rs` — pinned signer/epoch, cumulative paid amounts, and approval nonce-use evidence
- `src/checkpoints/history.rs` — accepted history from the finalized anchor, the signed-parent chain, and signer epochs
- `src/checkpoints/lifecycle_tests.rs` — paid cash reconciliation, bounded claim polling, and rotated authorities
- `src/checkpoints/reconciliation.rs` — finalized collateral health: shortfalls, deposit totals, payouts, surpluses, and the credit deadline
- `src/checkpoints/rpc/observation.rs` — same-block balance, epoch, paid total, and bounded claim reads
- `src/checkpoints/rpc/authority.rs` — accepted historical signer identities for rotated nonce journals
- `src/checkpoints/rpc/submission/tests/lifecycle.rs` — canonical pinned history and retired journal recovery
- `src/checkpoints/accounting/tests.rs` — cash/reference recomputation oracles and arithmetic boundaries
- `src/checkpoints/accounting/tests/receipts.rs` — receipt cash deltas, adoption, reversals, unpaid approvals and atomic failures
- `src/checkpoints/tests/receipts.rs` — immutable prefix coverage and refusal of unproved aggregate equality
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
- `src/checkpoints/tests/collateral.rs` — shortfall, ledger-difference, surplus, and uncredited-deposit deadline tests
- `src/checkpoints/scenario.rs` — isolated Anvil posting, credited/held reconciliation, tampering, truncation, surplus booking, shortfall, and reorg checks
- `src/checkpoints/scenario/source.rs` — signed synthetic source bound to the scenario vault
- `src/checkpoints/emission_budget.rs` — the vault's MONKEY cap against the emission period budget
- `src/checkpoints/emissions_scenario.rs` — the MONKEY emissions dev scenario on isolated Anvil: pro-rata
  allocation, cancelled periods, a restart across finalisation, the first halving, and claims that mint
  each entitlement once against the posted root
- `src/checkpoints/emissions_scenario/api.rs` — HTTP calls and a startup barrier on the worker's
  real collateral verdict before the scenario drives checkpoint-dependent commands
- `src/checkpoints/emissions_scenario/funding.rs` — actual Vault funding receipts and finalized
  custody progress for the emissions scenario

- `src/checkpoints/staking_scenario.rs` — the MONKEY staking dev scenario on isolated Anvil: staking
  mirrored from the Staking contract, an epoch above the floor paying pro-rata in USDC against the
  posted root, an unstake through the cooldown, and an epoch below the floor paying nothing
- `src/checkpoints/staking_scenario/scenario.rs` — the scenario's chain, deployments, and node,
  which it stops and restarts across an epoch boundary
- `src/checkpoints/staking_scenario/access.rs` — signed loopback screening and published Anvil keys
  for deposit-first eligibility and authenticated orders under the latest protocol
- `src/checkpoints/staking_scenario/funding.rs` — real Vault deposits, screened exact receipts,
  principal finalization, and authenticated custody frontiers
- `src/checkpoints/staking_scenario/claims.rs` — the scenario's on-chain steps: entitlement and
  distribution claims against posted roots, event mirroring, and balance reads
- `src/history/preflight.rs` — immutable history planning before authorized SQLite mutation
- `src/history/preflight/tests.rs` — private WAL resume and malformed geometry byte-preservation regressions
- `src/node/tests/custody_preflight.rs` — local custody refusal preserves log/history across image states
- `src/node/preflight.rs` — chain validation before writable history recovery, and the vault's MONKEY
  cap against the emission budget the log records
- `src/node/preflight/emission_tests.rs` — refused startup on an unreadable cap or another budget
- `src/node/preflight/tests.rs` — refused startup preserves history, WAL, and log bytes
- `src/sequencer/replay/auth_tests.rs` — trusted immediate/deferred completion evidence regressions
- `src/sequencer/replay/accounting_tests.rs` — historical fixture and outstanding-work accounting oracles
- `src/custody/mod.rs` — the custody providers (mock and EVM) and the USDC-to-`Fixed` conversion
- `src/custody/evm.rs` — the deposit watcher: the chain and token check, range reads from blocks
  proven to lead to the tagged final block, the completeness check, and stall alerts
- `src/custody/evm/lifecycle.rs` — confirmed receipt reconciliation, principal finality and the replayed scan frontier
- `src/custody/evm/progress.rs` — confirmed header and log windows read one batch per poll, kept
  Staking catch-up retains at most 66 proven binary-hierarchy anchors independently of deposit pruning,
  plus two private candidate anchors, with O(N log(N/chunk)) header work and one configured chunk of full hashes
  across polls while their endpoint stays canonical
- `src/custody/evm/types.rs` — the canonical deposit, staking-event, finality and reader types
- `src/custody/evm/staking.rs` — staking catch-up proofs anchored independently of deposit progress
- `src/custody/tests/fakes/files.rs` — temporary directories and custody test configurations
- `src/custody/evm/legacy.rs` — independent canonical finality proof before legacy signature redelivery
- `src/custody/staking.rs` — mirroring the staking contract's finalised events into `Stake`/`Unstake`
  commands once each, holding them until `InitStaking` has applied; V27 completed ranges, including
  empty ones, are authenticated by `StakingProgress` and the host cursor cannot advance them
- `src/custody/tests/staking/anchor.rs` — canonical endpoint binding for empty and partial log votes

- `src/custody/evm/lifecycle/tests.rs` — reversal, reinclusion, inconsistent receipt and amount boundaries
- `src/custody/chain.rs` — the chain of block hashes proven to lead to the tagged block, and the
  headers of an interrupted proof, kept apart until it ends there
- `src/custody/watchdog.rs` — alerts for a stale endpoint head, a final block that stops, and
  reads that keep trailing the final block
- `src/custody/stall.rs` — what a poll did, the watcher's run loop, and the alert each kind of
  stall raises
- `src/custody/sizing.rs` — read sizes that adapt to a provider's limits
- `src/custody/credit.rs` — crediting or reserving one deposit once, and the sequencer round trips it needs
- `src/custody/pending.rs` — bounded, backed-off log-derived retries of deposits left pending by source failures
- `src/custody/quorum.rs` — strict-majority reads at common block heights, provider alerts, and
  a Critical alert while quorum is lost
- `src/custody/rpc.rs` — the vault over JSON-RPC, trusting only compiled-in TLS roots, with
  errors scrubbed of the endpoint URL
- `src/custody/cursor.rs` — the deposit cursor kept between restarts
- `src/custody/tests/access.rs` — payer sanctions and holds across restart, cursor loss and source recovery
- `src/custody/tests/access/replacements.rs` — canonical payer screening across startup activation and retry
- `src/sequencer/access_tests.rs` — current access refusals, reconciliation admission and recovered signature release
- `src/sequencer/access.rs` — access-source failure and recovery alert episodes
- `src/custody/tests/mod.rs` — deposit watcher tests against a fake chain and a real sequencer
- `src/custody/tests/alerts.rs` — the watcher's alerts over time, in paused time
- `src/custody/tests/cursor.rs` — the deposit cursor across restarts, and which cursors a log
  refuses
- `src/custody/tests/fakes.rs` — the fake chain, test sequencer, log capture, and fixtures
- `src/custody/tests/quorum.rs` — fabrication, disagreement, lag, outages, and quorum recovery
- `src/custody/tests/quorum_diagnostics.rs` — malformed header exclusion, transport independence, and retained watchdog progress
- `src/custody/tests/rpc.rs` — the RPC adapter's log decoding, deadlines, and URL scrubbing
- `src/custody/tests/confirmed.rs` — confirmed collateral and finality over the real quorum and sequencer
- `src/custody/tests/confirmed/scan.rs` — empty-window progress during finality stalls, canonical hint invalidation and restart
- `src/custody/tests/confirmed/frozen.rs` — a pending receipt under frozen finality: each window read
  once across quota failures, lost replies, reorg and restart
- `src/custody/tests/confirmed/reinclusion.rs` — reincluded deposits move in one step and keep a
  solvent position open; an orphan with no replacement waits for the eligible head
- `src/custody/tests/confirmed/episodes.rs` — a reversal alert resolves only when its receipt is reinstated
- `src/custody/tests/legacy.rs` — legacy tag upgrades, principal eligibility and signature finality proofs
- `src/custody/tests/legacy/cache.rs` — bounded consumed header retention and unchanged approval/refusal outcomes
- `src/custody/tests/anvil_finality.rs` — Anvil receipts through RPC quorum, watcher and sequencer approvals
- `src/custody/tests/reorg.rs` — reorgs and forked answers against the proven chain
- `src/feeds/mod.rs` — the feed index and the mock feed, whose walks follow scheduled splits into
  the new share units, keeping a restart's pending splits and re-anchoring to a mark when the
  engine splits without the walk following
- `src/feeds/venue/mod.rs` — the venue task: connect, subscribe, sequence quotes, reconnect, and
  hold back quotes the engine would refuse
- `src/feeds/venue/alerts.rs` — the venue task's alerts: outages, silent symbols, a clock ahead of
  the host's, and refused messages
- `src/feeds/venue/transport.rs` — a connection's messages over a WebSocket, or in lines over TCP
- `src/feeds/venue/validation.rs` — the checks every venue's books pass before they are sequenced
- `src/feeds/venue/confirm.rs` — confirming every symbol with the engine each tick: which the
  venue is quoting, which markets it does not list, which contracts the listing does not name or
  the market does not take, and changed market hours; and settling a dropped connection's last
  quotes
- `src/feeds/venue/quotes.rs` — the quotes held between ticks until the host clock reaches them,
  and the rule that decides whether the quote the engine holds counts as the venue quoting
- `src/feeds/venue/setup.rs` — the TLS client config and reconnect backoff every connection shares
- `src/feeds/venue/tests.rs` — venue task unit tests: alert throttling and silence windows on
  paused time, held quotes against the engine's rule, kept books for unsubscribed symbols, the
  freshness rule, and backoff
- `src/feeds/coinbase.rs`, `src/feeds/bitstamp.rs` — venue adapters
- `src/feeds/massive.rs`, `src/feeds/tradermade.rs` — the Massive and TraderMade adapters: login,
  subscription, and quotes; Massive's tests are in `src/feeds/massive/tests.rs`
- `src/feeds/databento.rs` — the Databento adapter: the gateway's challenge-response handshake,
  symbol mappings, and top-of-book records; its tests are in `src/feeds/databento/tests.rs`
- `src/feeds/kraken/mod.rs` — the Kraken adapter: a kept order book verified against Kraken's
  checksum; `src/feeds/kraken/tests.rs` replays recorded book messages against it
- `src/feeds/tests/mod.rs` — venue tasks against loopback venues: what they sequence, refuse,
  and alert, and how they reconnect
- `src/feeds/tests/fake.rs` — the loopback venues, the feed harness with an engine that applies
  what the feed sequences, and the wait for the next alert
- `src/feeds/tests/late.rs` — venue tasks against a sequencer that answers late: when a quote
  counts as the venue quoting, and how a connection's last quotes are settled after it ends
- `src/feeds/tests/mock.rs` — the mock feed stopping one market at its `stop_after` and skipping
  one the engine has closed, and its walks rescaling at a split's opening, keeping a split applied
  between the startup query and the next, re-anchoring to the mark after a split they never
  followed, and restarting from a split market's mark
- `src/feeds/tests/scenario.rs` — the crypto feeds' dev scenario through the sequencer and engine
- `src/feeds/tests/rwa.rs` — the RWA price feeds' dev scenario: loopback vendors price FX, index,
  commodity, and share markets through the sequencer, across a market open and a futures roll,
  and the log replays
- `src/feeds/tests/vendors.rs` — loopback Massive, TraderMade, and Databento vendors
- `src/feeds/tests/hours.rs` — quotes held back while a market is closed and for a contract the
  market does not take, without silence alerts
- `src/feeds/tests/rwa_support.rs` — schedules open around the clock, signed relistings, and a
  record of every applied command, for the RWA feed tests
- `src/throttle.rs` — holds a repeating alert to one a minute, counting the ones it held back
- `src/clock.rs` — the wall and monotonic clocks the sequencer stamps and checks with
- `src/metrics/mod.rs` — the metrics registry, event counting, and the `/metrics` route
- `src/metrics/tests.rs` — metrics tests
- `src/monitor/mod.rs` — samples queue depth, the house, markets, and quote freshness into
  metrics, and raises `market_no_price`, `market_unfed`, `futures_roll_overdue`, and
  `sequencer_stalled`
- `src/monitor/tests.rs` — monitor tests
- `src/alerts/mod.rs` — the tracing layer that picks up alert lines, the router, and the page
  format
- `src/alerts/episodes.rs` — one page per episode start and end
- `src/alerts/webhook.rs` — webhook routes, the TLS client, and one delivery task per route, with
  retries
- `src/alerts/outbox.rs` — the single-owner durable SQLite outbox and disk/row validation
- `src/alerts/outbox/tests.rs` — restart, bounds, corruption and storage-failure tests
- `src/alerts/tests.rs` — alert routing tests
- `examples/access_source.rs` — loopback-only signed synthetic source for the access scenario
- `examples/reference_log.rs` — writes a protocol version's node reference log from engine-core's
- `tests/reference_logs.rs` — replays every committed node reference log, events included
- `tests/usd_settlement.rs` — the USD settlement dev scenario on the committed dev config: Germany 40
  valued at EUR-USD's index while EUR-USD moves, then refusing orders while EUR-USD is stale
- `tests/reference/` — one node-format reference log per protocol version, never regenerated
- `tests/replay.rs` — replays a log the node wrote before protocol versions, kept in
  `tests/fixtures/`, and checks that `engine replay` fails on a log it cannot verify
- `tests/common/access.rs` — shared explicit signed declarations, screening and financial scenario inputs
- `tests/production_genesis/` — loads `config/production-genesis.toml` as a dev node's genesis,
  checks the launch values and quote currencies, and steps it through weeks with holidays and early
  closes (`main.rs`); checks each market reaches every divergence stress level with screened house
  funding (`divergence.rs`);
  and recomputes each futures market's roll times from its exchange's calendar and rolls the engine
  through them (`futures.rs`)

## Usage

```bash
# Run a local node with mock TEE, custody, and feeds (API on https://127.0.0.1:8080)
cargo run -p engine-node -- run --config config/dev.toml

# Call it pinned to the attested TLS key: check that the served bundle hashes to report data bytes
# 0..32 and the rest is zero, then pin bundle bytes 73..105, the SHA-256 of the certificate's DER
# SubjectPublicKeyInfo
API=https://127.0.0.1:8080
ATT=$(curl -sk $API/v1/attestation)
BUNDLE=$(jq -r '.bundle[2:]' <<< "$ATT")
EXPECTED="$(xxd -r -p <<< "$BUNDLE" | sha256sum | cut -c1-64)$(printf %064d 0)"
if [ "$EXPECTED" = "$(jq -r '.report_data[2:]' <<< "$ATT")" ]; then
  PIN=${BUNDLE: -64}
  curl -s --insecure --pinnedpubkey "sha256//$(xxd -r -p <<< "$PIN" | base64)" $API/v1/health
else
  unset PIN
  echo "the bundle does not match the report data" >&2
fi

# Price BTC-USD and ETH-USD from live venues instead, leaving the other markets unpriced: set
# feeds.provider = "venues" in a copy
cargo run -p engine-node -- run --config <copy of config/dev.toml>

# Verify a node's log directory, a downloaded archive, or a single log file
cargo run -p engine-node -- replay --log data/dev/log

# Sign and submit admin commands as the operator, over TLS pinned to the attested key: the PIN (hex)
# the bundle check above sets. The key is read from --key-file, OPERATOR_KEY_FILE, or OPERATOR_KEY;
# config/dev.toml names the public key of the local test key 0x4242…42 (32 bytes of 0x42). A market
# or parameters file holds the keys of a [[genesis.markets]] entry or of [genesis.params] at its top
# level, without the table header.
export OPERATOR_KEY=0x4242424242424242424242424242424242424242424242424242424242424242
cargo run -p engine-node -- admin --pin $PIN set-reduce-only true             # every market
cargo run -p engine-node -- admin --pin $PIN set-reduce-only false --market 2
cargo run -p engine-node -- admin --pin $PIN set-stress-floor 1 --market 1
cargo run -p engine-node -- admin --pin $PIN set-benchmark-rate SOFR 430
cargo run -p engine-node -- admin --pin $PIN upsert-market market.toml        # no table header
cargo run -p engine-node -- admin --pin $PIN set-params params.toml           # no table header
# Once per log, from protocol V19: when emission period 0 starts (not before the command is
# sequenced) and its budget in MONKEY base units. Production values need the owner's approval.
cargo run -p engine-node -- admin --pin $PIN init-emissions --start 2027-01-01T00:00:00Z --period-budget <units>
cargo run -p engine-node -- admin --pin $PIN schedule-corporate-action action.toml  # see the runbook

# Once per log, from protocol V25: when staking epoch 0 starts (not before the command is
# sequenced), the house NAV liquidity floor, the epoch length, and the unstake cooldown in
# milliseconds. Production values need the owner's approval.
cargo run -p engine-node -- admin --pin $PIN init-staking --start 2027-01-01T00:00:00Z --liquidity-floor 10000000 --epoch-ms 3600000 --cooldown-ms 86400000

# Download the archive: the manifest, then each segment it lists (`--retry` waits out a 429)
curl -kf --retry 5 --create-dirs -o archive/manifest.jsonl https://127.0.0.1:8080/v1/archive/manifest.jsonl
jq -r '.file' archive/manifest.jsonl |
  xargs -I{} curl -kf --retry 5 -o archive/{} https://127.0.0.1:8080/v1/archive/{}

# Run the vault deposit scenario: Anvil, the vault, and a node with EVM custody
./scripts/dev-vault-deposits.sh

# Also fabricate a deposit at one provider, stop two providers, and recover quorum
RPC_QUORUM_SCENARIO=1 ./scripts/dev-vault-deposits.sh

# Exercise signed eligibility, API refusals, direct-vault and contract-wallet payer holds
./scripts/dev-access-enforcement.sh

# Run checkpoint posting, tampered replay, truncation, and shortfall checks on isolated Anvil
./scripts/dev-checkpoints.sh

# A production binary, which refuses mock providers (release images build it in image/)
cargo build --release -p engine-node --features production
# Run the MONKEY emissions scenario on isolated Anvil: allocation, halving, restart, and claims
./scripts/dev-monkey-emissions.sh

# Run the MONKEY staking scenario on isolated Anvil: staking mirrored from the contract, an epoch
# above the floor paying pro-rata in USDC, an unstake through the cooldown, and an epoch below the
# floor paying nothing
./scripts/dev-monkey-staking.sh
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
`SCENARIO_LOG_DIR` preserves its logs; the finality scenario also preserves its config and public sequencer log. `config/anvil.toml` expects local endpoints on 8545–8547 when
run directly; the scenario rewrites these to its proxy ports.

From V22, a withdrawal reserves an accepted request from trading cash, rounding down to
whole USDC base units and retaining any smaller residual in the account. A public FIFO queue
approves its head partially when cash is short; each part has its own payout nonce. Trading losses
trigger another queue pass but do not create vault tokens. New deposits and house capital add
physical payout capacity. The node signs only after the approval is synced and included in a
finalized accepted checkpoint; a logged signature remains unpaid until the vault reports its used
nonce. From V34 a request is at least 10 USDC. An authorization that expires unclaimed is not
signed again on a schedule or by a restart, so unclaimed approvals cost the node nothing. Its
account asks with `POST /v1/accounts/<address>/withdrawals/<payout-nonce>/reissue` and a body
`{"replaces_expires_at": <expiry of its latest authorization, 0 if none>, "signature": "0x…"}`.
The signature is the account's EIP-712 signature, under the same domain as its other requests, of
`ReissueRequest(address account,uint64 payoutNonce,uint64 replacesExpiresAt)`, so it works once.
The node queues the approval for a new 24-hour authorization with the same nonce, in approval
order, and answers `401` for another signer, `404` for an approval that is unknown, paid, or not
yet loaded by a restart's recovery, and `400` while the latest authorization is usable or the
request replaces an older one. A signer rotation reissues every unpaid approval under the new
epoch without a request. A queued request lives in memory: after a node restart, send the same
signed body again, which stays valid until an authorization replaces the one it names. While the
host leaves the log intact a restart reissues nothing; recovery from a checkpoint snapshot sees
only the authorizations after it, so an approval signed before it is signed once more.

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
reconciles at custody's `withdrawal_finality_tag`, independently of the early-credit tag and its
confirmation depth. Latest inclusion is
tracked separately so delayed finality never causes duplicate submission. Startup waits for a
pending inclusion to reach that boundary (or disappear in a reorg), bounded by
`custody.evm.finality_stalled_after`, before selecting its rollback anchor. Recovery then restores every outstanding durable signed
checkpoint and its original accounting before signing descendants; an unmined signature remains
live even when both chain views still show its parent. Permissionless relays matching that local
chain reconcile the covered prefix, including work beyond the receive queue. The queue is bounded, and
the worker finalizes one checkpoint at a time; while the queue is full the sequencer defers new
checkpoints with the Warning `checkpoint_backlog` and keeps sequencing, and approvals wait for the
next checkpoint that fits. Failed posts retry after `retry_interval` and emit `checkpoint_post_failed`. Withheld signing or
posting emits the Warning `checkpoint_withheld`, one episode per `cause` (`collateral`,
`custody_prefix`, `post_diverged`) however many attempts it spans, resolved when that hold clears. A conflicting
posted root or signer stops the node with a Critical alert. Following decision #15 §2, withdrawals
pause when the vault's USDC is less than the ledger requires, or the vault's deposits or payments
differ from it: collateral reconciles when the vault's paid total is within the checkpoint's
approvals, the vault's running deposit total through the checkpoint's last credited deposit equals
the ledger's credits, and the vault's USDC balance is at least its deposits less its payments.
Deposits the checkpoint has not yet credited do not pause withdrawals until the first of them stays
uncredited for `custody.evm.stall_critical_after`. A difference emits Critical
`collateral_mismatch` and pauses withdrawals; ordinary trading commands continue. While the latest
finalized checkpoint's nonempty custody prefix has no sequenced canonical receipt coverage, the
worker cannot compare it with the vault: it emits the Warning `collateral_pending_deposits` and
pauses new withdrawals (fail closed), while checkpoint signing continues, since a successor with a
proven prefix is the way out. Withdrawals resume at the first healthy comparison of a proven
checkpoint. A failed or absent observation never changes the gate, so it cannot erase a proved
divergence. `engine_withdrawals_paused` is 1 in every paused state (before the first reconciliation,
an unproven prefix, a pending catch-up past its deadline, a divergence), and
`engine_withdrawals_verdict_seq` is the finalized checkpoint behind the latest verdict. USDC sent to the
vault outside a deposit pauses nothing: it emits the Warning `collateral_surplus` until anyone calls
the vault's `bookSurplus`, which records it as a house deposit; a surplus below the 10 USDC minimum
deposit must be topped up first.

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
live sequencing and authenticated replay. Liabilities track net external principal, including locked
principal and unpaid approvals. Receipt events record their actual principal delta: adoption and
finality move no cash, a reversal removes principal once, and internal bad-debt absorption adds no
second debit. Historical accounting retains its matching successful `0x` credit rules, including
accepted zero-capital moves. Modern Work captures canonical count coverage from the sequenced
finalised frontier and active receipts above it, in constant time; later commands cannot change
that prefix. Historical reference tombstones supply no canonical proof. Before a canonical scan
is sequenced, equal aggregate balance/count cannot make a modern Work healthy. This coverage
never establishes finality for an unresolved legacy principal lock.
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

From V20, production credits trading collateral at `custody.evm.finality_tag = "latest"`,
minus the configured `confirmations`. It releases that principal for withdrawals at
`withdrawal_finality_tag = "finalized"`, without a second credit or a second confirmations delay.
A node with hardware attestation refuses any other tag pair before custody starts. Mock TEE
configs may choose other tags for dev scenarios. A release tag that stops advancing while quorum heads move is covered by the existing finality-stall watchdog.
Both tags require the fixed provider majority,
comparing hashes at a common height; missing quorum releases no principal and credits nothing.

Each exact receipt and its disposition live in the sequenced core state and state tree. An account's
`unfinalised_deposits` remains trading collateral and is included in its account view and proof;
the account view omits the field while zero. Withdrawal cash is its balance less that principal,
limited independently by equity above margin, so unrelated finalised cash remains available subject
to the existing margin check. Trading losses reduce available cash without reducing the principal
lock. Finality removes only the matching lock. The finalised scan frontier is also sequenced; the
host cursor cannot unlock principal or move the V20 scan past an unresolved receipt. The watcher
fixes its command format from the build's supported protocol before its first poll; activation
between reading a receipt and submitting it cannot turn that receipt into a legacy unlocked credit.
Before the first authenticated time sample, neither deposits nor finalised progress can be sequenced.

A quorum-proven canonical orphan emits Critical `deposit_reversed`. When the confirmed scan holds
the same transaction-local deposit again, with the same beneficiary and amount, in another block,
one `Replace` transition reverses the old receipt and credits the replacement, including when its
global log index or vault deposit id changes or it is an earlier reversed receipt returning: cash and
the principal lock net out and no close-out runs. An orphan with no replacement waits until the scan
reaches the eligible head, holding the finalised frontier below it, and then debits its principal
exactly once. A transaction canonical again with other economics is no replacement, so its orphan is
debited at once. After a debit, the existing balance-change close-out closes executable positions;
stale positions wait for a valid price. Once flat, a negative account balance is written off against
house cash through `BadDebt`. A negative house NAV does not halt trading. Accounting-mismatch withdrawal
gating belongs to #24, and public liquidity FIFO to #49. Exact receipts and conditional transition
revisions prevent old deliveries from repeating effects across restarts.

The adapter trusts an honest strict majority of independently operated providers. Linked block
hashes and complete event sets checked against counts at exact block hashes bound the transcript;
one compromised provider cannot fabricate a credit or reversal. Vault, token and provider configuration
remain host supplied pending attestation binding. Reorganisations undoing Ethereum-finalised blocks
remain outside the threat model; the watcher refuses a changed finalised anchor. Earlier protocols
retain their recorded tag/cursor semantics and do not gain V20 receipt state during historical execution.
V20 activation carries the successful legacy EVM credits reconstructed from the verified public log;
both sequencing and replay check that input against the original commands and events. Activation locks
their principal without moving cash. Normal mock references are excluded, and vault house capital is
tracked separately. Canonical receipt adoption preserves the lock without another credit; only quorum
proof at the withdrawal finality tag releases it. Unrelated independently finalised funds retain their
own eligibility. Neither an old credit, a host cursor nor a new config establishes legacy finality.

If an old receipt is missing or its global log index changed before an exact identity was recorded,
its original block cannot be reconstructed from the old log. The unresolved principal stays locked
and raises Critical `legacy_deposit_unproven`; the watcher does not invent an old block or apply an
extra cash movement. Other transactions may continue. Once an exact receipt has been adopted, its
reversals use the normal exact-once debit and close-out path. A stranded historical withdrawal that
would consume unresolved legacy principal, including one deferred behind a checkpoint, requires the
same independent finality proof before writable recovery or signature redelivery. Missing proof refuses that recovery without appending or
signing; a later restart retries the proof. Historical approvals, roots and events are preserved.

Each inherited approval retains its actual sequence and post-debit cash from authenticated replay,
even if it remains unsigned across activation. A reservation already retaining all unresolved principal
at that prefix keeps its eligibility. Otherwise later independently finalised funding may cover the
collectively pending debits only at that later prefix: signature release waits for a healthy posted
checkpoint covering that authorization sequence, while retaining the original approval tuple. Neither
activation nor a proof for one approval authorizes another. Read-only quorum proof changes no cash,
core lock or deposit disposition; restart re-establishes delivery authorization before any writer opens.

Run `./scripts/dev-deposit-finality.sh` for the private Anvil scenario. It exercises actual contract
receipts, the RPC quorum, watcher and durable sequencer, including immediate trading, pre-finality
approval limited to finalised headroom, approval after finality, and one reorg debit with Critical.
The EVM HTTP withdrawal route remains disabled until #23; this scenario proves approval eligibility
at the sequencer/core boundary and does not perform a physical payout or HTTP cash-out.

Endpoints: `GET /v1/health`, `GET /v1/attestation`, `GET /v1/markets`, `GET /v1/house`,
`GET /v1/accounts/{address}`, `GET /v1/accounts/{address}/nonce`,
`GET /v1/accounts/{address}/history`, `GET /v1/accounts/{address}/proof`,
`GET /v1/accounts/{address}/emissions`, `POST /v1/orders`, `POST /v1/orders/cancel`,
`POST /v1/withdrawals` (mock custody only until withdrawals are paid on-chain), `POST /v1/admin`
(the operator's), `POST /v1/dev/deposit` (mock custody only), `GET /v1/stream` (WebSocket), and
`GET /v1/archive/{file}` (`manifest.jsonl` or a segment it lists).
A client over its rate gets 429 with `Retry-After`, a body over `api.max_body_bytes` gets 413, a
body not received within `api.read_timeout` gets 408, and a subscriber over `api.max_subscribers`
gets 503. A connection over a connection cap or over its client's connection rate is closed before
its TLS handshake. A client is its socket peer IP, with IPv6 counted by its /64; forwarding headers
are ignored.

`GET /v1/attestation` returns `provider`, `enclave_address`, `config_sha256`,
`tls_public_key_sha256`, `bundle`, `report_data`, and `quote`, each byte string as `0x` hex. The
quote binds the 64 bytes of `report_data`: bytes 0..32 are the SHA-256 of `bundle`, and bytes 32..64
are reserved as zero. The bundle is 105 bytes, every field at a fixed width:

| Bytes | Content |
| --- | --- |
| 0..21 | the ASCII domain tag `engine.attestation.v1`, which names this layout; a new one takes a new tag |
| 21..41 | the enclave signing key's address |
| 41..73 | the SHA-256 of the config file, the value `sha256sum` prints for it |
| 73..105 | the SHA-256 of the TLS certificate's DER SubjectPublicKeyInfo |

A verifier checks the quote, recomputes the SHA-256 of `bundle` and compares it with report data
bytes 0..32, and then reads the fields from the bundle.

`/v1/stream` is live from now by default. `?from=<seq>` starts at the first message of `seq`
(`from=0` is the whole log), and `?after=<seq>:<index>` resumes after the `offset` of the last
message received. Each message carries its `offset`: the command of a seq is index 0, then its
withdrawals, then any checkpoint. A command logged before events were recorded in the log streams
without its `events` key.

The log is a directory (`node.log_dir`) of segment files named for their first seq, plus
`manifest.jsonl`, which gets one line per completed segment with its seq range, size, and SHA-256.
Without snapshots, the node loads the manifest at recovery, after checking it against the segment
files, and then holds it in memory, adding each segment it completes. With snapshots, an
authenticated segment catalog takes the manifest's place (see `[snapshots]` below). The archive
serves that copy, never the file on disk; with the catalog, it keeps the listing it last proved
and proves only the segments completed since, taking no backfill permit.
Backfill reads up to 256 messages at a time and keeps them until the subscriber takes them. It
checks a completed segment against the manifest in one pass the first time it reads from it, then
checks each chunk it reads, and refuses any other segment the manifest does not list. The segment
still being written is checked the same way against the length and SHA-256 of the prefix the
writer last flushed, which it publishes with its manifest after each batch; when that prefix grows,
or the segment completes, backfill hashes only the bytes after the last whole chunk it checked. Backfill yields nothing
past that prefix, so no part of a batch still being written reaches a subscriber from disk. While it reads a batch, a subscriber holds one of 16 permits and one
1 MiB chunk of a segment. If a new segment cannot be created, the current one keeps growing and the
node warns (`log_rotation_failed`) until it can. A log written as one file before segments existed
continues as the first segment: move it to `<log_dir>/00000000000000000000.jsonl`. Every segment is
kept for audit, fallback recovery, and rebuilding history. Full recovery replays as it reads, one
segment and one line at a time, and replays nothing of a completed segment before checking it
against the manifest. `engine replay` always audits the entire log, including with snapshots enabled.

`[snapshots]` is optional; omitting it keeps full replay and the SQLite history index below. Its
required fields are `dir` (a separate cache directory), `retain` (a positive count of caches),
`max_bytes` (a positive size limit for each signed file), and `trusted_signers` (EVM addresses
verified through enclave attestation). The dev config has a commented example retaining three caches
of at most 64 MiB each, with empty signer pins. With empty pins the node writes caches but every
start skips them with one `info` line, replays the whole log, and rebuilds history from it.
Recovery trusts a cache only when its signer is pinned and is the key the node attests now, which
also signed every checkpoint the cache carries; pin a new address after verifying its attestation.
Caches an earlier key signed are never used, and until the enclave key persists across restarts a
start under a new key fails closed. Trust never comes from a cache, its self-reported signer, or a
manifest on host storage.

With `[snapshots]`, account history and the segment catalog live in `dir` as authenticated
databases (`authenticated-<generation>.sqlite`), sparse Merkle trees (jmt 0.12, SHA-256) whose
every read and update is checked against a root the node signed, and `history.dir` is unused. The
catalog commits each completed segment's listing and the exact bytes of each checkpoint entry. At
each checkpoint the sequencer syncs the log, commits the catalog at that checkpoint and signs its
new head, and hands the history owner the engine state with the replay facts the checkpoint needs:
command count and prefix commitment, custody accounting, every outstanding checkpoint with the
accounting it was signed with, deferred withdrawals, and the finalized base checkpoint. Once the
history owner has indexed exactly that checkpoint, it signs a history head for it and publishes the
paired cache under its own deployment-bound signature domain: it syncs a staging file, renames it,
syncs the directory, and prunes caches beyond `retain`. A capture that finds the history owner busy
with the previous one is skipped. On shutdown the sequencer waits up to five seconds for an
in-flight capture before its final checkpoint, and history keeps running until it has taken the
rest of the stream, so the final checkpoint is cached too. The history owner also signs a head
after every 1,024 commands it indexes and when it stops, keeping the two newest, and collects
retired history versions after every 1,024 commands, however often captures sign heads. A
captured checkpoint costs the sequencer one encode of the engine state, which grows with the
accounts and positions it holds; the history owner signs and syncs the cache, and holds the cache
lock, which the sequencer also takes to publish catalog heads, only to rename and prune files.

Recovery reads and writes nothing else until the custody preflight passes. It tries caches newest
first, checks each signature against the pins, recomputes the state root, checks the checkpoint
signature, the recovery facts, the history head at the checkpoint, and the catalog's proof of the
checkpoint's exact bytes, and takes the newest signed history head at or after the checkpoint. It
then replays every physical entry after those bytes, checks the replayed command prefix against
the history head, and needs no segment before the checkpoint's. A cache that fails any check emits a
`snapshot_rejected` Warning and recovery tries the next one; when none passes, it replays the whole
log and rebuilds history and the catalog from it. A cache carries no legacy deposit ledger or
inherited approval provenance, which only full replay derives from verified commands, so recovery
refuses a cache from before custody finality activated, or one still owing a withdrawal approved
before then. Reading a cache rebuilds the engine's derived indexes, and each checkpoint's cached
canonical deposit coverage must match the engine's. The on-chain checkpoint preflight then runs on the
recovered state exactly as after full replay: signatures establish integrity, not freshness. Only
after it passes does the node roll back a transaction a kill left in a store's journal, discard
history and catalog versions committed after their newest signed heads, which a crash before a head
was signed leaves behind, and reclaim interrupted cache files. Until then recovery reads a store
with such a journal as the file stands, and every read is still checked against a signed root; a
kill while SQLite was writing a commit's pages can leave that unreadable, and then recovery replays
the whole log. A
failed cache write emits `snapshot_write_failed` and keeps the log usable. Failures at successive
checkpoints are one alert episode, which the next cache written resolves. The first cache written
after each start resolves it too, closing an episode an earlier run left open. A failed log sync
stays fatal. When the catalog cannot take a completed segment, the node warns once
(`log_catalog_unavailable`) and keeps rotating, holding up to 64 completed segments in memory; past
that the active segment grows, and the catalog retries at each checkpoint. A catalog write that
rolled back leaves the catalog writable; only a commit of uncertain outcome stops it until restart.

A cache does not carry the order in which approved withdrawals are paid, which full replay
validates, so recovery refuses a cache whose engine holds a V22 withdrawal approval, and the node
takes no cache once its engine holds one. The caches written before the first approval stay
retained, so a node that has approved a withdrawal restarts from the newest of them (or replays the
whole log when none remains), and its restart time grows with the log written since.

Before the first approval, a restart from a cache reads the cache, the checkpoint's segment, the suffix after it, and a bounded
number of proof lookups, so its time follows engine state and the checkpoint interval rather than
the length of the log or of retained history. History serves once it has indexed what the log
holds after its newest signed head, which after a clean shutdown is nothing. History and the
catalog take only entries the log has synced, so a log ending before a signed history head, or
short of segments the newest catalog head lists, means storage lost synced writes or the host
truncated the log, losing commands the node had acknowledged. Recovery then raises a Critical
`log_tail_lost` alert and resumes history and the catalog from the cache's own heads; after the
preflight it removes the catalog heads past the cache, discards the later versions of both, and
indexes and catalogs the rest of the log again. Each also resumes from the cache's head when no
retained head of its own verifies. A catalog head can also record checkpoints past the log's end in
the segment still being written, which recovery sees only through a history head that reaches
them. Without one, the checkpoint written again at such an offset fails to cache
(`snapshot_write_failed`), and once its segment completes the catalog takes no more segments
(`log_catalog_unavailable`); the next start finds the catalog holding another checkpoint there,
rejects every cache, and replays the whole log. The first start with `[snapshots]` on an existing log replays it in full
and builds history and the catalog from it once. `engine replay` still audits the whole log, and snapshots
never permit deleting it.

Run the restart dev scenario with `./scripts/dev-snapshot-restart.sh`. It runs a dev node with
`[snapshots]` and checkpoints every 100 commands, pins the enclave address the node attests, grows the
log and history from 400 to 4,000 dev deposits, and stops once history has caught up and restarts
after each step; then it corrupts the newest cache and restarts again. Each restart must recover from a cache with at most one checkpoint
interval replayed and serve every deposit as soon as the API answers; the corrupt cache must raise a
`snapshot_rejected` Warning and fall back to an earlier one; and `engine replay` must verify every
checkpoint the node wrote. It prints restart times without gating on them.
`cargo test -p engine-node dev_scenario_restart_stays_bounded -- --nocapture` runs the same scenario
through the node's startup path in-process.

From V24, every client action requires a signature, including logs that recorded development
`auth = "none"`. Earlier versions require it under `auth = "eip712"`. Each order, cancel, and
withdrawal body carries
`"auth": { "nonce": <next nonce>, "expires_at": <ms>, "signature": "0x…" }`: the account's EIP-712
signature under the domain of `custody.vault` on `custody.chain_id` (see `engine-core`'s
`sign_request` example). `expires_at` is the latest time, in milliseconds since the Unix epoch, at
which the request itself is accepted; a limit or stop order it places rests until it fills or is
cancelled, and from V24 its screening at placement covers it for that whole time. A
missing or invalid signature is refused with 401, an expired request with 400, a signed request
other than a declaration from an account that has never been credited with 403, and a nonce other
than the account's next with 409 and the `expected` nonce; none of these reaches the log. A request
refused with 403 or 409 uses no nonce, so it stays valid until its `expires_at`: resubmit that same
request once its nonce is next, or let it expire before signing different content at that nonce.
From V24, a never-credited account may sign only a declaration, admitted once a deposit for it is
pending and within its client's share of a budget all clients share; its other signed requests, and a
declaration without a pending deposit, get 403. The API screens a declaration like an order, and
the engine records it only on verified, allowed and clear evidence; a refused declaration is in the
public log with its reason and leaves the deposit pending, and once one is recorded the pending
deposit credits on its next retry.
Authenticated policy refusals consume the nonce. The recorded source supplies geography
and sanctions classifications; missing, unknown, stale or unauthenticated evidence fails closed.
See [access-source configuration](../../config/access-source.md) for the response schema, eligibility
statements, custody holds, operator alerts, and the local scenario.

An expiry more than five minutes ahead is sequenced and rejected with `expiry_too_far`, which uses
the nonce. Requests that both pass API admission for one nonce are sequenced, and the engine rejects
the second with `invalid_nonce`.

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
one event `type`: `deposited`, `deposit_rejected`, `custody_deposit_changed`,
`custody_deposit_rejected`, `withdrawal_approved`, `withdrawal_rejected`, `order_accepted`,
`order_rejected`, `order_cancelled`, `fill`, `close_out`, `bad_debt`, `financing`,
`emission_allocated`, `withdrawal_queued`, `withdrawal_queue_updated`, `corporate_dividend`, or
`corporate_cash_in_lieu`. `?limit=` is 1 to 200, 50 by default. A page is one search of a SQLite
index that reads at most `limit + 1` entries, and at most 16 pages are read at once. Until the index
has taken every command the log held when the node started, the route answers 503 (`history is
catching up with the log`). History is public, like the log.
`emission_allocated`, `staked`, `unstaked`, `staking_rejected`, or `distribution_allocated`.
`?limit=` is 1 to 200, 50 by default. A page is one search of a SQLite index that reads
`emission_allocated`, `eligibility_attested`, `eligibility_rejected`, `deposit_pending`, or
`deposit_held`. `?limit=` is 1 to 200, 50 by default. A page is one search of a SQLite index that reads
at most `limit + 1` entries, and at most 16 pages are read at once. Until the index has taken every
command the log held when the node started, the route answers 503 (`history is catching up with the
log`). History is public, like the log.

Without `[snapshots]`, the history index is a SQLite file in `history.dir`, derived from the log
alone. A supervised task
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

With `[snapshots]`, the same task feeds the authenticated history database instead. The commands it
has taken are indexed in updates of up to 128 commands with `history.retention` applied, and each
version is published once complete; a captured checkpoint first indexes every command up to it. A page
is a binary search over proved dense ranks for the account (and kind), and every row, event, and the
lookahead behind `next` are proved under the published root, so a missing or altered row fails the
page instead of shortening it. A page that fails a proof answers 500 (`history_unreadable`), and the
generation stops serving (503) until the history owner has rebuilt a fresh one from the log; the
old generation is removed once its readers finish. Busy storage and bad requests do not trigger a
rebuild.

`engine admin` reads the node config (`--config`, or `CONFIG_PATH`) for the operator's public key
and the custody chain id and vault, refuses a key that is not the operator's, takes the operator's
next nonce from `GET /v1/accounts/{operator address}/nonce`, and posts
`{ "command": <admin command>, "auth": { "nonce", "expires_at", "signature" } }` to
`POST /v1/admin` on `--node` (default `https://127.0.0.1:8080`). It connects over TLS 1.3 only when
the node's certificate key hashes to `--pin`, the TLS key hash in the attestation's bundle, so a
client that has checked the bundle against the quote's report data reaches the attested enclave. The
signature is EIP-712 over the command's JSON, its nonce, and an expiry two and a half minutes ahead,
under the same domain as client requests. It prints the signed request and the events the engine
produced, and fails when the engine answers `admin_rejected`. The node refuses a command only it
sequences (client auth, the operator, protocol activation) with 400, a signature that is not the
operator's with 401, an expired request with 400, and a nonce other than the operator's next with
409, and none of these reaches the log. The operator may list a market or replace its spec and
calendar, replace the protocol parameters, set a benchmark rate, set a stress floor, which raises a
market's stress level but never lowers it below the automatic one, and make one market or every
market reduce-only, where new risk is refused and trades that reduce or close a position still fill.
The engine also refuses parameters whose stress and toxicity spreads would price a closing trade at
the price itself. Once any deposit or house capital is credited, the engine refuses an unsigned
operator command, so only genesis, which credits its house capital last, runs unsigned. The first
start under protocol `V8` records `operator.public_key`, with the custody chain id and vault, in the
log; changing it needs a fresh log, and `engine replay` prints the operator a log records. A node
whose `tee.provider` is not `mock` refuses to start with an operator key that
`config/test-keys.toml` lists, in either SEC1 encoding: the repository publishes those keys' private
keys for local nodes.

`/v1/accounts/{address}/proof` returns, against the state root of the last checkpoint, whose `seq`
and `root` it names: the account's leaf (balance, withdrawal nonce, toxicity score with, from `V11`,
its fraction of a bp and the notional of history behind it, and number of open positions) with its sibling hashes; each position (its market, qty, and entry) with its leaf's
siblings; and the leaf and siblings of each market it holds a position in. Check `root` against that
checkpoint's signed entry, then check the proof with `engine-core`'s `AccountProof::verify` or its
`verify_proof` example. An account credited since the last checkpoint is not in its root yet, and
gets 404 until the next one. Each checkpoint logs, at `debug`, how long its root took and how many
leaves and tree nodes it hashed.

`/v1/accounts/{address}/emissions` returns, from `V19`, the account's cumulative MONKEY entitlement
against the state root of the last checkpoint: `seq`, `root`, `account`, `entitled` (base units of
the 18-decimal token, as a decimal string), and `siblings`, from the root down. The vault's
`claimEmissions` checks the proof against its latest posted root only, so a claim waits until the
vault's `latestSeq` equals the proof's `seq`, and a proof stops working once a later checkpoint is
posted; fetch a fresh one then. An account has no entitlement, and gets 404, until the first
checkpoint after a period in which it was allocated a share.

`/v1/accounts/{address}/distributions` returns, from `V25`, the account's cumulative USDC
distribution the same way (`entitled` in the 6-decimal token's base units). The vault's
`claimDistribution` pays the USDC the distribution adds to what the account was already paid,
against its latest posted root. An account has no distribution, and gets 404, until the first
checkpoint after an epoch in which it was allocated a share.

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
pages for them. A market priced from futures whose next contract's roll time has passed with no
gap recorded raises the Warning `futures_roll_overdue`, and it resolves when the market rolls.

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
resolve to its alert and drop duplicates. A resolve pages only when it ends an episode this node
paged as firing: code that re-logs `alert_resolved` while healthy, such as the checkpoint
reconciliation on every tick, pages nothing. Open episodes (up to 1024, keys of at most 256 bytes)
are kept in the outbox, so a resolve after a restart still pages; a resolve for an episode evicted
past that cap or whose firing page was refused pages nothing. `context` names the spans the line was logged in, such
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
`ceil(max_bytes / page_size) + 8 * max_pages + 16` SQLite pages, including indexes and retained
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

### Older unscanned custody anchors

Writable EVM recovery requires the finalized checkpoint's original authenticated accounting and
an exact credited deposit prefix. An empty bootstrap ledger proves prefix zero; a nonempty modern
checkpoint needs the canonical custody frontier recorded at that checkpoint. Later scanning cannot
supply a missing proof for an older immutable root. Startup refuses older nonempty unscanned anchors and every outstanding recovered signature with an
unproven prefix
before opening the log for append or creating or repairing history, rather than serving permanently
paused. Read-only `engine replay` remains available for their audit.

This project is prelaunch, holds no real funds, and the staking release already requires vault
redeployment. Such development logs require a fresh deployment or a separately specified and verified
operator migration. Preserve the original log and custody state; deleting the log alone cannot reset
a vault's accepted checkpoint, and this node does not infer or fabricate a migration proof.


### Corporate actions operator runbook

Protocol V23 accepts single-stock dividends and splits, and index dividend points, through
`engine admin schedule-corporate-action <file.toml>`. The client signs the complete instruction,
nonce and expiry through the existing pinned TLS operator channel. The public log records that signature,
`admin_applied` or `admin_rejected`, and the eventual application events. A future authenticated
feed can submit the same core instruction types; feed integration is separate.

Prepare one instruction per market, opening and adjustment kind. Its required fields are `id` (a
unique immutable identity), `market`, `previous_close` and `ex_open` (UTC epoch milliseconds), and
`[adjustment]`. Both boundaries must match the listed venue calendar, including holidays, special
sessions and DST; `previous_close` must be the close immediately before `ex_open`. Submit strictly
before that close. Submission at the close or later is rejected because the engine cannot infer
historical holdings. A pending action locks its market's calendar, symbol, asset class and quote
currency; margin and spread changes remain available.

For a dividend, `[adjustment]` contains `kind = "dividend"`, `gross_per_unit` in quote currency per
share or index unit held at `previous_close` (so a dividend going ex after a split is entered per
post-split share), and the required `withholding_rate`, a decimal fraction from `"0"` to `"1"`,
inclusive. The operator supplies the applicable rate on every signed instruction; the engine has no
default tax rate. Long quantities receive the net amount; short quantities pay the gross. For an
equity split it contains `kind = "split"`, `new_units` and `old_units`, positive unequal integers
such as 4 and 1 for a four-for-one split. ETFs, bonds, currencies, commodities and crypto do not
receive these adjustments, and indices do not split through this command.

The engine captures quantities before any command at the previous-close timestamp, retains them
across restart, and applies the action before any opening fill or margin check. Cash credits round
down and debits round up at each step. Non-USD amounts convert at the market's recorded FX valuation
mark, including a stale mark, using the same policy as financing valuation; an absent or unusable
mark fails explicitly. Cash transfers have exactly opposite house offsets. Cash adjustments and
missed financing rollovers run in chronological order. At one timestamp, captures precede opening
adjustments; dividends pay the captured pre-split holdings first, then splits apply, each in
market/id order. An incoming opening quote is stored in the adjusted units before execution and
margin checks; a coincident rollover follows that processing. A protocol activation, like a rejected
benchmark batch, runs no catch-up: missed rollovers and due openings wait for the next admitted
command. From V33 the engine catches up on at most `MAX_CATCH_UP_BOUNDARIES` (366) rollovers and
corporate boundaries per command and defers the command (`CatchUpDeferred`) when more are waiting; the
sequencer sequences `Tick`s, stamped a millisecond before the command, while `Engine::catch_up_pending`
holds, so no live command is deferred. A benchmark snapshot goes first, because the rollovers need its
rates, and the `Tick`s follow it.

Splits scale live positions and entry prices, resting orders, stored source quotes, marks and
processed indices, aggregates, stress price references and pending markouts, preflighting every
value and cash transfer before changing the books. Each scaled price rounds once at nine decimals:
an entry against its holder (up for a long, down for a short), a resting limit or stop price to the
side that never executes sooner than the client asked (a buy limit and a sell stop down, a sell
limit and a buy stop up), and marks, quotes, indices, stress references and markout prices toward
zero. Quantities round toward zero. The fraction rounded off a position is closed at the post-split
mark: its PnL against the rounded entry, rounded down and converted at the recorded FX valuation
mark, moves between the account and the house as `corporate_cash_in_lieu`, with no profit fee; like
a fill's realised P&L, it counts toward the account's MONKEY emission weight, which dividends do
not. A resting order whose quantity or price rounds to zero, or whose quantity would exceed the
1,000,000,000 input bound, is cancelled with reason `split`. An account the split leaves flat writes
off any negative balance against the house, as a close-out does. Pending dividend instructions keep
their signed amounts, and USD risk limits and historical fill notionals keep their values. Stored
scaled quotes retain their original ages; an incoming quote stamped before the most recent split is
rejected. Quotes received between a scheduled split's previous close and opening are rejected
because a venue may already publish the new share units during that closure. Quotes timestamped
across a future split boundary are also rejected. The first quote at the opening uses the new units;
a failed split refuses further quotes while its market stays blocked.

Check `corporate_action_scheduled` and the matching `admin_applied` after submission, then
`corporate_entitlements_captured` at the cutoff and `corporate_action_applied` at the opening.
Dividend events identify each account's signed USD movement and FX mark; split events identify the
ratio, followed by each account's cash in lieu and each cancelled order. Replaying the log verifies
these events and their state roots. Reusing an accepted id or submitting a second action of the same
kind at that market opening is rejected. An accepted instruction cannot be cancelled or replaced.

A failed action emits `corporate_action_failed` and a Critical `corporate_action_failed` alert, and
commits no partial economic mutation. A split fails when a market price or entry rounds to zero, a
scaled value overflows, a position quantity would exceed the 1,000,000,000 input bound, or a
rounded-off fraction has no mark or usable FX valuation to settle at. Its market then stops dealing,
including closing trades whose share units would be ambiguous, refuses quotes and further actions,
keeps its listing locked, and stops accruing financing. Nothing in this protocol unblocks it; an
operator repair command is a follow-up. A dividend fails without a usable FX valuation mark or on an
unrepresentable transfer: it pays nothing, and the market keeps dealing. Other markets retain their
normal rules. A refused or replayed operator request consumes no extra entitlement.

Run `./scripts/dev-corporate-actions.sh` for the isolated local scenario. It uses the published mock
operator key, an in-process sequencer with controlled trusted time and a loopback TLS API: long and
short positions plus a resting order, a signed dividend, a signed four-for-one split, a market order
before a new quote, shutdown/recovery and public replay. Its withholding input is synthetic test
data. The CI test job runs this script. V23 activation requires the preceding V19, V20, V21 and V22
implementations and their immutable reference logs; rollback after activation requires a build that
understands V23 or a fresh log. On a node run from `config/dev.toml`, the mock feed rescales each
market's walk by the split ratio for quotes stamped at or after a scheduled split's opening, and a
restarted feed walks a market that has split from its current mark.

## Configuration

`CONFIG_PATH`: path to the node config file, used when `run` or `admin` is given no config flag.

`OPERATOR_KEY`: the operator's private key in hex, for `admin` when no `--key-file` is given.

`OPERATOR_KEY_FILE`: a file holding the operator's private key in hex, for `admin`.

The alert webhook URLs are credentials, so config names the environment variable that holds each
(`alerts.critical.url_env`, `alerts.warning.url_env`) rather than the URL. A URL must be HTTPS
unless it is HTTP to a loopback address, and the node refuses to start when a named variable is
unset. A node whose TEE or custody provider is not `mock` refuses to start without
`[alerts.critical]`, since its Critical alerts would otherwise page no one.

Vendor API keys are credentials too: each `[[feeds.venues.connections]]` entry for Massive,
TraderMade, or Databento names the environment variable that holds its key (`api_key_env`), and a
node with `feeds.provider = "venues"` refuses to start while a named variable is unset or empty.
A Massive connection names one of its `/forex`, `/futures`, or `/stocks` feeds; a Databento one
names its live gateway as `tcp://<gateway>.lsg.databento.com:13000` and the `dataset` it serves; a
symbol for a market priced from futures names the `contract` it quotes, one listed in the market's
`contracts`, and the feed sends quotes for each contract the market takes now:

```toml
[[feeds.venues.connections]]
venue = "massive"
url = "wss://socket.massive.com/futures"
api_key_env = "MASSIVE_API_KEY"
symbols = [{ market = 20, symbol = "ESZ6", contract = "ESZ6" }, { market = 20, symbol = "ESH7", contract = "ESH7" }]

[[feeds.venues.connections]]
venue = "databento"
url = "tcp://glbx-mdp3.lsg.databento.com:13000"
api_key_env = "DATABENTO_API_KEY"
dataset = "GLBX.MDP3"
symbols = [{ market = 20, symbol = "ESZ6", contract = "ESZ6" }, { market = 20, symbol = "ESH7", contract = "ESH7" }]
```

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

# The production build's tests: it refuses the dev config the other tests load
cargo test -p engine-node --features production --lib production_build

# Dev scenario: loopback market-data vendors price FX, index and commodity futures, and a share,
# two vendors each, across a market open and a futures roll, and the log replays
./scripts/dev-rwa-feeds.sh
# Dev scenario: a signed dividend and four-for-one split through the TLS API, restart and replay
./scripts/dev-corporate-actions.sh
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

`src/custody/tests/rpc/staking.rs` checks complete hash-pinned staking log windows against a lagging fork backend.
