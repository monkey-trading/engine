# Alerts

Every alert the node and `engine ct-watch` can raise, what it means, and where to go. Read
[reference.md](reference.md) first for the checks the runbooks use.

## How alerts work

- An alert is a log line carrying `alert = "<name>"`. At `error` it is **Critical**; at `warn` it is a
  **Warning**. The same name with `alert_resolved = "<name>"` and the same fields ends it.
- With `[alerts.critical]` and `[alerts.warning]` configured, the node posts the start and the end of
  each episode as JSON to the webhook each section's `url_secret` names. A page has `status`
  (`firing` or `resolved`), `severity`, `alert`, a `key` that is the same for both so a receiver can
  match them, a `summary`, the line's `fields`, and its `context`. Delivery is at least once.
- A repeat of the same key does not page again until it has been quiet for `alerts.episode_quiet`.
  A page is stored before its first post and survives a restart in the alert outbox.
- Without a webhook a severity is only logged. A node with real attestation refuses to start without
  `[alerts.critical]`.
- The **Critical** alerts are the ones to wake someone for. That the alerts are split this way is in
  the code; who is woken is the owner's rota ([incident-response.md](incident-response.md), "Contacts").

## Start-up refusals

Most of these appear as `refusing to serve: <cause>`, after which the image powers the VM off. `secret_missing`, `secrets_unreadable`, `secret_unusable`, `signer_unregistered`, `legacy_deposit_unproven` and `log_tail_lost` do not stop the node by themselves.

| Alert | Level | Meaning and response |
| --- | --- | --- |
| `secret_missing` | Critical | Not by itself a refusal: no delivered bundle holds a secret the config names. For a chain or screening key the node holds, serving only its delivery listener; for a vendor key that connection stays off; for a webhook that route stays off. See [key-recovery.md](key-recovery.md), "Secrets". |
| `secrets_unreadable` | Critical | Not by itself a refusal: this release's sealed secrets file does not open, so the node runs as if it had none until a delivery replaces it. See [key-recovery.md](key-recovery.md), "Secrets". |
| `secret_unusable` | Critical | Not by itself a refusal: a sealed webhook URL is not acceptable, so its route stays off. Deliver an acceptable one. |
| `secrets_start_failed` | Critical | The node's first read of the chain failed, on a delivered key. Its delivery listener stays up for five minutes, or until a new bundle is sealed and its receipt has gone out, and then the image powers the VM off. If the provider refused the key, deliver a working one and start the VM again, with the `new-key` or `predecessor` file its first start had if it has no sealed keys yet. Otherwise see [feed-and-custody-failures.md](feed-and-custody-failures.md), "Checkpoint endpoint". |
| `clocksource_unprotected` | Critical | The guest kernel times on another clock than the TSC Secure TSC protects, one the host can scale, or the hardware reports the guest TSC's frequency under 1000 MHz, where the kernel calibrates against clocks the host emulates. See [restart-and-recovery.md](restart-and-recovery.md), "The node will not start". |
| `enclave_key_failed` | Critical | The sealed keys cannot be opened or made. See [key-recovery.md](key-recovery.md), "How the enclave key is kept". |
| `signer_unregistered` | Critical | Not a refusal: the vault does not name this key, so only `/v1/attestation` is served. See [deploy-and-rollback.md](deploy-and-rollback.md), "First deployment, step 4". |
| `custody_connect_failed` | Critical | The custody adapter could not start: providers unreachable or in disagreement. See [feed-and-custody-failures.md](feed-and-custody-failures.md), "Custody RPC providers". |
| `checkpoint_startup_failed` | Critical | The log fails the checks against the vault before anything is written, or the checkpoint RPC cannot be read. See [restart-and-recovery.md](restart-and-recovery.md), "The node will not start". |
| `log_rollback` | Critical | The on-chain log head names seqs, or a cumulative command-prefix digest, this log does not hold: at start the node refuses before writing anything; at run a posted head that overtakes this node, or names another prefix at a seq this node retains, freezes sequencing. A truncation or a second copy on this key. See [restart-and-recovery.md](restart-and-recovery.md), "A log the posted head does not hold". |
| `log_recovery_failed` | Critical | The log fails recovery: a config that differs from what the log records, a bad signature or root, an unsealed command. See [restart-and-recovery.md](restart-and-recovery.md), "The node will not start". |
| `history_recovery_failed` | Critical | The account history index cannot be opened or repaired. See [restart-and-recovery.md](restart-and-recovery.md), "Rebuild the derived stores". |
| `marks_cache_failed` | Critical | The marks cache cannot be opened or activated. See [restart-and-recovery.md](restart-and-recovery.md), "Rebuild the derived stores". |
| `emission_budget_mismatch` | Critical | The log's emission schedule differs from the vault's MONKEY cap. See [restart-and-recovery.md](restart-and-recovery.md), "The node will not start". |
| `genesis_refused` | Critical | The config's genesis does not apply to this log. See [restart-and-recovery.md](restart-and-recovery.md), "The node will not start". |
| `protocol_activation_failed` | Critical | The newest protocol version does not activate on this log. See [restart-and-recovery.md](restart-and-recovery.md), "The node will not start". |
| `benchmark_sync_prerequisites_missing` | Critical | A log with a rollover schedule lacks recorded client auth, operator or toxicity weighting. See [restart-and-recovery.md](restart-and-recovery.md), "The node will not start". |
| `collateral_relay_failed` | Critical | The vendor collateral relay could not be set up (usually `tee.processor` is missing). See [restart-and-recovery.md](restart-and-recovery.md), "The node will not start". |
| `log_heads_startup_failed` | Critical | With `[log_heads]`, the posted head could not be read through the custody quorum within the finality budget, or the poster's setup failed. The node refuses to serve: it cannot bound its log's rollback window. See [restart-and-recovery.md](restart-and-recovery.md), "The node will not start" and [feed-and-custody-failures.md](feed-and-custody-failures.md), "Custody RPC providers". A start that names a log-head submission deployment mismatch follows a change of `log_heads.relayer` or `log_heads.contract`: the reservation carries no authority, so move `/data/log/log-head-submission.json` aside and start again. |
| `legacy_withdrawal_unproven` | Critical | An inherited withdrawal has no canonical finality proof. See [restart-and-recovery.md](restart-and-recovery.md), "The node will not start". |
| `legacy_deposit_unproven` | Critical | Legacy principal stays locked: no exact canonical receipt was established. Only on a log that inherited credits from before deposit finality; not expected in production. Call the owner. |
| `log_tail_lost` | Critical | Recovery found the log ends before commands signed heads cover: acknowledged commands are lost from this disk. See [incident-response.md](incident-response.md), "The books look wrong". |

## Running faults

A part of the node stopped or stalled.

| Alert | Level | Meaning and response |
| --- | --- | --- |
| `secrets_restart_required` | Warning | A bundle is sealed that the running node does not use: what runs keeps the secrets it started with. Restart when the rotation is ready, and revoke the old key after. See [key-recovery.md](key-recovery.md), "Secrets". |
| `sequencer_failed` | Critical | The sequencer thread stopped with an error. The node exits. See [restart-and-recovery.md](restart-and-recovery.md), "After any restart". |
| `task_stopped` | Critical | A supervised task stopped, so the node stops. See [restart-and-recovery.md](restart-and-recovery.md), "After any restart". |
| `log_sync_held` | Critical | The oldest unsynced batch has waited over 5 s for its log sync: the sequencer takes no request until the sync finishes, so nothing is stamped behind it. An honest disk syncs in milliseconds; a stalled or full disk, or a host holding the sync, is the cause. Resolves when the sync finishes. The page commits to the alert outbox on the same disk first, so a whole-disk hold delays it: `engine_log_sync_held` (Prometheus `EngineLogSyncHeld`) reports the hold from memory. The snapshot capture's sync, the checkpoint's catalog maintenance, a rotation's catalog fold, and a handover's segment seal run outside this bound and page only as `sequencer_stalled`. A hold that outlasts `alerts.no_price_after` beyond that also raises `sequencer_stalled`, from the same cause. Check the host's disk, then see [restart-and-recovery.md](restart-and-recovery.md), "Restart a healthy node". |
| `sequencer_stalled` | Critical | The sequencer answered no monitor sample for `alerts.no_price_after`. With `log_sync_held` open, a held log sync is the cause. See [restart-and-recovery.md](restart-and-recovery.md), "Restart a healthy node". |
| `history_drain_timed_out` | Warning | On shutdown the history did not finish the stream in time and was stopped. Shutdown only: the history was stopped. Nothing to do unless it repeats on every stop. |
| `checkpoint_accounting_overflow` | Critical | Checkpoint accounting refused engine output; sequencing refuses further commands. See [incident-response.md](incident-response.md), "The books look wrong". |
| `response_signing_failed` | Critical | A response body could not be read to sign; the client got an error. An internal error reached a client. Read the console lines around it; if it repeats, keep the evidence ([incident-response.md](incident-response.md)) and restart the node. |
| `archive_unreadable` | Critical | The archive route could not read a checked segment. An internal error reached a client. Read the console lines around it; if it repeats, keep the evidence ([incident-response.md](incident-response.md)) and restart the node. |
| `history_unreadable` | Critical | The history route could not read the index. See [restart-and-recovery.md](restart-and-recovery.md), "Rebuild the derived stores". |
| `marks_unreadable` | Critical | The marks route could not read the cache. See [restart-and-recovery.md](restart-and-recovery.md), "Rebuild the derived stores". |
| `stream_backfill_failed` | Critical | A stream subscriber's catch-up from disk failed; its stream ended. One subscriber's stream ended. If it repeats, keep the evidence ([incident-response.md](incident-response.md)) and restart the node. |
| `stream_encode_failed` | Critical | A stream message could not be encoded; the subscriber's stream ended. One subscriber's stream ended. If it repeats, keep the evidence ([incident-response.md](incident-response.md)) and restart the node. |

## Derived stores and optional caches

Rebuilt from the log. None changes the books.

| Alert | Level | Meaning and response |
| --- | --- | --- |
| `history_index_discarded` | Warning | The history index is being rebuilt from the log. See [restart-and-recovery.md](restart-and-recovery.md), "Rebuild the derived stores". |
| `history_index_damaged` | Warning | The history index is damaged; pages are withheld until a verified repair. See [restart-and-recovery.md](restart-and-recovery.md), "Rebuild the derived stores". |
| `history_head_rejected` | Warning | A history head pointer was ignored. A pointer file in the history directory was ignored and is rebuilt. Nothing to do. |
| `history_head_write_failed` | Warning | The latest-complete pointer could not be written; it is optional. The pointer is optional. Check the data disk's space and health. |
| `snapshot_rejected` | Warning | A recovery snapshot was ignored. Only with `[snapshots]`, which the template leaves off. Recovery falls back to a full replay. Nothing to do. |
| `snapshot_write_failed` | Warning | A recovery snapshot could not be written; the checkpoint stays in the log. Only with `[snapshots]`. Recovery falls back to a full replay. Check the data disk's space. |
| `snapshot_cleanup_failed` | Warning | Cleanup of old caches or snapshot publications was deferred. Old caches stay until the next write. Check the data disk's space. |
| `log_catalog_unavailable` | Warning | Archive catalog maintenance was deferred; segments wait in memory, then the active one grows. Usually the data disk: check its space and health. The node keeps sequencing and the current segment grows meanwhile. |
| `log_rotation_failed` | Warning | A new log segment cannot be started, so the current one grows. Usually the data disk: check its space and health. The node keeps sequencing and the current segment grows meanwhile. |
| `marks_cache_discarded` | Warning | The marks cache failed validation and is rebuilt. See [restart-and-recovery.md](restart-and-recovery.md), "Rebuild the derived stores". |
| `marks_cache_rebuilt` | Warning | The marks cache is rebuilt after a refused reset or a failed validation. See [restart-and-recovery.md](restart-and-recovery.md), "Rebuild the derived stores". |
| `marks_prior_cache_dropped` | Warning | A prior marks cache could not be read or has another schema and was not imported. A prior cache is rebuilt from the log. Nothing to do. |
| `marks_prior_rows_dropped` | Warning | Prior marks rows failed authentication and were not imported. Those rows are rebuilt from the log. Nothing to do. |
| `marks_samples_dropped` | Warning | The marks queue was full; dropped samples reseed at the next recovery. Dropped samples reseed at the next recovery. If it repeats, look at the sequencer's load (`engine_sequencer_queue_depth`). |
| `marks_seed_failed` | Warning | Seeding the marks cache from the log failed; it is reset for re-derivation. See [restart-and-recovery.md](restart-and-recovery.md), "Rebuild the derived stores". |
| `marks_backfill_failed` | Warning | The optional provider candle backfill failed and retries. The optional backfill retries. History older than the log may be missing meanwhile. |

## Prices, markets and books

| Alert | Level | Meaning and response |
| --- | --- | --- |
| `market_no_price` | Critical | An open market a feed prices has no usable price. See [feed-and-custody-failures.md](feed-and-custody-failures.md), "A market has no price". |
| `market_unfed` | Warning | No configured feed prices this market, so it cannot trade. See [feed-and-custody-failures.md](feed-and-custody-failures.md), "Find what is down". |
| `venue_feed_down` | Warning | A venue or vendor connection is down and retrying. See [feed-and-custody-failures.md](feed-and-custody-failures.md), "A crypto venue is down or quiet". |
| `venue_symbol_silent` | Warning | A symbol has had no fresh quote while its market is open. See [feed-and-custody-failures.md](feed-and-custody-failures.md), "A crypto venue is down or quiet". |
| `venue_message_refused` | Warning | A venue message was refused (key redacted). See [feed-and-custody-failures.md](feed-and-custody-failures.md), "Find what is down". |
| `venue_clock_ahead` | Warning | A venue's timestamps are well ahead of the host clock. See [feed-and-custody-failures.md](feed-and-custody-failures.md), "Find what is down". |
| `venue_delayed` | Warning | A venue's stream may be further behind than `feeds.venues.max_delay`, so its quotes do not count. See [feed-and-custody-failures.md](feed-and-custody-failures.md), "Find what is down". |
| `prices_rejected` | Warning | The engine refused a price update; the reason is in the message. See [feed-and-custody-failures.md](feed-and-custody-failures.md), "Find what is down". |
| `futures_roll_overdue` | Warning | A futures market is past its roll time with no gap recorded. See [feed-and-custody-failures.md](feed-and-custody-failures.md), "Futures markets". |
| `futures_last_contract` | Warning | A futures market prices from the last contract its listing names. See [feed-and-custody-failures.md](feed-and-custody-failures.md), "Futures markets". |
| `benchmark_rate_missing` | Critical | A benchmark has no rate; its markets accrue no financing. See [feed-and-custody-failures.md](feed-and-custody-failures.md), "Benchmark rates". |
| `benchmark_rate_stale` | Critical | The newest rate is over five weekdays old; it still applies. See [feed-and-custody-failures.md](feed-and-custody-failures.md), "Benchmark rates". |
| `benchmark_rate_undated` | Critical | A legacy rate with no date still applies. See [feed-and-custody-failures.md](feed-and-custody-failures.md), "Benchmark rates". |
| `benchmark_fetch_failed` | Warning | A publisher fetch failed; history is retained and the worker retries. See [feed-and-custody-failures.md](feed-and-custody-failures.md), "Benchmark rates". |
| `benchmark_sync_failed` | Warning | The benchmark worker's submission failed; it retries. See [feed-and-custody-failures.md](feed-and-custody-failures.md), "Benchmark rates". |
| `benchmark_schedule_watch_failed` | Warning | The benchmark worker's schedule watch dropped; it reconnects. See [feed-and-custody-failures.md](feed-and-custody-failures.md), "Benchmark rates". |
| `benchmark_rate_refused` | Warning | The worker left out a published rate the `V46` cap refuses; the last applied rate keeps financing. See [feed-and-custody-failures.md](feed-and-custody-failures.md), "Benchmark rates". |
| `benchmark_sync_pending` | Warning | The node waits for the worker's signed history before catching up. See [feed-and-custody-failures.md](feed-and-custody-failures.md), "Benchmark rates". |
| `benchmark_sync_skipped` | Warning | The registry installed an operator whose key has yet to take effect, so the node does not wait for the worker's history; financing uses the last applied rates until a snapshot applies, which resolves it. See [feed-and-custody-failures.md](feed-and-custody-failures.md), "Benchmark rates". |
| `corporate_action_failed` | Critical | A scheduled dividend or split failed; no partial change is committed. See [corporate-actions.md](corporate-actions.md), "When an action fails". |
| `house_deficit` | Warning | House NAV is below zero; trading continues within the cap floors. See [reduce-only.md](reduce-only.md), "When". |
| `bad_debt` | Warning | An account's loss beyond its balance was written off against the house. The house absorbed a loss. Informational: read it with `house_deficit` and the house NAV in `/v1/house`. |

## Trusted time

| Alert | Level | Meaning and response |
| --- | --- | --- |
| `clock_drift` | Critical | The host clock or a stamp is outside the drift bound: sequencing is halted. See [feed-and-custody-failures.md](feed-and-custody-failures.md), "Trusted time". |
| `host_clock_behind` | Warning | The host clock has fallen a second behind monotonic time, and stamps keep monotonic time's pace. See [feed-and-custody-failures.md](feed-and-custody-failures.md), "Trusted time". |
| `trusted_time_unavailable` | Critical | No trusted time sample within the grace: sequencing is halted. See [feed-and-custody-failures.md](feed-and-custody-failures.md), "Trusted time". |
| `trusted_time_no_sample` | Warning | No trusted time sample could be taken. See [feed-and-custody-failures.md](feed-and-custody-failures.md), "Trusted time". |
| `roughtime_server_failing` | Warning | One Roughtime server keeps failing. See [feed-and-custody-failures.md](feed-and-custody-failures.md), "Trusted time". |
| `roughtime_server_disagrees` | Warning | One Roughtime server signed a time outside the agreed sample. See [feed-and-custody-failures.md](feed-and-custody-failures.md), "Trusted time". |

## Custody, deposits and screening

| Alert | Level | Meaning and response |
| --- | --- | --- |
| `custody_rpc_provider_failed` | Warning | One custody provider failed an operation; the quorum holds. See [feed-and-custody-failures.md](feed-and-custody-failures.md), "Custody RPC providers". |
| `custody_rpc_provider_disagreed` | Warning | One custody provider disagrees with the quorum. See [feed-and-custody-failures.md](feed-and-custody-failures.md), "Custody RPC providers". |
| `custody_rpc_quorum_lost` | Critical | No custody answer has a majority: crediting is paused (a Warning with `quorum=operators`: only the operator list is paused). See [feed-and-custody-failures.md](feed-and-custody-failures.md), "Quorum lost". |
| `operators_read_failed` | Warning | The operator list cannot be read at a final block (no provider quorum, a wrong chain or token, or an inconsistent lens). `/v1/operators` serves the last list until `custody.evm.finality_stalled_after`, then answers 503. The reader's quorum alerts above carry `quorum=operators` and page at Warning. See [feed-and-custody-failures.md](feed-and-custody-failures.md), "Custody RPC providers". |
| `custody_rpc_failed` | Warning | A deposit poll failed on a read. See [feed-and-custody-failures.md](feed-and-custody-failures.md), "Custody RPC providers". |
| `custody_chain_unsettled` | Warning | The chain's answers lag; the poll retries. See [feed-and-custody-failures.md](feed-and-custody-failures.md), "Custody RPC providers". |
| `custody_chain_inconsistent` | Critical | The chain's answers contradict the config or the cursor. See [feed-and-custody-failures.md](feed-and-custody-failures.md), "Custody RPC providers". |
| `custody_deposits_stalled` | Critical | No vault deposit was read for `stall_critical_after`. See [feed-and-custody-failures.md](feed-and-custody-failures.md), "Custody RPC providers". |
| `custody_deposits_lagging` | Critical | Deposit reads trail the final block. See [feed-and-custody-failures.md](feed-and-custody-failures.md), "Custody RPC providers". |
| `custody_head_stale` | Critical | A provider's head is not advancing. See [feed-and-custody-failures.md](feed-and-custody-failures.md), "Custody RPC providers". |
| `custody_finality_stalled` | Critical | The `finalized` block has stopped advancing while the head moves. See [feed-and-custody-failures.md](feed-and-custody-failures.md), "Custody RPC providers". |
| `custody_unavailable` | Critical | A signed withdrawal has no custody consumer. A signed withdrawal was sequenced with no custody to pay it: a mock or misconfigured custody. Not expected in production. Call the owner. |
| `deposit_rejected` | Critical | A deposit was not credited. See [feed-and-custody-failures.md](feed-and-custody-failures.md), "Custody RPC providers". |
| `deposit_reversed` | Critical | The quorum proved a credited deposit reversed. See [feed-and-custody-failures.md](feed-and-custody-failures.md), "Custody RPC providers". |
| `custody_deposit_rejected` | Critical | A custody transition was refused. See [feed-and-custody-failures.md](feed-and-custody-failures.md), "Custody RPC providers". |
| `house_deposit_rejected` | Critical | A house deposit was not credited. See [feed-and-custody-failures.md](feed-and-custody-failures.md), "Custody RPC providers". |
| `deposit_unrepresentable` | Critical | A deposit is too large to credit. See [feed-and-custody-failures.md](feed-and-custody-failures.md), "Custody RPC providers". |
| `staking_rejected` | Critical | A staking event was not mirrored; the engine's stake has diverged. A finalized event the engine will not mirror leaves its stake diverged from the chain. Do not restart to clear it. Call the owner with the reason in the message. |
| `election_rejected` | Critical | A registry event was not mirrored; the operator key has diverged. A finalized registry event the engine will not mirror leaves its operator key diverged from the chain. Do not restart to clear it. Call the owner with the reason in the message. |
| `access_source_failed` | Warning | The access source refused or failed; the request stays unexecuted. See [feed-and-custody-failures.md](feed-and-custody-failures.md), "The access source". |
| `access_deposit_retry_failed` | Warning | Pending deposits stay uncredited: screening retries fail. See [feed-and-custody-failures.md](feed-and-custody-failures.md), "The access source". |
| `access_deposit_held` | Critical | A deposit is held by a screening decision. See [feed-and-custody-failures.md](feed-and-custody-failures.md), "The access source". |
| `withdrawals_paused` | Warning | A withdrawal was refused until custody reconciles and the vault admits claims. See [withdrawal-stalls.md](withdrawal-stalls.md), "Find the cause". |

## Checkpoints, collateral and withdrawals

| Alert | Level | Meaning and response |
| --- | --- | --- |
| `checkpoint_backlog` | Warning | No checkpoint became final for too long, or the posting queue is full. See [withdrawal-stalls.md](withdrawal-stalls.md), "checkpoint_backlog". |
| `checkpoint_relayer_low` | Warning | The checkpoint relayer's balance is below `checkpoints.relayer_gas.min_balance`; posts stop when it cannot pay for gas. Resolves once refilled. See [feed-and-custody-failures.md](feed-and-custody-failures.md), "Checkpoint endpoint". |
| `checkpoint_post_failed` | Warning | A checkpoint post failed and is retried. See [withdrawal-stalls.md](withdrawal-stalls.md), "Posting fails". |
| `log_head_read_failed` | Warning | With `[log_heads]`, reading the posted head through the custody quorum failed; the poster retries and the last read still bounds the truncation window. Resolves when a read succeeds. See [feed-and-custody-failures.md](feed-and-custody-failures.md), "Custody RPC providers". |
| `log_head_post_failed` | Warning | With `[log_heads]`, posting the newest released head failed; the poster retries. Until a post lands, the truncation window is the unposted tail. Resolves when a post is accepted. See [feed-and-custody-failures.md](feed-and-custody-failures.md), "Checkpoint endpoint". |
| `log_head_stalled` | Warning | With `[log_heads]`, the on-chain head has not advanced for over three posting intervals while newer heads were released: the pending send may be dropped, and the poster reconciles its stored hash against the endpoint on the next attempt; or the chain holds a head at a seq the poster retains no prefix for, which it does not adopt as a parent, so its posts fail until a restart checks that head against the whole log. Resolves when a quorum read observes the head advance — acceptance proves no inclusion. See [feed-and-custody-failures.md](feed-and-custody-failures.md), "Checkpoint endpoint". |
| `checkpoint_withheld` | Warning | The node is not signing or posting a checkpoint; `cause` says why. See [withdrawal-stalls.md](withdrawal-stalls.md), "Checkpoints are held". |
| `checkpoint_conflict` | Critical | The vault holds a root or signer the log does not; the node stops. See [withdrawal-stalls.md](withdrawal-stalls.md), "The node has stopped". |
| `vault_exit_started` | Critical | The vault's exit has begun; no checkpoint can follow; the node stops. See [incident-response.md](incident-response.md), "Checkpoints cannot resume". |
| `vault_withdrawals_halted` | Critical | The vault is paused or has no signer; no authorization is signed. See [withdrawal-stalls.md](withdrawal-stalls.md), "The vault is paused or has no signer". |
| `collateral_mismatch` | Critical | The ledger and the vault disagree; withdrawals are paused. See [withdrawal-stalls.md](withdrawal-stalls.md), "Collateral does not reconcile". |
| `collateral_pending_deposits` | Warning | The finalized checkpoint's deposit coverage is unproven; withdrawals are paused. See [withdrawal-stalls.md](withdrawal-stalls.md), "Collateral does not reconcile". |
| `collateral_surplus` | Warning | The vault holds USDC no deposit brought in. See [withdrawal-stalls.md](withdrawal-stalls.md), "Collateral does not reconcile". |

## Certificates and attestation

| Alert | Level | Meaning and response |
| --- | --- | --- |
| `certificate_pending` | Warning | No CA certificate yet; the self-signed one is served. See [certificate-renewal-failure.md](certificate-renewal-failure.md), "Diagnose". |
| `certificate_expiring` | Warning | Under 14 days remain and renewal is failing. See [certificate-renewal-failure.md](certificate-renewal-failure.md), "Diagnose". |
| `sealed_keys_write_failed` | Warning | The ACME account or certificate could not be sealed; a restart orders again. See [certificate-renewal-failure.md](certificate-renewal-failure.md), "Diagnose". |
| `collateral_revoked` | Critical | The vendor revoked the attestation chain; verifiers now refuse the quote. Verifiers refuse the quote now. Nothing on the node fixes it. Call the owner. |
| `collateral_stale` | Warning | Attestation collateral is stale or missing and refreshing fails. The node keeps serving the last good chain. Check its outbound HTTPS to `tee.kds_base` and the vendor's service. |
| `unattested_certificate` | Critical | A certificate for the API domain has a key no verified attestation names (from `ct-watch`). See [unattested-certificate.md](unattested-certificate.md). |
| `attestation_unverified` | Critical | The watcher cannot verify the node's attestation (from `ct-watch`). See [unattested-certificate.md](unattested-certificate.md), "1. Is the watcher itself healthy?". |
| `ct_log_unreachable` | Critical | A followed CT log is not read to a recent tree head (from `ct-watch`). See [unattested-certificate.md](unattested-certificate.md), "1. Is the watcher itself healthy?". |
| `ct_watch_stopped` | Critical | The watcher stopped; the domain is no longer watched (from `ct-watch`). See [unattested-certificate.md](unattested-certificate.md), "1. Is the watcher itself healthy?". |

## Operator, release and standby

| Alert | Level | Meaning and response |
| --- | --- | --- |
| `release_superseded` | Critical | The vault names another release or generation; a restart would find no keys. See [deploy-and-rollback.md](deploy-and-rollback.md), "Upgrade to a new release, step 2". |
| `handover_unconfirmed` | Critical | A handover has not been confirmed or acknowledged. See [deploy-and-rollback.md](deploy-and-rollback.md), "Upgrade to a new release, step 5". |
| `handover_failed` | Critical | The node stopped before a successor confirmed; its keys no longer open. See [key-recovery.md](key-recovery.md), "The running release stopped". |
| `operator_elected` | Warning | An election names another operator (incumbent). See [operator-candidate.md](operator-candidate.md), "Handing the node to the next operator". |
| `handover_overdue` | Critical | The election's effective time passed with no successor confirmed (incumbent). See [operator-candidate.md](operator-candidate.md), "Handing the node to the next operator". |
| `standby_follow_failed` | Critical | The standby could not follow the operator's stream. See [standby.md](standby.md), "Run the standby". |
| `operator_log_rollback` | Critical | The operator's log no longer holds entries the standby copied; the standby keeps its copy unchanged. See [standby.md](standby.md), "Run the standby". |
| `standby_signal_failed` | Critical | The standby could not read the registry or vault for a promotion. See [standby.md](standby.md), "Run the standby". |
| `standby_takeover_failed` | Critical | A promotion arrived and the standby's copy could not be finalized. See [standby.md](standby.md), "After a promotion". |

## Seed relay

| Alert | Level | Meaning and response |
| --- | --- | --- |
| `seed_missing` | Critical | The turn whose draw deadline last passed has no relayed seed, so it draws from the last relayed seed hashed with its number. Every member's relay failed or was withheld. Resolves once a turn reaches its draw deadline with its seed. See [seed-relay.md](seed-relay.md), "When `seed_missing` fires". |
| `seed_relay_failed` | Warning | This node could not relay a turn's seed the receiver lacks, or a seed it relayed had not reached Arbitrum by `seed_relay.resend_after`. Resolves once both upcoming turns have their seeds. See [seed-relay.md](seed-relay.md), "When `seed_relay_failed` fires". |
| `seed_relayer_unfunded` | Warning | The seed relayer's Ethereum balance is below `seed_relay.min_balance`; relays stop when it cannot pay for one. Resolves once refilled. See [seed-relay.md](seed-relay.md), "When `seed_relayer_unfunded` fires". |
| `seed_relayer_balance_unreadable` | Warning | The seed relayer's Ethereum balance has been unreadable for ten minutes, so `engine_seed_relayer_balance_eth` stands still and `seed_relayer_unfunded` cannot fire while relays keep spending the key. The message names the cause. Resolves at the next read. See [seed-relay.md](seed-relay.md), "When `seed_relayer_balance_unreadable` fires". |

## API limits

Raised at most once a minute. The API refuses clients while it is true.

| Alert | Level | Meaning and response |
| --- | --- | --- |
| `api_connections_full` | Warning | The connection cap every client shares is full. Clients are being refused. Look for a flood from a few addresses; a cap that is too low for real traffic is a config change, so a new release. |
| `api_subscribers_full` | Warning | The stream subscriber cap is full. As above, for stream subscribers. |
| `api_initial_budget_full` | Warning | The shared budget for accounts that have not deposited is exhausted. Accounts that have not deposited share one request budget, and it is exhausted. Look for a flood from a few addresses. |
| `api_paid_declaration_budget_full` | Warning | The budget all declarations from wallets with a pending deposit share is exhausted. These wallets have paid, so they declare past the unpaid budgets, but they share this one, at the same rate. Look for a few clients repeating declarations the engine refuses, such as from a restricted region; their deposits stay pending until a declaration is recorded. |
| `api_declaration_slots_full` | Warning | The screening slots for declarations are full. The screening slots for declarations are full. Look for a flood; check the access source's latency. |
| `api_accept_failed` | Warning | Accepting connections is failing. Check the host's open-file limit and its network. |
| `api_proxy_header_refused` | Warning | A trusted proxy's connection has no valid PROXY protocol header. Check `[api.proxy_protocol].trusted` against the proxy's real addresses, and that the proxy sends PROXY v2 headers. |

## Prometheus rules

`ops/prometheus/engine-node-rules.yml` holds eight rules for the operator's Prometheus, which runs
outside the enclave and pages through a receiver other than the node's own webhook. They watch what
the node cannot page itself, and escalate two certificate alerts.

| Rule | Fires when | Node alert it pairs with |
| --- | --- | --- |
| `EngineNodeDown` | `up{job="engine"} == 0` for a minute | None: a node that is down cannot page. [restart-and-recovery.md](restart-and-recovery.md). |
| `EngineAlertPagesFailing` | Pages failed or were dropped in 15 minutes (`engine_alert_deliveries_total`) | None: it is about the pages themselves. [incident-response.md](incident-response.md), "Pages stop arriving". |
| `EngineAlertDeliveryFailing` | Three failed posts in 5 minutes (`engine_alert_attempts_failed_total`) | None, as above. |
| `EngineAlertRouteOff` | No delivered bundle holds a severity's webhook URL, so its alerts are only logged (`engine_alert_route_off`) | `secret_missing`, which that route cannot page. [key-recovery.md](key-recovery.md), "Secrets". |
| `EngineAlertLinesDropped` | Alert lines dropped before routing (`engine_alerts_dropped_total`) | None, as above. |
| `EngineAlertOutboxOverflow` | Pages refused at the outbox bound (`engine_alert_outbox_overflows_total`) | None, as above. |
| `EngineTlsCertificateMissing` | `engine_tls_certificate_pending == 1` for 30 minutes | `certificate_pending` ([certificate-renewal-failure.md](certificate-renewal-failure.md)) |
| `EngineTlsCertificateExpiring` | The certificate expires within 7 days (`engine_tls_certificate_not_after_seconds`) | `certificate_expiring` (same runbook) |

Six of the eight rules have no node alert the node can page behind them, by design. They exist because the node cannot
page that it, or its pages, are not getting through. The watcher exports the same alert metrics, so
scrape it in the same job and the rules cover it.

## Keeping this page right

Every alert name in the code appears above, and no name appears that the code does not raise. Check
with:

```sh
rg -o --no-filename 'alert = "[a-z_]+"' crates/node/src --glob '!**/tests/**' --glob '!**/*tests.rs' --glob '!**/test_tracing.rs' | sed 's/alert = //; s/"//g' | sort -u
```

That list is missing alerts raised through a variable: `custody_rpc_failed`, `custody_chain_unsettled`
and `custody_chain_inconsistent` (`custody/stall.rs`) and the six `api_*` alerts (`ThrottledAlert::new`).
