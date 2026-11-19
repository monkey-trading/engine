# Restart and recovery

Use this runbook to restart the trading server, to find out why a start refuses, and to recover from
a lost or damaged data disk. The trading server's state is its log. Every instruction is written to
the disk before a client is told that it applied, so a stopped or crashed trading server loses no
acknowledged instruction. A restart replays the log, checks it against the vault, opens the sealed
keys, and carries on.

The steps use the shell variables that [Variables](reference.md#variables) sets up. Commands and
their output call a running trading server a node.

## Facts that shape every step

- **Every stop of the image is a hard stop.** The guest ignores an ACPI shutdown request, so the
  engine neither drains its queue nor writes a final checkpoint. Instructions after the last
  checkpoint stay in the log, and the next checkpoint that the restarted trading server signs covers
  them. A trading server run by hand, outside the image, drains and writes a final checkpoint on
  SIGTERM or Ctrl-C.
- **The log lines go to the serial console.** Capture the guest's serial console to a file on the
  host, and keep it. Every `alert =` line that this runbook mentions is read there, and the webhook
  receives the same alerts as pages.
- **A restart replays the whole log, so it takes longer as the log grows.** The production template
  leaves `[snapshots]` off, so every start replays every instruction in the log before it sequences
  again, and the API answers nothing meanwhile. The replay checks the instructions' seals on a pool of
  threads, one per core unless `node.replay_workers` sets another number, so it runs faster on more
  cores. On a book of 100,000 accounts, a debug build on a shared development machine, with six
  threads checking seals, recovered about 6,300 instructions a second, seeding the marks cache and
  the dashboard record as it went. A weekday of launch flow logs about 9.8 million instructions, so
  each such day adds about 26 minutes to a restart, and 30 days of launch flow, about 246 million
  instructions, add about 11 hours. Plan a restart's downtime from the length of the log.
- **A restart keeps the keys.** With `[tee.sealing]`, the same image and configuration on the same
  chip open the same sealed keys. The enclave address, the signer epoch, the TLS key and therefore
  `$PIN`, the certificate, and the outstanding withdrawal authorisations are unchanged. A restart
  needs no governance action.
- **A restart can wait on the chain.** If a posted checkpoint has not yet become `finalized`, the
  start waits for it before it opens the log, for up to `custody.evm.finality_stalled_after`, which
  is 30 minutes in the template. After a restart that follows a recent post, expect the API to take
  up to that long to answer.
- **Time must be trusted first.** Nothing is sequenced until the first Roughtime sample arrives, and
  sequencing halts if the host's clock drifts. See `clock_drift` in [Alerts](alerts.md).
- **Deadlines run while the trading server is down,** as
  [Deadlines that run while the trading server is down](reference.md#deadlines-that-run-while-the-trading-server-is-down)
  describes. Note the time of the last checkpoint before you begin.

## Restart a healthy trading server

Use this procedure when a working trading server must restart: to pick up a rotated secret, or when
an alert's entry in [Alerts](alerts.md) sends you here. Clients cannot trade from the stop until the
trading server sequences again.

1. Note the time and the sequence number of the last checkpoint:

   ```shell
   cast call $VAULT 'lastCheckpointAt()(uint64)' --rpc-url $RPC
   cast call $VAULT 'latestSeq()(uint64)' --rpc-url $RPC
   ```

2. If new risk during the restart is a concern, make the markets reduce-only first, as
   [Make markets reduce-only](reduce-only.md) describes.

3. Stop the VM, and start it again with the same data disk and no marker files: no `new-key` and no
   `predecessor`.

   **Changes the live system.** The owner must approve this restart.

4. Watch the console. A healthy start logs these lines, in this order:

   - the enclave, the configuration and the TLS key, attested;
   - the history index, opened;
   - `recovered from log`, with the counts of instructions and checkpoints;
   - the Roughtime servers;
   - `metrics up`;
   - `api up over TLS`;
   - `first trusted time sample: sequencing started`.

5. Check the trading server:

   ```shell
   curl -sf $API/v1/health
   curl -sf $API/v1/attestation | jq -r .enclave_address
   curl -sf $API/v1/attestation | jq -r '.tls_public_key_sha256[2:]'
   ```

   The first command prints `ok`. The second prints `$SIGNER`, and the third prints `$PIN`, so the
   pin is unchanged. Then confirm the following:

   - `cast call $VAULT 'latestSeq()(uint64)' --rpc-url $RPC` rises again within a few minutes.
   - The metric `engine_withdrawals_paused` returns to `0` after the first reconciliation. It reads
     `1` until then.
   - No page is open that was not open before the restart, and the pages from the stop, such as
     `EngineNodeDown`, resolve.

6. Restart the companion processes that depend on the trading server, which is the benchmark
   worker, as [Companion processes](deploy-and-rollback.md#companion-processes) describes. A
   trading server with `[benchmarks]` sequences only required instructions until the worker has
   delivered a snapshot, and `benchmark_sync_pending` fires meanwhile. The exception is when the
   registry has installed an operator in place of the log's key, which `benchmark_sync_skipped`
   reports, as [Running as a standby](standby.md) describes.

7. Tell clients that any signed request to reissue a withdrawal that was queued before the restart
   was lost. The account sends the same signed body again, which stays valid until a new
   authorisation replaces it.

**How to tell it is resolved.** The checks in step 5 pass, and `latestSeq()` rises.

**When to escalate.** If the start refuses, follow
[The trading server will not start](#the-trading-server-will-not-start).

## The trading server will not start

Use this procedure when a start fails. The console prints `refusing to serve: <cause>` under one of
the alerts in the table below, and the image then powers the VM off. Clients cannot trade until the
trading server starts.

**Do not delete log files, the checkpoint sidecar or the sealed keys to make a refusal go away.**
Each refusal is a check that has found something wrong, and the log is the only record of the
books.

1. Read the cause after the colon in the console's `refusing to serve:` line, and the alert it
   appears under.

2. Find the alert in this table, and take the first action:

   | Alert | What it checks | First action |
   | --- | --- | --- |
   | `secret_missing`, `secrets_unreadable` | These are not refusals. The trading server lacks a delivered secret, or its sealed secrets file does not open. For a chain or screening key, it holds and serves only its delivery listener. | Deliver the secrets, as [Secrets](key-recovery.md#secrets) describes. The start then continues. |
   | `secrets_start_failed` | The first read of the chain failed on a delivered key. The VM powers off after five minutes, or once a new bundle is sealed. | See `secrets_start_failed` in [Alerts](alerts.md). |
   | `clocksource_unprotected` | Whether the guest kernel times on the TSC that Secure TSC protects, at the frequency the hardware reports. The cause names the clock that the kernel uses instead, because the host made the kernel mark the TSC unstable at boot, for example by naming another hypervisor in its CPUID leaves. Or the cause names a frequency below 1000 MHz, because the host asked at launch for a frequency the hardware cannot count in, and the kernel calibrated the TSC against clocks the host emulates. | For another clock, the kernel's earlier console line `Marking TSC unstable due to <reason>` says why. Start the VM as [Boot on an SEV-SNP host](../../image/README.md#boot-on-an-sev-snp-host) describes, with no other hypervisor's CPUID leaves. For a low frequency, the host's launch request is wrong, so launch again asking for the processor's TSC frequency. Never run the trading server on another clock. |
   | `enclave_key_failed` | Whether the sealed keys can be opened or made. They cannot when none are sealed for this release and generation, the configuration changed, the chip is another one, the firmware is lower, or a `new-key` marker sits beside an existing file. | Follow [Key recovery](key-recovery.md). |
   | `signer_unregistered` | This is not a refusal. The vault does not name this key, so the trading server serves only its attestation. | Governance registers the key, as [New keys](key-recovery.md#new-keys) describes. |
   | `custody_connect_failed` | Whether the custody adapter can start. It cannot when the providers are unreachable, or too few agree on the chain and the token. | Follow [Feed and custody failures](feed-and-custody-failures.md). |
   | `checkpoint_startup_failed` | The log against the vault, before anything is written. The cause is one of `vault signer differs from this enclave; signer recovery is required`, `log is shorter than the latest posted checkpoint`, `log differs from the latest posted checkpoint at seq N`, `vault did not authenticate the posted checkpoint`, a checkpoint RPC that cannot be read, or a checkpoint sidecar naming work that the log does not hold. | For a log behind or different from the vault, follow [A log shorter than the vault](#a-log-shorter-than-the-vault). For an unreadable RPC, follow [Feed and custody failures](feed-and-custody-failures.md). With a staking configuration in the log or the genesis, the cause `cannot read the staking contract's epoch and cooldown` means that the custody providers' quorum is lost, as [Feed and custody failures](feed-and-custody-failures.md) describes. |
   | `log_rollback` | With `[log_heads]`, the log against the newest head that this enclave key posted on chain. That head is the current signer epoch's, or, while that epoch holds none, the newest head of the earlier epochs that the key signed. The info line `startup checks the log against the head signer epoch <epoch> posted`, before the alert, names the epoch. The refusal means that the log ends before the posted sequence number, or that the log's cumulative command-prefix digest differs at that number. The disk was rolled back, the host cut the log, or a second copy of the key sequenced a fork. | Follow [A log the posted head does not hold](#a-log-the-posted-head-does-not-hold). |
   | `log_heads_startup_failed` | With `[log_heads]`, whether the posted head can be read through the custody quorum within the finality budget, and whether the poster can be built. After a change of `log_heads.relayer` or `log_heads.contract`, the cause can also be a log-head submission deployment mismatch. The stored reservation carries no authority. | For a read that fails, follow [Custody RPC providers](feed-and-custody-failures.md#custody-rpc-providers). For a deployment mismatch, move `/data/log/log-head-submission.json` aside, and start again. |
   | `log_recovery_failed` | The log against the configuration and the rules. It refuses a configuration that names another operator, client authentication, access policy, chain, vault or genesis than the log records. It also refuses a signature or root that does not verify, and an instruction after the last checkpoint that the enclave key did not seal. | Compare the configuration with the log's banner, as step 3 describes. A configuration that differs from what the log records is the wrong configuration, so use the one the log was written under. Never edit the log. |
   | `history_recovery_failed`, `marks_cache_failed` | Whether the account history index and the marks cache can be opened or repaired. Both are derived from the log. | Follow [Rebuild the derived stores](#rebuild-the-derived-stores). |
   | `staking_terms_mismatch` | Whether the staking configuration that the log records, or that the configuration's genesis would record, has the staking contract's `epoch()` and `cooldown()`. | If the genesis differs, fix the configuration and make a new release. If the log differs, it cannot be repaired on a live log, so stop and call the owner. |
   | `emission_budget_mismatch` | Whether the emission schedule that the log records has the period budget from which the vault's MONKEY cap was derived. | Stop and call the owner. It cannot be repaired on a live log. |
   | `genesis_refused`, `protocol_activation_failed` | Whether the configuration's genesis, or the newest protocol version, applies to this log. | The configuration is wrong for this log, or the build is older than the log. Use the release that wrote the log, as [Know before you start](deploy-and-rollback.md#know-before-you-start) describes. |
   | `benchmark_sync_prerequisites_missing` | Whether a log with a rollover schedule records the client authentication, the operator and the toxicity weighting. | A production log always records them, so call the owner. |
   | `collateral_relay_failed` | Whether the vendor collateral relay can be set up. With SEV-SNP, the usual cause is a missing `tee.processor`. | Fix the configuration, which needs a new release. |
   | `legacy_withdrawal_unproven` | Whether each inherited withdrawal has a canonical finality proof. | This is not expected on a production log, so call the owner. |

3. To compare the configuration with what the log records, replay a downloaded archive, as
   [Download the public log and verify it](reference.md#download-the-public-log-and-verify-it)
   describes:

   ```shell
   $ENGINE replay --log archive
   ```

   The replay prints the log's banner:

   ```text
   verified: <n> commands, <n> checkpoints, final root 0x…, protocol V…, client auth …, operator 0x… (chain …, vault 0x…)
   checkpoint signers: 0x…
   ```

   The operator, chain and vault must equal the address of the configuration's
   `operator.public_key`, `custody.chain_id` and `custody.vault`.

**When to escalate.** Call the owner for every refusal that the table says cannot be repaired on a
live log, and before you replace a log.

### A log shorter than the vault

Use this procedure when `checkpoint_startup_failed` names
`log is shorter than the latest posted checkpoint` or
`log differs from the latest posted checkpoint`. The log on this disk ends before a checkpoint that
the vault has already accepted, or its bytes differ from it. The disk was rolled back or replaced,
or the host altered it. The trading server refuses, because signing on this log would diverge from
the vault.

1. Do not start the trading server again, and keep the disk as it is.

2. Find a copy of the log that reaches the vault's `latestSeq`. Look first at a standby's copy, as
   [Running as a standby](standby.md) describes, and then at the archive mirrors. A mirror holds
   completed segments only, so the tail after its last segment is not in it.

3. Check the candidate copy, where `<candidate>` is its directory:

   ```shell
   $ENGINE replay --log <candidate>
   cast call $VAULT 'latestSeq()(uint64)' --rpc-url $RPC
   cast call $VAULT 'latestRoot()(bytes32)' --rpc-url $RPC
   ```

   The replay prints a line that begins `verified:`. Its last checkpoint's sequence number is at
   least the vault's `latestSeq`, with the same root as `latestRoot`.

4. Put the copy on a fresh data disk with the configuration, and start the trading server as
   [Restart a healthy trading server](#restart-a-healthy-trading-server) describes.

   **Changes the live system.** The owner must approve replacing the log.

**When to escalate.** If no copy reaches the vault's checkpoint, the books after the last mirrored
checkpoint cannot be rebuilt. Stop, and call the owner. This is the case that the exit exists for,
as [Checkpoints cannot resume](incident-response.md#checkpoints-cannot-resume) describes.

### A log the posted head does not hold

Use this procedure when `log_rollback` fires at start, naming a range of sequence numbers that the
log does not hold, or one sequence number whose link differs. The LogHeads contract holds a head
signed by this enclave key that this disk's log does not reproduce. The disk was rolled back or the
host cut the log, or a second copy of the key sequenced a fork. The trading server refuses before it
writes anything.

A standby's takeover drops the outgoing key's tail by design, and does not raise this alert. Posted
heads are kept for each signer epoch, and the new epoch starts with none.

1. Do not start the trading server again, and keep the disk as it is.

2. Read the head that the start checked. The console line
   `startup checks the log against the head signer epoch <epoch> posted` names its epoch:

   ```shell
   cast call $LOG_HEADS 'heads(uint64)(uint64,bytes32,uint64,uint64,uint64)' <epoch> --rpc-url $RPC
   ```

   The first two values are the head's sequence number and link. Without that console line, start
   from the vault's current epoch:

   ```shell
   cast call $VAULT 'signerEpoch()(uint64)' --rpc-url $RPC
   cast call $VAULT 'signerAtEpoch(uint64)(address)' <epoch> --rpc-url $RPC
   ```

   While `heads` answers sequence number 0 and `signerAtEpoch` is the enclave's address, try the
   epoch before. The alert named the missing sequence numbers, or the sequence number whose link
   differs.

3. **If the refusal is at the same sequence number with a different link, this is a fork, not a
   disk problem.** The key sequenced on another copy. Stop, and call the owner.

4. Otherwise, find a copy that holds the missing range. Look first at a standby's copy, as
   [Running as a standby](standby.md) describes, and then at the archive mirrors. A mirror holds
   completed segments only, so a tail inside the active segment is not in it.

5. Check the candidate copy, where `<candidate>` is its directory:

   ```shell
   $ENGINE replay --log <candidate>
   ```

   The replay prints a line that begins `verified:`, and its last sequence number is at least the
   posted one. The start compares the posted link itself, so it refuses a wrong copy before it
   writes anything.

6. Put the copy on a fresh data disk with the configuration, and start the trading server as
   [Restart a healthy trading server](#restart-a-healthy-trading-server) describes.

   **Changes the live system.** The owner must approve replacing the log.

**When to escalate.** Call the owner for a fork, and when no copy holds the missing range.

### Rebuild the derived stores

Use this procedure when `history_recovery_failed`, `marks_cache_failed`, or an alert about the
history index or the marks cache sends you here. The account history index and the marks cache are
derived from the log, and they are rebuilt from it when their directory is gone. Account history
and price history may be unavailable to clients until the rebuild finishes. The books are not
affected.

1. Stop the VM, and mount the data disk on the host.

   **Changes the live system.** The owner must approve the restart.

2. Remove `history/`, and nothing else.

3. Start the VM. The rebuild is a replay, so allow time in proportion to the length of the log.

**How to tell it is resolved.** The console reaches `history index holds the log's tip`.

## A lost or damaged data disk

Use this table when part of the data disk is lost or damaged.

| What is lost | Effect | What to do |
| --- | --- | --- |
| `history/` only | Nothing is lost, and the index rebuilds. | Follow [Rebuild the derived stores](#rebuild-the-derived-stores). |
| `alerts/` | Pages not yet delivered are lost. | Start the trading server. The outbox is created empty. Check `engine_alert_pages_pending`. |
| The cursors | The deposit and staking scans start again from `custody.evm.start_block`, and credit nothing twice. | Start the trading server. The scan is slow to catch up, and `custody_deposits_lagging` fires meanwhile. |
| The log | The books. | Find a copy that reaches the vault's `latestSeq`, as [A log shorter than the vault](#a-log-shorter-than-the-vault) describes. |
| The sealed keys, or the chip | The signing key. | Follow [New keys](key-recovery.md#new-keys). |
| `config.toml` | The release's configuration. | Take the published `node.toml` of the release that the vault names. Compute the release from the file's `sha256sum` and the release's launch digest, as steps 1 and 2 of [How the enclave key is kept](key-recovery.md#how-the-enclave-key-is-kept) describe. The result must equal the vault's `currentRelease()`, or the keys do not open. |

## After any restart

- Write down what stopped, when it stopped, and how long the checkpoints were stale, from
  `lastCheckpointAt()` before and after. The owner needs this if a deadline came close.
- If the stop was not planned, find its cause in the console log before you close the incident.
  `sequencer_failed`, `task_stopped` or `sequencer_stalled` means a fault in the trading server.
- A `log_rollback` while running means that an on-chain head overtook this log, through a
  truncation or a second copy of the key. **Do not restart onto a cut-down disk**, as
  [A log the posted head does not hold](#a-log-the-posted-head-does-not-hold) describes.
- In either case, read [Incident response](incident-response.md).
