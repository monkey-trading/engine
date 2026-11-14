# engine-node

## Overview

The node that runs `engine-core` inside a TEE (an Intel TDX or AMD SEV-SNP confidential VM). It
sequences every command into a public, append-only log, signs state-root checkpoints with an
attested enclave key, serves the client API, streams the log live, indexes each account's history
from the log, and talks to custody and price feeds. For its operator it serves Prometheus metrics
and posts alerts to webhooks. The same binary replays a published log and verifies its checkpoints.

## Features

- Production secrets delivered into the attested enclave: RPC provider keys, vendor keys, and alert
  webhook URLs, named by the config, encrypted to a one-time key the operator's tool first checks
  against the release manifest, signed by the operator key, and sealed per release
- Signed eligibility and fail-closed API/custody screening from V24, with public refusal reasons
  and reserved, uncredited sanctioned deposits
- Dated publisher ingestion with a separate signing worker, startup synchronization, and per-date
  rollover catch-up under V18
- Browser-origin allowlist (`api.allowed_origins`): CORS for the REST API and an `Origin` check on the
  stream, with no origin listed by default
- Single-owner sequencer on a dedicated OS thread: commands are logged and synced to disk before any
  client sees their effect
- Genesis all or nothing: the engine checks every genesis command at config load and again before
  any is written, and recovery refuses a log whose genesis stopped short of its closing checkpoint
  or is not the config's, and one whose genesis no current config can give (rules newer than the
  log refuse it at load), which needs a fresh log
- Signed checkpoints and withdrawals, bound to the chain id and vault, from an attested key
- While rotation is on and a daily turn is in effect whose rotation set names two or more members,
  withdrawals signed only through the head of the log the last boundary's check verified: the
  successor's own at a handover, and at a boundary the operator holds on through, an attested
  verification it asks the turn's order for, from a release that sequenced none of the range
- Strict checkpoint signature verification in replay and recovery, with explicit domains for early logs
- Command-origin seals for new entries, checked against the configured deployment and recovery signer;
  historical unsigned commands require an authorized covering checkpoint before writable recovery. A
  command after the last authenticated checkpoint that the current enclave key did not seal stops
  recovery for operator repair. With `[tee.sealing]` the enclave's keys survive a restart, so a
  drained shutdown ends the log at a checkpoint and restarts normally
- Enclave keys sealed to the release, its launch measurement and config, with the SEV-SNP derived
  key (a public stand-in under the mock TEE), opened only while the vault names that release as
  current, handed to the next release only after the timelock makes it current and each release
  attests the other, and a node the vault does not name as signer serving only its attestation
  until governance registers its key (see "Enclave keys")
- Optional checkpoint relaying, signed-parent vault commitments, startup rollback checks, and collateral
  reconciliation that pauses withdrawal signatures while trading continues
- On-chain log heads with `[log_heads]`: the enclave signs its newest released `(seq, prefix)` — the
  cumulative command-prefix digest, which checkpoints do not reset — off the
  sequencer — the sequencer publishes it on a watch after each durable batch — and a poster relays it
  through the checkpoint endpoint, on a dedicated funded relayer distinct from the checkpoint
  relayer, to the LogHeads contract at most every `log_heads.interval` (an hour by default) and only
  when the head moved; startup reads the posted head through the custody RPC quorum and refuses to
  start on a log that lacks it, naming the missing range or the seq whose link differs (Critical
  `log_rollback`), and a posted head that overtakes the running node, or names another prefix at a
  seq it retains, freezes sequencing; a node with `operator.rotation_enabled` refuses to start without
  `[log_heads]` and raises `log_head_stalled` after two posting intervals, not three, without an advance
- Attestation through Linux configfs-tsm, covering TDX and SEV-SNP, or a clearly marked mock: the
  report data is the SHA-256 of a served bundle of the enclave address, the config file's SHA-256,
  and the TLS key's hash; with configfs-tsm a start refuses, raising `clocksource_unprotected`,
  unless the guest kernel times on the TSC, the clock Secure TSC keeps the host from scaling, in
  the frequency the hardware reports
- A `production` cargo feature that refuses mock TEE, custody, and feed providers, an API
  without `[api.tls.acme]`, and a genesis market quoted by fewer venues than its `min_sources`, at
  config load
- Trusted time: every stamp is checked against signed Roughtime readings that independent servers
  agree on, and nothing, genesis included, is stamped before the first; sequencing halts with a
  Critical alert when the host clock is beyond the drift bound, or 60 seconds after the last
  sample, and resumes once a sample is within the bound; sequenced time keeps pace with monotonic
  time whatever the host clock does, giving up at most 10 ms of it a second, and after a restart
  stamps hold, dropping venue quotes, until that pace passes every quote carried across it
- REST API for orders, cancels, withdrawals, and reads, plus a WebSocket stream of the log
- Derived marks cache: each market's index-mark history served signed at `/v1/markets/{id}/marks`,
  paged per resolution with per-entry `log_derived`/`seeded_from_provider` provenance; recovery
  seeds it from the log's replay, live mark reports keep it current, and an optional provider
  backfill seeds the depth a fresh log cannot have
- Dashboard data: the House and MONKEY pages' series, position maps, MONKEY totals and activity,
  held in memory and rebuilt from the log's replay at start (`GET /v1/stats/series`,
  `/v1/house/positions`, `/v1/monkey`, `/v1/monkey/activity`, `/v1/monkey/distributions`, and more in `/v1/house`)
- Signed answers: every `/v1` response a client relies on carries the attested enclave key's
  signature over the request and its body, the client's challenge, the node's time, which it signs
  only while trusted time bounds it, the log head, and the status and body; a `signed=1` stream
  connection gets each message in a numbered frame
  chained to the one before it, and a signed heartbeat at least every `api.heartbeat_interval`;
  `engine verify-response` and `engine verify-frames` check them with the shared `engine-wire`
  definitions
- Executable quotes: a `signed=1` stream connection can ask for an account's quotes in up to
  `api.quote_max_markets` markets, and gets them again when one changes, at most every
  `api.quote_interval` (at least 100 ms), the first one as soon as the log head moves
- Orders, cancels, and withdrawals signed by the account (EIP-712), checked before they are
  sequenced; from `V41`, a session key the account registered at `/v1/session-keys` signs its
  orders, cancels, and (from `V44`) cancel-all and close-all requests on the key's own nonce sequence, while withdrawals keep requiring the account
  key; before V24, `auth = "none"` trusts the account field with mock TEE and custody; V24
  client actions and eligibility declarations require signatures in every mode
- The latest protocol version and the client auth activated at boot, at the log's next seq, after
  giving parameters a log set before `V11` the config's toxicity notional weighting
- Stream offsets of `<seq>:<index>`: a subscriber starts or resumes at any offset, reads the log
  from disk, then goes live, getting each message once; one that falls behind resumes from disk
- Segmented log: completed segments and a manifest of their seq ranges and SHA-256 hashes are a
  public archive served over HTTP
- TLS terminated in the enclave, with a key generated there and its hash in the attestation
- A public CA certificate for the API's domain, for that same key: ordered over ACME from inside the
  enclave, validated by TLS-ALPN-01 on the API's own listener, and renewed without a restart
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
  the operator's signature, next nonce, and an unexpired expiry; from `V35` the key a logged
  rotation hands over to also passes the route, and signs with `--rotated`; from `V37` that includes
  a key the operator registry elected
- `engine ct-watch`, a separate process outside the enclave that follows Certificate Transparency
  logs and raises a Critical `unattested_certificate` alert for any certificate for the API's domain
  whose key no verified attestation of the node names (`config/ct-watch.toml`)
- Proofs of an account's balance and positions, and of its positions' marks, against the last
  checkpoint's state root, from protocol `V9`
- MONKEY emissions from protocol `V19`: `engine admin init-emissions` records the schedule once,
  and `GET /v1/accounts/{address}/emissions` serves an account's cumulative entitlement with its proof,
  at the checkpoint the vault accepted when one is retained, for the vault's `claimEmissions`; with `[checkpoints]`, the
  node reads the period budget the vault's MONKEY cap was derived from, refuses a schedule with any
  other, and refuses to start on a log that records one
- MONKEY staking from protocol `V25`: `engine admin init-staking` or a genesis `staking` entry
  records the liquidity floor, epoch length, start, and unstake cooldown once; each epoch the
  engine distributes house NAV above the floor to stakers pro-rata, from `V47` less a fifth to the
  operator in effect while rotation is on, from `V50` split among the operators by their time in
  office, and
  `GET /v1/accounts/{address}/distributions` serves an account's cumulative USDC distribution with
  its proof, for the vault's `claimDistribution`; checkpoint
  reconciliation counts the USDC the vault has paid in distributions against the ledger
- The exit against the last posted root from protocol `V38`: `GET /v1/accounts/{address}/exit`
  serves every proof an account exits the vault with against the last checkpoint's root, and
  `engine exit-proof` builds the same from a published log alone, with each vault call ABI-encoded
- Close-spread ceilings from protocol `V40`: genesis adopts the most each market's closes may pay,
  whatever the parameters do, and `engine admin adopt-close-spread-ceiling` adopts a new market's
  ceiling — or lowers one — from seven days after it is logged; `SetParams` is never refused for a
  spread a close would pay over a ceiling, because the fill clamps to it, floored to the exact
  bound
- Supervised tasks: any task stopping stops the node, and SIGTERM drains to a final checkpoint
- Live crypto prices from Coinbase and Kraken over WebSocket: one supervised task per
  venue connection, quotes stamped with the venue's own time, and reconnects with backoff
- Prices only from sources reached over TLS that ends in the enclave: every venue endpoint is
  `wss://` on the venue's own domain, so the host can delay a source's quotes but not alter them
- A bound on a price's age under a hostile host, on the monotonic clock: every venue connection
  pings its venue every `feeds.venues.ping_interval`, and a quote is sequenced only within
  `feeds.venues.max_delay` of the newest ping answered when it was read; `SECURITY.md` states the
  bound
- A quiet venue kept quoting: on Coinbase, Kraken and Bitstamp connections, a pong with no change to
  a symbol's book since one read before its ping sequences that book's mid again, stamped no later
  than the venue's clock when the ping went out, for up to 5 s after the book was read
- Live FX, index and commodity futures, and US share prices from market-data vendors: Massive
  (FX, futures, and stocks feeds) and TraderMade over WebSocket with their API keys, and CME Group's
  WebSocket API for its exchanges' futures, whose handshake carries an OAuth bearer token fetched
  with a delivered client ID and secret and replaced before it expires; quotes for a
  futures contract carry its name, and a feed holds back quotes stamped or sent while their market
  is closed and quotes for a contract the market does not take
- Alerts for refused venue messages (at most one a minute per venue, counting the rest, excerpted
  only from a venue that holds no credential), a bearer token request a venue's token endpoint fails or refuses,
  venue outages, symbols with no fresh quote while their market is open, venue
  times well ahead of the host clock, a venue stream past its delay bound, and a host clock falling
  behind monotonic time
- A `house_deficit` alert when house NAV falls below zero, at most one a minute while it keeps
  crossing zero
- EVM custody: a strict majority of at least three RPC providers admits each deposit; from V20,
  sequencer confirmation credits trading collateral, Ethereum finality unlocks withdrawal principal,
  and canonical reversals debit once with a Critical alert. From V25, the same poll mirrors the
  staking contract's finalised `Staked`/`UnstakeCompleted` events (`custody.evm.staking`) into
  `Stake`/`Unstake` commands once each, holding them until `InitStaking` has applied, and from V37
  the operator registry's `RotationChanged`/`Elected` events (`custody.evm.staking.registry`, which
  must be the registry the staking contract records as its deployer) into
  `SwitchRotation`/`ElectOperator` commands in the same proven blocks, with each scan's finalised
  block time; its event
  reads narrow after an RPC size or timeout failure on a range of their own, apart from deposits'
- Mock custody and a mock price feed for local development
- Prometheus metrics on a separate operator address, from a registry the node owns, with no
  per-account labels or values, bound once for the process, so a member or a standby answers while
  it follows as well as while it sequences
- Alert routing: Critical and Warning alert lines are posted as JSON to a webhook per severity,
  once when an episode starts and once when it is resolved

## Repository Structure

- `src/main.rs` — the `engine` binary: `run`, `replay`, `verify-attestation`, `verify-response`,
  `verify-frames`, `admin`, and `ct-watch`
- `src/lib.rs` — module index
- `src/admin/mod.rs` — `engine admin`: the operator key, the commands it signs, and submission
- `src/admin/command.rs` — the admin command each action sends, built from its arguments and TOML
  files and checked before signing
- `src/admin/benchmark_feed.rs` — the separate signed publisher worker and rollover scheduling
- `src/admin/benchmark_watch.rs` — bounded schedule notifications and reconnection for the worker
- `src/benchmarks/` — official CSV fetching (`mod.rs`), exact decimal parsing (`parse.rs`), compact
  schedule notifications (`schedule.rs`), and tests
- `src/admin/tls.rs` — the TLS connection to a node, pinned to its attested key
- `src/admin/secrets.rs` — `engine admin deliver-secrets` and `secrets-status`: the quote checked
  against the release manifest before anything is encrypted, and the quoted receipt;
  `src/admin/secrets/tests.rs` runs the tool against the listener and against forgeries
- `src/admin/tests.rs` — `engine admin` tests: key sources, commands, signing, and pins
- `src/ct_watch/mod.rs` — `engine ct-watch`: the poll loop over the node's attestation and the logs
- `src/ct_watch/config.rs` — the watcher's config: domain, node, logs, and alert routing
- `src/ct_watch/attestation.rs` — polling `/v1/attestation` and verifying it, or a mock one for dev
- `src/ct_watch/log_client.rs` — the bounded RFC 6962 client: signed tree heads and entries
- `src/ct_watch/leaf.rs` — reading a log leaf's certificate or precertificate, and name matching
- `src/ct_watch/tracker.rs` — attested keys, log positions, grace waits, and the alerts they raise
- `src/ct_watch/state.rs` — the watcher's durable, locked state file
- `src/ct_watch/tests/` — leaf, log client, tracker, state, config, and loopback poll-flow tests
- `src/standby/mod.rs` — the standby: following the operator in effect's signed stream into a local
  copy of the published log, under rotation read again from the registry each stint, the archive
  bootstrap and refresh from it, the takeover's promotion watch, and finalizing the copy at the
  vault's latest checkpoint; `src/standby/stint.rs` is what one stint says about the follow alerts
- `src/standby/chain.rs` — whether the operator's stream extends the copy: a resumed stream repeats
  the copy's last command exactly, each later command carries the enclave's seal over the link that
  continues the copy, and a signed heartbeat never names a head behind it; otherwise the copy takes
  nothing more and the standby raises `operator_log_rollback`
- `src/standby/chain/tests.rs` — against a real sequencer's log, an operator that carries on extends
  the copy, one cut back and restarted is caught at the first entry or heartbeat that shows it, and
  an entry or archive segment that does not chain leaves the copy as it was
- `src/standby/tests.rs` — copy appends that verify frames and refuse a wrong signer, truncation at
  the resume checkpoint, stream resumption at the copy's last command, and a follow that stops at a
  cut-back operator or a heartbeat behind the copy with the copy unchanged
- `src/standby/follow.rs` — one follow stint: the signed stream at the copy's last command, each frame
  verified against the followed enclave, each new entry applied to a replica, when there is one,
  before the copy takes it (`ReplicaRefused` otherwise)
- `src/standby/operator.rs` — the operator a follower follows: under rotation the registry's candidate
  in effect at `https://` its registered API hostname, verified against the vault's signer at
  finality; without rotation the one `[standby]` names; a `--standby` node's, with `[standby]`'s
  mirrors; and the signers a promotion replays the copy against, the vault's recorded ones under
  rotation; `src/standby/operator/tests.rs` covers a turn change, rotation off, a standby and a
  promotion whose configured enclave is not the vault's signer
- `src/standby/replica.rs` — a member's `Follower`: the copy, its seal chain, and its live replica,
  built by one replay as the copy opens and fed every entry the copy takes after, a handover's tail
  included; `src/standby/replica/tests.rs` starts the sequencer from the replica with the copy's files
  altered, and extends the copy with a tail, refusing a forged one
- `src/node/mod.rs` — startup, task supervision, and shutdown; a member's start from its replica
  reads nothing from the chain and takes no trusted time sample, using what it read before connecting
- `src/node/run.rs` — `run_with_cpu`: a member's wait for its turns around each `serve_node`, or one
  `serve_node` for a node that is no member, beside the process's own tasks, one each for its life:
  the metrics listener and the seed relayer
- `src/node/ready.rs` — what a member reads before it connects for its turn, so the window from the
  confirmation to its first command holds no chain read and no time sample: the sealed certificate,
  opened first, the rotation check, the checkpoint setup with the vault's anchor checked against its
  replica, the MONKEY budget, the posted log heads, the turn's generation, and last the trusted time
  sample; `src/node/ready/tests.rs` starts a node from a replica with every chain endpoint and time
  server unreachable, and refuses an altered certificate store before any chain read. Between the
  confirmation and the first command, a start from the replica then runs: the certificate store
  and the keys the handover sealed, attested (one quote); the marks cache opened empty on its seed
  file (no import: `src/node/marks.rs`); the replica finished as the log (`LiveReplay::into_log`, its
  anchor among at most 4,096 retained checkpoints, no replay); the predecessor's head, the posted
  log head and the recovery checks against the finished engine; the history index opened without
  its integrity check, which the member's plan ran when it opened its copy
  (`HistoryPlan::for_member`); the sequencer built from the engine; and the marks cache moved into
  place, its prior file set aside by renames. Each is bounded by the engine's outstanding work or
  costs one file operation, none by the log the member followed or a cache's size. After the
  first command the marks task takes the prior cache and the replica's folded marks
  (`src/history/marks/run.rs`), and the history task indexes the log from where its index
  stopped, `GET /v1/markets/{id}/marks` and the history reads answering 503 until each is done
- `src/node/price_feeds.rs` — the price feed tasks: the mock feed, or one task per venue connection
- `src/node/ticker.rs` — the `Tick` command at `node.tick`, so sessions and rollovers fire without traffic
- `src/node/turn_keys.rs` — the generation the finalised schedule holds for each untaken turn the
  log assigns this node's key, which its `TurnTaken` waits on; `src/node/turn_keys/tests.rs` covers it
- `src/node/handover.rs` — the registry's rotation switch checked against the config at start, and the
  incumbent's election watcher
- `src/node/keys.rs` — this start's enclave keys and their attestation, and the check that the log
  reaches the final checkpoint of the release that handed the keys over, sealing the boundary's log
  check that start makes in the same write; a member sets its copy
  aside (`<log_dir>.aside`, `node::member::before_whole_log_take`) before any whole-log take, a
  turn's or an election's, since its copy's segment files are its own writer's
- `src/node/keys/platform.rs` — the platform a start runs on: the attestor its quotes come from and
  the sealer its keys are sealed under, the mock TEE's stand-in measurement included
- `src/node/keys/tests.rs` — sealed, new, refused, and unsealed keys, the vault's current release,
  the stand-in measurement, the first start's head check, and the release watch, which reads the
  keys' current generation
- `src/node/keys/tests/turns.rs` — a start while rotation is on: a daily turn naming another release
  refuses it, keys sealed in the turn's generation open, a returning holder's restart opens the keys
  it sealed there, a member without them asks the turn's source for them, no turn drawn takes no
  trusted time sample, and a turn drawn with no sample refuses it
- `src/node/recovery.rs` — the log a start runs from, before anything is written: a member's replica
  with no replay, its marks cache's log-derived rows kept and taken after its first command, or the
  log on disk replayed with the
  history index it must match, counted in `engine_full_replays` once it has run, a snapshot start
  that falls back to the whole log included (as are a member's replica builds and the replays of a
  cold handover in `handover::take` and `sync_predecessor_log`); `src/node/recovery/tests.rs`
  counts both
- `src/node/log_check.rs` — a node's part in the boundary's log check: the check the first start after
  a handover makes and seals (none for an upgrade's `--predecessor`, which carries the predecessor's
  verified head), the sequencer's setup with the sealed keys of a node that takes turns, and its
  supervised asking task
- `src/node/member.rs` — a rotation-set member's life: following the operator in effect between its
  turns, answering the operator's log checks on its handover endpoint meanwhile
  (`Member::until_turn_answering`), taking a turn with the tail past its copy's tip, starting from the
  replica, and following its successor once it hands over, a shutdown signal ending a replica's build
  at its next entry;
  `src/node/member/tests.rs` covers the loop between holdings, the metrics listener answering
  through it and through a held future, and the process's own tasks running through it
- `src/node/member/turn.rs` — `Member::until_turn`: what the member holds at trusted time, following
  the operator in effect, re-resolved at each boundary, into a replica bootstrapped from its archive
  when the copy is empty, pinning the posted log head, also as a replica is built, each alert of its
  following paged once per episode and resolved under the fields it paged with (`turn/episodes.rs`:
  `member_follow_blocked` for a step failing two polls in a row), and taking its
  turn with the tail, a stale copy caught up from the incumbent first, rebuilding the replica after a
  failed attempt only when the replica refused an entry or the attempt began changing the copy, and
  following for a stint before retrying, each attempt's lines in a `take{turn}` span; each source's
  refusal of the tip pages naming the turn, and each is classified, a later source's failure hiding
  none (`turn/tries.rs`), and after three refusals, or five minutes of failures no source refused
  (the start's reads, or no source answering), it leaves its turn to a whole-log start; after a
  confirmed handover it never
  holds again the generation it gave away, and its trusted reach, and the next start's, never falls
  below what it had reached then; `src/node/member/turn/tests/` drives it against stand-in registry,
  operator, archive, heads and time, `take.rs` there its tries at the turn, a take that changed the
  copy rebuilding the replica, against `src/handover/stand_in.rs`, `episodes.rs` its alerts'
  episodes, and `stop.rs` a shutdown during a replica's build
- `src/node/secrets.rs` — this start's delivered secrets: opening the sealed bundle, asking a
  predecessor, holding for what the node cannot run without, setting the rest into the config and
  the alert routes, `secret_missing`, and the listener handed back to a member's own operator key
  once its run hands the keys over; `src/node/secrets/tests/` covers each, and
  `src/node/secrets/tests/wiring.rs` drives `run_with_cpu` over a log that recorded a rotated
  operator: the listener refuses the configured key and takes the recorded one
- `src/node/registration.rs` — serving only the attestation until the vault names the key
- `src/node/shutdown.rs` — supervising the tasks until one stops, a signal arrives, or a handover
  stops sequencing, which commits only once the sequencer admits its final checkpoint; then ending
  history, and waiting for a successor to confirm a handover
- `src/node/tests.rs` — startup, history recovery, and incomplete-genesis tests
- `src/node/preflight/unscanned.rs` — unprovable prelaunch anchors refuse writable startup before changing log or history
- `src/node/preflight/tests/origin.rs` — integrity refusal preserves absent, corrupt, and WAL-backed history
- `src/telemetry.rs` — the log subscriber, which feeds alert lines to the router, and the metrics
  registry, set up before the node runs
- `src/sequencer/mod.rs` — the sequencer, checkpoints, and metrics
- `src/sequencer/config.rs` — the sequencer's construction config
- `src/sequencer/pipeline.rs` — group commit: a batch's data sync runs on the `log-sync` thread while the next requests are sequenced, and each batch's replies, stream messages, signed withdrawals, mark reports, house reads, stats jobs and checkpoint posts are released in batch order once its sync is done; a segment rotation's seal — the completed segment's fsync, its durable manifest listing, and the staged next segment's exclusive link — rides the batch that crosses the boundary, publishing before anything of the new segment is released; the released `(seq, link)` head goes to the log-heads poster on a watch; a failed sync stops the sequencer, and one held past 5 s raises Critical `log_sync_held` and takes no request until it finishes
- `src/sequencer/checkpoints/publish.rs` — a signed checkpoint reaches the poster, and its approvals the nonce-use queries, only once its batch is durable
- `src/sequencer/checkpoints/publish/tests.rs` — with the syncs on their own thread, a checkpoint and its approvals wait for their batch's sync, a failed sync posts nothing, and a post owed a queue slot defers the next checkpoint
- `src/api/state/durable_tests.rs` — a proof, a dashboard view and a fallback view are answered only once the log holds the state they read
- `src/sequencer/pipeline/tests.rs` — a batch leaves only after its sync and the next is sequenced meanwhile, a failed sync releases nothing and stops the sequencer, and a full pipeline takes no request
- `src/sequencer/pipeline/hold.rs` — waiting on the log-sync thread: no request, new, drained, or received past the deadline, while the oldest batch's sync is held past 5 s, and every wait on a sync while the sequencer runs waking at that bound to raise Critical `log_sync_held` once and resolve it
- `src/sequencer/pipeline/hold_tests.rs` — the held-sync bound: one page, nothing stamped, a quiet node and a settling one paged at the deadline, a request arriving past it kept, and judged again when a younger batch crosses the bound while the resolution is written, a finished sync never paged, and an earlier run's episode resolved
- `src/sequencer/wait_bench.rs` — what a price waits behind at 100k accounts, per occurrence, and the channel-to-stamp wait budget under reads and checkpoints
- `src/sequencer/messages.rs` — sequencer requests, stream messages, and signed withdrawals
- `src/sequencer/checkpoints.rs` — checkpoint creation, custody reconciliation, and signature release
- `src/sequencer/claims.rs` — MONKEY and USDC claim proofs retained for recently asking accounts at each new checkpoint, so one is served at the checkpoint the vault accepted
- `src/sequencer/payouts.rs` — durable unpaid delivery, finalized claim evidence, epoch reissues, reissue requests, the account payouts read, and the boundary's log check's hold on each release
- `src/sequencer/log_check.rs` — the sequencer's side of the boundary's log check: the verified head, restored from the sealed keys only when the log holds its checkpoint, the log's `TurnTaken` records attributing each range, the check due once this node holds a turn on that its verified head does not reach (with the checkpoint it writes after its `TurnTaken`), a member's verification taken only for the check due and from a release outside its range, and the boundary an unpaid approval waits for; `src/sequencer/log_check/tests.rs` covers a held turn's check, its target whether the checkpoint or the `TurnTaken` is noted first, a one-member set, rotation off, a reissue on a signer change and on request, a restart, and a handover's check
- `src/sequencer/payout_schedule.rs` — complete unpaid approval order, bounded epoch and requested reissue scheduling, and the hold behind unloaded recovered approvals
- `src/sequencer/payout_tests.rs` — release order across claim pages, durability, restart, expiry without reissue, and rotation
- `src/sequencer/payout_tests/` — the account payouts read across signing, reissue, payment and an unfinished restart load, paginated replacement signature order, paused claim acknowledgement, sealed approval recovery, FIFO signing across unloaded recovery windows, expired authorizations a rotation leaves unsigned, and no issuance or reissue while the vault pauses withdrawals or has a retired signer
- `src/sequencer/withdrawal_scenario/` — actual Anvil/TLS/stream/custody crash, FIFO, and guardian pause and signer retirement scenarios with local signed time,
  the enclave key lifecycle scenario (`keys.rs`), and the elected operator's handover (`handover/mod.rs`), ending with the
  operator in effect's upgrade while rotation is on (`handover/upgrade.rs`)
- `src/sequencer/withdrawal_scenario/` — actual Anvil/TLS/stream/custody crash, FIFO, and guardian pause and signer retirement scenarios with local signed time
- `src/sequencer/payout_tests/` — paginated replacement signature order, paused claim acknowledgement, sealed approval recovery, FIFO signing across unloaded recovery windows, and expired authorizations a rotation leaves unsigned
- `src/sequencer/withdrawal_scenario/` — actual Anvil/TLS/stream/custody crash and FIFO scenarios with local signed time,
  and the exit against the last posted root (`exit.rs`)

- `src/withdrawals.rs` — vault EIP-712 digests and exact approval inclusion verification
- `src/api/withdrawal_queue.rs` — public FIFO pagination and account queue positions
- `src/api/reissue.rs` — a holder's screened request to sign an expired approval again
- `src/api/payouts.rs` — an account's unpaid payouts and their latest signed authorizations, read from the sequencer's memory
- `src/node/signer_history.rs` — finalized accepted roots and immutable signer epochs before recovery writes
- `src/replay_audit.rs` and `src/replay_audit/` — read-only custody-backed historical signer audits and tests
- `src/log/segments/durability_tests.rs` — failed-fsync stream publication regression
- `src/test_support/readers.rs` — stand-ins for the marks reader and the stats record an `ApiState` holds
- `src/test_support/written.rs` — shared in-memory test output capture
- `src/sequencer/checkpoints/hold_back.rs` — holds the next checkpoint back while the last signed one is not yet final, and raises `checkpoint_backlog` when none becomes final
- `src/sequencer/checkpoints/handover.rs` — a key handover's final checkpoint, and its admission at the
  checkpoint gate
- `src/sequencer/checkpoints/accounting_tests.rs` — live/recovery accounting oracles and gated 100k-account capture scaling over migrated credits and fresh holds
- `src/sequencer/checkpoint_tests.rs` — confirmed-prefix signature gating and restart tests
- `src/sequencer/checkpoint_tests/unvalidated.rs` — worker observations preserve signing holds until collateral is compared, and an unproven finalized prefix pauses withdrawals until coverage is proven
- `src/sequencer/checkpoint_tests/bootstrap.rs` — recovered empty anchors reconcile while nonempty unscanned checkpoints wait
- `src/sequencer/checkpoint_tests/hold_back.rs` — the cadence waits for finality, held approvals share the next checkpoint, and the gate's alerts, a handover, a drain and a restart keep working behind a hold
- `src/sequencer/checkpoint_tests/backlog_alert.rs` — when `checkpoint_backlog` fires and resolves: the stall bound, a forced final checkpoint (handover, drain, failed seal) leaving a stall open, and an episode open across a restart
- `src/sequencer/checkpoint_tests/slow_poster.rs` — the real sequencer and worker in paused time over a chain with thirteen-minute finality: queue depth, withdrawal waits, and a stalled poster
- `src/sequencer/checkpoint_tests/handover_stop.rs` — a handover's stop is the gate's call: refused
  with its causes while it would withhold the final checkpoint, and only an admitted stop ends the
  sequencer's loop
- `src/sequencer/fixture_tests.rs` — test-only signing of copies of placeholder protocol fixtures,
  and logs whose genesis an older protocol applied but no current config can give
- `src/sequencer/recovery.rs` — pure recovery validation and exact authenticated checkpoint-tail restoration
- `src/sequencer/origin.rs` — command-origin coverage shared by replay and writable recovery
- `src/sequencer/replay.rs` — log replay, which recovery and `engine replay` share
- `src/sequencer/replay/snapshot.rs` — replay restored from a verified state snapshot
- `src/sequencer/replay/replayed_log.rs` — a log read and replayed before anything is written, or a
  member's copy whose writer is already open, and the head link and prefix ring the log-head poster
  reads from it
- `src/sequencer/replay/live.rs` — a replay kept live for a member's replica: each checkpoint's custody
  work and the latest commands' cumulative prefixes retained, and the finish recovery's replay of the
  same log anchored at the vault's finalized checkpoint would give; `src/sequencer/replay/live/tests.rs`
  compares both with a full replay
- `src/sequencer/prefixes.rs` — the cumulative command prefix after each of the latest commands the
  sequencer applied, which a handover checks a successor's tip against
- `src/sequencer/house.rs` — the latest checkpoint, the seq of the latest `SetParams`, signed, unsigned or voted, and the pending parameter and listing notices, which `GET /v1/house/state` answers from, with the mirrored votes
- `src/sequencer/governance.rs` — the `ApplyProposal` this node sequences for each passed change whose seven days have run
- `src/sequencer/tests/house_state.rs` — the house state before any command, as parameters are set and replaced, as checkpoints are logged, with a pending parameter or listing notice, and after a restart
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
- `src/snapshot/progress.rs` — the genesis, activation, checkpoint and latest-`SetParams` facts a snapshot carries
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
- `src/log/commitment.rs` — commitments to the complete typed command/event prefix, and the bounded
  ring of a log's (seq, cumulative prefix) pairs the log-head poster adopts on-chain heads against
  and the sequencer checks a successor's tip against, as many as a replica retains, whether a start
  replays the log or starts from a replica
- `src/log/suffix.rs` — recovery that replays only the suffix after a catalog-located checkpoint
- `src/log/truncate.rs` — takeover truncation of a standby's copy at the checkpoint a promotion
  resumes from, dropping the unsigned tail
- `src/log/writer/mod.rs` — appends, flushes, segment rotation, catalog folds, and catalog collection
- `src/log/writer/seal.rs` — sealing a completed segment: its fsync, durable manifest listing, and
  the staged next segment's exclusive link, inline or deferred as a `SealWork` riding the pipeline
- `src/log/view.rs` — manifest accessors over full listings and the lazy catalog
- `src/log/archive.rs` — completed-segment bytes for the archive, checked against the catalog
- `src/log/append_integrity.rs` — reopening the active segment for append only after its prefix rechecks
- `src/log/authenticated_tests/` — catalog, suffix, continuation, active-segment, stream, and
  publication tests
- `src/node/paired_tests/` — paired recovery through node startup: restart and fallback, crash
  recovery, the restart dev scenario (`restart.rs`), the on-chain anchor preflight (`anchor.rs`), a
  posted log head under the snapshot base bound to the on-disk prefix, and the finalized
  checkpoint's stamp where none is posted (`log_heads.rs`),
  custody receipts and their history across a cache restart (`custody.rs`), publication faults,
  public history pages, and the harness
- `src/sequencer/turns.rs` — the `TurnTaken` this node sequences first in each daily turn the log
  assigns its operator key, once its sealed keys are in the generation the finalised schedule holds
  for the turn, sealed there first when the schedule names this release afresh;
  `src/sequencer/turns/tests.rs` covers a returning and a continuing holder, and a failed seal
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
- `src/sequencer/benchmark_schedule.rs` — changed rollover notifications, publisher synchronization when due, and
  the end, at its next stamp, of a hold no snapshot can end while an installation supersedes the log's key
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
  records; recovery under the key the registry installed, and benchmark holds while it supersedes the
  log's
- `src/sequencer/tests/operator/hold.rs` — which keys end a benchmark hold: a recorded key the
  registry seats again over a due rotation, an installation's own due key, and a rotation back to a
  formerly installed key
- `src/sequencer/tests/operator/governance.rs` — parameter votes: the config's vote contract recorded
  once and another refused at recovery, a passed change applied at the first stamp past its notice and
  behind a benchmark snapshot, the access policy a V56 start leaves to a vote, a voted relisting ending
  a retired benchmark's alerts, and a waiting ceiling adoption in `/v1/house/state`
- `src/sequencer/tests/rotation.rs` — an operator rotation through the sequencer, and recovery after it
- `src/sequencer/batch_tests.rs` — batching: a batch ends after `MAX_BATCH` requests of any kind
- `src/sequencer/margin_tests.rs` — the margin and solvency dev scenario on the dev genesis (a
  close-out after the rollover's financing, and a withdrawal limited at a stale market's last
  mark), and a genesis with no house capital that trades and alerts on a house deficit
- `src/sequencer/metrics_tests.rs` — the sequencer's metrics and alert-line tests
- `src/sequencer/quotes.rs` — sequencing a venue's quote only while the bound on its age holds
- `src/sequencer/pace_tests.rs` — sequenced time against a stalled host clock, a held-back sequencer,
  and a restart: a held quote ages out on monotonic time
- `src/log/mod.rs` — the JSON-lines log format, stream offsets, and the single-file reader
- `src/log/command_seal.rs` — domain-separated canonical command/event seals and predecessor links
- `src/log/segments.rs` — segment files, rotation, the manifest, and the recovery read, a segment
  at a time, with its checks
- `src/log/reader.rs` — reads the log from any offset while it is being written, up to the last
  entry the writer has flushed
- `src/log/checked.rs` — reads a completed segment in chunks checked against its listing
- `src/log/tests/mod.rs` — log format, segment, and writer tests
- `src/log/tests/durability.rs` — writer durability and rotation: per-batch sync, reopened segments,
  staging, opening the log while a writer rotates, and a deferred rotation seal publishing with its
  sync, chaining within one sync, counting a seal riding an unfinished sync, never covered by group
  commit, and recovering from a crash before the seal as a crash mid-rotation does
- `src/log/tests/reader.rs` — reader tests: offsets, following the writer, flushed entries only,
  and the checks on what it reads
- `src/api/mod.rs` — REST routes, read handlers and the request span
- `src/api/state.rs` — request authentication, nonce checks, the session-key admission a
  registered key's request falls back to, and durable sequencer round trips
- `src/api/actions.rs` — signed client actions, session-key registration and revocation, and
  real-peer access screening
- `src/api/access.rs` — reads of one custody hold, one pending deposit, and an account's signed eligibility
- `src/api/access_tests.rs` — a declaration crediting its pending deposit, peer provenance and public access refusals
- `src/api/access_tests/admission.rs` — admission before sequencing: never-credited signers declare within per-client and shared budgets; declarations leave orders a screening slot; expired requests and exhausted screening capacity refuse without a nonce
- `src/api/access_tests/batch.rs` — `POST /v1/orders/cancel-all` and `/v1/positions/close-all`: one screening request, one log entry and one nonce each, refusals for a stale nonce, a forged or altered request and a stranger, and a session key signing both forms until revoked
- `src/api/access_tests/cancel.rs` — a client cancels its resting order while geo access is excluded, with no screening call
- `src/api/access_tests/declarations.rs` — a declaration screened like an order: public refusals that leave its deposit pending until an allowed one is recorded
- `src/api/access_tests/pre_deposit.rs` — a wallet's setup before its first deposit: the order of its declaration and session keys, what stays refused until a deposit credits it, both deposit orders, engine refusals, and the reads and scans it meets
- `src/api/access_tests/pre_deposit_budgets.rs` — what those requests spend: a token from the client's share and the shared budget, nothing when refused before the log, what a refused wallet sees, and a pending-deposit wallet's declaration past an empty budget
- `src/api/access_tests/reissue.rs` — a reissue request screened like a withdrawal, by account and peer, refused with 403 before the log, and a request for an approval that does not exist refused with 404 before any screening call
- `src/api/access_tests/session_keys.rs` — a registered session key trades for its account without the wallet until revoked, a withdrawal it signs refuses as out of scope in the log, an expired, mis-scoped, or rate-flooding key is refused before sequencing, and a never-credited account registers one only after declaring, with its cancels, stale nonces, and missing signatures
- `src/api/access_tests/source.rs` — the mock screening source the access tests answer with
- `src/api/access_tests/spreads.rs` — the `V52` dev scenario: fills paying their bands on the open
  and the close, the signed quote stream's closing steps, refused and noticed band changes, ceiling
  floors and a bound first listing, through the signed admin route over TLS, and replay
- `src/api/access_tests/holding_fee.rs` — the `V55` dev scenario: closes paying the profit fee their
  hold gives, an add and a flip moving the open time, a close-out of a gaining and a losing position,
  the account and house reads, a refused cut and a noticed raise over TLS, from `V57` an add after a
  noticed curve extension clamped at the launch curve's last hold, replay and a restart
- `src/access.rs`, `src/access/tests.rs` — bounded signed-source IO and failure-path tests
- `src/api/admin.rs` — the operator's admin route, checked before it is sequenced
- `src/api/distributions.rs` — an account's cumulative USDC distribution with its proof, at the
  checkpoint the vault accepted when one is retained, for the vault's `claimDistribution`
- `src/api/exit.rs` — every proof an account exits the vault with, against the last checkpoint's root
- `src/exit.rs` — `engine exit-proof`: a published log's commands applied through one checkpoint,
  the exit proofs against its root, and the vault calls that carry them, ABI-encoded
- `src/api/benchmark_schedule.rs` — capped, bounded watches of the durable rollover schedule
- `src/api/benchmark_tests.rs` — deterministic publisher/TLS/API restart and financing dev scenario
- `src/api/benchmark_tests/operator_bounds.rs` — the operator bounds dev scenario: rates inside and outside the cap, the worker signing around a refusal, the stress floor's limit and lapse, restart and replay
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
- `src/api/stream/signed.rs` — signed stream connections: numbered frames chained under the
  client's challenge and the opening request, and heartbeats naming the log head
- `src/api/stream/quotes/mod.rs` — a connection's quote subscription: the `quotes` op, one bounded
  sequencer read per interval while the log head moves, and only the quotes that changed
- `src/api/stream/quotes/ladder.rs` — one account's quotes in one market from `Engine::quote`: a
  price at each spread tier a close crosses, then per spread tier up to the largest order the
  account can open, and the closing price
- `src/api/stream/quotes/tests.rs` — the subscription against a stand-in sequencer under paused
  time: pacing, spreading, replacement, refusals, a full queue, a stopped log, and the ladder search
- `src/api/stream/quotes/api_tests.rs` — signed quotes through the API against a real sequencer
  that fill at their price until the index moves
- `src/api/stream/quotes/bench.rs` — the ignored benchmark of a quote read's time on the sequencer's
  thread
- `src/api/stream/signed/tests.rs` — signed frames from offset zero bound to their opening request,
  quiet-log heartbeats, a backfill read never cut short by a heartbeat, a connection closed while
  trusted time does not bound the host clock, and refused parameters
- `src/api/signing.rs` — the response signing layer and the routes it leaves unsigned
- `src/api/signing_tests.rs` — what a response signature covers, which responses carry one, the
  time they carry under a skewed host clock, and the node signing the `engine-wire` vectors byte
  for byte
- `src/api/signing_bench.rs` — the ignored cost benchmark: one frame, a signed account read, and a
  signed fan-out at `api.max_subscribers`
- `src/api/archive.rs` — the archive routes: the manifest and completed segments
- `src/api/attestation.rs` — the attestation routes: the boot attestation and its relayed vendor collateral
- `src/api/stats.rs` — the dashboard routes: `/v1/stats/series`, `/v1/house/positions`, `/v1/monkey`,
  and `/v1/monkey/activity`, read on the sequencer's thread right after the batch they arrive in is
  written, and answered once the log holds what they read
- `src/api/stats_tests.rs` — those routes through the router: refusals, the 503 before the first scan
- `src/api/auth_tests.rs` — signed-request API tests: signatures, nonces, expiries, refused
  accounts, and racing requests
- `src/api/origin.rs` — CORS headers and preflight for the allowed browser origins, and the stream's `Origin` check
- `src/api/origin_tests.rs` — listed, unlisted, and absent origins on reads, a POST, a preflight, a 429, and the stream
- `src/api/history.rs` — the account history route: its parameters and limits
- `src/api/closeout_tests.rs` — the account read serving a position's close-out price, and none for a flat account
- `src/api/claim_tests.rs` — MONKEY claim proofs while the vault lags: served at the accepted checkpoint, an entitlement that grew since, no accepted checkpoint, a restart, and a change of signer
- `src/api/proof_tests.rs` — account proof and emission entitlement API tests, including the state
  tree dev scenario: 100k accounts, checkpoint work, a proof checked against its logged root, and
  older logs replayed
- `src/api/tests.rs` — API tests, including the dev scenarios of the resumable stream and archive,
  and of account history, and `Retry-After`
- `src/api/tests/catalog_archive.rs` — archive responses over the lazy catalog
- `src/api/tests/collateral.rs` — the attestation collateral route's 404, 503, and served body
- `src/history/mod.rs` — the history index: which events name an account, the startup check
  against the log and the rebuild when it fails, and the supervised task that follows the log into
  the index
- `src/history/store.rs` — the index's SQLite schema, its writer, its page reader, and its discard;
  `src/history/store/tests.rs` covers them
- `src/history/authenticated/` — dense account/kind lists, complete cursor pages, chronological expiry,
  state proofs, and their tests
- `src/history/reader.rs` — legacy/authenticated page dispatch, version leases, and corruption-to-repair signals
- `src/history/reader/tests.rs` — damaged pages withhold the generation; busy storage and bad requests do not
- `src/history/rebuild.rs` — exact typed-prefix witnesses for log-derived migration and repair
- `src/history/startup.rs` — the authenticated history plan: read-only until preflight, then activation
- `src/history/runtime/` — the supervised history owner: indexing, checkpoint sealing, and repair
  (`mod.rs`), completing a capture into the paired cache (`capture.rs`), and signed heads,
  retained-version pins, and collection (`heads.rs`)
- `src/history/marks/mod.rs` — the derived marks cache: resolutions, per-resolution retentions,
  provenance, and the recovery seeder
- `src/history/marks/store/setup.rs` — opening the cache: schema creation and checks,
  discarding a file that fails them, and validating imported chains before serving
- `src/history/marks/store/mod.rs` — the cache's SQLite schema, its writer (per-resolution
  pruning, seeding that never overwrites log-derived rows, backfill markers), and its page reader
- `src/history/marks/store/auth.rs` — the row tag, the coverage a backfill recorded, and the
  committed state the writer and readers share: each pair's floor and head anchors and the pinned
  rows inside them
- `src/history/marks/store/chain.rs` — the chained index over the cache: each row names the bucket
  below it and the tag that row carried, the writer re-links only over its own deletes, refuses
  the rest and any row that is not the version it last wrote, and pins the rows it rewrote, and
  the reader checks every link, pin, and chain head before serving; the prune plans from the
  committed anchors and reads and deletes each pair by primary-key prefix (`tests/chain.rs`,
  `tests/refusals.rs`, `tests/replay.rs`: a replayed row refused at serve time and by every
  write, `tests/prune.rs`: the prune's query plan, and a read racing a commit)
- `src/history/marks/store/merge.rs` — the live write path: merging a mark into its bucket and
  re-linking the row above a fresh insert, with the statements prepared once per write and shared
  by every sample (`store/chain.rs` holds the shared chain queries; `store/tests/bench.rs` is the
  write's ignored cost-per-sample timing)
- `src/history/marks/store/reset.rs` — the recovery reset: validating every chain, deleting the
  log-derived buckets, and re-chaining the survivors under a new run key
- `src/history/marks/store/import.rs` — importing a previous run's rows (every row that verifies
  under that file's run key, log-derived included, which a full recovery's reset then deletes) and
  backfill markers into the seed file, re-tagged under this run's key; what cannot be
  authenticated is skipped
- `src/history/marks/store/catch_up.rs` — a member's start from its replica taking its earlier
  history after its first command: the prior cache imported, validated (one that fails is cleared)
  and discarded, then the replica's folded marks merged 50,000 buckets a write, its readers
  answering not ready until it is done
- `src/history/marks/backfill.rs` — the provider backfill: which feed symbols map to a candles
  adapter, the public candles client, and window paging (`tests.rs`)
- `src/history/marks/run.rs` — the supervised marks task: a member's catch-up (`store/catch_up.rs`)
  on a blocking thread beside the sequencer, the live reports taken into the buffer meanwhile so
  the 4,096-sample queue drops none and written only after it, then flushing live mark reports and
  backfilling listed markets one at a time
- `src/history/marks/fold.rs` — a member's marks while it follows another holder, with no key to
  write the cache: its replica's mark changes folded into per-resolution OHLC buckets, each
  resolution pruned to its `[history.marks]` retention as marks arrive (about 100 bytes per market
  and second at 1s, some 17 MB per market at the default 48 h, and as much per bucket at each
  coarser resolution; no raw sample kept), and merged into the cache as `log_derived` buckets
  (`Store::observe_buckets`) once the node starts from the replica; `src/history/marks/fold/tests.rs`
  checks the bound, and `src/history/marks/task_tests/fold.rs` that the merged buckets equal
  recovery's for a first turn and a return after another member's turn
- `src/stats/mod.rs` — the dashboard record: its settings, the lock, recovery's restart and finish
- `src/stats/series.rs` — the bucketed series: metrics, kinds and units, retention pruning, paging
- `src/stats/record.rs` — what each applied command adds: volume, levels, totals, activity, scans
- `src/stats/snapshot.rs` — the scan of every account: open interest and margin, long and short, the
  close-out and profit maps; light for the scans a replay does not serve
- `src/stats/background.rs` — the scan's worker thread: a copy of the engine that applies every command
  the sequencer applies once its batch is durable, so a scan reads the sequencer's state at its seq without
  holding the sequencer, and the API's views queue behind those commands on the same copy
- `src/stats/closeout.rs` — the estimated close-out price of one position, other positions at their
  marks, across margin bands and for the market that converts its own currency (`closeout/tests.rs`),
  and each position's close-out in the account read (`tests/closeout_price.rs` checks it against the engine)
- `src/stats/activity.rs` — the bounded feed of loss allocations, stakes and unstakes
- `src/stats/view.rs` — the answers built from the record and the engine
- `src/stats/bench.rs` — the ignored cost benchmarks at 100k accounts: a scan, `observe`, and each command's
  cost across a scan interval with the scan inline and handed to the worker, the worker's copy
  of the engine in memory, and how far the worker falls behind at 1000 and 5000 commands a second
- `src/stats/tests.rs`, `src/stats/tests/` — the record against the engine: volume and levels, the
  close-out map and each position's close-out price against engine close-outs, the account read's
  cost (`closeout_cost.rs`, ignored), MONKEY totals and epochs, bounds, recovery
- `src/sequencer/replay/stats.rs` — the replay's sink into the record
- `src/history/marks/task_tests/` — recovery seeding from a short log, live maintenance through
  the sequencer's reports, a member's catch-up dropping no live sample while it merges a day of
  folded marks (`catch_up.rs`), and a cache write fault during a backfill stopping the task
- `src/history/tests/mod.rs` — history index tests: pages, filters, retention, following the log,
  and restarts
- `src/history/tests/recovery.rs` — startup checks against the log, readiness to serve, and
  failures
- `src/history/tests/rebuild.rs` — power losses and other damage: an index left behind the log
  resumes, and one ahead of it, contradicting it, damaged, or pruned under another retention is
  rebuilt
- `src/test_support/mod.rs` — the shared test harness
- `src/test_support/alerts.rs` — the capture of alerts, and of the lines that resolve them, for
  the tests that assert them
- `src/test_support/events.rs` — one sample of every engine event
- `src/test_support/log_view.rs` — publishes a log directory's manifest for tests
- `src/test_support/tests.rs` — the time refresher's one sample per second and its end with the queue
- `src/test_tracing.rs` — the tracing setup the test collectors share: one process-wide
  interested subscriber, so no callsite can be cached uninteresting and a collector never
  silently misses an alert or line
- `src/tee/clock.rs` — the start check that the guest kernel times on the TSC, in the frequency
  the hardware reports; the attestation and its bundle are `engine-verify`'s
- `src/tee/mod.rs` — the enclave key, signatures, and attestation providers; the attestation and
  its bundle are `engine-verify`'s
- `src/tee/digests.rs` — the digests the enclave key signs: withdrawal
  authorizations, checkpoint roots with and without their exact parent, and the on-chain log
  heads with the parent head they extend, each mirroring its contract's packed encoding
- `src/tee/certificates.rs` — the member's sealed certificate, per release and apart from the keys,
  found only for the domain, TLS key, and ACME directory it was issued for, and what the host can do
  with its file; `src/tee/certificates/tests.rs` covers it
- `src/tee/keys.rs` — the key set a restart keeps and an upgrade hands over: the signing key, the
  TLS key, the ACME account, the predecessor's final checkpoint, and the head the last boundary's log
  check verified, and an earlier release's certificate, read but never handed over
- `src/tee/sealing.rs` — the sealed key file, AES-256-GCM under the platform's derived key, the
  reseal into a daily turn's new generation, and the mock TEE's public stand-in for sealing;
  `src/tee/sealing/tests.rs` covers it
- `src/tee/release_file.rs` — a file sealed to a release alone, under a tag naming its kind: the
  secrets' and the certificate's
- `src/tee/secrets.rs` — the sealed secrets file, per release and apart from the keys, and what the
  host can do with it; `src/tee/secrets/tests.rs` covers it
- `src/tee/snp.rs` — the SEV-SNP sealing key and report through `/dev/sev-guest` (Linux)
- `src/seed_relay/mod.rs` — the seed relayer: each poll reads the seed receiver at a block of the
  custody credit tag, raises `seed_missing` and `seed_relayer_unfunded`, and relays the upcoming turns'
  seeds the receiver lacks, one transaction in flight at a time, raising `seed_relay_failed` when it
  cannot; `src/seed_relay/tests.rs` and its `tests/` modules cover its decisions over fake chains, and
  `src/seed_relay/tests/scenario.rs` is the two-Anvil dev scenario `scripts/dev-seed-relay.sh` runs,
  whose stand-in Ethereum, beacon node, bridge and relayer config also seed the daily rotation scenario
- `src/seed_relay/settle.rs` — which transaction the relayer's next send replaces, and at which nonce;
  `src/seed_relay/balance.rs` — the relayer's balance, its funding alert and its unreadable alert
- `src/seed_relay/abi.rs` — the relay's, receiver's and Inbox's ABIs
- `src/seed_relay/chains.rs` — the node's chains behind the relayer: Ethereum and the beacon node
  through the relayer's endpoints, and Arbitrum through the custody providers
- `src/seed_relay/beacon.rs` — the beacon API: the finalized header and its SSZ state, bounded in
  time and size
- `src/seed_relay/ethereum.rs` — Ethereum through a strict majority of the relayer's providers, and
  sends through its signing endpoint, replacing a pending relay at its nonce;
  `src/seed_relay/ethereum/tests.rs` runs it over stub endpoints
- `src/seed_relay/header.rs` — the beacon block header and its SSZ root
- `src/seed_relay/schedule.rs` — `SeedSchedule`'s turns, draw deadlines and seed epochs
- `src/seed_relay/ssz.rs` — SSZ merkleization and branches; `src/seed_relay/ssz/tests.rs` covers them
- `src/seed_relay/state.rs` — the Fulu `BeaconState`'s fields, and the branches to its fork version
  and an epoch's mix; `src/seed_relay/state/tests.rs` covers them, with mainnet fields and their roots
  from `tests/fixtures/seed-relay-state-fields.json`
- `src/secrets/mod.rs` — delivering secrets: who may, what binds a bundle, and what a host can do at
  each step
- `src/secrets/bundle.rs` — the bundle, the operator's signature over it, and what a config can use
- `src/secrets/delivery.rs` — the delivery's messages, what each quote binds, and the encryption
- `src/secrets/listener.rs` — the enclave's side: an offer per connection, the operator key in effect,
  and the seal; `src/secrets/tests.rs` runs it over loopback from the host's side,
  `src/secrets/listener/tests.rs` covers a real enclave's refusal of a published signer and the
  connection cap, and `src/secrets/tests/leaks.rs` checks where a value must never go;
  `src/secrets/bundle/tests.rs` covers what the signature binds and a file that does not parse
- `src/handover/mod.rs` — the handover's framing, report data, HPKE encryption of the keys,
  the vault's current release check that a start and a handover share, the registry's handover
  source an elected successor resolves the incumbent from, and the predecessor's published-log
  fetch that precedes it
- `src/handover/messages.rs` — the handover's wire messages: the predecessor's challenge, the
  successor's request, the predecessor's answers (the keys, a refusal, a tail refused before its
  stop, a confirmation, the secrets), and the successor's confirmation
- `src/handover/evidence.rs` — a peer's quote and its collateral, produced and verified
- `src/handover/successor.rs` — the new release's side: request with the restored tip and the newest
  protocol version its build reads, which the predecessor checks before it stops, verify, take
  every log segment past it, each bound to its listing inside the keys, require the chain to reach
  the handed-over checkpoint, seal, confirm
- `src/handover/tail.rs` — the tail-only handover: a following successor's tip and the protocol
  versions it reads, bound in its report data, and the version check every request for the keys
  passes; the entries past the tip the predecessor reads from its
  log and lists inside the keys; the successor's chain, schedule and signer reads before it connects,
  its checks of the tail against its copy, the head and the posted log head, and a predecessor's
  refusal before its stop (`TailMiss`: a tip not on its log, one older than it retains, or a tail
  over its bound), which never sends whole segments onto a member's copy, its segment files being
  its own writer's; `src/handover/tail/tests.rs` covers the pieces
- `src/handover/stand_in.rs` — a scripted stand-in predecessor for tests outside the handover: the
  keys with a given tail, attested as a predecessor attests them, or a refusal of the tip
- `src/handover/published.rs` — what the sequencer has published since the start, which becomes a
  handover's final head, with the incumbent's last stamp logged
- `src/handover/predecessor.rs` — the running release's side: its listener, the current release, the
  stop, the final checkpoint, and the sealed tail segments it sends after the keys;
  `src/handover/predecessor/tests/` runs it over loopback, the stop and final-checkpoint tests in
  `stop.rs`, the successor-side transfer tests in `successor.rs`, a daily turn's handover from its
  boundary in `turns.rs`, with a member taking the keys from its source while that source's archive
  is unreachable, and a later turn's member admitted after an unconfirmed handover, and the tail
  handover in `tail_handover.rs`: only the entries past the tip move, a forged entry, a wrong seal, a
  head mismatch and a posted-head mismatch are refused before the confirmation, a lagging tip and one
  not on the log are refused before the stop while the predecessor sequences on, a member whose copy
  its own writer cut falls back to the whole log once it sets the copy aside, and an older reader,
  following or with no copy, is refused before the stop; `src/handover/predecessor/tail.rs` holds
  the predecessor's checks on a request for the keys: the versions every successor reads, and a tail
  request's tip, on its log and still retained
- `src/handover/secrets.rs` — the new release's side of moving the delivered secrets with an
  approved upgrade, checked by the operator's signature; `src/handover/predecessor/secrets.rs` is
  the running release's, which refuses an election's successor; `src/handover/predecessor/tests/`'s
  `secrets.rs` covers both, against the switch in its `vault.rs`
- `src/handover/turns.rs` — a daily turn's handover authorisation: the finalised schedule's member, once trusted time reaches the turn's boundary, in the generation the schedule fixed; and the operators the member takes the keys from, its turn's source and then each earlier turn's, a week back; `src/handover/turns/tests.rs` walks those sources
- `src/handover/hostname.rs` — the API hostname the registry records for the operator in effect, read at a finalized block every `api.operators_interval` for `GET /v1/operator`, under daily turns the member's of the turn in effect at trusted time from the finalised schedule, withdrawn with Warning `operator_hostname_read_failed` while it cannot be read, and the check that a certificate names the registered hostname, a member taking a turn its own; its tests cover the read, the turn in effect and the failure episode
- `src/handover/tests.rs` — what each step binds, the encryption, the framing, and the evidence;
  `src/handover/tests/archive.rs` covers the published-log fetch ahead of a handover, which only an
  archive failing its integrity checks refuses
- `src/verify/mod.rs` — attestation verification for the CLI and replay: loading or fetching the
  attestation, release manifest, and collateral, `engine-verify` on the system clock, and the
  checkpoint-signer check, one attestation per signer, that lets `engine replay --attestation` span
  a change of checkpoint signer
- `src/verify/fetch.rs` — the key-distribution service client that fetches a quote's VCEK,
  intermediate, root, and revocation list
- `src/verify/refresh.rs` — the node's boot fetch and periodic refresh of that collateral, checked
  with `engine-verify` before it is served, and the `collateral_stale` and `collateral_revoked`
  alerts
- `src/verify/answers.rs` — offline checks of a saved signed response or stream connection, for
  `engine verify-response` and `engine verify-frames`
- `src/verify/answers/tests.rs` — saved headers in curl's layout, each refused alteration, and a
  frame removed from a capture
- `src/verify/tests/mod.rs` — verification through fetched collateral, and the loopback dev
  scenario
- `src/verify/tests/replay_dispatch.rs` — the dev-scenario regression replaying a signed log
  through `engine replay --attestation` with and without a `--signer` pin
- `src/verify/tests/relay.rs` — the relay: what the node refuses to serve, the bounded document,
  and a verifier checking relayed collateral with no vendor call
- `src/verify/tests/relay_scenario.rs` — the relay's dev scenario, over the node's API route and
  the `engine` binary
- `src/verify/tests/revocation.rs` — the root-issued revocation list through fetched collateral:
  enforced against the intermediate, never the VCEK
- `src/verify/tests/stub.rs` — the key-distribution service stand-ins
- `src/trusted_time/mod.rs` — the trusted time task: queries the Roughtime servers, backs off
  failing ones, and turns agreeing replies into a sample, which it publishes and sends to the
  sequencer
- `src/trusted_time/gate.rs` — the sequencer's check of each stamp against the latest sample, and
  `TrustedClock`, the same check of the time the API signs answers with
- `src/trusted_time/tests/` — agreement, round, back-off, gate, and alert key tests, and the
  offline dev scenario
- `src/roughtime/mod.rs` — the Roughtime client: builds requests and verifies signed replies, for
  version 1 (RFC 10049) and the final drafts' 0x8000000c
- `src/roughtime/message.rs` — the Roughtime wire format
- `src/roughtime/testing.rs` — a Roughtime server for tests that signs with its own keys
- `src/roughtime/tests.rs` — client tests over recorded and locally signed replies
- `src/roughtime/fixtures/replies.json` — replies recorded from the servers in `config/dev.toml`
- `src/tls/proxy.rs` — reading a trusted proxy's PROXY protocol v2 header before the TLS handshake
- `src/tls/mod.rs` — the TLS key and its self-signed certificate, the choice between the API and a
  TLS-ALPN-01 validation from each ClientHello, and the server loop that applies the connection caps
  and timeouts
- `src/tls/certificates.rs` — the certificates the key serves: a CA certificate swapped in for the
  attested key only, and the challenge certificate while a validation is answered
- `src/tls/acme/mod.rs` — the certificate task: the ACME account, the order, the challenge,
  checking and installing what the CA issues, and restoring the member's sealed certificate
- `src/tls/acme/http.rs` — the ACME client's HTTP, against compiled-in roots or a test CA's root
- `src/tls/acme/schedule.rs` — when the certificate is renewed, the retry backoff, and the
  `certificate_pending` and `certificate_expiring` alerts
- `src/tls/acme/tests/mod.rs` — the issued-certificate checks, the certificate request, and the
  schedule's renewals and alerts
- `src/tls/acme/tests/orders.rs` — attempts against a scripted CA: validation, a reused validation,
  and an order resumed after a lost exchange but replaced after a refusal
- `src/tls/acme/tests/fake_ca.rs` — the scripted ACME CA behind the client's HTTP
- `src/tls/acme/tests/http.rs` — the ACME HTTP client against a loopback HTTPS stand-in: its roots,
  its response cap, and its refusal to follow redirects
- `src/tls/acme/tests/sealed.rs` — the sealed account and the member's sealed certificate across a
  restart and its next turn, a restored certificate's renewal by trusted time, and a certificate that
  cannot be sealed
- `src/tls/tests/mod.rs` — the API end to end over TLS: key pinning, limits, caps, and the harness
- `src/tls/tests/acme.rs` — TLS-ALPN-01 on the listener, and a CA certificate installed without a
  restart
- `src/tls/tests/proxy.rs` — the PROXY protocol header: the client it names, the trusted ranges, and every refusal
- `src/tls/tests/stream.rs` — the stream end to end over TLS: the subscriber and message caps
- `src/limits/mod.rs` — request rate limits, connection caps, the stream subscriber cap, and the allowed browser origins
- `src/limits/tests.rs` — the limiters' buckets, caps, and alert throttle
- `src/config/mod.rs` — the node config schema, loading, and the config file's hash
- `src/config/stats.rs` — the `[stats]` section: retention, activity length, position-map buckets
- `src/config/api.rs` — the `[api]` section: connection and rate limits, timeouts, the quote, heartbeat and
  stats scan intervals, origins, TLS and the proxy protocol
- `src/config/api_defaults.rs` — defaults of the `[api]` keys with a default
- `src/config/benchmarks.rs` — publisher endpoints, cadence, timeout, and catalogue coverage
- `src/config/collateral.rs` — the `[tee]` keys of the attestation collateral relay and their bounds
- `src/config/checkpoints.rs` — the `[checkpoints]` relayer config and validation, and the
  `[log_heads]` section: the LogHeads contract, a dedicated relayer distinct from the checkpoint
  relayer, and the posting interval, which `operator.rotation_enabled` requires
- `src/config/seed_relay.rs` — the `[seed_relay]` section: Ethereum providers, the signing endpoint
  and relayer, the beacon API, fees and the balance threshold, and their checks;
  `src/config/seed_relay/tests.rs` covers them
- `src/config/feeds.rs` — the `[feeds]` section: the mock feed, the venues and vendors with their
  redacted API keys, the checks on their endpoints, credentials, and symbols, and the venues
  quoting each instrument of a market
- `src/config/feeds_tests.rs` — vendor connection config tests, and the production build's refusal
  of a market fewer venues quote than its `min_sources`
- `src/config/access.rs` — redacted source configuration, recorded identity and freshness bounds
- `src/config/custody.rs` — the `[custody.evm]` section, independent RPC providers, redacted
  URLs, and custody checks
- `src/config/secrets.rs` — secrets a config names but never holds: `[tee.sealing.secrets]`, a URL's
  `{name}`, the redacting `SecretUrl` and `SecretValue`, which field reads what, and a production
  build's refusal of a secret from the environment; `src/config/secrets/tests.rs` covers them
- `src/config/telemetry.rs` — the metrics listener and alert webhook config, and the redacting
  `WebhookUrl`
- `src/config/tests.rs` — config loading and validation tests
- `src/config/api_tests.rs` — the `[api]` heartbeat, quote and stats scan intervals' defaults and minimums, and the fresh-account budgets' check
- `src/config/mock_key_tests.rs` — published mock-only signer opt-in and hardware refusal regressions
- `src/config/custody_tests.rs` — independent provider configuration checks
- `src/config/production_tests.rs` — the config file's hash, a production build's refusal of fresh-account budgets above its values, and its refusal of
  mock providers
- `src/config/tls.rs` — the `[api.tls.acme]` section, and a production build's refusal of an API with
  only its self-signed certificate
- `src/config/tls_tests.rs` — `[api.tls.acme]` parsing and validation, the production refusal of fresh-account budgets above its values at load, and the production refusal
- `src/config/sealing.rs` — the `[tee.sealing]` section and its checks
- `src/config/telemetry_tests.rs` — metrics and alert config tests: the Critical route, the dev
  config's telemetry, and webhook URLs
- `src/config/proxy.rs` — `[api.proxy_protocol]`: the trusted proxy ranges and a production build's refusal of loopback and wide ones
- `src/config/proxy_tests.rs` — the proxy section's parsing, matching, and refusals
- `src/config/fresh_accounts.rs` — `[api.fresh_accounts]`: the budgets a never-credited account's declaration and session-key requests share, the check that a client's share is below the shared budget, and a production build's refusal of rates above its values
- `src/config/origins.rs` — `api.allowed_origins` validation: exact browser origins, `https` only in a production build
- `src/config/origins_tests.rs` — the origin allowlist's accepted and refused spellings
- `src/config/validation.rs` — the checks a parsed config must also pass, and the production
  build's refusal of mocks and of a genesis market fewer venues quote than its `min_sources`
- `src/config/test_keys.rs` — the published test keys from `config/test-keys.toml`, which a node
  with real attestation refuses, and their tests
- `src/checkpoints/mod.rs` — posting, retry, signed-parent commitment checks, and collateral reconciliation
- `src/checkpoints/chain.rs` — the chain access the worker reads and posts through, the vault's current release included,
  and the registry's operator in effect and whether the log's staking scan has passed its seating
- `src/checkpoints/relayer_gas.rs` — the relayer's balance read on each retry, its metric, and `checkpoint_relayer_low`
- `src/checkpoints/tests/relayer_gas.rs` — the alert firing while the balance is below the minimum and resolving once it is refilled
- `src/checkpoints/sweep.rs` — bounded approval pages and the sweep of further pages at one finalized block
- `src/checkpoints/accounting.rs` — bounded custody totals and held reserves derived from computed engine events
- `src/checkpoints/accounting/access_tests.rs` — actual pending and hold outputs against independent reconciliation totals
- `src/checkpoints/lifecycle.rs` — pinned signer/epoch, cumulative paid amounts, and approval nonce-use evidence
- `src/checkpoints/history.rs` — accepted history from the finalized anchor, the signed-parent chain, and signer epochs
- `src/checkpoints/lifecycle_tests/` — paid cash reconciliation, bounded claim polling, rotated authorities, and the vault's pause and retired signer observed, alerted and finalized
- `src/checkpoints/reconciliation.rs` — finalized collateral health: shortfalls, deposit totals, payouts, surpluses, the credit deadline, and the vault's halt
- `src/checkpoints/rpc/schedule.rs` — the daily turn the vault's registry's schedule names at a time, and a drawn turn's order (each distinct member its positions pick, with its registration), read at a finalized block
- `src/checkpoints/rpc/takeover.rs` — the standby's takeover signal, read at one finalized block
- `src/checkpoints/rpc/observation.rs` — same-block balance, epoch, paid total, and bounded claim reads, and whether a claim can prove against a checkpoint
- `src/checkpoints/rpc/authority.rs` — accepted historical signer identities for rotated nonce journals
- `src/checkpoints/rpc/submission/tests/lifecycle.rs` — canonical pinned history and retired journal recovery
- `src/checkpoints/accounting/tests.rs` — cash/reference recomputation oracles and arithmetic boundaries
- `src/checkpoints/accounting/tests/receipts.rs` — receipt cash deltas, adoption, reversals, unpaid approvals and atomic failures
- `src/checkpoints/tests/receipts.rs` — immutable prefix coverage and refusal of unproved aggregate equality
- `src/checkpoints/signature.rs` — shared strict checkpoint signature and domain verification
- `src/checkpoints/rpc.rs` — pinned-block vault reads and transaction relay through a signing RPC
- `src/checkpoints/rpc/tests.rs` — sampled ancestry and durable relayer request regressions, and the
  registry's operator read at the pinned finalized block
- `src/checkpoints/rpc/submission.rs` — bounded locked crash-durable nonce/hash reservations
- `src/checkpoints/rpc/submission/posting.rs` — persisted reservation and exact transaction retry transitions, and the next nonce for a send that reverted
- `src/checkpoints/rpc/submission/progress.rs` — canonical finalized obsolete nonce-consumer proofs
- `src/checkpoints/rpc/submission/tests.rs` — response-loss, restart, identity, and reservation regressions
- `src/checkpoints/rpc/submission/tests/fakes.rs` — isolated relay and canonical transaction RPC fixtures
- `src/checkpoints/rpc/submission/tests/nonce_progress.rs` — free, obsolete, and resurfaced nonce regressions
- `src/checkpoints/setup.rs` — finalized chain anchor and registry operator lookup and pending-inclusion startup wait;
  a member taking its turn reads the finalized checkpoint with no wait, the incumbent's checkpoint in
  flight resumed from its replica as outstanding work
- `src/checkpoints/tests.rs` — signature mutations, rollback, retry, and reconciliation tests
- `src/checkpoints/tests/latest.rs` — the worker reports the vault's latest included checkpoint ahead of the finalized one, and when its signer is retired
- `src/checkpoints/tests/collateral.rs` — shortfall, ledger-difference, surplus, and uncredited-deposit deadline tests
- `src/checkpoints/tests/torn.rs` — conflicts read once are retried; a conflict a fresh observation repeats stops the worker
- `src/checkpoints/scenario.rs` — isolated Anvil posting, credited/held reconciliation, tampering, truncation, surplus booking, shortfall, and reorg checks
- `src/checkpoints/scenario/anvil.rs` — the scenario's local Anvil child process, killed on drop
- `src/checkpoints/scenario/source.rs` — signed synthetic source bound to the scenario vault
- `src/checkpoints/scenario/tasks.rs` — the scenario's node tasks, and waits that fail on a stopped task, retry the checkpoint post until its inclusion, or report the poster's last alert
- `src/checkpoints/emission_budget.rs` — the vault's MONKEY cap against the emission period budget
- `src/checkpoints/emissions_scenario.rs` — the MONKEY emissions dev scenario on isolated Anvil: pro-rata
  allocation, cancelled periods, a restart across finalisation, the first halving, claims that mint
  each entitlement once, a proof claimed after later checkpoints post, and a retired and reinstalled
  signer ending its roots' claims
- `src/checkpoints/emissions_scenario/api.rs` — HTTP calls and a startup barrier on the worker's
  real collateral verdict before the scenario drives checkpoint-dependent commands
- `src/checkpoints/emissions_scenario/claims.rs` — the scenario's on-chain claims against the
  checkpoints the proofs name, and the guardian's and timelock's signer changes
- `src/checkpoints/emissions_scenario/funding.rs` — actual Vault funding receipts and finalized
  custody progress for the emissions scenario

- `src/checkpoints/staking_scenario.rs` — the MONKEY staking dev scenario on isolated Anvil: staking
  mirrored from the Staking contract, an epoch above the floor paying pro-rata in USDC against a
  proof read before two later checkpoints post, an unstake through the cooldown, and an epoch below
  the floor paying nothing
- `src/checkpoints/staking_scenario/scenario.rs` — the scenario's chain, deployments, and node,
  which it stops and restarts across an epoch boundary
- `src/checkpoints/staking_scenario/access.rs` — signed loopback screening and published Anvil keys
  for deposit-first eligibility and authenticated orders under the latest protocol
- `src/checkpoints/staking_scenario/funding.rs` — real Vault deposits, screened exact receipts,
  principal finalization, and authenticated custody frontiers
- `src/checkpoints/staking_scenario/claims.rs` — the scenario's on-chain steps: entitlement and
  distribution claims against posted roots, event mirroring, and balance reads
- `src/log_heads/mod.rs` — the released `(seq, prefix)` head, the cumulative command-prefix digest;
  the poster that reads the posted head
  through the custody quorum and relays moved heads under an `engine.log-head.v1` signature off the
  sequencer, adopting an on-chain head as the next post's parent only when its link is the prefix
  this log holds at its seq, freezing sequencing on a rollback or a foreign prefix, and the startup
  check that refuses a log the posted head does not hold
- `src/log_heads/prefix.rs` — the snapshot-scan prefix pass: extends the running command-prefix
  digest over each sealed command entry up to the snapshot base, refuses a prefix the snapshot
  does not certify, and returns the stamps of the posted command and of the command a
  vault-finalized checkpoint under the base covers
- `src/log_heads/rpc/mod.rs` — the LogHeads chain access: strict-majority quorum reads of the signer
  epoch and posted head at one agreed latest block, the startup walk back through the epochs the
  starting key signed at that block, posts through the checkpoint endpoint on the dedicated relayer,
  persists the complete signed call before each broadcast, and holds one reservation operation at a
  time
- `src/log_heads/rpc/tests.rs` — the chain access's tests: the pending bound, the rebroadcast's exact
  bytes, the replacement record's consistency, and against a loopback endpoint the call durable
  before its broadcast and the send, wait, resend, and rebroadcast branches; and the bounded store IO,
  one operation outstanding at a time
- `src/log_heads/submission.rs` — crash-durable relayer nonce reservations for log-head posts:
  a nonce advances only on chain evidence, an unsent reservation is reused for the newer head, a
  pending one is awaited rather than replaced, and the lock is held inside each blocking write
- `src/log_heads/read.rs` — the latest posted log head, read for a member that follows the operator
  in effect
- `src/log_check/mod.rs` — the boundary's log check: the verified head, the statement a member's
  quote binds and its report data, the gate's condition (rotation on and a turn in effect whose set
  names two or more members), the log's `TurnTaken` records that attribute a range to the releases
  that sequenced it, and the node's own check at a handover; `src/log_check/tests.rs` covers the attribution, the
  own check, a snapshot start's release in office, and the report data
- `src/log_check/ask.rs` — the operator's asking: the members of the turn's order, first to last,
  but its own candidate, those with no handover endpoint and releases in the range, each asked with a
  fresh challenge, and a member's answer admitted only from the release it registered, under a
  policy and TCB no weaker than its own, binding what was asked, `log_check_pending` paging a check
  due for 30 minutes; `src/log_check/ask/tests.rs` covers the admission, and the order: the first
  qualifying member, then the next when the first has not verified the checkpoint
- `src/log_check/answer.rs` — a following member's answers on its handover endpoint, from the
  checkpoints its replica verified, with the turn assignment taken at each;
  `src/log_check/answer/tests.rs` covers what it attests and refuses, and the endpoint freed once it
  stops
- `src/log_heads/setup.rs` — reading the posted head before the log replays (for a member, before it
  connects for its turn), so the startup gate
  binds to the link the replay captures at the posted seq (seq-only at or under an attested
  snapshot base), refuses a log the posted head does not hold within the custody finality budget —
  the current epoch's head or, while it holds none, the newest head of the earlier epochs the
  starting key signed — and builds the poster with the watch at the recovered head and its stall
  alarm counting from the checked head's stamp, or the vault-finalized checkpoint's with no head
  posted
- `src/log_heads/tests/mod.rs` — the poster's loop tests: pacing, lost responses, rollback freezes
  at a higher or forked posted head, stall and transport-failure alert episodes, and the reorg
  reparenting
- `src/log_heads/tests/stall.rs` — the stall alarm's window: three posting intervals, or two under rotation, and one page per episode
- `src/log_heads/tests/recovery.rs` — recovery behaviors: heads that land despite a lost response
  or across a restart, the stall alarm counting across a restart from the checked head or the
  finalized checkpoint, same-epoch reorg reparenting, an emptied epoch, the transport episodes'
  resolutions, and adoption by this log's prefixes: a foreign prefix freezes, a head with no
  retained evidence is not adopted
- `src/log_heads/tests/fake_chain.rs` — the tests' fake chain: the posted head, epoch, and failures
  the test drives, enforcing the contract's parent-chaining rule
- `src/log_heads/tests/submission.rs` — the submission store's nonce reservations, reconcile-down,
  and the timed-out write that keeps its lock until it completes
- `src/log_heads/tests/recovery_binding.rs` — the posted-head check's binding to the replayed
  bytes: the captured cumulative command-prefix digest, its continuity across a shutdown
  checkpoint, the host-tamper proof, and the poster's evidence and stall alarm seeded from the replay
- `src/log_heads/tests/rollback.rs` — the rollback check's refusals: seqs the log lacks, a link
  mismatch at the posted seq, and a seq the replay did not capture
- `src/custody/tests/election_scenario.rs` — the operator election dev scenario on isolated Anvil:
  amount delegations moved, taken back and reduced by an unstake, a close while rotation is off, an
  election by those tallies mirrored through the custody quorum, and the elected key signing from
  its effective time
- `src/custody/tests/election_scenario/chain.rs` — the scenario's chain at a fixed genesis time, its
  RPC proxies, deployments, and registry and timelock calls, optionally finalising two blocks behind
  its head
- `src/custody/tests/election_scenario/node/mod.rs` — the scenario's sequencer on a test clock,
  custody watcher, and API, and a daily turn member's seat: its own operator key and turn release,
  its API over TLS at its registered hostname, its checkpoints posted, its log check with its sealed
  keys and its asking, and a start from a member's replica anchored at the vault's checkpoint;
  `node/setup.rs` holds the custody and checkpoint sections and a seat's asking, `node/http.rs` the
  admin route, GETs and POSTs, and `node/stamps.rs` the wall time of a seat's last command and its
  `TurnTaken`
- `src/custody/tests/election_scenario/parameter_votes.rs` — the parameter votes dev scenario
- `src/custody/tests/election_scenario/daily_turns.rs` — the daily rotation dev scenario
  `scripts/dev-daily-rotation.sh` runs: a rotation set of three following the operator in effect
  under load, five turns drawn from relayed seeds, and at each boundary the turn's key, the tail
  handover while finality trails, the switch's window, `TurnTaken`, the time in office and
  `/v1/operator` checked, a stream client following each holder, the boundary's log check leg, then
  the log replayed, and an installation of the last turn's own candidate ending the turns
  (`daily_turns/install.rs`)
- `src/custody/tests/election_scenario/daily_turns/payouts.rs` — the boundary's log check leg: a
  withdrawal approved during A's turn pays only after B's handover, a second approved during B's
  turn waits for the boundary B holds on through, where a verification from B's own release is
  refused and C's, with A first in the order down, releases it
- `src/custody/tests/election_scenario/daily_turns/seat.rs` — a member's wait for its turns between
  its holdings, and its node while it holds a turn: sealed keys, the handover listener, the stop a
  handover asks for, and a start from the replica
- `src/custody/tests/election_scenario/daily_turns/switch.rs` — one switch: the window on the wall
  clock, the tail's bytes against the incumbent's log, no whole-log replay, counted in the member's
  own metrics, which its successor node's start shares, and the incumbent
  following its successor; a member's wait stopped and started again from boot
- `src/custody/tests/election_scenario/daily_turns/load.rs` — the load of signed BTC-USD orders on
  the node holding the turn, and the stream client following the operator in effect
- `src/custody/tests/election_scenario/daily_turns/seeds.rs` — the scenario's seed source over the
  seed relay scenario's two-chain fixture
- `src/custody/tests/election_scenario/daily_turns/checks.rs` — the time in office, the operator
  view, the stopped log's commands, and a copy's commands against the log, which the scenario checks
- `src/custody/tests/election_scenario/operator_share.rs` — the operator's share dev scenario on
  isolated Anvil: the election's candidate and a staker claim through the vault, an epoch below the
  floor pays neither, and the candidate's unclaimed leaf registers and pays through the exit
- `src/custody/tests/election_scenario/time_in_office.rs` — the `V50` part of the operator's share
  dev scenario: a second election takes effect halfway through an epoch, whose close credits the
  outgoing and incoming candidates about half the share each, both in their histories and both
  claimed through the vault
- `src/history/preflight.rs` — immutable history planning before authorized SQLite mutation, and a
  member's plan, which checks the index's integrity when the member opens its copy so its start
  from the replica does not
- `src/history/preflight/tests.rs` — private WAL resume and malformed geometry byte-preservation regressions
- `src/node/tests/custody_preflight.rs` — local custody refusal preserves log/history across image states
- `src/node/preflight.rs` — chain validation before writable history recovery, and the vault's MONKEY
  cap against the emission budget the log records
- `src/node/preflight/emission_tests.rs` — refused startup on an unreadable cap or another budget
- `src/node/preflight/tests.rs` — refused startup preserves history, WAL, and log bytes
- `src/sequencer/replay/auth_tests.rs` — trusted immediate/deferred completion evidence regressions
- `src/sequencer/replay/accounting_tests.rs` — historical fixture and outstanding-work accounting oracles
- `src/custody/mod.rs` — the custody providers (mock and EVM), the USDC-to-`Fixed` conversion,
  and `ContractQuorum`, the custody providers' agreed reads of a contract at one block of the custody
  credit tag
- `src/custody/evm.rs` — the deposit watcher: the chain and token check, range reads from blocks
  proven to lead to the tagged final block, the completeness check, and stall alerts
- `src/custody/evm/lifecycle.rs` — confirmed receipt reconciliation, principal finality and the replayed scan frontier
- `src/custody/evm/progress.rs` — confirmed header and log windows read one batch per poll, kept
  Staking catch-up retains at most 66 proven binary-hierarchy anchors independently of deposit pruning,
  plus two private candidate anchors, with O(N log(N/chunk)) header work and one configured chunk of full hashes
  across polls while their endpoint stays canonical
- `src/custody/evm/types.rs` — the canonical deposit, staking-event, finality and reader types
- `src/custody/evm/staking.rs` — staking catch-up proofs anchored independently of deposit progress, and the registry, schedule and vote contract bindings checked at a window's end
- `src/custody/tests/fakes/files.rs` — temporary directories, custody test configurations, and the
  stored deposit cursor
- `src/custody/tests/fakes/schedule.rs` — the schedule the fake chain's registry names, the
  registry that schedule records, and the schedule a vote contract records
- `src/custody/tests/fakes/sequencer.rs` — in-memory sequencer stand-ins for the watcher tests, with no
  log and no trusted time
- `src/custody/tests/fakes/logs.rs` — log lines captured from a test's tasks, for alert assertions
- `src/custody/tests/fakes/epochs.rs` — the fake chain's signer history and earlier epochs' log heads,
  answered as contract calls
- `src/custody/evm/legacy.rs` — independent canonical finality proof before legacy signature redelivery
- `src/custody/registry.rs` — the operator registry's rotation switch, election and installation events, its schedule's rotation set and turn events, and the vote contract's passed proposals, decoded for the staking mirror
- `src/custody/tests/rpc/registry.rs` — registry logs decoded only from the configured registry, with effective times in milliseconds
- `src/custody/tests/rpc/vote.rs` — passed proposals decoded only from the configured vote contract, with their canonical command or none, and a command not hashing to its event refused
- `src/custody/tests/staking/vote.rs` — passed proposals mirrored only once the vote contract is read to name the configured schedule
- `src/custody/tests/staking/registry.rs` — registry events mirrored once with the stakes, a refused one stalling the mirror, the staking contract's deployer checked against the configured registry, and the schedule checked against the one the registry and the config name and against the registry it records
- `src/api/house_state.rs` — `GET /v1/house/state`: the parameters with the seq that set them, the pending listing notices, and the latest checkpoint
- `src/api/operator.rs` — `GET /v1/operator`: the registry's `api_hostname` for the operator in effect, the key in effect, its pending rotation, whether rotation is on, the last election, and from `V51` the rotation set in effect and the next with its time, today's and tomorrow's turns, and the turn history (`?before=&limit=`)
- `src/api/operator_list.rs` — `GET /v1/operators`: the list `custody/operators/` last read, 404 without a lens and 503 before the first read or once it is stale
- `src/custody/operators/` — the operator list's reader over its own custody quorum: `lens.rs` is the `RegistryLens` ABI and the checks that make the list, `proposals.rs` the vote contract's open proposals read at the same block (its tests in `proposals/tests.rs`), `mod.rs` the reader, its loop and the feed the API serves from, `tests.rs` the stand-in providers
- `src/stats/distributions.rs` — the bounded history of closed distribution epochs behind `GET /v1/monkey/distributions`
- `src/stats/turns.rs` — the bounded history of turn assignments, with each member's `TurnTaken`, that `GET /v1/operator` pages
- `src/custody/tests/election_scenario/staking_reads.rs` — the staking reads dev scenario
- `src/api/admin_tests/delivery_authority.rs` — who a running node takes deliveries from: the key in
  effect by trusted time, and a named or installed successor's own
- `src/api/admin_tests/election.rs` — the operator route and the admin route after an election, published keys included
- `src/api/admin_tests/notice.rs` — the V49 dev scenario: stress parameters and a market's markup refused with no notice and before its time, applied at it through `engine admin` and TLS, and replayed
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
- `src/custody/quorum/posted_head.rs` — the LogHeads contract's posted head, read at the highest
  block a strict majority has reached and agrees on, block and head both
- `src/custody/rpc/mod.rs` — the vault over JSON-RPC, trusting only compiled-in TLS roots, with
  errors scrubbed of the endpoint URL
- `src/custody/rpc/decode.rs` — decoding the vault's deposit logs, the staking and registry events,
  and the deposit count
- `src/custody/cursor.rs` — the deposit cursor kept between restarts
- `src/custody/tests/access.rs` — payer sanctions and holds across restart, cursor loss and source recovery
- `src/custody/tests/access/replacements.rs` — canonical payer screening across startup activation and retry
- `src/sequencer/access_tests.rs` — current access refusals, reconciliation admission, recovered signature release, and reissue screening that keeps a refused approval held
- `src/sequencer/access.rs` — access-source failure and recovery alert episodes
- `src/custody/tests/mod.rs` — deposit watcher tests against a fake chain and a real sequencer
- `src/custody/tests/alerts.rs` — the watcher's alerts over time, in paused time
- `src/custody/tests/cursor.rs` — the deposit cursor across restarts, and which cursors a log
  refuses
- `src/custody/tests/fakes.rs` — the fake chain, test sequencer, log capture, and fixtures
- `src/custody/tests/quorum.rs` — fabrication, disagreement, lag, outages, and quorum recovery
- `src/custody/tests/log_heads.rs` — the log-head read through the quorum: block and head at the
  highest height a strict majority agrees on, never the finality tag, falling back past a faulty
  provider, a lagging minority excluded, and one quorum loss when no height wins; startup's walk back
  through the epochs the starting key signed; and the log-head and relayer-balance quorums' alerts
  under their own names at Warning
- `src/custody/tests/quorum_diagnostics.rs` — malformed header exclusion, transport independence, and retained watchdog progress
- `src/custody/tests/rpc.rs` — the RPC adapter's log decoding, deadlines, and URL scrubbing
- `src/custody/tests/rpc/redaction.rs` — a key a provider's refusal echoes stays out of the error
- `src/custody/tests/user_agent.rs` — the RPC adapter's JSON-RPC requests carry the engine's User-Agent
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
- `src/feeds/venue/mod.rs` — the venue task: connect, subscribe, sequence each symbol's newest
  quote when its mid moves or the engine's quote needs refreshing, reconnect, and hold back
  quotes the engine would refuse
- `src/feeds/venue/adapter.rs` — a venue's protocol on one connection, whichever adapter speaks it
- `src/feeds/venue/alerts.rs` — the venue task's alerts: outages, silent symbols, a clock ahead of
  the host's, and refused messages
- `src/feeds/venue/emit.rs` — sequencing the newest quote per symbol as book updates arrive, as soon
  as it is read, within a per-symbol budget (one per `feeds.venues.min_quote_spacing` after a
  burst, past which updates coalesce to the newest), holding a repeat of the sequenced mid until
  the engine's quote is half of `stale_after` or the market's `max_age_ms` old, and the emit timer
  that releases what the budget held or a repeat that falls due
- `src/feeds/venue/transport.rs` — a connection's messages and pings over a WebSocket over TLS,
  and a close frame read by its code and the length of its reason; its tests are in
  `src/feeds/venue/transport/tests.rs`
- `src/feeds/venue/delay.rs` — how far behind a connection's stream can be, from its pings on the
  monotonic clock, refusing frames past `feeds.venues.max_delay` and giving each quote the time its
  bound runs out; its tests are in `src/feeds/venue/delay/tests.rs`
- `src/feeds/venue/standing.rs` — the book each symbol last showed on a connection, re-dated by a
  ping's answer for up to 5 s after the ping answered before it was read, and a later book stamped
  no earlier than a re-dated one unless that stamp would lead the highest host time read by more
  than the engine allows, and not re-dated into the session before a closure its venue stamp lies
  in; its tests are in `src/feeds/venue/standing/tests.rs`
- `src/feeds/venue/validation.rs` — the checks every venue's books pass before they are sequenced
- `src/feeds/venue/confirm.rs` — confirming every symbol with the engine each tick: which the
  venue is quoting, which markets it does not list, which contracts the listing does not name or
  the market does not take, and changed market hours; and settling a dropped connection's last
  quotes
- `src/feeds/venue/quotes.rs` — the newest quote held between book updates, the rule that
  decides whether a quote repeats the sequenced mid and when it falls due, and
  the rule that decides whether the quote the engine holds counts as the venue quoting
- `src/feeds/venue/quiet_tests.rs` — the repeat rule: when a repeat waits, goes, or is dropped,
  the emit path arming its timer and clearing its memory, and the engine's index compared at every
  millisecond against an engine handed every update, under sequencer delays inside and past the
  refresh margin
- `src/feeds/venue/setup.rs` — the timing from the config, the TLS client config and reconnect
  backoff every connection shares
- `src/feeds/venue/auth.rs` — what a connection authenticates with: an API key, or an OAuth client
  of the venue's token endpoint whose bearer token the handshake carries, refetched three quarters
  of the way through its life, and the `venue_token_failed` alert; its tests, against the loopback
  CME token endpoint, are in `src/feeds/venue/auth/tests.rs`
- `src/feeds/venue/replace.rs` — the connection that replaces one on a fresh token, opened beside it
  and switched to once it is subscribed, a ping sent then is answered, and a book it then sends is
  in hand (or a ping interval has passed), that ping answered within `feeds.venues.max_delay` or
  sent again, so the venue's quotes run on under a current delay bound;
  one that fails leaves the old connection streaming for a retry on a new token; its tests are in
  `src/feeds/venue/replace/tests.rs`
- `src/feeds/venue/receive.rs` — reading one venue message into the books a connection holds
- `src/feeds/venue/tests.rs` — venue task unit tests: alert throttling and silence windows on
  paused time, held quotes against the engine's rule, kept books for unsubscribed symbols, the
  freshness rule, and backoff
- `src/feeds/coinbase.rs`, `src/feeds/bitstamp.rs` — venue adapters
- `src/feeds/massive.rs`, `src/feeds/tradermade.rs` — the Massive and TraderMade adapters: login,
  subscription, and quotes; Massive's tests are in `src/feeds/massive/tests.rs`
- `src/feeds/cme/mod.rs` — the CME Group adapter: its login status, the top-of-book subscription of
  each product whole, outrights only, each `TOB` restated whole, and the config check that a symbol
  names its contract by product code and period; its message-shape tests are in `src/feeds/cme/tests.rs`
- `src/feeds/kraken/mod.rs` — the Kraken adapter: a kept order book verified against Kraken's
  checksum; `src/feeds/kraken/tests.rs` replays recorded book messages against it
- `src/feeds/tests/mod.rs` — venue tasks against loopback venues: what they sequence, refuse,
  and alert, and how they reconnect
- `src/feeds/tests/emission.rs` — one sequenced quote per book update as it is read: a burst
  goes at once, the budget coalesces what arrives faster to the newest, and a quote stamped ahead
  of the host clock is not held for it
- `src/feeds/tests/quiet.rs` — a venue repeating its top of book: a repeated mid is sequenced only
  to refresh the engine's quote, a moved mid or a new connection's first quote goes at once
- `src/feeds/tests/fake.rs` — the loopback venues, the feed harness with an engine that applies
  what the feed sequences, and the wait for the next alert
- `src/feeds/tests/delay.rs` — a connection's pings against a venue that answers them or does not
- `src/feeds/tests/redate.rs` — a quiet book re-dated by a venue's answers: its stamps and the
  delay bound each carries, a delayed answer, the 5 s limit, a venue that stops answering, a later book replacing it, refused messages,
  a changed book dropped while the stream is behind, and emptied books; and, through the real
  sequencer, a market kept priced until the venue stops answering and a book not re-dated past its
  market's closure
- `src/feeds/tests/hostile.rs` — a host clock at the edge of the drift bound with frames held back
  to match, through the real sequencer: the market goes unpriced once the delay passes the bound
- `src/feeds/tests/late.rs` — venue tasks against a sequencer that answers late: when a quote
  counts as the venue quoting, and how a connection's last quotes are settled after it ends
- `src/feeds/tests/mock.rs` — the mock feed stopping one market at its `stop_after` and skipping
  one the engine has closed, and its walks rescaling at a split's opening, keeping a split applied
  between the startup query and the next, re-anchoring to the mark after a split they never
  followed, and restarting from a split market's mark
- `src/feeds/tests/scenario.rs` — the crypto feeds' dev scenario through the sequencer and engine
- `src/feeds/tests/rwa.rs` — the RWA price feeds' dev scenario: loopback vendors behind TLS price
  FX, index, commodity, and share markets through the sequencer, across a market open and a futures
  roll, and the log replays
- `src/feeds/tests/vendors.rs` — loopback Massive and TraderMade vendors
- `src/feeds/tests/toxicity.rs` — the toxicity score's dev scenario (`V54`): loopback Coinbase and
  Kraken stand-ins price BTC-USD; a toxic client pays its tier's add-on from its 21st fill, as the
  test computes from the moves, an edgeless client stays at the best price, round trips leave the
  score at its tier, and the log replays and a restart recovers the score
- `src/feeds/tests/cme_support.rs` — a loopback CME token endpoint and market data stream behind TLS
- `src/feeds/tests/cme.rs` — the CME feed against the loopback CME: rollovers with no gap in its
  quotes, a slow or failed replacement, connections ended at their token's expiry, a quiet closure,
  a refused token alerted with no credential or token logged, and the dev scenario, which prices US
  500 from CME and Massive through the sequencer and replays the log
- `src/feeds/tests/hours.rs` — quotes held back while a market is closed and for a contract the
  market does not take, without silence alerts
- `src/feeds/tests/rwa_support.rs` — schedules open around the clock, signed relistings, a record
  of every applied command, and TLS fronts a test CA certifies, for the RWA feed tests
- `src/throttle.rs` — holds a repeating alert to one a minute, counting the ones it held back
- `src/clock.rs` — the wall and monotonic clocks the sequencer stamps and checks with, and the pace
  sequenced time keeps with monotonic time (`StampPace`)
- `src/metrics/mod.rs` — the metrics registry, event counting, and the `/metrics` route
- `src/metrics/listener.rs` — the metrics listener, bound once for the process and served on a task
  of its own beside everything it runs, so a following member or standby answers too, through a
  replica's build
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
- `examples/seed_witness.rs` — a `SeedRelay` witness from a saved beacon state and header, for
  `scripts/seed-relay-fixtures.sh`
- `examples/reference_log.rs` — writes a protocol version's node reference log from engine-core's
- `tests/reference_logs.rs` — replays every committed node reference log, events included
- `tests/usd_settlement.rs` — the USD settlement dev scenario on the committed dev config: Germany 40
  valued at EUR-USD's index while EUR-USD moves, then refusing orders while EUR-USD is stale
- `tests/reference/` — one node-format reference log per protocol version, never regenerated
- `tests/replay.rs` — replays a log the node wrote before protocol versions, kept in
  `tests/fixtures/`, and checks that `engine replay` fails on a log it cannot verify
- `tests/common/access.rs` — shared explicit signed declarations, screening and financial scenario inputs
- `tests/production_node_template.rs` — loads `ops/runbooks/production-node.toml` assembled with the
  benchmark publishers and the production genesis, checks it holds what a production build requires
  but for the markets short of `min_sources` TLS venues, which it names, and that every placeholder
  carries a marker
- `tests/production_genesis/` — loads `config/production-genesis.toml` as a dev node's genesis,
  checks the launch values and quote currencies, and steps it through weeks with holidays and early
  closes (`main.rs`); checks each market reaches every divergence stress level with screened house
  funding (`divergence.rs`); checks each market's spread bands are the calibrated tiers in tenths of
  a bp (`spreads.rs`);
  and recomputes each futures market's roll times from its exchange's calendar and rolls the engine
  through them (`futures.rs`)

## Usage

```bash
# Run a local node with mock TEE, custody, and feeds (API on https://127.0.0.1:8080)
cargo run -p engine-node -- run --config config/dev.toml

# With [tee.sealing]: seal new keys for a release that has none (a first deployment, or after a
# compromise), or take them from the running release's handover listener once the vault makes this
# release current; under the mock TEE, name a stand-in launch measurement as well
cargo run -p engine-node -- run --config <config> --new-key
cargo run -p engine-node -- run --config <config> --predecessor 10.0.0.5:9471
cargo run -p engine-node -- run --config <config> --mock-launch-digest <96 hex digits> --new-key

# Watch Certificate Transparency logs for a certificate for the API's domain whose key no verified
# attestation names (outside the enclave; see config/ct-watch.toml)
cargo run -p engine-node -- ct-watch --config config/ct-watch.toml

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

# The proofs an account exits the vault with, from the log alone, at the vault's latestSeq, with
# each vault call ABI-encoded to send in order (cast send <vault> <calldata>), skipping a call the
# vault refuses for the reason its description names; --house builds the refunds of the house's
# pending deposits. The log's commands must reproduce that checkpoint's root; signatures are not
# checked, since the vault checks every proof against the root it accepted.
cargo run -p engine-node -- exit-proof --log data/dev/log --seq <latestSeq> --account <address>

# Verify a signed response: send a fresh challenge, save the headers and body, and check them
# against the attested enclave address and the deployment (config/dev.toml's chain id and vault)
SIGNER=$(jq -r .enclave_address <<< "$ATT")
CHALLENGE=$(od -An -N32 -tx1 /dev/urandom | tr -d ' \n')
curl -sk -D headers -o body -H "X-Engine-Challenge: $CHALLENGE" $API/v1/markets
cargo run -p engine-node -- verify-response --signer $SIGNER --chain-id 31337 \
  --vault 0x0000000000000000000000000000000000000000 --challenge $CHALLENGE --path /v1/markets \
  --headers headers --body body
# Frames saved one a line from wss://127.0.0.1:8080/v1/stream?signed=1&challenge=$CHALLENGE, and a
# POST's answer with the body it sent
cargo run -p engine-node -- verify-frames --signer $SIGNER --chain-id 31337 \
  --vault 0x0000000000000000000000000000000000000000 --challenge $CHALLENGE \
  --path "/v1/stream?signed=1&challenge=$CHALLENGE" --frames frames.jsonl
cargo run -p engine-node -- verify-response ... --method POST --path /v1/orders --request-body order.json \
  --headers headers --body body

# Run the signed API dev scenario: verified responses and frames, refused tampering, replay and gaps
./scripts/dev-signed-api.sh

# Sign and submit admin commands as the operator, over TLS pinned to the attested key: the PIN (hex)
# the bundle check above sets. The key is read from --key-file, OPERATOR_KEY_FILE, or OPERATOR_KEY;
# config/dev.toml names the public key of the local test key 0x4242…42 (32 bytes of 0x42). A market
# or parameters file holds the keys of a [[genesis.markets]] entry or of [genesis.params] at its top
# level, without the table header.
export OPERATOR_KEY=0x4242424242424242424242424242424242424242424242424242424242424242
cargo run -p engine-node -- admin --pin $PIN set-reduce-only true             # every market
cargo run -p engine-node -- admin --pin $PIN set-reduce-only false --market 2
cargo run -p engine-node -- admin --pin $PIN set-stress-floor 1 --market 1
# From protocol V46 a rate moves at most 300 bps a day from the applied rates next to its date.
cargo run -p engine-node -- admin --pin $PIN set-benchmark-rate SOFR 430
cargo run -p engine-node -- admin --pin $PIN upsert-market market.toml        # no table header
cargo run -p engine-node -- admin --pin $PIN set-params params.toml           # no table header
# From protocol V49, new stress parameters and a listed market's markup apply only seven days after
# a notice, by a set-params carrying that file or an upsert-market carrying that markup.
cargo run -p engine-node -- admin --pin $PIN announce-params --stress-from params.toml --effective-at 2027-01-08T00:00:00Z
# From protocol V54, new toxicity score tiers, each within 50% of its launch score and add-on, the same way.
cargo run -p engine-node -- admin --pin $PIN announce-params --score-tiers-from params.toml --effective-at 2027-01-08T00:00:00Z
cargo run -p engine-node -- admin --pin $PIN announce-markup 1 300 --effective-at 2027-01-08T00:00:00Z
# From protocol V52, a listed market's spread and margin bands, and its markup, apply only seven days
# after a listing notice, by an upsert-market carrying them, within the market's launch bands.
cargo run -p engine-node -- admin --pin $PIN announce-listing 1 --spreads-from market.toml --margins-from market.toml --effective-at 2027-01-08T00:00:00Z
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

# Stream the trader's executable quotes on that node: an order at the streamed ask fills within its
# bound, and one at a stale ask is declined once the index moves
./scripts/dev-quotes.sh

# Run checkpoint posting, tampered replay, truncation, and shortfall checks on isolated Anvil
./scripts/dev-checkpoints.sh

# Post the released log head on isolated Anvil, restart onto it, refuse a cut log, and freeze on
# an overtaking head
./scripts/dev-log-heads.sh

# A production binary, which refuses mock providers (release images build it in image/)
cargo build --release -p engine-node --features production
# Run the MONKEY emissions scenario on isolated Anvil: allocation, halving, restart, and claims
./scripts/dev-monkey-emissions.sh

# Run the MONKEY staking scenario on isolated Anvil: staking mirrored from the contract, an epoch
# above the floor paying pro-rata in USDC, an unstake through the cooldown, and an epoch below the
# floor paying nothing
./scripts/dev-monkey-staking.sh

# Run the operator election scenario on isolated Anvil: amount delegations, an election by their
# tallies mirrored through the custody quorum, and the elected key signing from its effective time
./scripts/dev-operator-election.sh

# Run the operator's share scenario on isolated Anvil: an elected candidate and a staker each claim
# their share of an epoch through the vault, an epoch below the floor pays neither, and the
# candidate's unclaimed leaf registers and pays through the vault's exit; then a second election
# takes effect halfway through an epoch, and its close splits the share between the two candidates
./scripts/dev-operator-share.sh

# Deliver secrets into a node's enclave, on its [tee.sealing.secrets] listener. The tool refuses unless
# the enclave's quote matches the release manifest and the config's hash; --allow-mock accepts the mock
# TEE, development only. The file holds `version = <n>` and a [secrets] table of names and values.
cargo run -p engine-node -- admin --config <node.toml> --key-file <operator.key> deliver-secrets \
  --to <host:port> --release <release.toml> --secrets-file <secrets.toml>

# What the enclave attests it holds: the sealed and running versions, and the secrets it lacks
cargo run -p engine-node -- admin --config <node.toml> --key-file <operator.key> secrets-status \
  --to <host:port> --release <release.toml>

# Run the production secrets scenario on isolated Anvil: a node that holds until its keys are
# delivered, refused bundles, a restart, a rotation, a restored and a damaged sealed file
./scripts/dev-production-secrets.sh

# Run the parameter notice scenario on a dev node: stress parameters and a market's markup refused
# with no notice and before its time, applied at it through engine admin and TLS, and replayed
./scripts/dev-params-notice.sh
```

EVM custody requires at least three entries in `custody.evm.providers`, each with a unique
`name` and a distinct `rpc_url`, whose credential a delivered secret fills (see Delivered secrets).
Use independently operated services in
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
Under access enforcement the request is screened like a new withdrawal, by account and transport
peer, and judged at the sequencer's trusted time; a request for an approval the sequencer does not
hold is answered `404` before it is screened, so a key that holds none costs no screening call. The node queues the approval for a new 24-hour
authorization with the same nonce, in approval order, and answers `401` for another signer, `403`
with the access reason when screening refuses the account or its peer, `404` for an approval that
is unknown, paid, or not yet loaded by a restart's recovery, and `400` while the latest
authorization is usable or the request replaces an older one. A refused approval stays debited and
unpaid, and its account can ask again once screening clears it. A signer rotation reissues, without
a request, each unpaid approval that was never signed or whose authorization is still usable; an
expired authorization waits for its account's screened request. A queued request lives in memory:
after a node restart, send the same signed body again, which stays valid until an authorization
replaces the one it names. While the host leaves the log intact a restart reissues nothing, and no
approval is signed while an older one that may still need a signature waits in an unloaded
recovery window; recovery from a checkpoint snapshot sees only the authorizations after it, so an
approval signed before it is signed once more.

`GET /v1/accounts/<address>/payouts?after=<nonce>&limit=50` returns `seq`, `loaded`, `payouts` and
`next`: the account's unpaid approvals, oldest first (1–200 per page), each as `nonce`, `amount` and
`withdrawal`, the latest `SignedWithdrawal` exactly as the stream carries it, or `null` until one is
signed. A reissue or a signer rotation replaces it. The sequencer answers from the payouts it
already holds, a range scan bounded by the page, with no log, disk or custody read, and the
response is signed like the rest of `/v1`. `next` is the nonce to pass as `after` when the page was
full, else `null`. An approval custody has not yet reported paid is still listed, so a client
still reads the vault for it. A nonce is absent because it is paid only when `loaded` is true and
no earlier page left a `next` unfollowed: `loaded` is false while a restart is still loading
approvals, and always false on a node without a checkpoint gate. Approvals inherited from before
V22, signed without an authorization, are not listed.

`GET /v1/withdrawals?limit=50&after=<request-seq>` returns `seq`, `queue` and `next`.
`GET /v1/accounts/<address>/withdrawals` filters the same FIFO and reports each request's global
one-based `position`. Both endpoints bound their scan to 1–200 requests; an account page may be
empty and still have `next`, which clients must follow. Queue records include the reserved amount,
remaining amount and original request sequence; queue changes also appear in the public log and
account history. `POST /v1/withdrawals` retains the signed client request format. Its later
`withdrawal` stream message carries the exact approved amount and nonce, signer epoch, issue/expiry
seconds, accepted checkpoint and approval proof needed by `Vault.withdraw`.

On-chain checkpoint posting uses an optional `[checkpoints]` section, required with hardware EVM
custody. `node.checkpoint_every` sets the periodic frequency; every approved withdrawal also asks
for one. Each root is durable locally before the relayer sends it, with a gas limit a quarter
above its estimate, and the node releases no
withdrawal signature until its approval is covered by a confirmed on-chain prefix and collateral
reconciles at custody's `withdrawal_finality_tag`, independently of the early-credit tag and its
confirmation depth. Latest inclusion is
tracked separately so delayed finality never causes duplicate submission. Startup waits for a
pending inclusion to reach that boundary (or disappear in a reorg), bounded by
`custody.evm.finality_stalled_after`, before selecting its rollback anchor. Recovery then restores every outstanding durable signed
checkpoint and its original accounting before signing descendants; an unmined signature remains
live even when both chain views still show its parent. Permissionless relays matching that local
chain reconcile the covered prefix, including work beyond the receive queue. The queue is bounded, and
the worker finalizes one checkpoint at a time, so the relayer can post only one per finality delay at
`withdrawal_finality_tag`. A periodic checkpoint and an approval's wait while the last signed one is
not yet final, since one signed sooner would wait its turn covering less. So `node.checkpoint_every`
is a ceiling, at most one checkpoint is on its way, and the cadence follows the chain's finality without
tuning. An approval made meanwhile gets the next checkpoint as soon as that one is final, however many
others share it: on Ethereum, whose finality is about 13 minutes, it is authorized 13 to 26 minutes
later. A handover's final checkpoint, the one a stopping node writes, and those of required commands
are not held back. Signing rehashes everything changed since the last checkpoint on the sequencer's
thread: about 325 ms for 100k changed accounts and 1,000 approval proofs
(`held_checkpoint_cost_at_100k_accounts`). A hold that outlasts `custody.evm.finality_stalled_after`
with none becoming final raises the Warning `checkpoint_backlog`, as does a full queue; the sequencer
keeps sequencing either way, and approvals wait for the next checkpoint. A stall's episode resolves when a
newer checkpoint becomes final, or when a checkpoint is signed with none on its way, and a full queue's when a
checkpoint fits. A final checkpoint signed around the hold (a handover's, a shutdown's) resolves neither, so a
shutdown during a stall leaves the episode open. Open episodes survive a restart in the alert outbox, and the
restarted node resolves one at the first evidence the poster has caught up. Failed posts retry after `retry_interval` and emit `checkpoint_post_failed`. Withheld signing or
posting emits the Warning `checkpoint_withheld`, one episode per `cause` (`collateral`,
`custody_prefix`, `post_diverged`, `signer_retired`) however many attempts it spans, resolved when that hold clears. A conflicting
posted root or signer, or a vault checkpoint this node did not sign, stops the node with the Critical `checkpoint_conflict` once the next retry's fresh observation
repeats it; seen once, it retries with `checkpoint_post_failed`, since one read can be torn across a block the
endpoint is still mining. A failed post once the vault's exit against the last posted root has started
(`exitStartedAt`) stops the node with the Critical `vault_exit_started`: no checkpoint can follow it. Following decision #15 §2, withdrawals
pause when the vault's USDC is less than the ledger requires, or the vault's deposits or payments
differ from it: collateral reconciles when the vault's paid total is within the checkpoint's
approvals, the vault's running deposit total through the checkpoint's last credited deposit equals
the ledger's credits, and the vault's USDC balance is at least its deposits less its payments.
Deposits the checkpoint has not yet credited do not pause withdrawals until the first of them stays
uncredited for `custody.evm.stall_critical_after`, which must therefore exceed the credit tag's lag,
one finality delay, and the wait for the next periodic checkpoint. A difference emits Critical
`collateral_mismatch` and pauses withdrawals; ordinary trading commands continue. While the latest
finalized checkpoint's nonempty custody prefix has no sequenced canonical receipt coverage, the
worker cannot compare it with the vault: it emits the Warning `collateral_pending_deposits` and
pauses new withdrawals (fail closed), while checkpoint signing continues, since a successor with a
proven prefix is the way out. Withdrawals resume at the first healthy comparison of a proven
checkpoint. A failed or absent observation never changes the gate, so it cannot erase a proved
divergence. `engine_withdrawals_paused` is 1 in every paused state (before the first reconciliation,
an unproven prefix, a pending catch-up past its deadline, a divergence, a vault that pauses
withdrawals or has no signer), and
`engine_withdrawals_verdict_seq` is the finalized checkpoint behind the latest verdict. USDC sent to the
vault outside a deposit pauses nothing: it emits the Warning `collateral_surplus` until anyone calls
the vault's `bookSurplus`, which records it as a house deposit; a surplus below the 10 USDC minimum
deposit must be topped up first.

The vault's guardian, the deployment's multisig acting without the timelock's delay, may pause
withdrawals for seven days or retire the vault's signer. Each observation reads the vault's
withdrawal pause and signer at the latest and at the withdrawal finality block, and a change seen
only at the latest block supplies no release evidence until it is final; a failed post still
reports a finalized halt, without releasing anything. While the finalized vault pauses
withdrawals or has no signer, the node signs no withdrawal authorization, first or reissued,
refuses new withdrawals with `payout_unavailable`, sets `engine_withdrawals_paused` to 1, and
emits the Critical `vault_withdrawals_halted`, one episode per `cause` (`paused`,
`signer_retired`), resolved once the vault lifts it. Checkpoints keep posting during a pause.
While the signer is retired the vault accepts no checkpoint, so the sequencer signs none
(`checkpoint_withheld`, cause `signer_retired`), starts no deposit catch-up deadline, and keeps
sequencing; once the timelock installs this enclave's key again, the next epoch begins, the next
checkpoint carries the whole state, and the node reissues each authorization still usable, while
another key stops the node with `checkpoint_conflict`. A node started while the signer is
retired refuses to start until the timelock installs one.

```toml
[checkpoints]
rpc_url = "http://127.0.0.1:8545"
relayer = "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266"
retry_interval = "2s"
rpc_timeout = "10s"

[checkpoints.relayer_gas] # optional
min_balance = "0.01"
```

With `[checkpoints.relayer_gas]`, each retry reads the relayer's balance through the custody RPC quorum
at the highest block a majority declares final, exports it as `engine_checkpoint_relayer_balance_eth`,
and raises the Warning `checkpoint_relayer_low` while it is below `min_balance` (ether, a decimal
string), resolving once it is at or above it. A read without a quorum changes nothing. The
quorum's own `custody_rpc_*` alerts report the providers.

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
the reservation intact. A send that reverted, as one in flight when the vault's guardian retires
the signer does, moves to the next nonce once finalized history shows that exact call consumed
the reserved one and the vault at that block lacks the checkpoint. These nonce proofs never
authorize withdrawal release.
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

Endpoints: `GET /v1/health`, `GET /v1/attestation`, `GET /v1/attestation/collateral`,
`GET /v1/markets`, `GET /v1/markets/{id}/marks`, `GET /v1/house`, `GET /v1/house/state`, `GET /v1/house/positions`,
`GET /v1/monkey`, `GET /v1/monkey/activity`, `GET /v1/monkey/distributions`, `GET /v1/stats/series`, `GET /v1/operator`, `GET /v1/operators`,
`GET /v1/accounts/{address}`, `GET /v1/accounts/{address}/nonce`,
`GET /v1/accounts/{address}/history`, `GET /v1/accounts/{address}/proof`,
`GET /v1/accounts/{address}/emissions`, `POST /v1/orders`, `POST /v1/orders/cancel`,
`POST /v1/orders/cancel-all`, `POST /v1/positions/close-all`,
`POST /v1/withdrawals` (mock custody only until withdrawals are paid on-chain), `POST /v1/admin`
(the operator's), `POST /v1/dev/deposit` (mock custody only), `GET /v1/stream` (WebSocket), and
`GET /v1/archive/{file}` (`manifest.jsonl` or a segment it lists).
From `V44`, `POST /v1/orders/cancel-all` takes `{ "account", "auth" }` and cancels every order the
account rests, each with the `order_cancelled` a single cancel emits (an account with none gets
`[]`). `POST /v1/positions/close-all` takes `{ "account", "closes": [{ "market", "quoted" }],
"max_slippage_bps", "auth" }` (`max_slippage_bps` optional) and closes the account's whole position
in each listed market, in list order, as the single reduce-only market order (from `V45` of at most
`Fixed::MAX_INPUT` units of a larger position, the rest staying open in the fill's `position`): its
answer is that order's events per entry, and a refused entry is an `order_rejected` whose `client_id` is the
entry's index in `closes`. A list that is empty, or names a market twice, is one rejection of the
whole request, and so is one naming more markets than are listed, which the API refuses with 400
before the signature is checked. Each is one signed request (`CancelAllRequest`,
`CloseAllRequest` in `engine-core`'s EIP-712 types, `closes` signed in order), one nonce, one
rate-limit count, and one screening; the account key or a registered session key may sign either.

A client over its rate gets 429 with `Retry-After`, a body over `api.max_body_bytes` gets 413, a
body not received within `api.read_timeout` gets 408, and a subscriber over `api.max_subscribers`
gets 503. A connection over a connection cap or over its client's connection rate is closed before
its TLS handshake. A client is its socket peer IP, with IPv6 counted by its /64; forwarding headers
are ignored.

`[api.proxy_protocol]` is for a layer-4 proxy that passes TLS through without decrypting, which the
node would otherwise see as one client. `trusted` lists the proxy's address ranges (CIDR; at least
one, and not every address). A connection from a listed range must begin with a PROXY protocol v2
header, read before the TLS handshake and under `api.handshake_timeout`; its source address then
replaces the socket peer for the request, connection and rate limits, connection caps, and the
access screening, and the TLS handshake (including a TLS-ALPN-01 validation) follows. A listed peer
whose first bytes are not a valid header (no header, version 1, a UNIX or unspecified family, a
datagram transport, over 1024 bytes) is closed and raises the `api_proxy_header_refused` Warning
alert. A LOCAL header is the proxy's own connection, such as a health check: the proxy's socket
address stands in for the client, so it is counted against the caps and rates and screened as the
proxy, the header's addresses are ignored, and the TLS handshake follows. That address is the real
peer, not one a client chose. A header from any other peer is never
read: the bytes go to the TLS handshake, which fails, so it is closed too. Until its header arrives a
proxy connection counts against the total connection cap only. A production build refuses a trusted
range that holds a loopback address or is wider than /8 (IPv4) or /32 (IPv6). The host controls the
network, so a malicious host can still present any address as the proxy's. The header is
unauthenticated: list only addresses that reach the node solely through the operator's own proxy,
not a shared provider's egress range its other customers can point at the node. Absent, no
connection may carry a header.

`api.allowed_origins` lists the browser origins (`scheme://host[:port]`, exact; no `*`, path, or
duplicate; `https` only in a production build) that may call the API. It is empty by default, which
sends no CORS headers and refuses every stream upgrade that carries an `Origin`. A request from a listed origin gets `Access-Control-Allow-Origin` with that
origin and `Vary: Origin`, on error responses too, and a preflight `OPTIONS` on any `/v1` route
answers 204 with `GET, POST, OPTIONS`, the headers `Content-Type` and `X-Engine-Challenge`, and a
600-second max age, and counts against the client's `api.per_ip` rate like any request (429 when
over). `/v1/admin` never gets either. `GET /v1/stream` with an `Origin` header not on the list, or
any `Origin` when the list is empty, gets 403 before the upgrade, since browsers do not apply CORS
to WebSockets; a request without `Origin` is unaffected. The config is bound by the attestation, so
changing the list is a new release.

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

Every `/v1` response but those of `/v1/health`, `/v1/attestation`, `/v1/attestation/collateral`,
and `/v1/archive/*` is signed by the enclave key whose address the bundle holds, error responses
included. A request may send `X-Engine-Challenge`, 64 hex characters; one that sends none is
answered under 32 zero bytes, and a malformed one gets an unsigned 400. The response carries
`X-Engine-Signature` (65-byte recoverable secp256k1, `0x` hex), `X-Engine-Time` (the host clock in
milliseconds, within `time.max_drift` of trusted time), `X-Engine-Seq` (the seq of the last log
entry the node had flushed when it answered; 0 before the first), and `X-Engine-Challenge` (echoed
in lowercase). Every answer on these routes also carries `Cache-Control: no-store`, which the
signature does not cover, so no browser or proxy keeps an answer bound to one request's challenge.
The signature covers
`keccak256("engine.response.v1" ‖ chain_id (u64 BE) ‖ vault (20 bytes) ‖ challenge (32) ‖ time (u64
BE) ‖ seq (u64 BE) ‖ status (u16 BE) ‖ sha256(method ‖ 0x00 ‖ path and query) ‖ sha256(request
body) ‖ sha256(response body as sent))`, for the custody chain id and vault in the config; a `HEAD`
response is signed over its empty body. A client verifies with the challenge, request line, and
request body it sent, never the echoed challenge, and sends a fresh random challenge with each
request it relies on: then a recorded response answers no other request, and one to a substituted
body fails. A response a layer outside the signer produces is unsigned: a listed browser origin's
preflight, which has no body and which the browser rather than the page reads; a stream upgrade
refused for its origin; a 429 from the rate limiter; and a 408 or 413 from the body read. A client
never shows an unsigned response on a signed route as data. A listed origin may read the four
`X-Engine-*` headers (`Access-Control-Expose-Headers`). The time is the host clock, signed only
while it is within `time.max_drift` of every time the latest trusted time sample allows, and that
sample is no older than 60 seconds, the bound the sequencer holds its stamps to; otherwise the
response goes out unsigned. Signatures prove who answered, not that the answer arrived: a man in the
middle can still delay or drop it. They bind the bytes of the request and challenge, not one
delivery of them: a man in the middle can send a copy of a request first, and the node's refusal of
the second, a `409` invalid nonce or any other status, verifies as the answer to the client's
request. Before showing a refusal of a state-changing request, a client confirms it with a signed
read of the state the request changes, under a fresh challenge sent after the refusal arrived: for
an order, cancel, or withdrawal, `GET /v1/accounts/{address}` or `/v1/accounts/{address}/withdrawals`,
which the sequencer answers from state that holds every command logged before the refusal. A next
nonce past the request's does not show it ran, since a request refused for its expiry uses its nonce
too.

`GET /v1/stream?signed=1&challenge=<64 hex>` (both required together) sends each message, and a
heartbeat (`{"type":"heartbeat","head":"<seq>:<index>"}`, the last entry flushed, or `null` before
the first) at once and then every `api.heartbeat_interval`, as
`{"v":1,"n":<u64>,"time":<ms>,"msg":"<the message as a JSON string>","sig":"0x…"}`, with `time` held
to the same bound as a response's; while trusted time does not bound the host clock, the
connection closes instead of sending its next frame, and the client resumes it with `after`. `n`
counts from 0 on each connection, and the signature
covers `keccak256("engine.frame.v1" ‖ chain_id ‖ vault ‖ challenge ‖ sha256("GET" ‖ 0x00 ‖ path and
query of the request that opened the connection) ‖ the previous frame's digest (32 zero bytes for
frame 0) ‖ n (u64 BE) ‖ time (u64 BE) ‖ sha256(msg))`. The chain and the opening request mean a
frame fits only its own connection and place: not one opened from another start, and not a second
connection opened with the same challenge. A client closes the connection on a frame that fails,
and treats the feed as stale when no heartbeat has come for about two intervals; the interval is
`api.heartbeat_interval` in the config the attestation binds. A heartbeat's `head` is the last entry
the node has written, which a connection still catching up, or the live tail just behind it, has not
received yet; it says how far the log has gone, not what this connection has missed. Without
`signed=1` the stream is unchanged. `crates/wire` holds these digests, the client checks, and the known-answer vectors a
browser verifier replays; `crates/verify-wasm` builds those checks and the attestation verifier for
browsers.

A `signed=1` connection can also ask for an account's executable quotes, by address and without a
signature, as the account reads are public: it sends
`{"op":"quotes","account":"0x…","markets":[1,2]}` (at most `api.quote_max_markets` distinct markets;
sending it again replaces the subscription, and an empty `markets` ends it) and receives, in signed
frames, one message per market,
`{"type":"quote","market":1,"open":true,"reduce_only":false,"stress_level":0,"toxicity_bps":0,"mark":"…","ask":[…],"bid":[…],"close":{…}|null}`,
where `toxicity_bps` is the spread the account's toxicity adds to every price the message quotes,
and again for each market whose message changed. The node reads the engine only after the log head
has moved: at once when the connection's last read began at least `api.quote_interval` ago, and
otherwise when that interval ends, so a connection reads at most once per interval while the head keeps
moving. It sends no sooner than an interval after its last quote message on the connection. `ask` is what a buy pays and `bid` what a sell receives: a list of `{"qty","price"}`
steps by size, one for the top of each spread tier as the account would reach it, then, between the
last tier top the account can open and the first it cannot, and in the unbounded last tier, one
at the largest order it can open (its margin and the house's exposure caps limit it). A step the
account cannot open also carries `declined` (`insufficient_margin` or `house_capacity`) and is
priced as a fill would be. An order takes the price of the first step whose `qty` is at least its
own. That price is the average for the step's whole size, which is never better for the trader than
the price of any smaller order, but for the rounding of a fill's cost up to a unit before it is
divided by the order's size: an order of a cent's notional can price about a thousandth of a basis
point above its step, and a slippage bound of a basis point covers it. An order far smaller than
that, such as closing the last dust of a position, can price above its step by more than the bound:
the rounding is a unit of cost whatever the order's size, so no step price covers every size, and
the engine's pricing is what it is. A step's price is also the
worst of its own and those of the steps before it, so it holds where a tier charges less than the
one before. The steps are accurate to the search's resolution, a thousandth of the gap between the
last size known to fill and the first known to be declined, so a slightly larger order than the
last step may still fill. So an order sent with the
step's price as `quoted`, and its slippage bound, fills when nothing moved, and is declined with
`Slippage` when the live price is worse than the bound. An order larger than the last step with no
`declined`, or a side with no steps, cannot be opened. `close` is the price of closing the
account's whole position, or of `Fixed::MAX_INPUT` units of a larger one, the most a close-all closes
at once. `unavailable` (with `ask` and `bid` empty) carries why a market has no
price now: closed, no valid index, or no FX price. Quotes come from `Engine::quoter`, which prices as
a fill does but does not re-evaluate stress or check access screening, so an order is declined for
screening whatever its quote. A request the node cannot serve, such as an uncredited account, an
unknown market, or too many markets, gets `{"type":"quote_error","error":"…"}` and no subscription, at most one such message per
`api.quote_interval`.
Without `signed=1`, what a connection sends is dropped. Each read runs on the sequencer's thread
right after the batch it arrives in is written, and its quotes are sent once the log holds the state
they came from. Many connections react to one head move, so at most four reads (across all
connections) wait in the sequencer's queue at once, and a connection past that waits its turn: a
command waits behind at most four reads. `api/stream/quotes/bench.rs` measures the reads against the
command latency budget, and the time from a head move to each connection's quote.

`GET /v1/attestation/collateral` returns `vcek`, `intermediate`, `root`, and `crl` as `0x` hex DER,
and `fetched_at` in Unix milliseconds by the host's clock, for a client that cannot call the
vendor's key-distribution service. It answers 404 for a quote with no collateral (a mock quote), 503
until the first fetch succeeds, and carries no per-account data. It is not signed and needs no
signature: a verifier checks the chain to a root pinned in its own binary, the VCEK against the
quote's chip id and reported TCB, the list against that root, and the report's signature against
the VCEK, as `engine verify-attestation` does. The node fetches it from `tee.kds_base` for
`tee.processor` at boot and every `tee.collateral_refresh` (1 minute to 24 hours, default 12 hours),
drops any fetch that fails its own checks, bounds every answer, and keeps serving the last good
copy, refreshing sooner when the list nears its `nextUpdate`; `image/README.md` lists each bound.
A fetch whose root-signed list revokes the node's intermediate is the one failing fetch it serves,
so verifiers refuse the chain, and it raises the Critical `collateral_revoked`.
The Warning `collateral_stale` fires when a refresh fails with the served list within a day of its
`nextUpdate`, or when one fails after the node has gone a refresh interval (at most 30 minutes)
without any collateral.
`engine verify-attestation --collateral <file or URL>` verifies from the served document instead of
calling the vendor (save a self-signed node's response with `curl -k`, or pass the URL of a node
with a CA certificate). A browser passes the same document to `crates/verify-wasm`'s
`verifyAttestation`.

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
that its account may not yet make, because the account has never been credited, with 403, and a nonce
other than the account's next with 409 and the `expected` nonce; none of these reaches the log. A
request refused with 403 or 409 uses no nonce, so it stays valid until its `expires_at`: resubmit that
same request once its nonce is next, or let it expire before signing different content at that nonce.

From V24, a wallet sets up before its first deposit, in this order:

- `POST /v1/eligibility` needs no deposit. The API screens a declaration like an order, and the engine
  records it only on verified, allowed and clear evidence; a refused one is in the public log with its
  reason. A second declaration gets 403 `account has already declared`.
- `POST` and `DELETE /v1/session-keys` work once it has declared, and get 403
  `a never-credited account must declare first` before that. A request the engine refuses, such as a
  key over the ten-key cap, answers 200 with `session_key_rejected`, is in the public log, and uses its
  nonce.
- Its orders, cancels and withdrawals, signed by it or by a session key, get 403
  `account has never been credited` until a deposit credits it. A session key's signature on a
  declaration or a key request gets 401 `invalid signature`. A deposit after the declaration credits
  at its first screening; a deposit without one stays pending and credits on the retry after a later
  declaration.

Each of these never-credited requests must expire within five minutes (400 otherwise) and takes one
token from its client's share of a budget all clients share (429 `too many new wallets are being set
up; try again later` with `Retry-After` when either is out). A declaration from an account with a
pending deposit takes neither, only a token from a bucket of its own at the share's rate and one from a budget all such declarations share. A declaration also waits for a screening slot (503 `declaration screening is at capacity` with
`Retry-After` when declarations hold their share). See [access-source configuration](../../config/access-source.md) for the budgets' rates and the
growth they allow.
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
`emission_allocated`, `staked`, `unstaked`, `staking_rejected`, `distribution_allocated`, or
`operator_share_allocated`.
`?limit=` is 1 to 200, 50 by default. A page is one search of a SQLite index that reads
`emission_allocated`, `eligibility_attested`, `eligibility_rejected`, `deposit_pending`, or
`deposit_held`. `?limit=` is 1 to 200, 50 by default. A page is one search of a SQLite index that reads
at most `limit + 1` entries, and at most 16 pages are read at once. Until the index has taken every
command the log held when the node started, the route answers 503 (`history is catching up with the
log`). History is public, like the log.

`GET /v1/house` answers `cash`, `nav` and `gross_exposure` at the last command, and from the last
scan `seq`, `snapshot_ms`, `open_interest` (the notional of every client position, long and
short) and `open_positions`, with `markets`: each market's `long`, `short` and `net`
notional in USD, position counts, `unrealised` (the house's result at the mark) and `volume`; those
come from the latest scan of either kind, at most `api.stats_light_interval` old. `open_margin`, with
its own `margin_seq` and `margin_snapshot_ms`, comes from the last full scan, at most
`api.stats_interval` old. The scan figures are `null` until the first scan; `volume` sums the last 24 hourly buckets and
`volume_since_ms` says where that window starts. `GET /v1/house/positions?market=` answers one
market's positions as aggregates only: `closeout`, USD and counts by the move from the mark that
would close the account out (the account's other positions held at their marks; a long falls, a
short rises), with `due_now` and `beyond_range`, and `profit` buckets by side; no account is named,
and the answer is 503 until the first scan. `GET /v1/monkey` answers the emission schedule and
totals (`emissions`), the stake (`staking`) and the distribution state (`distribution`, whose
`excess_usd` is what the engine would distribute at the last scan, in USD, and the amounts of USDC
are named `_usdc_units`, whole base units). `GET /v1/monkey/activity?limit=&before=`
pages the newest loss allocations, stakes and unstakes, newest first, with ids and `next` as decimal
strings. `GET /v1/monkey/distributions?limit=&before=` pages the closed distribution epochs, newest first
(`limit` 1 to 200, 50 by default; `before` an epoch number, as a page's `next`): each has its `epoch`, the
`close_seq` and `close_ms` of the command that closed it, `ended_ms`, `nav`, `floor`, `excess_usdc_units`,
`allocated_usdc_units`, `stakers_usdc_units` with the number of `stakers` paid, `operator_shares` (every
recipient with its `amount`: before `V50` none for a close with rotation off, and from `V50` each account
whose time in office in the epoch earned a part) and `skipped` (the range of earlier epochs no command
saw the end of, which that close distributed nothing for). It states where its data begins like the other
answers, and `retained_from_epoch` names the oldest epoch held, with `truncated` true once `[stats] distribution_limit`
(5000 by default) has dropped older epochs. `GET /v1/stats/series?metric=&market=&resolution=(1m|1h|1d)&from_ms=&to_ms=&limit=` pages a
metric's buckets oldest first: `nav`, `cash`, `withdrawal_queue`, `total_staked`, `total_allocated`,
`cumulative_distributed`, `long` and `short` (per market), `volume` (per market or all),
`monkey_per_loss` and `distributed_per_epoch`. The series, activity and MONKEY answers state where their data begins
(`begins_ms`, `begins_seq`, `from_genesis`), and positions are dated by their full scan (`seq`, `snapshot_ms`);
the activity feed also says when it has dropped older entries (`truncated`, `retained_from_ms`). The record is derived from the log and held in memory:
recovery rebuilds it by replay, so after a recovery from a snapshot it begins at the first command
after it. A full scan of every account (margin, the position maps and the distribution excess) is due at
most once per `api.stats_interval` of sequenced time, and between full scans a light one, which totals
only each market's long and short, the open interest and the open positions, at most once per
`api.stats_light_interval` (5 s by default; a value above `api.stats_interval` leaves the full scans
alone). Each light scan samples the long and short series, whose buckets and retention are unchanged. A running node takes them on a worker thread holding a copy of the engine that applies
every command the sequencer applies, so a scan reads the engine at the seq it fell due at and never holds
the sequencer (100k levered accounts with staking initialised: a full scan takes about 29 ms and a light
one about 5 ms, now on the worker, and the longest command in a scan interval cost 12 us on the
sequencer's thread, `stats::bench`). The copy takes about 20 MiB at 100k such accounts and a core that
the scans use 0.15% of; at 1000 and 5000 commands a second the worker fell at most 55 and 205 commands
(55 and 41 ms) behind, against a queue of 100,000. The copy
also answers the API's engine-only views (markets, benchmarks, account, nonce, withdrawal queue, operator and
access holds) off the sequencer's thread: it applies a batch's commands once the batch is durable and a
view queues behind them, so a view shows each command whose reply its caller has had and nothing a crash could
take back, and a scan in progress delays views, not prices. House and dashboard views, which read the record
a batch ahead of the copy, proofs, quote reads and the checks that admit a signed request stay on the
sequencer, and their answers wait for the log to hold the seq they read. The result is installed by the next command or read
after it finishes,
every due scan is taken, and if the worker stops the record scans on the
sequencer's thread again and raises the `stats_scan_inline` warning. A replay scans on its own thread, lightly for all but the last, so a replay of a log at 100k accounts spends about 5 ms per light interval of sequenced time on it. `[stats]` sets each resolution's retention (1m 48h, 1h 90d, 1d
unbounded by default), the activity length, and the close-out and profit buckets.

From `V54`, `GET /v1/accounts/{address}` adds `toxicity`: `{"score", "fills", "tier", "add_on_bps"}`, the
account's score in standard errors, the decayed fills behind it, the tier that applies (0 for none)
and the spread it adds to every fill but a close-out, as of the read; `toxicity_bps` stays 0.

`GET /v1/accounts/{address}` adds `closeouts`, one entry per position in market order:
`{"market", "reach": "due" | "at" | "beyond", "price"}`. `price`, in the market's quote currency, is set
only for `at`: the price the market must reach for the account to fall below its close-out level, the
account's other positions held at their marks, so it moves when they or the account's balance do. `due`
is an account already below the level at its marks, and `beyond` one no price closes out: a long's would
be zero or below, and a short's ten times its mark or more. It is the calculation behind the close-out map
(`stats/closeout.rs`), over a wider range, with the map as of its last scan and the read as of now. It is an estimate of where the account crosses the
level: a close-out closes the largest losing position that can trade first, which may be another one, and
none trades outside its market's session. A move in an FX market also moves the accounts' positions
quoted in its currency, which the estimate holds at their marks. The read runs on the sequencer's
thread (about 9 µs p99 for an account holding all 37 launch markets, 7 µs of it the close-outs, against the 200 µs command budget; `stats::tests::closeout_cost`).
The read also gives `close_fees`, keyed by market, one entry per position from `V55` and empty
before it: `{"opened_at", "fee_bps", "falls_at"}`, the position's open time (an add moves it to the
size-weighted average of the add's time and the open time, counted as no earlier than the add's time
less the curve's last hold, from `V57` the launch curve's), the profit fee rate in bps a gain closed
now pays, and the sequenced time that rate next falls, `null` once it is the base. "Now" is the
engine's last sequenced time.

`GET /v1/markets/{id}/marks?resolution=(1s|1m|1h|1d)&from_ms=&to_ms=&limit=` returns
`{"market", "resolution", "entries", "next", "max_limit"}`: one market's index-mark history from the
derived marks cache, oldest first. Each entry is one bucket — its window start `ts` and its OHLC,
prices as decimal strings — with a `provenance` of `log_derived` or `seeded_from_provider`, so a
client can weigh seeded history against what the public log proves; after a full recovery without
`[snapshots]`, a bucket a log mark has reached holds what the public log proves, because that
recovery re-derives the log-derived buckets from the verified log, so a `log_derived` label does
not cover rows the host wrote while the node was stopped. A node with `[snapshots]` re-derives
none and serves the rows it imported. Every row is also chained: each bucket names the bucket below it, the row's
authentication tag binds that name, and per-pair chain anchors — the floor and the head, committed
in memory atomically with each write — reject a host that deletes, reorders, or rolls the cache
back mid-run: the next read over the broken span or ends fails before anything is signed, and a
write never re-links over a deletion it did not itself make. A row also records the tag the row
below it carried when the row was written, so the row above vouches for the content a bucket ended
with. A row a write re-links, or changes after another arrived above it (a gap filled under it, a
late sample merged into it, a re-floor), is pinned in memory at its new tag, and a read refuses a
pinned row at any other version, whether it serves the row or only ends beside it. A write refuses
to merge into, re-link, re-floor, or bind the tag of a row that is not the version it last wrote.
Rows are authenticated under a key derived from the enclave key and a nonce kept in each cache file,
and the recovery reset draws a new nonce and re-tags the rows it keeps, so a row captured in an
earlier run, or before the reset, does not verify after it, and an older authentic version of a row that the
host puts back while the node runs is refused wherever it would be served, including by a page that
would hide a bucket behind it. A host can still delete or corrupt the cache while the node runs,
which fails reads, and stops the node when a later write trips over it, until the cache is rebuilt,
but not make a read serve an older version of a row this run wrote. The pins live in memory with
the anchors. At startup the prior cache's rows are verified under that file's key, checked by link,
and re-tagged under this run's key, so an older authentic row the host restores while the node is
stopped, or newest rows it removes, are not detected. A full recovery without `[snapshots]`
re-derives the log-derived rows from the verified log and corrects them, though not provider-seeded
rows the host removed, and a node with `[snapshots]` attaches no marks sink, whichever way it
recovers, so it corrects none.
`?limit=` is 1 to `max_limit` (2000), 500 by default, and a window wider than that pages by
passing a page's `next` as `from_ms`. The cache is derived state, never sequenced: recovery seeds
it from the log's replay, the sequencer's live mark reports keep it current, and
`[history.marks.backfill]`, where configured, fetches historical candles from the feed venue's
public endpoint (Coinbase's, for its markets) to seed the depth a fresh log cannot have; removing
`history.dir` while the node is stopped rebuilds it. A cache file written by an older version is
not imported: a full recovery re-derives what the log proves and a configured backfill re-seeds
the rest.
`[history.marks]` also tunes
each resolution's retention — 1s for 48h, 1m for 60d, 1h for 2y, 1d unbounded by default — and
`[history.marks.backfill]` sets whether to backfill, the endpoint's base URL (the venue's own,
over TLS), and each resolution's depth. Markets no bucket has reached yet answer an empty page.

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
log; changing it needs a fresh log, except by a rotation: from `V35`, `engine admin rotate-operator
<address>` hands the role to another key seven days after it is logged, `--rotated` signs with that
key, and a restart accepts the log's rotated key under the config that names the original one, as
long as the chain and vault match. `announce-params` gives the notice a profit fee or close-out level
change needs, and from `V49` with `--stress-from <params.toml>` the notice for the stress levels, move
window and cooldown that a `set-params` carrying that file applies, and from `V54` with
`--score-tiers-from <params.toml>` the notice for the toxicity score's tiers, and from `V55` with
`--profit-fee-curve-from <params.toml>` the notice for its `profit_fee_curve`, which must stay at or
above the launch curve at every hold, as the profit fee stays at or above its launch value; `announce-markup <market> <bps>
--effective-at <time>` gives the notice a listed market's markup change needs before an
`upsert-market` carries it. From `V52`, `announce-listing <market>` with `--spreads-from` and
`--margins-from` (TOML market specs whose bands it names) and `--markup-bps` gives the notice a listed
market's spread band, margin band or markup change needs, and `adopt-close-spread-ceiling --listing
<spec.toml>` names the listing an adoption for a market not yet listed binds. From `V37`, while the operator registry has rotation on, the engine refuses
`rotate-operator`, and an election's key takes over the same way: from the election's effective
time, or seven days after the election is logged when that is later, and once the staking mirror
has completed a finalised block stamped at or after the registry's effective time, signing with
`--rotated`.
`GET /v1/house/state` returns `{"seq", "params", "params_notice", "score_rules", "listing_notices",
"checkpoint"}` from memory, signed like the other reads, with no log scan, so a client learns the
protocol parameters, the pending notices and the latest checkpoint without replaying the log.
`params_notice` is the pending parameter notice (`null` while none waits): each of
`profit_fee_bps`, `closeout_level_bps`, `stress`, from `V54` `score_tiers` and from `V55`
`profit_fee_curve` it names, and `effective_at`. `listing_notices` maps each
market with a pending notice to it (`spread_tiers` in tenths of a bp, `margin_tiers`, `markup_bps`,
each left out when not named, and `effective_at`); the bands in force are `GET /v1/markets`'s. `seq` is the log head the read was taken at (`null` on an empty log).
`params` is `null` until the first `SetParams` applies, then the parameters in force, as the
`set_params` command encodes them (`profit_fee_bps`, `closeout_level_bps`, `house_total_cap_bps`,
`house_total_cap_floor` left out when zero, `stress_levels`, from `V55` `profit_fee_curve`, and the
rest), plus `seq`, the seq of the
`SetParams` that set them (`null` only when a snapshot written before it carried the seq restored the
node and no `SetParams` has applied since; an activation that converts the parameters in place, as
`V13`'s divergence shares, `V52`'s 1x multipliers, `V54`'s score tiers and `V55`'s default curve and
launch base do, leaves it at that `SetParams`).
From `V54`, `score_rules` gives the toxicity score's protocol constants: `horizon_ms`, `half_life_ms`,
`min_fills`, `weight_cap`, `grid_ms` and `window_ms`; the score's tiers are `params.toxicity.score_tiers`.
`checkpoint` is `null` until the log holds one, then the
latest checkpoint as the stream carries it (`offset`, `type: "checkpoint"`, `seq`, `root`,
`signature`, `signer`, and the rest). The sequencer keeps the checkpoint as it logs it and the seq as
it applies the command; recovery restores both from the replay it already runs, or from the snapshot
it starts at, which carries them. The answer waits for the batch before it to be flushed, so it never
shows what a crash could take back.
`GET /v1/operator` returns the key in effect, the pending rotation and its time, whether rotation is
on, and the last election. With `operator.rotation_enabled` and `[checkpoints]` it also names
`api_hostname`: the API hostname the vault's registry records for the operator it names, with the
`block` and `block_hash` of the finalized block it was read at, refreshed every `api.operators_interval`.
Stream clients reconnect there and resume with `from=<seq>`; answers keep verifying against the same
enclave address because the keys move with the turn. The value comes only from the registry, never
from this node's config, and is left out while it cannot be read. `GET /v1/operators` lists the registry's approved candidates (at most 100, by
registration order) with each one's address, `admin_key`, `enclave`, `self_bond`, `release`, `registration`,
`api_hostname`, `handover_endpoint`, `votes`, `eligible` and `role` (`candidate`, `elected` or `operator`), and the
registry's `rotation_enabled`, `parameters`, open epoch, `total_staked`, `operator_in_effect`, `named_operator`
and `pending_election`. It is read through `RegistryLens` (`contracts/src/RegistryLens.sol`) over the custody
quorum, in an `overview` call and pages of 25 candidates at one block the providers' majority declares final
(`custody.evm.withdrawal_finality_tag`), every `api.operators_interval` (60 s by default, at least 1 s); the answer
names that `block`'s number, hash and time. The lens's return data must be identical across a strict majority,
executed in the canonical block by hash, on the configured chain and token, and the lens must name the configured
registry; an inconsistent answer is not served. A failed read raises the `operators_read_failed` Warning and leaves the last list
up for `custody.evm.finality_stalled_after`, after which the route answers 503 until a read succeeds. Without
`custody.evm.staking.registry.lens` the route answers 404 and the node runs as before. With
`custody.evm.staking.registry.schedule`, the registry's `OperatorSchedule`, the watcher mirrors its
`RotationSetAdopted` and `TurnAssigned` events with the registry's from protocol `V51`, once a read at a
window's end shows the schedule names the configured registry, and refuses a schedule event before then. With
`[custody.evm.staking.registry.vote]`, the `ParameterVote` over the schedule and its `restriction_ttl`, the node
records both in the log once protocol `V56` applies (`SetGovernance`), and refuses to start on a log that records
another; the watcher mirrors the vote contract's `ProposalPassed` events from `V56`, once a read at a window's end
shows the vote contract names the configured schedule, each carrying the admin command its bytes are the canonical
JSON of, and the sequencer sequences `ApplyProposal` for a passed normal change at its first stamp past its seven
days. While rotation is on, the node skips its own protocol activation and access policy, which wait for a vote.
`GET /v1/house/state` adds a `governance` section, the recorded contract, each change waiting with its
`applies_at`, each restriction in force with its `expires_at`, and the baseline of each field one holds, and
`GET /v1/operators` adds the vote contract's open `proposals`, read at the same block, newest first, with each one's
weights, window and decoded command, or marked `unavailable` when the read fails. A credited proposal the engine refuses raises the `proposal_refused` Warning,
and a passed change the engine refuses at its time `proposal_rejected`. Building the answer from the lens's return data at 100 candidates takes about 80 µs for the quorum's votes, the decode and the checks and about 100 µs for its 46 KB of JSON (`operators::tests::build_cost_at_the_registry_limit`); a read makes six lens calls to each provider, none on the sequencer's thread. A 200-epoch page of `/v1/monkey/distributions` from a full history of 100,000 epochs takes about 4 µs under the record's lock and 130 µs with its 63 KB of JSON (`stats::tests::distributions::page_cost_from_a_full_history`). A refused registry mirror raises a Critical `election_rejected` alert and
holds the staking mirror behind it. With real attestation, `rotate-operator` and the admin route refuse an address that `config/test-keys.toml` lists, and the route refuses a request such a key signs unless it is the configured operator's, and the route admits a pending rotation's key only once its time has come by the node's clock. `engine replay` prints the operator a log records. A node
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
as the proof does: `seq`, `root`, `account`, `entitled` (base units of the 18-decimal token, as a
decimal string), and `siblings`, from the root down. The vault's `claimEmissions` takes the proof's
`seq` and checks the proof against the root it recorded there, in the signer epoch in force, and the
node's newest checkpoint runs ahead of it while posting lags, so the answer also says what it is
proven against: `accepted_seq` (the vault's latest included checkpoint as the node last read it,
`null` before one, and while the signer that recorded it is replaced or retired), `newest_seq`,
`newest_entitled` (the amount the newest root holds) and `claimable`. When `claimable` is true the
proof is at `accepted_seq`, and the claim can land, as it still can once later checkpoints post; its
`entitled` can be below `newest_entitled`, since an entitlement only grows, and the vault pays
`entitled` less what it has paid, the rest on a later claim. When false the proof is the newest
checkpoint's and waits until the vault records its `seq` in the signer epoch in force, so ask again
rather than keep it.
The node keeps an account's proof for each checkpoint from its first request here (a 404 counts),
for the 128 accounts that asked most recently, until the vault accepts a later checkpoint; so a
first answer is not claimable unless the vault is at the newest checkpoint, and a restart forgets
what was kept. A change of signer ends every earlier proof for good, and a proof at the new signer's
first checkpoint pays what they held and anything since.
An account has no entitlement, and gets 404, until the first checkpoint after a period in which it
was allocated a share.

`/v1/accounts/{address}/distributions` returns, from `V25`, the account's cumulative USDC
distribution the same way (`entitled` in the 6-decimal token's base units, with the same
`accepted_seq`, `newest_seq`, `newest_entitled` and `claimable`). The vault's
`claimDistribution` pays the USDC the distribution adds to what the account was already paid,
against the root of the proof's `seq`, under the same rule, and `claimable` does not read a withdrawal
pause, under which the vault refuses the claim. An account has no distribution, and gets 404, until
the first checkpoint after an epoch in which it was allocated a share. From `V47`, while rotation is on, the
operator's fifth of each epoch's distribution is in this same leaf, under its candidate's address,
and from `V50` each operator's part of it by time in office is in its own.

`/v1/accounts/{address}/exit` returns, from `V38`, every proof the account exits the vault with
against the state root of the last checkpoint: `terms` (the vault deposit cursor and the pending and
held principal the exit reserves), `account` (its leaf, positions and their markets, as
`/v1/accounts/{address}/proof` serves them), `approvals` (every approval it holds; the vault's
`usedNonces` says which are unpaid), `queued`, `distribution`, and `pending_deposits` (deposits for
it pending a declaration or screening, refunded to their payer). Amounts are the engine's decimal
strings. The vault checks them against its latest posted root, so they serve the exit once the
vault's `latestSeq` is the proof's `seq`. The route gets 404 until the first checkpoint under `V38`.
`engine exit-proof` builds the same bundle from the public log after the node is gone.

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


### Public API certificate

With `[api.tls.acme]`, the node has a public CA certify the TLS key it generated in the enclave, the
key whose hash the attestation carries, for the API's domain. Browsers then trust the API, and a
client that pins the attested key still matches it. A production build refuses a config without the
section.

```toml
[api.tls.acme]
domain = "api.example.com"
directory_url = "https://acme-v02.api.letsencrypt.org/directory"
contact = "mailto:ops@example.com"
# renew_before = "30d" # at least 14 days; one third of each certificate's life when unset
```

Setting the section agrees to the CA's terms of service. The domain is in the config file whose
SHA-256 the attestation binds, so it is part of the release manifest; the attestation bundle's
layout is unchanged.

At boot the node registers an ACME account under a key it generates in memory, orders a
certificate for the domain, and answers the CA's TLS-ALPN-01 challenge on `node.listen`: while the
challenge is open, a handshake that offers `acme-tls/1` for the domain gets the challenge
certificate, and every other handshake gets the API's certificate with `http/1.1`. Validation
connections pass the same connection caps and handshake timeout as any other. The order is
finalized with a request signed by the API's key, and the issued chain is served from the next
handshake once its first certificate names the domain and carries the attested key. Neither key is
written to disk or logged.

The sequencer never waits for the CA. Until the first certificate, the API serves its self-signed
one and the node raises `certificate_pending` (Warning). It renews once `renew_before` of the
certificate's life remains, measured on the monotonic clock from the order's finalize request
against its own validity period, so the host's wall clock does not move it; a CA that backdates `notBefore`
(Let's Encrypt by an hour) makes renewal that much later, which the floor of 14 days on
`renew_before` keeps well before expiry. Because the life counts from the finalize request, which
precedes the certificate's arrival, a certificate downloaded late is renewed sooner, and one whose
life ran out before it arrived is refused. A failed attempt keeps the current certificate serving and
retries after a minute, doubling up to an hour; once fewer than 14 days remain, each failure raises
`certificate_expiring` (Warning). A failed attempt takes up its order again unless the CA ended it,
so a lost answer or a passing problem (`serverInternal`, `rateLimited`, any 5xx or 429) never costs
another issuance; an account the CA no longer knows is registered again. Every request to the CA has a
30-second deadline and every attempt a 10-minute one, and the CA is authenticated against the
compiled-in Mozilla roots, so the host, which resolves its name and carries the connection, can
delay or drop the exchange but not answer it without a certificate for the CA's name (validity is
judged by the host's clock, a TODO in `src/tls/acme/http.rs`).
`engine_tls_certificate_not_after_seconds` on the metrics listener holds the served CA
certificate's expiry, 0 before the first, and `engine_tls_certificate_pending` is 1 until the first;
`ops/prometheus/engine-node-rules.yml` pages when a node stays pending for 30 minutes or its
certificate comes within 7 days of expiry.

Before the first production certificate:

- Point the domain at the host, or at a pass-through proxy that never decrypts, which sends a PROXY
  protocol v2 header and is listed under `[api.proxy_protocol]` (see the API limits above), and
  forward port 443 unmodified at layer 4 to `node.listen`: the CA validates by connecting to port 443, and
  TLS terminates in the enclave.
- Publish a CAA record that allows only the chosen CA, and only TLS-ALPN-01, such as
  `api.example.com. CAA 0 issue "letsencrypt.org; validationmethods=tls-alpn-01"`. The CAA record
  narrows issuance to that CA and method, but not to the enclave: the host forwards port 443, so it
  can answer the CA's TLS-ALPN-01 validation itself and have a certificate issued for a key of its
  own, as can whoever controls the domain's DNS. A browser that trusts the certificate alone cannot
  tell; the console's in-browser attestation check and signed answers (#292) are what bind it to the
  enclave, and `engine ct-watch` flags any certificate whose key is not the attested one. With
  `[tee.sealing]` the ACME account is sealed with the keys and moves with them, so a CAA
  `accounturi` naming it binds issuance to the enclave's account too, across every handover; the
  account's URL is in the node's log when it registers.
- Start `engine ct-watch` (`config/ct-watch.toml`) on a host independent of the node's, before the
  first production certificate is issued: it reads each log from its current end the first time, so
  it does not see a certificate logged before it started.
- Run against the CA's staging directory first (for Let's Encrypt,
  `https://acme-staging-v02.api.letsencrypt.org/directory`), then its production one.

With `[tee.sealing]`, the account is sealed with the TLS key, and each certificate in the member's
own file, `<release id>.certificates.json`, sealed to the release alone as the delivered secrets are,
with the domain, the TLS key, and the directory it was issued for. Under daily turns every member
serves its own hostname with the same TLS key, so a certificate stays with the member it names: no
handover moves the file, and the keys the next member takes carry none. A restart, or the member's
next turn under the same TLS key, serves the certificate it holds from the first handshake, without
`certificate_pending`, and renews it on schedule, its age read from trusted time once a sample
arrives, under the same account; one inside its renewal window, or past expiry, is renewed at once,
and one for another domain, key, or directory is never served. So daily turns stay within the CA's
per-hostname limits. A new release, an upgrade included, orders its own certificate under the account
the keys carry. A file that does not open stops the start before the keys open or move
(`sealed_certificates_unreadable`, Critical); one that cannot be written leaves the new certificate
served (`sealed_certificate_write_failed`, Warning). Without sealing, every start generates a new TLS key
and ACME account and orders a new certificate, which the CA's issuance limits cap: Let's Encrypt
publishes, among others, 5 certificates per exact set of identifiers every 7 days, 10 new accounts
per IP address every 3 hours, and 5 failed authorizations per identifier per account every hour
([rate limits](https://letsencrypt.org/docs/rate-limits/)). A host that deletes the sealed file, or
a new key after a compromise, orders again under these limits, and rehearsals belong on the staging
directory.

### Delivered secrets

A production node's credentials never reach the host in plaintext. The config, which is attested and
published, names which secret each field reads, and the operator delivers the values into the enclave:

```toml
[tee.sealing.secrets]
# The delivery listener, plain TCP: the host carries it, so the quote, the encryption, and the
# operator's signature protect it, not the transport. Keep it off public interfaces (below).
listen = "10.0.2.15:9472"

[custody.evm]
providers = [
  # {name} in a URL's path or query. The host stays in the attested file: these providers' quorum
  # credits deposits, so a bundle can change a provider's key, never which provider is asked.
  { name = "provider-a", rpc_url = "https://rpc.provider-a.example/v2/{provider_a_key}" },
]

[[feeds.venues.connections]]
venue = "massive"
url = "wss://socket.massive.com/futures"
api_key_secret = "massive_api_key"
symbols = [{ market = 20, symbol = "ESZ6", contract = "ESZ6" }]

[alerts.critical]
url_secret = "alerts_critical_url"
```

`checkpoints.rpc_url` and `access.endpoint` take a `{name}` the same way. A value is percent-encoded
into its place, so it cannot end the path or query it sits in. A URL with no `{name}` is used as
written, for a provider that needs no key. A production build refuses `api_key_env` and `url_env`,
but for `run --standby`, which no enclave runs: the guest's only environment is the measured kernel
command line, and a host that could set one would choose the secret.

- **Delivering.** `engine admin deliver-secrets` sends a fresh nonce to the listener, and the enclave
  answers with a one-time HPKE key (the suite the key handover uses) and a quote over the nonce, that
  key, its config's hash, and what it holds. The tool checks the quote's chain to the pinned vendor
  roots, its measurement, policy, and TCB against the release manifest, and the config hash against
  the manifest and its own `--config`, and only then encrypts, with the release's id in the HPKE
  context. Another image or config has neither the private key nor the context, so it cannot read a
  bundle, and it fails the tool's checks before one is sent.
- **Who may deliver.** The bundle carries a signature over the chain id, the vault, its version, and
  its contents. A running node takes the key the log records or a rotation's that has come due, as
  `POST /v1/admin` does, by trusted time, so a host's clock brings no rotation in early; while the
  registry has rotation on it takes only its own config's key, so an elected key reaches into no other
  operator's enclave, and so does a node whose config names the key the registry installed, until
  that key is the log's. A node that
  has not yet recovered its log takes the key its attested config names, since reading the vault can
  take a delivered key, so no log can be authenticated yet. That is the one key that can end a hold, so
  a rotated operator keeps its old key until a release's config names the new one; a redelivery under
  the new key replaces the sealed signature, for the upgrade that follows. A real enclave refuses a
  published test key.
- **Reaching the listener.** Anyone who can connect can read the offer, whose names are in the
  published config anyway, and cause a quote; only the operator's signature seals anything. The listener
  runs a few connections at once with short deadlines, so an idle peer does not hold up a delivery, but
  a peer that fills them can delay one, as it can for any open port. The host forwards it only from the
  operator's addresses, as it does the handover port.
- **What a restart keeps.** The bundle is sealed per release in `<release id>.secrets.json` under
  `tee.sealing.dir`, with AES-256-GCM under the platform's derived key and the config hash bound to the
  ciphertext, and without the vault's generation: it must open before the vault can be read, and it
  signs nothing a generation would need to fence. A restart opens it with no delivery.
- **A missing or undecryptable bundle.** A missing or damaged file raises `secret_missing` (Critical)
  for each field, and `secrets_unreadable` for a file that no longer opens, and no default or variable
  stands in. Without a custody or checkpoint RPC key or a screening source's key, the node holds:
  chain access is what opens its sealed keys, a smaller quorum would weaken what credits deposits, and
  no screening would admit what it should refuse. It serves only the delivery listener until a bundle
  supplies them, then goes on starting, with no restart. Without a vendor's key that connection stays
  off and the markets it alone prices go unpriced (`market_no_price`). Without a webhook's URL that
  route stays off, its alerts are only logged, and any pages waiting for it stay in the outbox;
  `engine_alert_route_off` shows it to the operator's Prometheus, which pages by `EngineAlertRouteOff`.
- **A key a provider refuses.** A start whose first read of the chain fails, while `checkpoints.rpc_url`
  names a delivered key, does not power the guest off at once, since that key may be why and only a
  delivery can repair it. It serves only the delivery listener under `secrets_start_failed` (Critical)
  for up to five minutes, then exits, or exits once a new bundle is sealed and its receipt has gone
  out, so the next start takes it. A failure after that read is no delivered key's doing, and exits at once.
- **A rotation.** Deliver a higher `version`, holding every secret: a bundle replaces the whole sealed
  one, so the tool refuses one lacking what the node holds for unless given `--partial`. The enclave
  seals it at once, but what runs keeps the clients it built when it started, so the receipt says
  `restart required`, `secrets_restart_required` (Warning) stays open until a restart, and the old key
  should stay valid at its provider until then. The restart keeps the enclave's keys. A retry at the
  same version with the same secrets, after a receipt the host dropped, is accepted as already sealed;
  the same version with others is refused, as is a lower one.
- **An upgrade.** With `--predecessor`, the new release asks the running one for the bundle before it
  reads the vault. The running release checks the new one's quote and that the vault names it, and
  sends the bundle encrypted to its one-time key, with no stop; the keys follow in their own handover.
  The new release cannot yet read the vault to check who answered, so it takes a bundle only under the
  signature of the operator its own config names; a bundle signed before a rotation is refused, and the
  operator delivers again. The bundle moves whole, since the signature covers all of it: a higher version
  without a retired secret removes it. Since the keys' handover ends the release
  holding the bundle, the new release first makes a few attempts to take it.
- **An election.** No secret moves. While the registry has rotation on, the vault's release changes
  only by election or promotion, and the running release refuses, reading the switch at the finalized
  block that named the release and at the latest one: the keys go to the elected enclave,
  but its vendor accounts and provider keys are its own, delivered under its own key before or after
  it takes over. A standby runs outside any enclave on its operator's own host, so a config with
  `[standby]` writes its keys out and, run with `--standby`, may read `url_env` and `api_key_env` in a production build; its registered release
  takes its own delivery after a promotion. It runs no feeds, so a config with `[standby]` loads
  with vendor connections that name delivered secrets (CME's OAuth pair included) and no
  `[tee.sealing.secrets]`; a node started on that file without `--standby` is refused.
- **What the host can do.** It carries the connection and holds the file. It can delay or drop a
  delivery and forge a refusal, which is the denial it has anyway; the tool reports no receipt, and a
  retry is safe. It cannot read a bundle, substitute an enclave, or replay an offer, since the quote
  binds the connection's nonce; replay a delivery, since each connection has a new key; forge a bundle
  or a receipt. Restoring an older sealed file gives the older bundle, which nothing on the disk can
  tell from the newest: its URLs have the same attested origins, so it can hold only an older key for
  the same provider, which a rotation for cause has revoked, or an older webhook. `secrets-status`
  shows the version the enclave holds, under its quote, and a higher delivery repairs it.
- **Where a value goes.** Nowhere but the enclave's memory, the seal, and the connection it
  authenticates. `Debug` and `Display` redact it, errors name a secret and never its value, the crates
  that see a credential on its way out log only at `warn` or above whatever `logging.level` says, and
  neither the log, metrics, nor a page holds one.

### Enclave keys

With `[tee.sealing]`, the enclave's keys live on the data disk, sealed to the release: its launch
measurement and the config file it runs with, which the vault's timelock names together as the
current release (`setCurrentRelease`), each time in a new generation (`releaseGeneration`). With
`[checkpoints]`, a node opens or seals keys only while the vault's current release at custody
finality is its own, and only in the vault's current generation of it, so the timelock supersedes a
release by naming another, and a release it names again takes the keys afresh from the running
release instead of reopening an older copy; a mock configuration without `[checkpoints]` skips
those checks and seals in generation 0.

```toml
[tee.sealing]
dir = "/data/keys"

[tee.sealing.handover] # needs [checkpoints]
listen = "0.0.0.0:9471"
# Archive mirrors of the incumbent's published log, tried in order before the incumbent's
# registered API hostname when an election names this release the successor; TLS verification
# stays on unless danger_accept_invalid_certs = true (development only).
# mirrors = ["https://mirror.example.com"]
# danger_accept_invalid_certs = false
```

An election handover needs the operator in effect registered as a candidate with the handover
endpoint and API hostname its candidacy registered: the registry records them when the election is
made, and a successor whose incumbent left nothing on chain refuses to start with a clear error.
The runbook registers the operator in effect as a candidate before rotation's first election.

- **What a restart keeps.** The signing key (the vault's signer and the attested address), the
  API's TLS key, and the ACME account, in one file per release and generation
  (`<release id>-<generation>.sealed.json`, as the vault records them), encrypted with AES-256-GCM
  under a key the SEV-SNP firmware derives from the chip's VCEK, the launch measurement, the guest
  policy, and the TCB committed when the file was sealed, with the config's SHA-256 and the
  generation bound to the ciphertext. Only the same code under the same config, launched under the
  same policy on the same chip, opens it, in the same generation; a firmware update leaves the older
  TCB derivable. A restart opens the file, and the certificate's own file beside it
  (`<release id>.certificates.json`), so
  the attested address, the vault's signer and epoch, the outstanding withdrawal authorizations, and
  the certificate are unchanged, and no governance transaction is needed.
- **What forces new keys.** A compromise, or keys lost with no release running to hand them over
  (the file deleted, the VM moved to another chip, the firmware rolled below the sealed TCB). The
  node then refuses to start until the operator starts it with `--new-key` (in the image, by creating
  `/data/new-key`), which refuses beside an existing file. New keys reach the vault through the
  timelock's `setSigner`, which raises the signer epoch: the vault rejects the retired key's
  authorizations at once, and the node reissues those still usable under the new epoch; an expired
  one is reissued on its account's signed request (`src/sequencer/payouts.rs`). Clients
  see the new address in `/v1/attestation`, and `engine ct-watch` accepts the new TLS key once a
  verified attestation names it; the console pins the release, not the address.
- **A key the vault does not name.** While the vault's signer at its latest or finalized block is
  another address, the node serves only `/v1/attestation` on `node.listen`, raises
  `signer_unregistered` (Critical), and signs nothing; governance verifies that attestation and
  registers the address, and the node then starts as usual. A first deployment passes through this.
- **An upgrade or a config change.** The release changes, so it cannot open the old file.
  Governance first has the timelock call `setCurrentRelease(launchDigest, configHash)` on the vault
  with the new release's launch measurement and config hash; once that block is final, the operator
  starts the new release with `--predecessor <host:port>` (in the image, `/data/predecessor` holding
  the address), naming the running release's handover listener. The running release keeps
  sequencing until then, though a restart would now refuse its keys. Setting it back as current calls
  the upgrade off in another generation, so it then hands its keys to a fresh start of itself the same
  way to keep them across a restart. A running release raises `release_superseded` (Critical) within
  a minute of the vault naming another release or generation, since a restart would then find no
  keys. On each connection the running release sends a fresh challenge, which the new one's quote
  binds with its one-time key and config hash, so a recorded request or confirmation is refused on
  any other connection. It checks that quote against the vendor's chain at trusted time, the
  measurement it proves and the config hash it binds against the vault's current release at custody
  finality, and its guest policy and TCB against its own. It refuses while its checkpoint signing is
  withheld (`checkpoint_withheld`), since it could not sign a final checkpoint, and goes on
  sequencing; otherwise it stops sequencing, takes its final checkpoint, and sends the keys
  encrypted with HPKE to the new release's attested one-time key, with its own quote over them. The new release takes them only if the vault already names their signing key, so an
  enclave the host runs cannot hand it a key of its own, and only while the vault still names it; it
  seals them for its release in the vault's current generation and confirms with the handed-over
  key, and the old release acknowledges with the same key and exits.
  The new release's first start refuses a log that does not end at the old release's final
  checkpoint, which the old release takes from its sequencer's own output rather than the disk. The
  address, signer epoch, authorizations, and certificate carry over. Either side that misses the
  other's signed confirmation or acknowledgement raises `handover_unconfirmed` (Critical). The old
  release stays stopped and goes on serving new releases until one confirms, since its own copy no
  longer opens: stopping it then loses the keys. The new one refuses to start with the keys sealed,
  and is started again only once the old one has stopped. The old release's sealed copy stays on its disk but
  never opens again: the vault names another release, and naming the old one again starts a later
  generation, in which it takes the keys afresh from the running release. An upgrade that fixes a
  flaw in it still rotates the signer, since the old release has held the key.
- **What the host can do with the file.** Delete it: the node refuses to start, and new keys need
  governance. Restore an older copy: it opens only for the release that sealed it, while the vault
  names that release in the generation it was sealed in, and holds the same keys, or keys the vault has since retired, which the
  startup signer check holds at the attestation-only wait. Rename it or rewrite its header for
  another release: decryption fails. Alter it: decryption fails and the node refuses to start.
  Start a second copy of the current release from a copy of the disk: both unseal the same keys, but every withdrawal waits for a checkpoint that extends the
  vault's latest, so the copy whose checkpoint the vault rejects stops with `checkpoint_conflict`
  and nothing pays twice; its signed API answers still verify against the attested key.
- **Under the mock TEE** the stand-in measurement comes from `--mock-launch-digest <hex>`, never
  from the config, and the stand-in key is a public hash of it, so anyone can open the file; a
  production build refuses the mock TEE.

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

The alert webhook URLs are credentials, so config names the delivered secret that holds each
(`alerts.critical.url_secret`, `alerts.warning.url_secret`) or, in a development build, the
environment variable (`url_env`), rather than the URL. A URL must be HTTPS unless it is HTTP to a
loopback address, and the node refuses to start when a named variable is unset. A node whose TEE or custody provider is not `mock` refuses to start without
`[alerts.critical]`, since its Critical alerts would otherwise page no one.

Vendor API keys are credentials too: each `[[feeds.venues.connections]]` entry for Massive or
TraderMade names the delivered secret that holds its key (`api_key_secret`) or, in a development
build, the environment variable (`api_key_env`), and a node with `feeds.provider = "venues"`
refuses to start while a named variable is unset or empty. Every endpoint is `wss://` on the
venue's own domain, since only a source reached over TLS that ends in the enclave may set a price.
A Massive connection names one of its `/forex`, `/futures`, or `/stocks` feeds; a symbol for a
market priced from futures names the `contract` it quotes, one listed in the market's `contracts`,
and the feed sends quotes for each contract the market takes now. A production build refuses a
config in which fewer venues quote a genesis market, or any contract its genesis listing names,
rolled off or not, than its `min_sources`. Genesis is fixed for the life of a log, so a later
release keeps those venues and contracts even after a relisting lowers `min_sources` or rolls past
a contract. A futures connection, in a development build:

```toml
[[feeds.venues.connections]]
venue = "massive"
url = "wss://socket.massive.com/futures"
api_key_env = "MASSIVE_API_KEY"
symbols = [{ market = 20, symbol = "ESZ6", contract = "ESZ6" }, { market = 20, symbol = "ESH7", contract = "ESH7" }]
```

A CME Group connection logs in with OAuth instead of a key: its `oauth` names CME's token endpoint and
the delivered secrets holding the API ID and its password, which no build reads from the
environment. A symbol names a contract as CME's subscription does, by product code and period
(`ES:202612`), since a contract code's one-digit year does not fix the decade, and the config refuses
one that does not name its `contract`:

```toml
[[feeds.venues.connections]]
venue = "cme"
url = "wss://markets.api.cmegroup.com/marketdatastream/v1"
oauth = { token_url = "https://auth.cmegroup.com/as/token.oauth2", client_id_secret = "cme_api_id", client_secret_secret = "cme_api_password" }
symbols = [{ market = 20, symbol = "ES:202612", contract = "ESZ6" }, { market = 20, symbol = "ES:202703", contract = "ESH7" }]
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
The public `GET /v1/benchmarks` returns the next rollover, the retained rate history, any undated rate
the cap measures from, and the protocol version in force, so the worker
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
From `V46` the protocol refuses a signed rate more than 300 bps per day from the applied rates next
to its date, and any change to a date already applied, a publisher's revision included. The worker
leaves such a rate out of its snapshot and logs `benchmark_rate_refused` at Warning severity, naming
the benchmark, and signs everything else, so a refusal holds back neither the other benchmarks nor a
rollover waiting for a snapshot. The worker routes its alerts through the config's `[alerts]` webhooks,
so it needs their environment variables too, and keeps its own outbox beside the node's, in the
outbox directory's name with `-benchmark-worker` added, which must be creatable and writable on the
worker's own host: the worker opens it before it signs anything, and fails naming the path if it
cannot. The last applied rate keeps financing, the limit
grows with each day from it, so a real move lands once it fits (but for a rate measured from an undated
rate, which only a log from before V18 can hold with no dated history, and whose one-day limit is walked
by hand), and `benchmark_rate_stale` escalates if the benchmark stays behind for five weekdays. `research/benchmark-cap/` lists what the cap would have
refused in each benchmark's published history.
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

# Dev scenario: a listed browser origin gets CORS headers and the stream, any other gets neither
./scripts/dev-browser-origin.sh

# Dev scenario: the dashboard routes answer with the figures the accounts placed, verify, and survive
# a restart (waits out one ten-minute MONKEY period, about twelve minutes)
./scripts/dev-dashboard-data.sh

# Dev scenario: limits and screening use the client address a trusted proxy's PROXY header names
./scripts/dev-proxy-protocol.sh

# Dev scenario: a mock feed stops, the market goes stale, and a local webhook gets one page
./scripts/dev-metrics-alerts.sh

# The production build's tests: it refuses the dev config the other tests load
cargo test -p engine-node --features production --lib production_build

# Dev scenario: the Pebble ACME test CA certifies a dev node's attested key over TLS-ALPN-01 and the
# node renews it without a restart; a stopped CA raises certificate_expiring
./scripts/dev-acme.sh

# Dev scenario: loopback market-data vendors behind TLS price FX from two vendors, and index and
# commodity futures and a share from one, across a market open and a futures roll, and the log replays
./scripts/dev-rwa-feeds.sh
# Dev scenario: loopback crypto venues price BTC-USD; a toxic client's tier from its 21st fill, an
# edgeless client at the best price, round trips that leave the score, a replay and a restart
./scripts/dev-toxicity.sh
# Dev scenario: a loopback CME token endpoint and stream behind TLS price US 500 beside a loopback
# Massive across token rollovers; a refused token alerts and leaves it unpriced, and the log replays
./scripts/dev-cme-feeds.sh
# Dev scenario: a signed dividend and four-for-one split through the TLS API, restart and replay
./scripts/dev-corporate-actions.sh
# Dev scenario: the Certificate Transparency watcher against a mock node and a loopback stand-in
# log (needs python3, OpenSSL 3, curl and jq)
./scripts/dev-ct-watch.sh
# Dev scenario: sealed keys only for the vault's current release, across a restart, a missing file,
# an edited config, an upgrade governance makes current with a withdrawal outstanding, the superseded
# release, that release named again, and a second copy, on Anvil (needs Anvil, Forge, and the pinned
# contract dependencies)
./scripts/dev-key-lifecycle.sh
# Dev scenario: on Anvil, a trader's open position is checkpointed and the node stops; the exit
# starts, registers and pays its equity at the root's mark once from proofs the log alone gives,
# refunds a later deposit to its payer once, and releases the remainder through the timelock
./scripts/dev-exit.sh
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
